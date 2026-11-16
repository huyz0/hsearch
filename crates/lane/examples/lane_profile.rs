//! M03.08 measurements against a real S3-compatible store (RustFS). Markdown to stdout.
//!
//!     eval "$(scripts/rustfs.sh up)"
//!     cargo run --release -p hsearch-lane --example lane_profile -- [section] [scale]
//!
//! sections: latency | sweep | reestablish | promote | all. `scale` (default 0.25) shrinks the
//! small-index sweep's active indexes and events together, which leaves per-lane arrival rates, and
//! so PUTs per write, unchanged. Request counts are the S3 adapter's wire attempts. Provisional: one
//! shared host, a container, a loopback store, no network, no KMS.
use std::sync::Arc;
use std::time::{Duration, Instant};

use hsearch_format::{Durability, Id, KeyIdentity, Layout, PolicyTuple};
use hsearch_lane::batch::{BatchPolicy, Batcher, OwnedPayload, OwnedRecord};
use hsearch_lane::headcas::HeadCasLog;
use hsearch_lane::hot::{HotRule, HotTracker};
use hsearch_lane::{LaneId, LaneWriter, block_on, create_lane, dedicated_group, shared_group};
use hsearch_s3::{S3Config, S3Store};

fn id(n: u64) -> Id {
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&n.to_be_bytes());
    out[15] = 0xA5;
    out
}

fn layout() -> Layout {
    Layout::new("bench").unwrap()
}

fn policy() -> PolicyTuple {
    PolicyTuple {
        key: KeyIdentity::DeploymentDefault,
        residency: "region-a".into(),
        durability: Durability::Object,
        format_version: 1,
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn store(label: &str) -> Arc<S3Store> {
    let endpoint = std::env::var("HSEARCH_S3_ENDPOINT").expect("run scripts/rustfs.sh up first");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let store = S3Store::new(S3Config {
        endpoint,
        bucket: format!("hsl-{label}-{nanos}"),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").unwrap(),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").unwrap(),
        region: "us-east-1".into(),
    })
    .unwrap();
    let ctx = hsearch_object::RequestContext::new(
        Duration::from_secs(60),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 28,
            response_bytes: 1 << 28,
            attempts: 10_000,
        },
    )
    .unwrap();
    store.prepare_bucket(&ctx).unwrap();
    Arc::new(store)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v
}

fn diff(a: hsearch_s3::RequestCounts, b: hsearch_s3::RequestCounts) -> (u64, u64, u64, u64) {
    (
        b.put - a.put,
        b.get - a.get,
        b.head - a.head,
        b.list - a.list,
    )
}

// ---------------------------------------------------------------- latency and the head-CAS comparison

