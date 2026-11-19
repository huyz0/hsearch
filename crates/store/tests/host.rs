//! The serving host: acknowledged writes survive a restart, versions never go backwards, bulks
//! share records, and a base keeps the replay bounded. Everything runs on the in-memory store, so
//! the numbers are provisional.
#[path = "support/history.rs"]
mod history;
mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use history::*;
use hsearch_commit::Dependencies;
use hsearch_engine::{Kind, Outcome, StoredValue};
use hsearch_index::{Field, ItemResult, Op, Wait};
use hsearch_lane::{block_on, read_head};
use hsearch_store::host::{Config, Host, HostError, OpenArgs};
use hsearch_store::publish::AnyDependencies;
use support::*;

fn deps() -> Dependencies {
    Dependencies {
        schema: [1; 32],
        crypto: [1; 32],
        resolved: [1; 32],
    }
}

fn put(id: &str, tag: &str) -> Op {
    Op::Put {
        id: id.into(),
        fields: vec![Field::Str(Arc::new(tag.into()))],
    }
}

fn del(id: &str) -> Op {
    Op::Delete { id: id.into() }
}

fn config(label: &str, window_ms: u64, base_every: u64, seed: u8) -> Config {
    Config {
        index: INDEX,
        shard: SHARD,
        incarnation: 1,
        scratch: std::env::temp_dir().join(format!("hsearch-host-{label}-{}", std::process::id())),
        limits: big(),
        deps: deps(),
        engine_format: "lucene-test".into(),
        mapping: [5; 32],
        crypto_envelope: [6; 16],
        window: Duration::from_millis(window_ms),
        base_every_batches: base_every,
        token_seed: [seed; 8],
        merge: hsearch_engine::MergeMode::Inline,
        searchable_when_empty: false,
        pin: None,
    }
}

/// Open a host on `lane`: a fresh committer fences whatever came before.
fn open(store: &Arc<Crash>, lane: hsearch_lane::LaneId, config: Config) -> Host {
    open_schema(store, lane, config, schema())
}

fn open_schema(
    store: &Arc<Crash>,
    lane: hsearch_lane::LaneId,
    config: Config,
    schema: hsearch_engine::Schema,
) -> Host {
    Host::open(
        OpenArgs {
            store: Arc::clone(store),
            publisher: publisher(store),
            committer: writer(store, lane),
            lane,
            dependencies: AnyDependencies,
            schema,
        },
        config,
    )
    .unwrap()
}

fn new_lane() -> (Arc<Crash>, hsearch_lane::LaneId) {
    let store = Crash::new();
    let (lane, first) = open_lane(&store);
    drop(first);
    (store, lane)
}

fn tag(host: &Host, id: &str) -> Option<String> {
    let reader = host.reader()?;
    reader.reader.get(id, &["tag"]).unwrap().map(|hit| {
        let StoredValue::String(tag) = &hit.fields[0].1 else {
            panic!("tag is a string")
        };
        tag.clone()
    })
}

fn versions(results: &[ItemResult]) -> Vec<i64> {
    results.iter().map(|r| r.version).collect()
}

#[test]
fn a_searchable_bulk_is_visible_and_takes_server_assigned_versions() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("basic", 0, 0, 1));
    assert!(host.reader().is_none());
    let got = host
        .bulk(vec![put("a", "x"), put("b", "y")], Wait::Searchable)
        .unwrap();
    // The first batch is sequence 0: versions start at 1 << 20 and rise by position.
    assert_eq!(versions(&got), vec![1 << 20, (1 << 20) + 1]);
    assert!(got.iter().all(|r| r.outcome == Outcome::Created));
    assert_eq!(tag(&host, "a").as_deref(), Some("x"));
    assert_eq!(tag(&host, "b").as_deref(), Some("y"));
    // A durable bulk is acknowledged before it is searchable; a refresh makes it so.
    let next = host.bulk(vec![put("a", "z")], Wait::Durable).unwrap();
    assert_eq!(next[0].outcome, Outcome::Updated);
    assert_eq!(next[0].version, 2 << 20);
    assert_eq!(tag(&host, "a").as_deref(), Some("x"));
    host.refresh().unwrap();
    assert_eq!(tag(&host, "a").as_deref(), Some("z"));
    let gone = host.bulk(vec![del("b")], Wait::Searchable).unwrap();
    assert_eq!(gone[0].outcome, Outcome::Deleted);
    assert_eq!(tag(&host, "b"), None);
    assert!(matches!(
        host.bulk(vec![], Wait::Durable),
        Err(HostError::Invalid)
    ));
}

