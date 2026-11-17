//! M13.02 measurements. Markdown to stdout. Deterministic (fixed seed), no I/O.
//!
//!     cargo run --release -p hsearch-lane --example fairness_profile
//!
//! Four scenarios, each against a control that lacks the mechanism: one hot tenant among many
//! small ones (admission against a plain global cap), a cold cache storm (quota against one shared
//! LRU), AZ imbalance and skew (weighted placement), and the cost of a placement. Provisional: a
//! model driven by a fixed arrival process and one shared host's CPU; relative, not absolute.
use std::collections::VecDeque;
use std::time::Instant;

use hsearch_format::Id;
use hsearch_lane::admission::{Admission, Config as AdmissionConfig, Permit};
use hsearch_lane::quota::{CacheQuota, Config as QuotaConfig};
use hsearch_lane::rendezvous::{Ask, Fleet, Node};

fn id(n: u64) -> Id {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&n.to_be_bytes());
    out[15] = 0xA5;
    out
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

/// Outcome of one admission run.
struct Run {
    small_offered: u64,
    small_refused: u64,
    hot_offered: u64,
    hot_served: u64,
    util: f64,
}

/// `reserve`: Some(permille) runs the fair admission; None the control (one global cap, first come
/// first served, which is what a node without per-tenant admission does).
fn admission(reserve: Option<u16>) -> Run {
    const CAPACITY: u64 = 1000;
    const SERVICE: usize = 10;
    const TICKS: usize = 2000;
    const SMALL: u64 = 999;
    let mut fair = reserve.map(|r| {
        Admission::new(AdmissionConfig {
            capacity: CAPACITY,
            reserve_permille: r,
        })
        .unwrap()
    });
    let mut rng = Lcg(7);
    let mut ring: Vec<Vec<Permit>> = (0..SERVICE).map(|_| Vec::new()).collect();
    let mut control_ring = [0u64; SERVICE];
    let mut control_used = 0u64;
    let mut run = Run {
        small_offered: 0,
        small_refused: 0,
        hot_offered: 0,
        hot_served: 0,
        util: 0.0,
    };
    let mut used_sum = 0u64;
    for tick in 0..TICKS {
        let slot = tick % SERVICE;
        // Completions.
        if let Some(a) = fair.as_mut() {
            for p in ring[slot].drain(..) {
                a.release(p);
            }
        } else {
            control_used -= control_ring[slot];
            control_ring[slot] = 0;
        }
        // Arrivals: the hot tenant offers 400 a tick; each small tenant 2% a tick (about 20).
        let mut arrivals: Vec<u64> = vec![0; 400];
        for t in 1..=SMALL {
            if rng.next() % 100 < 2 {
                arrivals.push(t);
            }
        }
        for i in (1..arrivals.len()).rev() {
            let j = (rng.next() as usize) % (i + 1);
            arrivals.swap(i, j);
        }
        let measured = tick >= 100;
        for who in arrivals {
            let hot = who == 0;
            let admitted = match fair.as_mut() {
                Some(a) => match a.admit(id(who), 1, 1) {
                    Ok(p) => {
                        ring[slot].push(p);
                        true
                    }
                    Err(_) => false,
                },
                None if control_used < CAPACITY => {
                    control_used += 1;
                    control_ring[slot] += 1;
                    true
                }
                None => false,
            };
            if measured {
                if hot {
                    run.hot_offered += 1;
                    run.hot_served += u64::from(admitted);
                } else {
                    run.small_offered += 1;
                    run.small_refused += u64::from(!admitted);
                }
            }
        }
        if measured {
            used_sum += fair.as_ref().map_or(control_used, Admission::used);
        }
    }
    run.util = used_sum as f64 / ((TICKS - 100) as u64 * CAPACITY) as f64;
    run
}

fn print_admission() {
    println!(
        "## Hot tenant among 999 small ones (one node, capacity 1,000 in flight, service 10 ticks)\n"
    );
    println!(
        "Hot tenant offers 400 a tick (4,000 in flight against 1,000 capacity); each small tenant offers 2% a tick (about 200 in flight in all).\n"
    );
    println!("| admission | small tenants refused | hot tenant served | node utilisation |");
    println!("|---|---|---|---|");
    for (name, reserve) in [
        ("control: one global cap", None),
        ("fair, reserve 100", Some(100)),
        ("fair, reserve 250", Some(250)),
        ("fair, reserve 500", Some(500)),
    ] {
        let r = admission(reserve);
        println!(
            "| {name} | {:.2}% of {} | {:.1}% of {} | {:.1}% |",
            100.0 * r.small_refused as f64 / r.small_offered as f64,
            r.small_offered,
            100.0 * r.hot_served as f64 / r.hot_offered as f64,
            r.hot_offered,
            100.0 * r.util
        );
    }
    println!();
}