fn latency() {
    println!("## Append latency and requests: fenced slot vs payload + head CAS\n");
    println!(
        "One writer, one append in flight, 1 KiB records, 300 appends per row (after 20 warm-up). Wire attempts from the S3 adapter.\n"
    );
    println!(
        "| protocol | records per batch | batch bytes | p50 ms | p95 ms | p99 ms | PUT per ack | GET per ack | HEAD per ack |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for records in [1usize, 16, 256] {
        let body = vec![0xABu8; 1024];
        let s = store("lat");
        let (domain, gid) = shared_group(&policy(), 0);
        let lane = LaneId::new(domain);
        block_on(create_lane(
            &*s,
            &layout(),
            &lane,
            &policy(),
            gid,
            1000,
            &[id(1)],
        ))
        .unwrap();
        let (mut w, _) = block_on(LaneWriter::open(Arc::clone(&s), layout(), lane, 0, |_| {
            Ok(())
        }))
        .unwrap();
        let specs: Vec<_> = (0..records)
            .map(|n| hsearch_format::RecordSpec {
                index: id(1),
                seq: n as u64,
                op: hsearch_format::RecordOp::Put,
                payload: hsearch_format::PayloadRef::Inline(&body),
            })
            .collect();
        for _ in 0..20 {
            block_on(w.append(&specs)).unwrap();
        }
        let before = s.counts();
        let mut lat = Vec::new();
        let mut bytes = 0;
        for _ in 0..300 {
            let t = Instant::now();
            let ack = block_on(w.append(&specs)).unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
            bytes = ack.bytes;
        }
        let (p, g, h, _) = diff(before, s.counts());
        let l = sorted(lat);
        println!(
            "| fenced slot | {records} | {bytes} | {:.1} | {:.1} | {:.1} | {:.2} | {:.2} | {:.2} |",
            pct(&l, 0.5),
            pct(&l, 0.95),
            pct(&l, 0.99),
            p as f64 / 300.0,
            g as f64 / 300.0,
            h as f64 / 300.0
        );

        let s = store("latcas");
        let lane = LaneId::new(id(900));
        let mut log = block_on(HeadCasLog::open(Arc::clone(&s), layout(), lane)).unwrap();
        let payload = vec![0xABu8; bytes];
        for _ in 0..20 {
            block_on(log.append(&payload)).unwrap();
        }
        let before = s.counts();
        let mut lat = Vec::new();
        for _ in 0..300 {
            let t = Instant::now();
            block_on(log.append(&payload)).unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
        }
        let (p, g, h, _) = diff(before, s.counts());
        let l = sorted(lat);
        println!(
            "| payload + head CAS | {records} | {bytes} | {:.1} | {:.1} | {:.1} | {:.2} | {:.2} | {:.2} |",
            pct(&l, 0.5),
            pct(&l, 0.95),
            pct(&l, 0.99),
            p as f64 / 300.0,
            g as f64 / 300.0,
            h as f64 / 300.0
        );
    }
    println!();
}

// ---------------------------------------------------------------- the small-index sweep

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Option3 {
    /// One lane per index.
    A,
    /// Shared lanes for default-key indexes, a BYOK index alone.
    B,
}

struct Workload {
    name: &'static str,
    records_per_s: f64,
    seconds: f64,
    hot_fraction: f64,
    hot_share: f64,
    hot_offset: u64,
}

const BYTES: usize = 1024;
const GROUP: u64 = 1000;
const WAIT_MS: u64 = 250;

fn is_byok(index: u64) -> bool {
    let mut z = index.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z ^= z >> 29;
    z.wrapping_mul(0xBF58_476D_1CE4_E5B9) % 100 < 5
}

struct LaneRun {
    lane: LaneId,
    policy: PolicyTuple,
    members: Vec<Id>,
    byok: bool,
    events: Vec<(u64, u64)>, // (ms, index)
    writer: Option<LaneWriter<S3Store>>,
    lat: Vec<f64>,
    slots: u64,
    records: u64,
    window: Vec<hsearch_lane::hot::WindowReport>,
}

fn trace(w: &Workload, active: u64, scale: f64, rng: &mut Rng) -> Vec<(u64, u64)> {
    let events = (w.records_per_s * w.seconds * scale) as u64;
    let hot_n = ((active as f64 * w.hot_fraction).ceil() as u64).max(1);
    (0..events)
        .map(|_| {
            let ms = (rng.unit() * w.seconds * 1000.0) as u64;
            let index = if rng.unit() < w.hot_share {
                w.hot_offset + rng.next() % hot_n
            } else {
                rng.next() % active
            };
            (ms, index)
        })
        .collect()
}

