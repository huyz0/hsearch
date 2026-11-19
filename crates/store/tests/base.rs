//! A serving writer publishes a base that `recover_writer` opens, and a recovery can hold its base
//! with a pin while it fetches.
#[path = "support/history.rs"]
mod history;
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use history::*;
use hsearch_commit::Dependencies;
use hsearch_engine::{Engine, Mutation, StoredValue, Value, with_local_engine};
use hsearch_lane::{block_on, read_head};
use hsearch_store::pins::Pins;
use hsearch_store::publish::AnyDependencies;
use hsearch_store::rebuild::{Hold, RebuildError, recover_writer, recover_writer_held};
use hsearch_store::recover::Expect;
use hsearch_store::serve::{BaseError, BaseTarget, Serving};
use support::*;

fn deps() -> Dependencies {
    Dependencies {
        schema: [1; 32],
        crypto: [1; 32],
        resolved: [1; 32],
    }
}

/// Write batch `i` of the history through the serving writer.
fn write_batch<S, D>(
    writer: &mut Serving<S, D>,
    engine: &Engine<'_>,
    i: usize,
    steps: &[Step],
) -> Result<u64, hsearch_store::serve::ServeError>
where
    S: hsearch_object::MetadataStore,
    D: hsearch_commit::DependencyView,
{
    let fields: Vec<[(&str, Value<'_>); 1]> = steps
        .iter()
        .map(|s| [("tag", Value::String(s.tag.unwrap_or("")))])
        .collect();
    let mutations: Vec<Mutation<'_>> = steps
        .iter()
        .zip(&fields)
        .map(|(s, f)| match s.tag {
            Some(_) => Mutation::Put {
                id: s.id,
                version: s.version,
                fields: f,
            },
            None => Mutation::Delete {
                id: s.id,
                version: s.version,
            },
        })
        .collect();
    writer
        .write(&schema(), engine, &format!("r{i}"), &mutations)
        .map(|written| written.committed.map_or(0, |c| c.ordinal))
}

fn target_of(lane: &hsearch_lane::LaneId) -> BaseTarget<'_> {
    BaseTarget {
        lane,
        shard: SHARD,
        incarnation: 1,
        engine_format: "lucene-test",
        mapping: [5; 32],
        crypto_envelope: [6; 16],
    }
}

/// Everything the engine holds for the history's ids, as the ledger states it.
fn assert_ledger(engine: &Engine<'_>, h: &[Vec<Step>]) {
    let snapshot = engine.refresh().unwrap();
    for (id, (version, tag)) in ledger(h) {
        let got = snapshot.get(id, &["tag"]).unwrap();
        match tag {
            None => assert!(got.is_none(), "{id}"),
            Some(tag) => {
                let hit = got.unwrap_or_else(|| panic!("{id} missing"));
                assert_eq!(hit.version, version, "{id}");
                assert_eq!(
                    hit.fields,
                    vec![("tag".to_string(), StoredValue::String(tag.to_string()))],
                    "{id}"
                );
            }
        }
    }
}

#[test]
fn a_base_the_serving_writer_publishes_is_opened_by_recovery() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let h = history();
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-base-w-{}", std::process::id()));
    with_local_engine(&dir, schema.clone(), |engine, local| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        // Nothing committed yet: there is nothing for a base to cover.
        let p = publisher(&store);
        assert!(matches!(
            writer.publish_base(
                engine,
                local,
                &p,
                &target_of(&lane),
                token(1),
                &AnyDependencies
            ),
            Err(BaseError::Nothing)
        ));
        for (i, steps) in h.iter().enumerate().take(3) {
            write_batch(&mut writer, engine, i, steps).unwrap();
        }
        let first = writer
            .publish_base(
                engine,
                local,
                &p,
                &target_of(&lane),
                token(2),
                &AnyDependencies,
            )
            .unwrap();
        assert_eq!(first.root.covered, 3);
        // A second base after more writes replaces the first and covers more.
        for (i, steps) in h.iter().enumerate().skip(3) {
            write_batch(&mut writer, engine, i, steps).unwrap();
        }
        // The writer's own files are not what recovery reads: it reads the store.
        assert_ledger(engine, &h);
        let second = writer
            .publish_base(
                engine,
                local,
                &p,
                &target_of(&lane),
                token(3),
                &AnyDependencies,
            )
            .unwrap();
        assert_eq!(second.root.covered, 5);
        assert_eq!(second.root.generation, first.root.generation + 1);
    })
    .unwrap();
    // Recovery from the second base replays no tail and matches the ledger.
    let scratch = std::env::temp_dir().join(format!("hsearch-base-r-{}", std::process::id()));
    recover_writer(
        &publisher(&store),
        &target(&lane, big(), Expect::default()),
        &AnyDependencies,
        &schema,
        &scratch,
        |engine, _, rebuilt| {
            assert_eq!(rebuilt.recovered.from_seq, 5);
            assert_eq!(rebuilt.recovered.replayed, 0);
            assert_ledger(engine, &h);
        },
    )
    .unwrap();
}