#[test]
fn acknowledged_writes_recover_and_versions_never_go_backwards() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("restart", 0, 0, 1));
    let first = host
        .bulk(vec![put("a", "1"), put("b", "2")], Wait::Durable)
        .unwrap();
    let second = host
        .bulk(vec![put("a", "3"), del("b")], Wait::Durable)
        .unwrap();
    let before = second
        .iter()
        .chain(&first)
        .map(|r| r.version)
        .max()
        .unwrap();
    host.stop();
    // Nothing local survives: a new host recovers from the lane alone.
    let again = open(&store, lane, config("restart-2", 0, 0, 2));
    assert_eq!(tag(&again, "a").as_deref(), Some("3"));
    assert_eq!(tag(&again, "b"), None);
    let next = again.bulk(vec![put("c", "4")], Wait::Searchable).unwrap();
    assert!(next[0].version > before, "{} > {before}", next[0].version);
    assert_eq!(tag(&again, "c").as_deref(), Some("4"));
}

#[test]
fn a_restart_after_every_prefix_serves_exactly_what_was_acknowledged() {
    let batches: Vec<Vec<Op>> = vec![
        vec![put("a", "1"), put("b", "1")],
        vec![put("a", "2")],
        vec![del("b"), put("c", "1")],
        vec![put("b", "3"), del("c")],
        vec![put("d", "1")],
    ];
    for acknowledged in 0..=batches.len() {
        let (store, lane) = new_lane();
        let host = open(&store, lane, config("prefix", 0, 2, 1));
        let mut ledger = std::collections::BTreeMap::new();
        let mut top = 0;
        for ops in batches.iter().take(acknowledged) {
            let got = host.bulk(ops.clone(), Wait::Durable).unwrap();
            top = top.max(*versions(&got).iter().max().unwrap());
            for op in ops {
                match op {
                    Op::Put { id, fields } => {
                        let Field::Str(tag) = &fields[0] else {
                            unreachable!()
                        };
                        ledger.insert(id.clone(), Some((**tag).clone()));
                    }
                    Op::Delete { id } => {
                        ledger.insert(id.clone(), None);
                    }
                }
            }
        }
        host.stop();
        let again = open(&store, lane, config("prefix-2", 0, 2, 2));
        for (id, want) in &ledger {
            assert_eq!(&tag(&again, id), want, "after {acknowledged} batches: {id}");
        }
        let next = again.bulk(vec![put("zz", "1")], Wait::Durable).unwrap();
        assert!(next[0].version > top);
    }
}

#[test]
fn a_write_the_store_refuses_is_not_acknowledged_and_the_host_serves_on() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("refuse", 0, 0, 1));
    host.bulk(vec![put("a", "1")], Wait::Searchable).unwrap();
    store.allow.store(0, Ordering::SeqCst);
    let refused = host.bulk(vec![put("b", "2")], Wait::Searchable);
    assert!(
        matches!(refused, Err(HostError::Rejected(_))),
        "{refused:?}"
    );
    store.allow.store(i64::MAX, Ordering::SeqCst);
    assert!(!host.is_broken());
    assert_eq!(tag(&host, "b"), None);
    host.bulk(vec![put("b", "3")], Wait::Searchable).unwrap();
    assert_eq!(tag(&host, "b").as_deref(), Some("3"));
    host.stop();
}