fn build_lanes(option: Option3, events: &[(u64, u64)]) -> Vec<LaneRun> {
    use std::collections::BTreeMap;
    let mut by_lane: BTreeMap<(bool, u64), Vec<(u64, u64)>> = BTreeMap::new();
    for &(ms, index) in events {
        let key = match option {
            Option3::A => (false, index),
            Option3::B if is_byok(index) => (true, index),
            Option3::B => (false, index / GROUP),
        };
        by_lane.entry(key).or_default().push((ms, index));
    }
    let mut out = Vec::new();
    for ((byok, key), mut ev) in by_lane {
        ev.sort_unstable();
        let (policy, lane, members) = match (option, byok) {
            (Option3::A, _) => {
                let (d, _) = dedicated_group(&policy(), &id(key), b"solo");
                (policy(), LaneId::new(d), vec![id(key)])
            }
            (Option3::B, true) => {
                let p = PolicyTuple {
                    key: KeyIdentity::Byok(format!("kms/{key}")),
                    ..policy()
                };
                let (d, _) = dedicated_group(&p, &id(key), b"byok");
                (p, LaneId::new(d), vec![id(key)])
            }
            (Option3::B, false) => {
                let (d, _) = shared_group(&policy(), key);
                let mut members: Vec<Id> = (key * GROUP..(key + 1) * GROUP)
                    .filter(|i| !is_byok(*i))
                    .map(id)
                    .collect();
                members.sort_unstable();
                (policy(), LaneId::new(d), members)
            }
        };
        out.push(LaneRun {
            lane,
            policy,
            members,
            byok,
            events: ev,
            writer: None,
            lat: Vec::new(),
            slots: 0,
            records: 0,
            window: Vec::new(),
        });
    }
    out
}

fn parallel<T: Send>(items: &mut [T], threads: usize, f: impl Fn(&mut T) + Sync) {
    let chunk = items.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for part in items.chunks_mut(chunk) {
            let f = &f;
            scope.spawn(move || part.iter_mut().for_each(f));
        }
    });
}

#[derive(Default)]
struct Row {
    lanes: u64,
    events: u64,
    slots: u64,
    records: u64,
    create: (u64, u64, u64, u64),
    open: (u64, u64, u64, u64),
    append: (u64, u64, u64, u64),
    lat: Vec<f64>,
    max_hot_byte_share: f64,
    max_hot_record_share: f64,
    busy_lanes: u64,
    promote_candidates: u64,
}

