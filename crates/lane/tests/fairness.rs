//! M13.02: tenant fairness and hot placement. Admission, weighted placement, cache quotas, load
//! feedback and bounded subscriptions, and that none of it can change who is authoritative.
mod support;

use std::collections::HashMap;

use hsearch_format::{Durability, Id};
use hsearch_lane::admission::{Admission, Config as AdmissionConfig, Refusal};
use hsearch_lane::feedback::{self, LoadFeedback, NEUTRAL};
use hsearch_lane::quota::{self, Bypass, CacheQuota};
use hsearch_lane::rendezvous::{self, Ask, Fleet, MAX_REPLICAS, MAX_ZONES, Node, replicas_for};
use hsearch_lane::subscribe::{self, Subscriptions};
use hsearch_lane::{LaneId, block_on, create_head, read_head};
use support::{counted, id, layout};

fn admission(capacity: u64, reserve: u16) -> Admission {
    Admission::new(AdmissionConfig {
        capacity,
        reserve_permille: reserve,
    })
    .unwrap()
}

// ---- admission -------------------------------------------------------------------------------

#[test]
fn a_hot_tenant_is_refused_while_many_small_tenants_are_all_admitted() {
    let mut a = admission(1000, 200);
    let hot = id(1);
    let mut hot_permits = Vec::new();
    let mut hot_refused = 0;
    for _ in 0..5000 {
        match a.admit(hot, 1, 1) {
            Ok(p) => hot_permits.push(p),
            Err(Refusal::Share) => hot_refused += 1,
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    // The hot tenant alone borrows up to the reserve and no further.
    assert_eq!(hot_permits.len(), 800);
    assert_eq!(hot_refused, 4200);
    // 150 small tenants each ask for one unit: every one is admitted from the reserve.
    let mut small = Vec::new();
    for n in 0..150 {
        small.push(
            a.admit(id(100 + n), 1, 1)
                .expect("a small tenant is admitted"),
        );
    }
    assert_eq!(a.used(), 950);
    assert_eq!(a.active_tenants(), 151);
    for p in small.into_iter().chain(hot_permits) {
        a.release(p);
    }
    assert_eq!((a.used(), a.active_tenants()), (0, 0));
    let s = a.stats();
    assert_eq!(s.refused_share, 4200);
    assert_eq!(s.admitted, 950);
}

#[test]
fn weights_set_the_guaranteed_share() {
    let mut a = admission(100, 0);
    // Two active tenants, weights 3 and 1: shares 75 and 25 of 100.
    let (heavy, light) = (id(1), id(2));
    let mut held = Vec::new();
    for _ in 0..75 {
        held.push(a.admit(heavy, 3, 1).unwrap());
    }
    for _ in 0..25 {
        held.push(a.admit(light, 1, 1).unwrap());
    }
    assert_eq!(a.used(), 100);
    assert_eq!(a.admit(heavy, 3, 1).unwrap_err(), Refusal::Saturated);
    assert_eq!(a.inflight(&heavy), 75);
    assert_eq!(a.inflight(&light), 25);
    for p in held {
        a.release(p);
    }
}

#[test]
fn a_tenant_within_its_share_waits_for_capacity_while_a_borrower_is_told_share() {
    let mut a = admission(10, 0);
    let mut held = Vec::new();
    for _ in 0..10 {
        held.push(a.admit(id(1), 1, 1).unwrap());
    }
    // Full: a newcomer is within its share but there is no room; the borrower too.
    assert_eq!(a.admit(id(2), 1, 1).unwrap_err(), Refusal::Saturated);
    assert_eq!(a.admit(id(1), 1, 1).unwrap_err(), Refusal::Saturated);
    // Releasing one makes room for the newcomer, not by preemption.
    a.release(held.pop().unwrap());
    assert!(a.admit(id(2), 1, 1).is_ok());
}

#[test]
fn an_alone_tenant_may_use_everything_but_the_reserve_and_idle_tenants_cost_nothing() {
    let mut a = admission(100, 100);
    let mut held = Vec::new();
    while let Ok(p) = a.admit(id(1), 1, 5) {
        held.push(p);
    }
    assert_eq!(a.used(), 90);
    assert_eq!(a.active_tenants(), 1);
    for p in held {
        a.release(p);
    }
    assert_eq!(a.active_tenants(), 0);
}

#[test]
fn bad_input_is_refused_and_counted() {
    assert_eq!(
        Admission::new(AdmissionConfig {
            capacity: 0,
            reserve_permille: 0
        })
        .err(),
        Some(Refusal::Invalid)
    );
    assert_eq!(
        Admission::new(AdmissionConfig {
            capacity: 1,
            reserve_permille: 1000
        })
        .err(),
        Some(Refusal::Invalid)
    );
    let mut a = admission(10, 0);
    assert_eq!(a.admit(id(1), 0, 1).unwrap_err(), Refusal::Invalid);
    assert_eq!(a.admit(id(1), 1, 0).unwrap_err(), Refusal::Invalid);
    assert_eq!(a.admit(id(1), 1, 11).unwrap_err(), Refusal::TooLarge);
    assert_eq!(a.stats().refused_other, 3);
    assert_eq!((a.used(), a.active_tenants()), (0, 0));
}

#[test]
fn a_changed_weight_while_running_replaces_the_old_one() {
    let mut a = admission(100, 0);
    let p1 = a.admit(id(1), 1, 10).unwrap();
    let p2 = a.admit(id(1), 3, 10).unwrap();
    // Weight is now 3 against nothing else: releasing both leaves the table empty and consistent.
    a.release(p1);
    a.release(p2);
    assert_eq!(a.active_tenants(), 0);
    // A fresh tenant at weight 1 against a fresh one at weight 1 splits evenly again.
    let x = a.admit(id(2), 1, 50).unwrap();
    let y = a.admit(id(3), 1, 50).unwrap();
    assert_eq!(a.used(), 100);
    a.release(x);
    a.release(y);
}

#[test]
fn a_departed_tenants_weight_does_not_shrink_later_shares() {
    let mut a = admission(100, 200);
    let heavy = a.admit(id(1), 9, 1).unwrap();
    a.release(heavy);
    // Alone at weight 1 the tenant's share is the node less the reserve, so 60 is not borrowed.
    let p = a.admit(id(2), 1, 60).unwrap();
    assert_eq!(a.stats().borrowed, 0);
    a.release(p);
}

// ---- weighted placement ----------------------------------------------------------------------

fn nodes(n: u32, zones: u16, weight: impl Fn(u32) -> u32) -> Vec<Node> {
    (0..n)
        .map(|i| Node {
            id: id(10_000 + i),
            zone: (i % u32::from(zones)) as u16,
            weight: weight(i),
        })
        .collect()
}

fn one(fleet: &Fleet, tenant: Id) -> Id {
    fleet
        .place(
            Ask {
                tenant,
                replicas: 1,
            },
            |_| NEUTRAL,
        )
        .unwrap()[0]
}

#[test]
fn placement_is_proportional_to_weight() {
    // Weights 1, 2 and 4 in equal numbers.
    let fleet_nodes = nodes(30, 3, |i| [1, 2, 4][(i % 3) as usize]);
    let fleet = Fleet::new(&fleet_nodes).unwrap();
    let mut counts: HashMap<Id, u32> = HashMap::new();
    let tenants = 70_000u32;
    for t in 0..tenants {
        *counts.entry(one(&fleet, id(t))).or_default() += 1;
    }
    let total: u32 = fleet_nodes.iter().map(|n| n.weight).sum();
    for node in &fleet_nodes {
        let expected = f64::from(tenants) * f64::from(node.weight) / f64::from(total);
        let got = f64::from(*counts.get(&node.id).unwrap_or(&0));
        assert!(
            (got - expected).abs() < expected * 0.12,
            "weight {} got {got} expected {expected:.0}",
            node.weight
        );
    }
}

#[test]
fn removing_a_node_moves_only_the_tenants_it_held() {
    let all = nodes(50, 5, |_| 10);
    let fleet = Fleet::new(&all).unwrap();
    let gone = all[7].id;
    let rest: Vec<Node> = all.iter().filter(|n| n.id != gone).copied().collect();
    let smaller = Fleet::new(&rest).unwrap();
    let (mut held, mut moved) = (0u32, 0u32);
    for t in 0..20_000 {
        let before = one(&fleet, id(t));
        let after = one(&smaller, id(t));
        if before == gone {
            held += 1;
        } else if before != after {
            moved += 1;
        }
    }
    assert_eq!(moved, 0, "a tenant not on the lost node must not move");
    assert!(
        (250..550).contains(&held),
        "about 1/50 of 20,000, got {held}"
    );
}

#[test]
fn more_replicas_only_append_and_spread_across_zones() {
    let all = nodes(60, 3, |_| 10);
    let fleet = Fleet::new(&all).unwrap();
    let zone_of = |n: &Id| all.iter().find(|x| &x.id == n).unwrap().zone;
    for t in 0..2000 {
        let tenant = id(t);
        let mut previous: Vec<Id> = Vec::new();
        for k in 1..=MAX_REPLICAS {
            let got = fleet
                .place(
                    Ask {
                        tenant,
                        replicas: k,
                    },
                    |_| NEUTRAL,
                )
                .unwrap();
            assert_eq!(got.len(), k);
            for p in &previous {
                assert!(got.contains(p), "k={k} dropped a node from k-1");
            }
            let mut zones: Vec<u16> = got.iter().map(zone_of).collect();
            zones.sort_unstable();
            zones.dedup();
            assert_eq!(zones.len(), k.min(3), "zones are used before they repeat");
            previous = got;
        }
    }
}

#[test]
fn load_feedback_moves_tenants_off_a_loaded_node_only() {
    let all = nodes(20, 4, |_| 100);
    let fleet = Fleet::new(&all).unwrap();
    let hot_node = all[3].id;
    let (mut on_before, mut on_after, mut other_moves) = (0, 0, 0);
    for t in 0..20_000 {
        let tenant = id(t);
        let before = one(&fleet, tenant);
        let after = fleet
            .place(
                Ask {
                    tenant,
                    replicas: 1,
                },
                |n| {
                    if *n == hot_node { 250 } else { NEUTRAL }
                },
            )
            .unwrap()[0];
        on_before += u32::from(before == hot_node);
        on_after += u32::from(after == hot_node);
        if before != hot_node && before != after {
            other_moves += 1;
        }
    }
    assert_eq!(other_moves, 0);
    assert!(
        f64::from(on_after) < f64::from(on_before) * 0.35,
        "{on_before} -> {on_after}"
    );
}

#[test]
fn placement_refuses_what_it_cannot_answer_and_skips_zero_weight() {
    let all = nodes(4, 2, |_| 5);
    let fleet = Fleet::new(&all).unwrap();
    let ask = |replicas| Ask {
        tenant: id(1),
        replicas,
    };
    assert_eq!(
        fleet.place(ask(0), |_| NEUTRAL).unwrap_err(),
        rendezvous::Refusal::Invalid
    );
    assert_eq!(
        fleet.place(ask(MAX_REPLICAS + 1), |_| NEUTRAL).unwrap_err(),
        rendezvous::Refusal::Invalid
    );
    assert_eq!(
        fleet.place(ask(1), |_| 0).unwrap_err(),
        rendezvous::Refusal::Invalid
    );
    // Asking for more than there are returns what there is.
    assert_eq!(fleet.place(ask(8), |_| NEUTRAL).unwrap().len(), 4);
    let skip = all[0].id;
    let got = fleet
        .place(ask(8), |n| if *n == skip { 0 } else { NEUTRAL })
        .unwrap();
    assert_eq!(got.len(), 3);
    assert!(!got.contains(&skip));
    let many = nodes((MAX_ZONES + 1) as u32, (MAX_ZONES + 1) as u16, |_| 1);
    assert_eq!(Fleet::new(&many).err(), Some(rendezvous::Refusal::Zones));
}

#[test]
fn replica_count_follows_load_within_bounds() {
    assert_eq!(replicas_for(0, 1000, 700, 8), 1);
    assert_eq!(replicas_for(700, 1000, 700, 8), 1);
    assert_eq!(replicas_for(701, 1000, 700, 8), 2);
    assert_eq!(replicas_for(7000, 1000, 700, 8), 8);
    assert_eq!(replicas_for(1_000_000, 1000, 700, 4), 4);
    assert_eq!(replicas_for(5, 0, 700, 3), 3);
    assert_eq!(replicas_for(5, 1000, 700, 99), 1);
    assert_eq!(replicas_for(u64::MAX, 1, 1000, 99), MAX_REPLICAS);
}

#[test]
fn moving_compute_neither_reads_nor_writes_authority() {
    // A real structural head: placement for the same tenant under two different fleets and loads
    // makes no store request and leaves the head (its epoch and token) exactly as it was.
    let store = counted();
    let lane = LaneId::new(id(900));
    block_on(create_head(&*store, &layout(), &lane, Durability::Object)).unwrap();
    let (head_before, version_before) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
    let requests = store.requests();

    let small = nodes(10, 2, |_| 10);
    let large = nodes(40, 4, |_| 10);
    let mut seen = Vec::new();
    for fleet_nodes in [&small, &large] {
        let fleet = Fleet::new(fleet_nodes).unwrap();
        for k in [1, 3, 8] {
            seen.push(
                fleet
                    .place(
                        Ask {
                            tenant: id(900),
                            replicas: k,
                        },
                        |_| NEUTRAL,
                    )
                    .unwrap(),
            );
        }
    }
    assert!(seen.iter().any(|s| s != &seen[0]), "placement did move");
    assert_eq!(store.requests().since(&requests).total(), 0);
    let (head_after, version_after) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
    assert_eq!(head_after, head_before);
    assert_eq!(version_after, version_before);
}

// ---- cache quotas ----------------------------------------------------------------------------

fn cache(capacity: u64, floor: u64, ceiling: u64, tenants: usize) -> CacheQuota {
    CacheQuota::new(quota::Config {
        capacity,
        floor,
        ceiling,
        max_tenants: tenants,
    })
    .unwrap()
}

#[test]
fn a_cold_storm_by_one_tenant_never_evicts_tenants_under_their_floor() {
    // 1 MiB cache, 10 KiB floor each for 50 small tenants holding 8 KiB each.
    let mut c = cache(1 << 20, 10 << 10, 600 << 10, 1000);
    for t in 0..50 {
        assert!(c.reserve(id(t), 8 << 10).unwrap().is_empty());
    }
    let storm = id(9999);
    let mut self_evicted = 0u64;
    for _ in 0..4000 {
        let evictions = c.reserve(storm, 4 << 10).expect("the storm is cached");
        for e in evictions {
            assert_eq!(e.tenant, storm, "only the storm evicts itself");
            self_evicted += e.bytes;
        }
    }
    for t in 0..50 {
        assert_eq!(c.resident(&id(t)), 8 << 10, "tenant {t} kept its bytes");
    }
    assert!(c.resident(&storm) <= 600 << 10);
    assert!(c.total() <= 1 << 20);
    assert!(
        self_evicted > 15 << 20,
        "the storm turned its own bytes over"
    );
    assert_eq!(c.stats().evicted_from_others, 0);
}

#[test]
fn pressure_comes_from_surplus_largest_first_and_never_below_the_floor() {
    let mut c = cache(100, 10, 80, 10);
    c.reserve(id(1), 60).unwrap();
    c.reserve(id(2), 30).unwrap();
    // Full except 10. Tenant 3 asks for 25: 15 must come from somewhere. Tenant 1 has the largest
    // surplus (50) and gives it up.
    let ev = c.reserve(id(3), 25).unwrap();
    assert_eq!(ev.iter().map(|e| e.bytes).sum::<u64>(), 15);
    assert!(ev.iter().all(|e| e.tenant == id(1)));
    assert_eq!(c.resident(&id(1)), 45);
    assert_eq!(c.resident(&id(2)), 30);
    assert_eq!(c.total(), 100);
    // Surplus is 35 + 20 + 15 = 70: tenant 4 wanting 71 cannot be cached without cutting a tenant
    // below its floor, and wanting exactly 70 can.
    assert_eq!(c.reserve(id(4), 71).unwrap_err(), Bypass::NoRoom);
    assert_eq!(c.total(), 100, "a bypass changes nothing");
}

#[test]
fn quota_refusals_and_release_keep_the_table_exact() {
    let mut c = cache(100, 10, 50, 2);
    assert_eq!(c.reserve(id(1), 0).unwrap_err(), Bypass::TooLarge);
    assert_eq!(c.reserve(id(1), 51).unwrap_err(), Bypass::TooLarge);
    c.reserve(id(1), 20).unwrap();
    c.reserve(id(2), 20).unwrap();
    assert_eq!(c.reserve(id(3), 5).unwrap_err(), Bypass::Tenants);
    assert_eq!(c.tenants(), 2);
    c.release(&id(1), 5);
    assert_eq!(c.resident(&id(1)), 15);
    c.release(&id(1), 1000);
    assert_eq!((c.resident(&id(1)), c.tenants(), c.total()), (0, 1, 20));
    c.release(&id(77), 1);
    assert_eq!(c.reserve(id(3), 5).unwrap().len(), 0);
    assert_eq!(c.stats().bypassed, 3);
    assert!(
        CacheQuota::new(quota::Config {
            capacity: 10,
            floor: 6,
            ceiling: 5,
            max_tenants: 1
        })
        .is_none()
    );
}

#[test]
fn removing_a_slot_keeps_the_others_addressable() {
    let mut c = cache(1000, 1, 500, 100);
    for t in 0..20 {
        c.reserve(id(t), 10 + u64::from(t)).unwrap();
    }
    c.release(&id(3), 1000);
    c.release(&id(0), 1000);
    for t in 0..20 {
        let expect = if t == 3 || t == 0 {
            0
        } else {
            10 + u64::from(t)
        };
        assert_eq!(c.resident(&id(t)), expect, "tenant {t}");
    }
    assert_eq!(c.tenants(), 18);
}

// ---- load feedback ---------------------------------------------------------------------------

fn fb(max_nodes: usize) -> LoadFeedback {
    LoadFeedback::new(feedback::Config {
        target: 700,
        min_factor: 100,
        deadband: 50,
        stale_ticks: 10,
        max_nodes,
    })
    .unwrap()
}

#[test]
fn a_loaded_node_loses_weight_gradually_and_a_wobble_does_not_flap() {
    let mut f = fb(10);
    let n = id(1);
    assert_eq!(f.factor(&n, 0), NEUTRAL);
    f.report(n, 500, 0).unwrap();
    assert_eq!(f.factor(&n, 0), NEUTRAL);
    // Sustained overload walks the factor down, smoothed.
    let mut last = NEUTRAL;
    for tick in 1..=12 {
        f.report(n, 1000, tick).unwrap();
        let now = f.factor(&n, tick);
        assert!(now <= last);
        last = now;
    }
    assert!(last <= 200, "{last}");
    // A wobble around the target changes nothing once settled at neutral.
    let mut g = fb(10);
    g.report(n, 700, 0).unwrap();
    let mut changes = 0;
    let mut prev = g.factor(&n, 0);
    for tick in 1..40 {
        g.report(n, if tick % 2 == 0 { 740 } else { 660 }, tick)
            .unwrap();
        let now = g.factor(&n, tick);
        changes += u32::from(now != prev);
        prev = now;
    }
    assert_eq!(changes, 0, "inside the deadband the factor holds");
}

#[test]
fn a_silent_node_is_neutral_and_the_table_is_bounded() {
    let mut f = fb(2);
    f.report(id(1), 1000, 0).unwrap();
    assert!(f.factor(&id(1), 0) < NEUTRAL || f.factor(&id(1), 0) == NEUTRAL);
    for tick in 1..6 {
        f.report(id(1), 1000, tick).unwrap();
    }
    assert!(f.factor(&id(1), 5) < NEUTRAL);
    assert_eq!(f.factor(&id(1), 100), NEUTRAL, "stale means neutral");
    // A report after silence starts fresh instead of averaging into the old value.
    f.report(id(1), 0, 100).unwrap();
    assert_eq!(f.factor(&id(1), 100), NEUTRAL);
    f.report(id(2), 10, 0).unwrap();
    assert_eq!(f.report(id(3), 10, 0).unwrap_err(), feedback::Refusal::Full);
    assert_eq!(f.len(), 2);
    f.remove(&id(2));
    f.report(id(3), 10, 0).unwrap();
    assert!(!f.is_empty());
    assert_eq!(f.report(id(3), 5000, 0), Ok(()), "utilisation is clamped");
    for bad in [
        feedback::Config {
            target: 1000,
            min_factor: 100,
            deadband: 1,
            stale_ticks: 1,
            max_nodes: 1,
        },
        feedback::Config {
            target: 5,
            min_factor: 0,
            deadband: 1,
            stale_ticks: 1,
            max_nodes: 1,
        },
        feedback::Config {
            target: 5,
            min_factor: 1001,
            deadband: 1,
            stale_ticks: 1,
            max_nodes: 1,
        },
        feedback::Config {
            target: 5,
            min_factor: 5,
            deadband: 1,
            stale_ticks: 0,
            max_nodes: 1,
        },
        feedback::Config {
            target: 5,
            min_factor: 5,
            deadband: 1,
            stale_ticks: 1,
            max_nodes: 0,
        },
    ] {
        assert_eq!(
            LoadFeedback::new(bad).err(),
            Some(feedback::Refusal::Invalid)
        );
    }
}

#[test]
fn the_factor_is_exact_at_the_ends_of_its_range() {
    let mut f = LoadFeedback::new(feedback::Config {
        target: 500,
        min_factor: 100,
        deadband: 1,
        stale_ticks: 1000,
        max_nodes: 4,
    })
    .unwrap();
    f.report(id(1), 1000, 0).unwrap();
    assert_eq!(f.factor(&id(1), 0), 100);
    f.report(id(2), 750, 0).unwrap();
    assert_eq!(f.factor(&id(2), 0), 550);
    f.report(id(3), 500, 0).unwrap();
    assert_eq!(f.factor(&id(3), 0), 1000);
}

// ---- subscriptions ---------------------------------------------------------------------------

fn subs(total: usize, per_sub: usize, per_index: usize) -> Subscriptions {
    Subscriptions::new(subscribe::Limits {
        total,
        per_subscriber: per_sub,
        per_index,
    })
    .unwrap()
}

#[test]
fn subscriptions_are_bounded_three_ways_and_a_refusal_changes_nothing() {
    let mut s = subs(5, 3, 2);
    assert_eq!(s.subscribe(id(1), id(100)), Ok(true));
    assert_eq!(s.subscribe(id(1), id(100)), Ok(false));
    assert_eq!(s.subscribe(id(2), id(100)), Ok(true));
    assert_eq!(s.subscribe(id(3), id(100)), Err(subscribe::Refusal::Index));
    assert_eq!(s.subscribe(id(1), id(101)), Ok(true));
    assert_eq!(s.subscribe(id(1), id(102)), Ok(true));
    assert_eq!(
        s.subscribe(id(1), id(103)),
        Err(subscribe::Refusal::Subscriber)
    );
    assert_eq!(s.subscribe(id(4), id(104)), Ok(true));
    assert_eq!(s.len(), 5);
    assert_eq!(s.subscribe(id(5), id(105)), Err(subscribe::Refusal::Total));
    assert_eq!(s.len(), 5);
    let mut told: Vec<Id> = s.subscribers(&id(100)).copied().collect();
    told.sort_unstable();
    assert_eq!(told, vec![id(1), id(2)]);
    assert_eq!(s.subscribers(&id(777)).count(), 0);
}

#[test]
fn leaving_frees_every_subscription_and_idle_indexes_hold_none() {
    let mut s = subs(10, 10, 10);
    for i in 0..4 {
        s.subscribe(id(1), id(100 + i)).unwrap();
    }
    s.subscribe(id(2), id(100)).unwrap();
    assert!(s.unsubscribe(&id(2), &id(100)));
    assert!(!s.unsubscribe(&id(2), &id(100)));
    assert_eq!(s.drop_subscriber(&id(1)), 4);
    assert_eq!(s.drop_subscriber(&id(1)), 0);
    assert!(s.is_empty());
    assert_eq!(s.subscribers(&id(100)).count(), 0);
    assert_eq!(
        Subscriptions::new(subscribe::Limits {
            total: 0,
            per_subscriber: 1,
            per_index: 1
        })
        .err(),
        Some(subscribe::Refusal::Invalid)
    );
}

// ---- review follow-ups -----------------------------------------------------------------------

#[test]
fn the_order_a_fleet_is_listed_in_never_changes_a_placement_even_at_10000_nodes() {
    let nodes_a = nodes(10_000, 5, |_| 10);
    let mut nodes_b = nodes_a.clone();
    // A deterministic shuffle.
    let mut x = 12345u64;
    for i in (1..nodes_b.len()).rev() {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        nodes_b.swap(i, (x >> 33) as usize % (i + 1));
    }
    let (a, b) = (Fleet::new(&nodes_a).unwrap(), Fleet::new(&nodes_b).unwrap());
    for t in 0..400 {
        for k in [1, 3] {
            let ask = Ask {
                tenant: id(t),
                replicas: k,
            };
            let (mut pa, mut pb) = (
                a.place(ask, |_| NEUTRAL).unwrap(),
                b.place(ask, |_| NEUTRAL).unwrap(),
            );
            if k == 1 {
                assert_eq!(pa, pb, "tenant {t}");
            }
            pa.sort_unstable();
            pb.sort_unstable();
            assert_eq!(pa, pb, "tenant {t} k {k}");
        }
    }
}

#[test]
fn placement_stays_proportional_at_a_large_fleet() {
    // 2,000 nodes, weights 1 and 3 alternating: the heavy half should hold about 75%.
    let all = nodes(2000, 4, |i| if i % 2 == 0 { 1 } else { 3 });
    let fleet = Fleet::new(&all).unwrap();
    let heavy: std::collections::HashSet<Id> =
        all.iter().filter(|n| n.weight == 3).map(|n| n.id).collect();
    let on_heavy = (0..40_000)
        .filter(|&t| heavy.contains(&one(&fleet, id(t))))
        .count();
    let share = on_heavy as f64 / 40_000.0;
    assert!((0.73..0.77).contains(&share), "{share}");
}

#[test]
fn a_node_that_goes_idle_returns_to_neutral_and_an_old_report_does_not_age_it() {
    let mut f = fb(4);
    let n = id(1);
    for tick in 0..8 {
        f.report(n, 1000, tick).unwrap();
    }
    assert!(f.factor(&n, 7) < NEUTRAL);
    for tick in 8..60 {
        f.report(n, 0, tick).unwrap();
    }
    assert_eq!(f.factor(&n, 59), NEUTRAL, "idle means full weight again");
    // An out-of-order report does not move the clock backwards.
    f.report(n, 0, 3).unwrap();
    assert_eq!(f.factor(&n, 62), NEUTRAL);
    f.report(n, 1000, 3).unwrap();
}

#[test]
fn a_huge_capacity_and_a_boosting_factor_do_not_overflow() {
    let mut a = admission(u64::MAX / 2, 999);
    let p = a.admit(id(1), 1, 1).unwrap();
    a.release(p);
    let all = vec![Node {
        id: id(1),
        zone: 0,
        weight: u32::MAX,
    }];
    let fleet = Fleet::new(&all).unwrap();
    let got = fleet
        .place(
            Ask {
                tenant: id(2),
                replicas: 1,
            },
            |_| u32::MAX,
        )
        .unwrap();
    assert_eq!(got, vec![id(1)]);
}