#[test]
fn a_base_then_a_tail_recovers_and_the_recovered_writer_continues() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let h = history();
    let schema = schema();
    let dir = std::env::temp_dir().join(format!("hsearch-base-t-{}", std::process::id()));
    with_local_engine(&dir, schema.clone(), |engine, local| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        for (i, steps) in h.iter().enumerate().take(2) {
            write_batch(&mut writer, engine, i, steps).unwrap();
        }
        writer
            .publish_base(
                engine,
                local,
                &publisher(&store),
                &target_of(&lane),
                token(2),
                &AnyDependencies,
            )
            .unwrap();
        for (i, steps) in h.iter().enumerate().skip(2) {
            write_batch(&mut writer, engine, i, steps).unwrap();
        }
    })
    .unwrap();
    let scratch = std::env::temp_dir().join(format!("hsearch-base-tr-{}", std::process::id()));
    recover_writer(
        &publisher(&store),
        &target(&lane, big(), Expect::default()),
        &AnyDependencies,
        &schema,
        &scratch,
        |engine, _, rebuilt| {
            assert_eq!(rebuilt.recovered.from_seq, 2);
            assert_eq!(rebuilt.recovered.replayed, 3);
            assert_ledger(engine, &h);
        },
    )
    .unwrap();
}

fn pins(store: &Arc<Crash>) -> Pins<Crash> {
    Pins::new(Arc::clone(store), layout(), Duration::from_secs(30))
}

fn world(label: &str) -> (Arc<Crash>, hsearch_lane::LaneId) {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let schema = schema();
    let h = history();
    let dir = std::env::temp_dir().join(format!("hsearch-base-{label}-{}", std::process::id()));
    with_local_engine(&dir, schema, |engine, local| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        for (i, steps) in h.iter().enumerate().take(3) {
            write_batch(&mut writer, engine, i, steps).unwrap();
        }
        writer
            .publish_base(
                engine,
                local,
                &publisher(&store),
                &target_of(&lane),
                token(2),
                &AnyDependencies,
            )
            .unwrap();
    })
    .unwrap();
    (store, lane)
}

fn pin_head(store: &Arc<Crash>) -> Result<(), hsearch_object::Error> {
    let object = hsearch_object::ObjectKey::new(layout().pin(&INDEX, &[9; 16]).as_bytes()).unwrap();
    block_on(hsearch_object::ObjectBackend::head(
        &**store,
        &ctx(),
        &object,
    ))
    .map(|_| ())
}

fn hold<'a>(pins: &'a Pins<Crash>, now: &'a dyn Fn() -> u64, ttl_ms: u64) -> Hold<'a, Crash> {
    Hold {
        pins,
        pin: [9; 16],
        token: token(7),
        ttl_ms,
        skew_ms: 0,
        now,
    }
}

fn scratch_dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "hsearch-base-{label}-scratch-{}",
        std::process::id()
    ))
}

#[test]
fn a_recovery_holds_its_base_with_a_pin_and_releases_it() {
    let (store, lane) = world("pin");
    let pins = pins(&store);
    let now = || 1_000u64;
    let before = store.writes();
    recover_writer_held(
        &publisher(&store),
        &target(&lane, big(), Expect::default()),
        &AnyDependencies,
        &schema(),
        &scratch_dir("pin"),
        Some(&hold(&pins, &now, 60_000)),
        |_, _, rebuilt| assert_eq!(rebuilt.recovered.from_seq, 3),
    )
    .unwrap();
    // One store write for the recovery, the pin; and it is gone once the files are local.
    assert_eq!(store.writes() - before, 1);
    assert!(matches!(
        pin_head(&store),
        Err(hsearch_object::Error::NotFound)
    ));
}