#[test]
fn a_lost_reply_breaks_the_host_and_a_reopen_shows_what_is_in_the_log() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("lost", 0, 0, 1));
    host.bulk(vec![put("a", "1")], Wait::Durable).unwrap();
    store.fail_resolve.store(true, Ordering::SeqCst);
    store.lose.store(store.writes() as i64, Ordering::SeqCst);
    let lost = host.bulk(vec![put("b", "2")], Wait::Durable);
    assert!(
        matches!(lost, Err(HostError::Unknown | HostError::Broken)),
        "{lost:?}"
    );
    assert!(host.is_broken());
    assert!(matches!(
        host.bulk(vec![put("c", "3")], Wait::Durable),
        Err(HostError::Broken)
    ));
    host.stop();
    store.fail_resolve.store(false, Ordering::SeqCst);
    let again = open(&store, lane, config("lost-2", 0, 0, 2));
    // The lost write is durable, so it is served now; the one refused after it is not.
    assert_eq!(tag(&again, "a").as_deref(), Some("1"));
    assert_eq!(tag(&again, "b").as_deref(), Some("2"));
    assert_eq!(tag(&again, "c"), None);
}

#[test]
fn bulks_that_queue_together_share_records_and_the_same_identity_keeps_its_order() {
    let (store, lane) = new_lane();
    let host = Arc::new(open(&store, lane, config("group", 100, 0, 1)));
    let threads: Vec<_> = (0..6)
        .map(|i| {
            let host = Arc::clone(&host);
            std::thread::spawn(move || {
                host.bulk(vec![put(&format!("d{i}"), "v")], Wait::Durable)
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.len(), 6);
    let records = host.stats().records;
    assert!(records < 6, "six bulks made {records} records");
    host.refresh().unwrap();
    for i in 0..6 {
        assert_eq!(tag(&host, &format!("d{i}")).as_deref(), Some("v"));
    }
    // Two operations on one identity in one bulk become two records, in order.
    let before = host.stats().records;
    let got = host
        .bulk(
            vec![put("same", "first"), put("same", "second")],
            Wait::Searchable,
        )
        .unwrap();
    assert!(got[1].version > got[0].version);
    assert_eq!(host.stats().records, before + 2);
    assert_eq!(tag(&host, "same").as_deref(), Some("second"));
}

#[test]
fn a_base_on_a_schedule_bounds_what_a_restart_replays() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("base", 0, 2, 1));
    for i in 0..5 {
        host.bulk(vec![put(&format!("d{i}"), "v")], Wait::Durable)
            .unwrap();
    }
    // Five batches, a base after every second: two bases, the head names one. The base runs
    // after the acknowledgement, so a refresh queues behind it.
    host.refresh().unwrap();
    assert_eq!(host.stats().bases, 2);
    assert_eq!(host.stats().base_failures, 0);
    let head = block_on(read_head(store.inner_ref(), &layout(), &lane))
        .unwrap()
        .0;
    assert!(head.checkpoint.is_some());
    host.stop();
    let again = open(&store, lane, config("base-2", 0, 2, 2));
    for i in 0..5 {
        assert_eq!(tag(&again, &format!("d{i}")).as_deref(), Some("v"));
    }
}

#[test]
fn a_base_that_cannot_be_published_is_counted_and_never_fails_a_write() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("basefail", 0, 1, 1));
    host.bulk(vec![put("a", "1")], Wait::Durable).unwrap();
    // The base runs after the acknowledgement; a refresh queues behind it.
    host.refresh().unwrap();
    assert_eq!(host.stats().bases, 1);
    // Let the record through and refuse everything the base needs after it.
    store.allow.store(1, Ordering::SeqCst);
    host.bulk(vec![put("b", "2")], Wait::Durable).unwrap();
    let _ = host.refresh();
    assert!(host.stats().base_failures >= 1);
    store.allow.store(i64::MAX, Ordering::SeqCst);
    host.bulk(vec![put("c", "3")], Wait::Searchable).unwrap();
    host.refresh().unwrap();
    assert_eq!(host.stats().bases, 2);
    assert_eq!(tag(&host, "b").as_deref(), Some("2"));
}