/// A cache of `capacity` bytes where each read of a missing key inserts it; hit ratio of the
/// small tenants, whose working set (50 tenants x 8 objects x 1 KiB) fits.
fn cache(fair: bool) -> (f64, u64) {
    const CAPACITY: u64 = 1 << 20;
    const OBJECT: u64 = 1 << 10;
    let mut quota = CacheQuota::new(QuotaConfig {
        capacity: CAPACITY,
        floor: 16 << 10,
        ceiling: 512 << 10,
        max_tenants: 4096,
    })
    .unwrap();
    // Per-tenant LRU (fair) or one LRU (control): keys are (tenant, object).
    let mut lru: VecDeque<(u64, u64)> = VecDeque::new();
    let mut resident = std::collections::HashSet::<(u64, u64)>::new();
    let (mut hits, mut reads) = (0u64, 0u64);
    let mut rng = Lcg(11);
    let mut storm_next = 0u64;
    let mut evicted_small = 0u64;
    for round in 0..60_000u64 {
        // One storm read of a never-seen object, then one small-tenant read.
        for (tenant, object, small) in [
            (
                9999u64,
                {
                    storm_next += 1;
                    storm_next
                },
                false,
            ),
            (rng.next() % 50, rng.next() % 8, true),
        ] {
            let key = (tenant, object);
            let hit = resident.contains(&key);
            if small && round >= 2000 {
                reads += 1;
                hits += u64::from(hit);
            }
            if hit {
                if let Some(at) = lru.iter().position(|k| *k == key) {
                    lru.remove(at);
                }
                lru.push_back(key);
                continue;
            }
            if fair {
                if let Ok(evictions) = quota.reserve(id(tenant), OBJECT) {
                    for e in evictions {
                        let mut left = e.bytes / OBJECT;
                        let who = u64::from_be_bytes(e.tenant[..8].try_into().unwrap());
                        let mut at = 0;
                        while left > 0 && at < lru.len() {
                            if lru[at].0 == who {
                                let gone = lru.remove(at).unwrap();
                                resident.remove(&gone);
                                if gone.0 < 50 {
                                    evicted_small += 1;
                                }
                                quota.release(&e.tenant, OBJECT);
                                left -= 1;
                            } else {
                                at += 1;
                            }
                        }
                    }
                    resident.insert(key);
                    lru.push_back(key);
                }
            } else {
                while resident.len() as u64 * OBJECT + OBJECT > CAPACITY {
                    let gone = lru.pop_front().unwrap();
                    resident.remove(&gone);
                    if gone.0 < 50 {
                        evicted_small += 1;
                    }
                }
                resident.insert(key);
                lru.push_back(key);
            }
        }
    }
    (hits as f64 / reads as f64, evicted_small)
}

fn print_cache() {
    println!(
        "## Cold cache storm (1 MiB cache; one tenant reads never-seen 1 KiB objects as fast as 50 small tenants re-read theirs)\n"
    );
    println!("The small tenants' working set is 50 x 8 objects x 1 KiB = 400 KiB, which fits.\n");
    println!("| cache | small tenants' hit ratio | small objects evicted |");
    println!("|---|---|---|");
    let (h, e) = cache(false);
    println!("| control: one shared LRU | {:.1}% | {e} |", 100.0 * h);
    let (h, e) = cache(true);
    println!(
        "| quota: floor 16 KiB, ceiling 512 KiB | {:.1}% | {e} |",
        100.0 * h
    );
    println!();
}

fn fleet(zone_sizes: &[u32]) -> Vec<Node> {
    let mut out = Vec::new();
    let mut n = 0u64;
    for (zone, &count) in zone_sizes.iter().enumerate() {
        for _ in 0..count {
            n += 1;
            out.push(Node {
                id: id(1_000_000 + n),
                zone: zone as u16,
                weight: 100,
            });
        }
    }
    out
}

fn print_placement() {
    println!("## Placement skew and AZ imbalance (100,000 tenants)\n");
    println!(
        "| fleet (nodes per zone) | replicas | busiest node / mean | busiest zone share of tenant-replicas |"
    );
    println!("|---|---|---|---|");
    for (zones, k) in [
        (vec![20, 20, 20], 1usize),
        (vec![20, 20, 20], 3),
        (vec![20, 20, 10], 1),
        (vec![20, 20, 10], 3),
        (vec![30, 15, 5], 3),
    ] {
        let nodes = fleet(&zones);
        let fleet_view = Fleet::new(&nodes).unwrap();
        let mut load = std::collections::HashMap::<Id, u64>::new();
        let mut per_zone = [0u64; 8];
        for t in 0..100_000u64 {
            let got = fleet_view
                .place(
                    Ask {
                        tenant: id(t),
                        replicas: k,
                    },
                    |_| 1000,
                )
                .unwrap();
            for n in got {
                *load.entry(n).or_default() += 1;
                per_zone[nodes.iter().find(|x| x.id == n).unwrap().zone as usize] += 1;
            }
        }
        let max = *load.values().max().unwrap() as f64;
        let mean = (100_000 * k) as f64 / nodes.len() as f64;
        let zone_max = *per_zone.iter().max().unwrap() as f64 / (100_000 * k) as f64;
        println!(
            "| {zones:?} | {k} | {:.2} | {:.1}% |",
            max / mean,
            100.0 * zone_max
        );
    }
    println!();
}

fn print_cost() {
    println!("## Cost of one placement (release build, one thread)\n");
    println!("| nodes | replicas | ns per placement |");
    println!("|---|---|---|");
    for n in [100u32, 1_000, 10_000] {
        let nodes = fleet(&[n / 3, n / 3, n - 2 * (n / 3)]);
        let f = Fleet::new(&nodes).unwrap();
        for k in [1usize, 3] {
            let rounds = (2_000_000 / n).max(200);
            let start = Instant::now();
            let mut sink = 0u8;
            for t in 0..rounds {
                let got = f
                    .place(
                        Ask {
                            tenant: id(u64::from(t)),
                            replicas: k,
                        },
                        |_| 1000,
                    )
                    .unwrap();
                sink ^= got[0][0];
            }
            let ns = start.elapsed().as_nanos() as f64 / f64::from(rounds);
            println!("| {n} | {k} | {ns:.0} (sink {sink}) |");
        }
    }
    println!();
}

fn main() {
    print_admission();
    print_cache();
    print_placement();
    print_cost();
}