#[test]
fn the_pin_is_registered_before_the_first_file_is_fetched() {
    let (store, lane) = world("order");
    let pins = pins(&store);
    let now = || 1_000u64;
    // The store takes no more writes, so the pin cannot be registered: nothing may be fetched.
    store.allow.store(0, Ordering::SeqCst);
    let scratch = scratch_dir("order");
    let err = recover_writer_held(
        &publisher(&store),
        &target(&lane, big(), Expect::default()),
        &AnyDependencies,
        &schema(),
        &scratch,
        Some(&hold(&pins, &now, 60_000)),
        |_, _, _| (),
    )
    .unwrap_err();
    assert!(matches!(err, RebuildError::Pin(_)), "{err:?}");
    assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
}

#[test]
fn a_pin_that_lapses_during_the_fetch_stops_the_recovery_and_is_released() {
    let (store, lane) = world("lapse");
    let pins = pins(&store);
    // The clock moves a second at every reading, and the pin lives a little over one.
    let tick = AtomicU64::new(1_000);
    let now = || tick.fetch_add(1_000, Ordering::SeqCst);
    let err = recover_writer_held(
        &publisher(&store),
        &target(&lane, big(), Expect::default()),
        &AnyDependencies,
        &schema(),
        &scratch_dir("lapse"),
        Some(&hold(&pins, &now, 1_500)),
        |_, _, _| (),
    )
    .unwrap_err();
    assert_eq!(err, RebuildError::PinLapsed);
    assert!(matches!(
        pin_head(&store),
        Err(hsearch_object::Error::NotFound)
    ));
}

#[test]
fn a_pin_with_no_lifetime_is_refused_before_anything_is_fetched() {
    let (store, lane) = world("expired");
    let pins = pins(&store);
    let now = || 1_000u64;
    let scratch = scratch_dir("expired");
    let err = recover_writer_held(
        &publisher(&store),
        &target(&lane, big(), Expect::default()),
        &AnyDependencies,
        &schema(),
        &scratch,
        Some(&hold(&pins, &now, 0)),
        |_, _, _| (),
    )
    .unwrap_err();
    assert!(matches!(err, RebuildError::Pin(_)), "{err:?}");
    assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
}

#[test]
fn the_lane_cover_is_one_past_the_last_slot_even_when_slots_belong_to_others() {
    let store = Crash::new();
    let (lane, mut committer) = open_lane(&store);
    // A record of another member takes the first slot, so the index's first record is at slot 1.
    commit(&mut committer, req(B, 0, "other"));
    let h = history();
    let dir = std::env::temp_dir().join(format!("hsearch-base-cover-{}", std::process::id()));
    let last = with_local_engine(&dir, schema(), |engine, local| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        let mut last = 0;
        for (i, steps) in h.iter().enumerate().take(2) {
            last = write_batch(&mut writer, engine, i, steps).unwrap();
        }
        // The index's first record is not at slot 0: the lane's opening fence and the other
        // member's record come first.
        assert!(last >= 3);
        writer
            .publish_base(
                engine,
                local,
                &publisher(&store),
                &target_of(&lane),
                token(2),
                &AnyDependencies,
            )
            .unwrap();
        last
    })
    .unwrap();
    let (head, _) = block_on(read_head(store.inner_ref(), &layout(), &lane)).unwrap();
    assert!(head.checkpoint.is_some());
    assert_eq!(head.cover, last + 1);
}

#[test]
fn a_recovered_writer_can_publish_before_it_writes_and_replaces_the_base_it_recovered_on() {
    let (store, lane) = world("resume");
    let h = history();
    let schema = schema();
    let committer = writer(&store, lane);
    let scratch = scratch_dir("resume");
    let (again, later) = recover_writer(
        &publisher(&store),
        &target(&lane, big(), Expect::default()),
        &AnyDependencies,
        &schema,
        &scratch,
        |engine, local, rebuilt| {
            let mut serving = Serving::recovered(committer, INDEX, &rebuilt.recovered, deps());
            assert_eq!(serving.next_seq(), 3);
            let publish = |serving: &mut Serving<_, _>, n| {
                serving
                    .publish_base(
                        engine,
                        local,
                        &publisher(&store),
                        &target_of(&lane),
                        token(n),
                        &AnyDependencies,
                    )
                    .unwrap()
            };
            // Nothing written yet: the recovered writer already knows its cover and its base.
            let again = publish(&mut serving, 4);
            for (i, steps) in h.iter().enumerate().skip(3) {
                write_batch(&mut serving, engine, i, steps).unwrap();
            }
            (again, publish(&mut serving, 5))
        },
    )
    .unwrap();
    // Each base replaced the one before it rather than starting the generations over.
    assert_eq!((again.root.generation, again.root.covered), (2, 3));
    assert_eq!((later.root.generation, later.root.covered), (3, 5));
}