fn run_option(
    s: &Arc<S3Store>,
    option: Option3,
    events: &[(u64, u64)],
    rule: HotRule,
    seconds: f64,
) -> (Row, Row) {
    let mut lanes = build_lanes(option, events);
    let threads = 24;
    let c0 = s.counts();
    parallel(&mut lanes, threads, |l| {
        block_on(create_lane(
            &**s,
            &layout(),
            &l.lane,
            &l.policy,
            [7; 16],
            1000,
            &l.members,
        ))
        .unwrap();
    });
    let c1 = s.counts();
    parallel(&mut lanes, threads, |l| {
        let (w, _) = block_on(LaneWriter::open(Arc::clone(s), layout(), l.lane, 0, |_| {
            Ok(())
        }))
        .unwrap();
        l.writer = Some(w);
    });
    let c2 = s.counts();
    let body = vec![0x5Au8; BYTES];
    parallel(&mut lanes, threads, |l| {
        let mut batcher = Batcher::new(BatchPolicy {
            max_wait: Duration::from_millis(WAIT_MS),
            ..BatchPolicy::default()
        })
        .unwrap();
        let mut hot = HotTracker::new(rule, 4096);
        let w = l.writer.as_mut().unwrap();
        let mut flush = |batch: hsearch_lane::batch::Batch,
                         lat: &mut Vec<f64>,
                         slots: &mut u64,
                         records: &mut u64| {
            let specs = batch.specs();
            let t = Instant::now();
            block_on(w.append(&specs)).unwrap();
            lat.push(t.elapsed().as_secs_f64() * 1e3);
            *slots += 1;
            *records += specs.len() as u64;
        };
        for (n, &(ms, index)) in l.events.iter().enumerate() {
            let now = Duration::from_millis(ms);
            if let Some(batch) = batcher.poll(now) {
                flush(batch, &mut l.lat, &mut l.slots, &mut l.records);
            }
            let seq = n as u64 + 1;
            let rec = OwnedRecord {
                index: id(index),
                seq,
                op: hsearch_format::RecordOp::Put,
                payload: OwnedPayload::Inline(body.clone()),
            };
            if let Some(report) = hot.record(id(index), BYTES as u64, now) {
                l.window.push(report);
            }
            for batch in batcher.push(rec, now).unwrap() {
                flush(batch, &mut l.lat, &mut l.slots, &mut l.records);
            }
        }
        if let Some(batch) = batcher.flush() {
            flush(batch, &mut l.lat, &mut l.slots, &mut l.records);
        }
        l.window.push(hot.finish());
    });
    let c3 = s.counts();
    let _ = seconds;
    let mut rows = (Row::default(), Row::default());
    for l in &lanes {
        let row = if l.byok { &mut rows.1 } else { &mut rows.0 };
        row.lanes += 1;
        row.events += l.events.len() as u64;
        row.slots += l.slots;
        row.records += l.records;
        row.lat.extend(&l.lat);
        for report in &l.window {
            if report.records >= rule.floor_records {
                row.busy_lanes += 1;
                row.max_hot_byte_share = row.max_hot_byte_share.max(report.hottest_byte_share);
                row.max_hot_record_share =
                    row.max_hot_record_share.max(report.hottest_record_share);
                row.promote_candidates += report.hot.len() as u64;
            }
        }
    }
    // Phase deltas are for the whole option; split them across default-key and BYOK lanes by count.
    let (create, open, append) = (diff(c0, c1), diff(c1, c2), diff(c2, c3));
    let total_lanes = (rows.0.lanes + rows.1.lanes).max(1);
    let total_slots = (rows.0.slots + rows.1.slots).max(1);
    let share = |part: u64, total: u64, v: (u64, u64, u64, u64)| {
        let f = part as f64 / total as f64;
        (
            (v.0 as f64 * f).round() as u64,
            (v.1 as f64 * f).round() as u64,
            (v.2 as f64 * f).round() as u64,
            (v.3 as f64 * f).round() as u64,
        )
    };
    for row in [&mut rows.0, &mut rows.1] {
        row.create = share(row.lanes, total_lanes, create);
        row.open = share(row.lanes, total_lanes, open);
        row.append = share(row.slots, total_slots, append);
    }
    (rows.0, rows.1)
}