#[test]
fn a_schema_evolved_online_is_written_and_a_host_opened_on_it_recovers_the_field() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("evolve", 0, 0, 1));
    host.bulk(vec![put("a", "1")], Wait::Searchable).unwrap();
    let evolved = schema()
        .evolve(2, &[("extra", hsearch_engine::Kind::Keyword)])
        .unwrap();
    host.evolve(evolved.clone(), deps()).unwrap();
    host.bulk(
        vec![Op::Put {
            id: "b".into(),
            fields: vec![
                Field::Str(Arc::new("2".into())),
                Field::Str(Arc::new("e".into())),
            ],
        }],
        Wait::Searchable,
    )
    .unwrap();
    assert_eq!(tag(&host, "b").as_deref(), Some("2"));
    assert_eq!(tag(&host, "a").as_deref(), Some("1"));
    host.stop();
    // The log does not carry the schema: the caller opens on the evolved one, as the node does
    // from its durable mapping, and the document with the new field replays.
    let again = open_schema(&store, lane, config("evolve-2", 0, 0, 2), evolved);
    assert_eq!(tag(&again, "b").as_deref(), Some("2"));
    assert_eq!(tag(&again, "a").as_deref(), Some("1"));
}

#[test]
fn request_counts_per_acknowledged_write_and_per_restart_do_not_scale_with_history() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("counts", 0, 0, 1));
    // One acknowledged record is one write; writes scale with operations, never with history.
    let per_bulk: Vec<usize> = [1, 20, 32]
        .iter()
        .map(|n| {
            let before = store.writes();
            let ops: Vec<Op> = (0..*n).map(|i| put(&format!("k{n}-{i}"), "v")).collect();
            host.bulk(ops, Wait::Durable).unwrap();
            store.writes() - before
        })
        .collect();
    assert_eq!(per_bulk, vec![1, 2, 2]);
    host.stop();
    // A restart costs the same writes (the seal and the guard) after one record or after many.
    let cost = |records: usize, label: &str| {
        let (store, lane) = new_lane();
        let host = open(&store, lane, config(label, 0, 0, 1));
        for i in 0..records {
            host.bulk(vec![put(&format!("r{i}"), "v")], Wait::Durable)
                .unwrap();
        }
        host.stop();
        let before = store.writes();
        let again = open(&store, lane, config(label, 0, 0, 2));
        let cost = store.writes() - before;
        again.stop();
        cost
    };
    assert_eq!(cost(1, "cost-a"), cost(40, "cost-b"));
}

#[test]
fn a_bulk_that_fails_midway_leaves_a_durable_prefix_and_nothing_after_it() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("prefix-fail", 0, 0, 1));
    // Three operations on one identity are three records: the engine refuses the middle one (a
    // document over the batch bound), and the third must not be written after it.
    let huge = "h".repeat(200_000);
    let failed = host.bulk(
        vec![put("x", "a"), put("x", &huge), put("x", "c")],
        Wait::Durable,
    );
    assert!(matches!(failed, Err(HostError::Refused(_))), "{failed:?}");
    host.stop();
    let again = open(&store, lane, config("prefix-fail-2", 0, 0, 2));
    assert_eq!(tag(&again, "x").as_deref(), Some("a"));
}