#[test]
fn a_broken_writer_publishes_nothing() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let h = history();
    let dir = std::env::temp_dir().join(format!("hsearch-base-broken-{}", std::process::id()));
    with_local_engine(&dir, schema(), |engine, local| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        write_batch(&mut writer, engine, 0, &h[0]).unwrap();
        // The next write's reply is lost and cannot be resolved: the writer is broken.
        store.fail_resolve.store(true, Ordering::SeqCst);
        store.lose.store(store.writes() as i64, Ordering::SeqCst);
        assert!(write_batch(&mut writer, engine, 1, &h[1]).is_err());
        assert!(matches!(
            writer.publish_base(
                engine,
                local,
                &publisher(&store),
                &target_of(&lane),
                token(2),
                &AnyDependencies
            ),
            Err(BaseError::Broken)
        ));
        assert!(
            block_on(read_head(store.inner_ref(), &layout(), &lane))
                .unwrap()
                .0
                .checkpoint
                .is_none()
        );
    })
    .unwrap();
}

/// Raise the shard's collection boundary the way a sweep does: the object a publish reads.
fn raise_boundary(store: &Arc<Crash>, to: u64) {
    use sha2::{Digest as _, Sha256};
    let mut bytes = b"HSGB\x01".to_vec();
    bytes.extend_from_slice(&to.to_le_bytes());
    let digest = Sha256::digest(&bytes);
    bytes.extend_from_slice(&digest[..8]);
    let ctx = hsearch_object::RequestContext::new(
        Duration::from_secs(5),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 4096,
            response_bytes: 4096,
            attempts: 4,
        },
    )
    .unwrap();
    let put = hsearch_object::Mutation {
        request_id: hsearch_object::RequestId::new(b"raise-boundary").unwrap(),
        key: hsearch_object::ObjectKey::new(layout().gc_boundary(&INDEX, &SHARD, 1).as_bytes())
            .unwrap(),
        payload: hsearch_object::Payload::new(&bytes).unwrap(),
        metadata: hsearch_object::Metadata::new(b"").unwrap(),
        precondition: hsearch_object::Precondition::Absent,
    };
    block_on(hsearch_object::ObjectBackend::create(
        store.inner_ref(),
        &ctx,
        &put,
    ))
    .unwrap();
}

#[test]
fn a_base_below_the_collection_boundary_is_refused_and_leaves_the_head_alone() {
    let store = Crash::new();
    let (lane, committer) = open_lane(&store);
    let h = history();
    let dir = std::env::temp_dir().join(format!("hsearch-base-swept-{}", std::process::id()));
    with_local_engine(&dir, schema(), |engine, local| {
        let mut writer = Serving::new(committer, INDEX, 0, deps());
        write_batch(&mut writer, engine, 0, &h[0]).unwrap();
        let p = publisher(&store);
        let first = writer
            .publish_base(
                engine,
                local,
                &p,
                &target_of(&lane),
                token(1),
                &AnyDependencies,
            )
            .unwrap();
        let named = block_on(read_head(store.inner_ref(), &layout(), &lane))
            .unwrap()
            .0
            .checkpoint;
        assert_eq!(named, Some(first.manifest));
        raise_boundary(&store, first.root.generation + 5);
        write_batch(&mut writer, engine, 1, &h[1]).unwrap();
        let swept = writer.publish_base(
            engine,
            local,
            &p,
            &target_of(&lane),
            token(2),
            &AnyDependencies,
        );
        assert!(
            matches!(
                swept,
                Err(BaseError::Publish(
                    hsearch_store::publish::Error::Swept { .. }
                ))
            ),
            "{swept:?}"
        );
        // Nothing names the refused base, and the writer is not broken: writes still land.
        let after = block_on(read_head(store.inner_ref(), &layout(), &lane))
            .unwrap()
            .0
            .checkpoint;
        assert_eq!(after, named);
        write_batch(&mut writer, engine, 2, &h[2]).unwrap();
    })
    .unwrap();
}