fn sweep(scale: f64) {
    println!("## Requests per acknowledged write: small-index sweep (scale {scale})\n");
    println!(
        "Workloads and parameters are M03.09's (`cost-calibration-inputs.json`): 1 KiB records, {WAIT_MS} ms batch wait, groups of {GROUP}, 5% BYOK, 30 s window. Active indexes = {:.0} (1M x scale). One seed. PUT per write is the data-slot PUTs (the commit cost); lane creation and open are reported apart because they happen once per lane in the window, not per write.\n",
        1_000_000.0 * scale
    );
    let workloads = [
        Workload {
            name: "steady_small",
            records_per_s: 2000.0,
            seconds: 30.0,
            hot_fraction: 0.01,
            hot_share: 0.8,
            hot_offset: 0,
        },
        Workload {
            name: "write_storm",
            records_per_s: 20000.0,
            seconds: 30.0,
            hot_fraction: 1.0,
            hot_share: 0.0,
            hot_offset: 0,
        },
        Workload {
            name: "one_hot_index",
            records_per_s: 2000.0,
            seconds: 30.0,
            hot_fraction: 1e-6,
            hot_share: 0.5,
            hot_offset: (1..).find(|i| !is_byok(*i)).unwrap_or(1),
        },
    ];
    println!(
        "| workload | option | scale | lanes | writes | slots | PUT / write (commit) | records / slot | lane create PUT | lane open PUT / GET / HEAD | commit PUT / GET / HEAD | all-in PUT / GET / HEAD per write | append p50 / p99 ms | busy lanes | hottest index share (bytes / records) | promotion candidates |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let rule = HotRule::default();
    let mut model_check = None;
    let _ = scale;
    for w in &workloads {
        // The write storm at scale 1 is 600k writes; cap its scale so a run stays minutes.
        for option in [Option3::A, Option3::B] {
            // One lane per index needs a lane for nearly every write, so option A is run at a
            // small scale (its PUT per write does not depend on scale); the storm is capped for B.
            let cap = match (option, w.name) {
                (Option3::A, _) => 0.05,
                (_, "write_storm") => 0.1,
                _ => 1.0,
            };
            let ws = scale.min(cap);
            let active_w = (1_000_000.0 * ws) as u64;
            let mut rng = Rng(0xC0FFEE ^ w.name.len() as u64);
            let events = trace(w, active_w, ws, &mut rng);
            let s = store("sweep");
            let (d, b) = run_option(&s, option, &events, rule, w.seconds);
            for (label, row) in [
                ("A one lane per index", &d),
                ("B default-key (shared)", &d),
                ("B BYOK (own lane)", &b),
            ] {
                let want = matches!(
                    (option, label.as_bytes()[0]),
                    (Option3::A, b'A') | (Option3::B, b'B')
                );
                if !want || row.lanes == 0 {
                    continue;
                }
                if label.contains("BYOK") && option == Option3::A {
                    continue;
                }
                let l = sorted(row.lat.clone());
                let per = |a: u64, b: u64, c: u64| (a + b + c) as f64 / row.events.max(1) as f64;
                println!(
                    "| {} | {label} | {ws} | {} | {} | {} | {:.3} | {:.1} | {} | {} / {} / {} | {} / {} / {} | {:.2} / {:.2} / {:.2} | {:.0} / {:.0} | {} | {:.3} / {:.3} | {} |",
                    w.name,
                    row.lanes,
                    row.events,
                    row.slots,
                    row.slots as f64 / row.events.max(1) as f64,
                    row.records as f64 / row.slots.max(1) as f64,
                    row.create.0,
                    row.open.0,
                    row.open.1,
                    row.open.2,
                    row.append.0,
                    row.append.1,
                    row.append.2,
                    per(row.create.0, row.open.0, row.append.0),
                    per(0, row.open.1, row.append.1),
                    per(row.create.2, row.open.2, row.append.2),
                    pct(&l, 0.5),
                    pct(&l, 0.99),
                    row.busy_lanes,
                    row.max_hot_byte_share,
                    row.max_hot_record_share,
                    row.promote_candidates
                );
                if w.name == "steady_small"
                    && option == Option3::B
                    && label.starts_with("B default")
                {
                    model_check = Some(row.slots as f64 / row.events.max(1) as f64);
                }
            }
        }
    }
    println!();
    if let Some(m) = model_check {
        println!(
            "steady_small, default-key shared lanes: measured {m:.3} commit PUT per write against the M03.09 model's 0.247.\n"
        );
    }
}

// ---------------------------------------------------------------- re-establish and promotion

fn reestablish() {
    println!("## Lane re-establish time after a poison\n");
    println!(
        "A poisoned writer is reopened from the caller's cover. Cost is the descriptor read, two requests per slot above the cover, one HEAD past the end and one fence PUT; the table is one lane, median of 5 opens, 1 KiB records, 16 per slot.\n"
    );
    println!("| slots above the cover | open ms | PUT | GET | HEAD |");
    println!("|---|---|---|---|---|");
    for above in [0u64, 10, 100, 1000] {
        let s = store("reest");
        let (domain, gid) = shared_group(&policy(), 0);
        let lane = LaneId::new(domain);
        block_on(create_lane(
            &*s,
            &layout(),
            &lane,
            &policy(),
            gid,
            1000,
            &[id(1)],
        ))
        .unwrap();
        let (mut w, _) = block_on(LaneWriter::open(Arc::clone(&s), layout(), lane, 0, |_| {
            Ok(())
        }))
        .unwrap();
        let body = vec![1u8; 1024];
        let specs: Vec<_> = (0..16)
            .map(|n| hsearch_format::RecordSpec {
                index: id(1),
                seq: n,
                op: hsearch_format::RecordOp::Put,
                payload: hsearch_format::PayloadRef::Inline(&body),
            })
            .collect();
        for _ in 0..above {
            block_on(w.append(&specs)).unwrap();
        }
        drop(w);
        let mut times = Vec::new();
        let mut counts = (0, 0, 0, 0);
        for _ in 0..5 {
            let before = s.counts();
            let t = Instant::now();
            let _ = block_on(LaneWriter::open(Arc::clone(&s), layout(), lane, 1, |_| {
                Ok(())
            }))
            .unwrap();
            times.push(t.elapsed().as_secs_f64() * 1e3);
            counts = diff(before, s.counts());
        }
        let l = sorted(times);
        println!(
            "| {above} | {:.1} | {} | {} | {} |",
            pct(&l, 0.5),
            counts.0,
            counts.1,
            counts.2
        );
    }
    println!();
    println!(
        "Counts are for the fifth open, which also replays the four fences the earlier opens left (fences are retained), so row 0 is 1 descriptor read + 4 fences + the end, and each slot above adds one GET and one HEAD. The cover is what bounds this, and it is advanced by checkpoints (M06).\n"
    );
}

fn promote() {
    println!("## Hot-index promotion on a real store\n");
    let s = store("promo");
    let (domain, gid) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    let members: Vec<Id> = (0..1000u64).map(id).collect();
    block_on(create_lane(
        &*s,
        &layout(),
        &lane,
        &policy(),
        gid,
        1000,
        &members,
    ))
    .unwrap();
    let (mut w, _) = block_on(LaneWriter::open(Arc::clone(&s), layout(), lane, 0, |_| {
        Ok(())
    }))
    .unwrap();
    let body = vec![1u8; 1024];
    let mut seq = 0;
    for _ in 0..40 {
        let specs: Vec<_> = (0..50u64)
            .map(|n| {
                seq += 1;
                hsearch_format::RecordSpec {
                    index: id(if n % 2 == 0 { 0 } else { n }),
                    seq,
                    op: hsearch_format::RecordOp::Put,
                    payload: hsearch_format::PayloadRef::Inline(&body),
                }
            })
            .collect();
        block_on(w.append(&specs)).unwrap();
    }
    let before = s.counts();
    let t = Instant::now();
    let done = block_on(w.promote(id(0), seq)).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1e3;
    let (p, g, h, l) = diff(before, s.counts());
    println!(
        "Index 0 held half of 2,000 records in a 1,000-index lane. `promote`: {ms:.1} ms, {p} PUT / {g} GET / {h} HEAD / {l} LIST (seal slot, target lane descriptor, source descriptor CAS). Sealed at position {}.\n",
        done.sealed_at
    );
    let again = block_on(w.append(&[hsearch_format::RecordSpec {
        index: id(0),
        seq: 1,
        op: hsearch_format::RecordOp::Put,
        payload: hsearch_format::PayloadRef::Inline(&body),
    }]));
    println!(
        "A write for the promoted index to the source lane afterwards: `{:?}`.\n",
        again.err()
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let section = args.next().unwrap_or_else(|| "all".into());
    let scale: f64 = args.next().and_then(|v| v.parse().ok()).unwrap_or(0.25);
    let all = section == "all";
    if all || section == "latency" {
        latency();
    }
    if all || section == "sweep" {
        sweep(scale);
    }
    if all || section == "reestablish" {
        reestablish();
    }
    if all || section == "promote" {
        promote();
    }
}