#[test]
fn a_malformed_bulk_in_a_group_is_refused_alone_and_the_waits_mix() {
    let (store, lane) = new_lane();
    let host = Arc::new(open(&store, lane, config("mixed", 100, 0, 1)));
    let send = |ops: Vec<Op>, wait: Wait| {
        let host = Arc::clone(&host);
        std::thread::spawn(move || host.bulk(ops, wait))
    };
    let durable = send(vec![put("a", "1")], Wait::Durable);
    let searchable = send(vec![put("b", "2")], Wait::Searchable);
    let empty = send(vec![], Wait::Durable);
    let wide = send(
        vec![Op::Put {
            id: "c".into(),
            fields: vec![
                Field::Str(Arc::new("3".into())),
                Field::Str(Arc::new("extra".into())),
            ],
        }],
        Wait::Durable,
    );
    assert!(durable.join().unwrap().is_ok());
    assert!(searchable.join().unwrap().is_ok());
    assert_eq!(empty.join().unwrap().unwrap_err(), HostError::Invalid);
    assert_eq!(wide.join().unwrap().unwrap_err(), HostError::Invalid);
    // The searchable bulk made the reader current for the whole group.
    assert_eq!(tag(&host, "b").as_deref(), Some("2"));
    assert_eq!(tag(&host, "a").as_deref(), Some("1"));
    assert_eq!(tag(&host, "c"), None);
}

#[test]
fn the_batching_window_is_a_deadline_not_a_wait_after_each_arrival() {
    let (store, lane) = new_lane();
    let host = Arc::new(open(&store, lane, config("deadline", 100, 0, 1)));
    // Bulks from other callers arrive every 30 ms for about a second, each well inside the window.
    let stream: Vec<_> = (0..30)
        .map(|i| {
            let host = Arc::clone(&host);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30 * (i + 1)));
                let _ = host.bulk(vec![put(&format!("s{i}"), "v")], Wait::Durable);
            })
        })
        .collect();
    let started = std::time::Instant::now();
    host.bulk(vec![put("first", "v")], Wait::Durable).unwrap();
    let waited = started.elapsed();
    for t in stream {
        t.join().unwrap();
    }
    assert!(
        waited < Duration::from_millis(500),
        "the first bulk waited {waited:?} behind a stream"
    );
}

#[test]
fn a_refused_operation_fails_alone_and_the_rest_of_its_run_is_written() {
    let (store, lane) = new_lane();
    let host = Arc::new(open(&store, lane, config("alone", 100, 0, 1)));
    let huge = "h".repeat(200_000);
    // Both bulks fall in one group, so their operations share one run.
    let bad = {
        let host = Arc::clone(&host);
        let ops = vec![put("a", "1"), put("b", &huge), put("c", "3")];
        std::thread::spawn(move || host.bulk_items(ops, Wait::Searchable))
    };
    let innocent = {
        let host = Arc::clone(&host);
        std::thread::spawn(move || host.bulk_items(vec![put("d", "4")], Wait::Searchable))
    };
    let bad = bad.join().unwrap().unwrap();
    let innocent = innocent.join().unwrap().unwrap();
    assert!(bad[0].is_ok() && bad[2].is_ok(), "{bad:?}");
    assert!(bad[1].is_err(), "{bad:?}");
    assert!(innocent[0].is_ok(), "{innocent:?}");
    for (id, want) in [
        ("a", Some("1")),
        ("b", None),
        ("c", Some("3")),
        ("d", Some("4")),
    ] {
        assert_eq!(tag(&host, id).as_deref(), want, "{id}");
    }
}

/// A host whose committer packs up to `records` records into one slot.
fn open_packing(
    store: &Arc<Crash>,
    lane: hsearch_lane::LaneId,
    records: usize,
    label: &str,
) -> Host {
    let (w, _) = block_on(hsearch_lane::LaneWriter::open_with_head(
        Arc::clone(store),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap();
    let policy = hsearch_lane::batch::BatchPolicy {
        max_records: records,
        max_bytes: 4 << 20,
        max_wait: Duration::from_millis(1),
    };
    let committer =
        hsearch_commit::Committer::new(w, hsearch_commit::AnyDependencies, policy).unwrap();
    Host::open(
        OpenArgs {
            store: Arc::clone(store),
            publisher: publisher(store),
            committer,
            lane,
            dependencies: AnyDependencies,
            schema: schema(),
        },
        config(label, 0, 0, 1),
    )
    .unwrap()
}

fn many(n: usize) -> Vec<Op> {
    (0..n)
        .map(|i| put(&format!("id{i:04}"), &format!("t{i}")))
        .collect()
}

#[test]
fn runs_of_one_bulk_share_a_slot_and_recover_from_it() {
    // 112 distinct identities are 7 records (the engine's bounded profile holds 128); a slot that
    // takes 64 holds them all.
    let cost = |records: usize, label: &str| {
        let (store, lane) = new_lane();
        let host = open_packing(&store, lane, records, label);
        let before = store.writes();
        let got = host.bulk(many(112), Wait::Durable).unwrap();
        assert_eq!(got.len(), 112);
        let cost = store.writes() - before;
        host.stop();
        let again = open_packing(&store, lane, records, &format!("{label}-again"));
        for i in [0usize, 17, 60, 111] {
            assert_eq!(
                tag(&again, &format!("id{i:04}")),
                Some(format!("t{i}")),
                "{i}"
            );
        }
        // Versions stay strictly above everything recovered.
        let more = again
            .bulk(vec![put("id0000", "new")], Wait::Searchable)
            .unwrap();
        assert!(more[0].version > got[0].version);
        cost
    };
    let alone = cost(1, "alone");
    let packed = cost(64, "packed");
    assert_eq!(alone, 7, "a record to a slot");
    assert_eq!(packed, 1, "the records share one slot");
}

#[test]
fn a_refusal_in_the_middle_of_a_packed_run_leaves_the_runs_around_it_written() {
    let (store, lane) = new_lane();
    let host = open_packing(&store, lane, 64, "middle");
    let huge = "h".repeat(200_000);
    // Distinct identities cut into runs of 16; the bad document is in the third run.
    let mut ops = many(64);
    // (64 identities are four records: the bad one is in the third.)
    ops[40] = put("bad", &huge);
    let items = host.bulk_items(ops, Wait::Searchable).unwrap();
    assert!(items[40].is_err());
    assert_eq!(items.iter().filter(|i| i.is_ok()).count(), 63);
    assert_eq!(tag(&host, "id0000").as_deref(), Some("t0"));
    assert_eq!(tag(&host, "id0063").as_deref(), Some("t63"));
    assert_eq!(tag(&host, "bad"), None);
    // Versions rose with the records actually written.
    let versions: Vec<i64> = items.iter().flatten().map(|r| r.version).collect();
    assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
    host.stop();
    let again = open_packing(&store, lane, 64, "middle-again");
    assert_eq!(tag(&again, "id0063").as_deref(), Some("t63"));
}

#[test]
fn a_strict_bulk_writes_nothing_after_the_operation_the_engine_refuses() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("strict", 0, 0, 1));
    let huge = "h".repeat(200_000);
    // All three share one run: it is split to find the refused one, and the third must not be
    // written beside it.
    let got = host.bulk(
        vec![put("a", "1"), put("b", &huge), put("c", "3")],
        Wait::Searchable,
    );
    assert!(matches!(got, Err(HostError::Refused(_))), "{got:?}");
    host.refresh().unwrap();
    assert_eq!(tag(&host, "a").as_deref(), Some("1"));
    assert_eq!(tag(&host, "b"), None);
    assert_eq!(tag(&host, "c"), None);
}

#[test]
fn an_evolve_that_cannot_publish_its_reader_yet_still_succeeds() {
    let (store, lane) = new_lane();
    let host = open(&store, lane, config("evolve-pinned", 0, 0, 1));
    let mut held = Vec::new();
    for i in 0..2 {
        host.bulk(vec![put(&format!("k{i}"), "x")], Wait::Searchable)
            .unwrap();
        held.extend(host.reader());
    }
    let evolved = schema().evolve(2, &[("extra", Kind::Keyword)]).unwrap();
    host.evolve(evolved, deps()).unwrap();
    drop(held);
    host.refresh().unwrap();
    assert!(host.reader().is_some());
}
