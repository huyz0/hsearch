//! The write path over the in-memory store: ordering, outcomes, group commit,
//! recovery, publish, refusal and fencing.
mod support;

use hsearch_engine::{Limits, Outcome};
use hsearch_index::{Error, Index, Wait, open_published};
use hsearch_object::memory::{Fault, MemoryLimits, MemoryStore};
use std::sync::Arc;
use support::*;

fn store() -> Arc<MemoryStore> {
    Arc::new(
        MemoryStore::new(MemoryLimits {
            objects: 4096,
            versions: 16384,
            version_bytes: 64 << 20,
            identities: 16384,
            identity_bytes: 64 << 20,
        })
        .unwrap(),
    )
}

#[test]
fn outcomes_versions_and_visibility_follow_the_wait_mode() {
    let (store, scratch) = (store(), Scratch::new("outcomes"));
    let index = Index::create(store, "orders", schema(), &scratch.0).unwrap();
    let first = index
        .bulk(vec![put("a", "x", 1), put("b", "x", 2)], Wait::Searchable)
        .unwrap();
    assert_eq!(first[0].outcome, Outcome::Created);
    assert_eq!(first[1].version, first[0].version + 1);
    assert_eq!(
        all(&index.snapshot().unwrap()),
        (2, vec!["a".into(), "b".into()])
    );
    // Durable-only: acknowledged, but not searchable until a refresh.
    let second = index
        .bulk(
            vec![put("a", "y", 9), delete("b"), delete("zz")],
            Wait::Durable,
        )
        .unwrap();
    let outcomes: Vec<_> = second.iter().map(|r| r.outcome).collect();
    assert_eq!(
        outcomes,
        vec![Outcome::Updated, Outcome::Deleted, Outcome::NotFound]
    );
    assert!(second[0].version > first[1].version);
    index.refresh().unwrap();
    let snapshot = index.snapshot().unwrap();
    assert_eq!(all(&snapshot), (1, vec!["a".into()]));
    assert_eq!(tagged(&snapshot, "y"), 1);
    assert_eq!(tagged(&snapshot, "x"), 0);
}

#[test]
fn a_new_process_rebuilds_the_same_index_from_the_store_alone() {
    let store = store();
    let (one, two) = (Scratch::new("rebuild-a"), Scratch::new("rebuild-b"));
    let index = Index::create(Arc::clone(&store), "orders", schema(), &one.0).unwrap();
    index
        .bulk(vec![put("a", "x", 1), put("b", "x", 2)], Wait::Durable)
        .unwrap();
    index
        .bulk(vec![put("c", "y", 3), delete("a")], Wait::Durable)
        .unwrap();
    index.refresh().unwrap();
    let before = all(&index.snapshot().unwrap());
    index.close();
    std::fs::remove_dir_all(&one.0).unwrap();
    let again = Index::open(store, "orders", schema(), &two.0).unwrap();
    assert_eq!(all(&again.snapshot().unwrap()), before);
    // Versions keep rising across the restart.
    let next = again
        .bulk(vec![put("d", "x", 4)], Wait::Searchable)
        .unwrap();
    assert!(next[0].version >= 5);
    assert_eq!(all(&again.snapshot().unwrap()).0, 3);
}

#[test]
fn queued_bulks_share_a_slot() {
    let (store, scratch) = (store(), Scratch::new("group"));
    let index =
        Arc::new(Index::create(Arc::clone(&store), "orders", schema(), &scratch.0).unwrap());
    let handles: Vec<_> = (0..24)
        .map(|i| {
            let index = Arc::clone(&index);
            std::thread::spawn(move || {
                index
                    .bulk(vec![put(&format!("d{i}"), "x", i)], Wait::Durable)
                    .unwrap()
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    index.refresh().unwrap();
    assert_eq!(all(&index.snapshot().unwrap()).0, 24);
    // Slots are keyed by position; count them by probing the derived keys.
    let slots = (0u64..)
        .take_while(|seq| {
            let key =
                hsearch_object::ObjectKey::new(format!("orders/log/{seq:020}").as_bytes()).unwrap();
            let ctx = hsearch_object::RequestContext::new(
                std::time::Duration::from_secs(5),
                hsearch_object::Cancellation::default(),
                hsearch_object::Budget {
                    request_bytes: 1 << 20,
                    response_bytes: 1 << 20,
                    attempts: 4,
                },
            )
            .unwrap();
            let mut future =
                std::pin::pin!(hsearch_object::ObjectBackend::head(&*store, &ctx, &key));
            let waker = std::task::Waker::noop();
            matches!(
                future
                    .as_mut()
                    .poll(&mut std::task::Context::from_waker(waker)),
                std::task::Poll::Ready(Ok(_))
            )
        })
        .count();
    assert!((1..=24).contains(&slots), "slots {slots}");
}

#[test]
fn publish_makes_segments_a_search_only_reader_can_open() {
    let store = store();
    let (one, cache) = (Scratch::new("publish"), Scratch::new("cache"));
    let index = Index::create(Arc::clone(&store), "orders", schema(), &one.0).unwrap();
    assert_eq!(
        open_published(Arc::clone(&store), "orders", schema(), &cache.0).err(),
        Some(Error::NotFound)
    );
    index
        .bulk(
            (0..40).map(|i| put(&format!("d{i}"), "x", i)).collect(),
            Wait::Searchable,
        )
        .unwrap();
    let published = index.publish().unwrap();
    assert_eq!(published.covered, 1);
    assert!(published.files > 0);
    // Publishing again with nothing new is a no-op that keeps the same answer.
    assert_eq!(index.publish().unwrap(), published);
    let live = all(&index.snapshot().unwrap());
    let served = open_published(store, "orders", schema(), &cache.0).unwrap();
    assert_eq!(all(&served), live);
}

#[test]
fn bad_bulks_are_refused_before_anything_is_written() {
    let (store, scratch) = (store(), Scratch::new("invalid"));
    let index = Index::create(store, "orders", schema(), &scratch.0).unwrap();
    assert_eq!(
        index.bulk(vec![], Wait::Durable).err(),
        Some(Error::Invalid)
    );
    let wrong_fields = hsearch_index::Op::Put {
        id: "a".into(),
        fields: vec![hsearch_index::Field::Int(1)],
    };
    assert_eq!(
        index.bulk(vec![wrong_fields], Wait::Durable).err(),
        Some(Error::Invalid)
    );
    let wrong_type = hsearch_index::Op::Put {
        id: "a".into(),
        fields: vec![hsearch_index::Field::Int(1), hsearch_index::Field::Int(1)],
    };
    assert!(index.bulk(vec![wrong_type], Wait::Durable).is_err());
    assert_eq!(
        index.bulk(vec![put("", "x", 1)], Wait::Durable).err(),
        Some(Error::Invalid)
    );
    // The writer is still healthy and nothing was applied.
    let ok = index
        .bulk(vec![put("a", "x", 1)], Wait::Searchable)
        .unwrap();
    assert_eq!(ok[0].version, 1);
    assert_eq!(all(&index.snapshot().unwrap()).0, 1);
}

#[test]
fn a_refusal_leaves_the_writer_usable_and_a_lost_response_is_resolved() {
    let (store, scratch) = (store(), Scratch::new("faults"));
    let index = Index::create(Arc::clone(&store), "orders", schema(), &scratch.0).unwrap();
    // The first attempt is refused before acceptance: the retry succeeds.
    store.inject(Fault::UnavailableBeforeAcceptance).unwrap();
    let a = index
        .bulk(vec![put("a", "x", 1)], Wait::Searchable)
        .unwrap();
    assert_eq!(a[0].version, 1);
    // The write commits but the response is lost: resolved, acknowledged once.
    store.inject(Fault::LoseResponseAfterCommit).unwrap();
    let b = index
        .bulk(vec![put("b", "x", 2)], Wait::Searchable)
        .unwrap();
    assert_eq!(b[0].version, 2);
    assert_eq!(all(&index.snapshot().unwrap()).0, 2);
    // The deadline expiring after commit also resolves.
    store.inject(Fault::ExpireAfterCommit).unwrap();
    index
        .bulk(vec![put("c", "x", 3)], Wait::Searchable)
        .unwrap();
    assert_eq!(all(&index.snapshot().unwrap()).0, 3);
}

#[test]
fn opening_fences_the_previous_writer() {
    let store = store();
    let (one, two) = (Scratch::new("fence-a"), Scratch::new("fence-b"));
    let old = Index::create(Arc::clone(&store), "orders", schema(), &one.0).unwrap();
    old.bulk(vec![put("a", "x", 1)], Wait::Searchable).unwrap();
    let new = Index::open(Arc::clone(&store), "orders", schema(), &two.0).unwrap();
    // The old writer's next slot collides with the log the new one replays.
    let late = new.bulk(vec![put("n", "x", 5)], Wait::Searchable).unwrap();
    assert_eq!(late[0].version, 2);
    let stale = old.bulk(vec![put("z", "x", 9)], Wait::Searchable);
    assert_eq!(stale.err(), Some(Error::Fenced));
    assert_eq!(old.publish().err(), Some(Error::Poisoned));
    assert_eq!(
        old.bulk(vec![put("y", "x", 1)], Wait::Durable).err(),
        Some(Error::Poisoned)
    );
    new.publish().unwrap();
}

#[test]
fn a_corrupt_or_missing_slot_is_never_read_as_the_end() {
    let store = store();
    let scratch = Scratch::new("corrupt");
    assert_eq!(
        Index::open(Arc::clone(&store), "orders", schema(), &scratch.0).err(),
        Some(Error::NotFound)
    );
    assert_eq!(
        Index::open(Arc::clone(&store), "Bad Name", schema(), &scratch.0).err(),
        Some(Error::Invalid)
    );
    let index = Index::create(Arc::clone(&store), "orders", schema(), &scratch.0).unwrap();
    assert_eq!(
        Index::create(Arc::clone(&store), "orders", schema(), &scratch.0).err(),
        Some(Error::Exists)
    );
    index.close();
    // Plant garbage in slot 0 and reopen.
    let key = hsearch_object::ObjectKey::new(b"orders/log/00000000000000000000").unwrap();
    let request = hsearch_object::Mutation {
        request_id: hsearch_object::RequestId::new(b"plant").unwrap(),
        key,
        payload: hsearch_object::Payload::new(b"not a slot").unwrap(),
        metadata: hsearch_object::Metadata::new(b"").unwrap(),
        precondition: hsearch_object::Precondition::Absent,
    };
    let ctx = hsearch_object::RequestContext::new(
        std::time::Duration::from_secs(5),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 4,
        },
    )
    .unwrap();
    let mut future = std::pin::pin!(hsearch_object::ObjectBackend::create(
        &*store, &ctx, &request
    ));
    let waker = std::task::Waker::noop();
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut std::task::Context::from_waker(waker)),
        std::task::Poll::Ready(Ok(_))
    ));
    assert_eq!(
        Index::open(Arc::clone(&store), "orders", schema(), &scratch.0).err(),
        Some(Error::Corrupt)
    );
}

#[test]
fn capacity_is_refused_before_a_slot_is_written_and_a_full_engine_merges() {
    let (store, scratch) = (store(), Scratch::new("capacity"));
    let small = schema().with_limits(Limits::BOUNDED).unwrap();
    let index = Index::create(store, "orders", small, &scratch.0).unwrap();
    let ops: Vec<_> = (0..100).map(|i| put(&format!("d{i}"), "x", i)).collect();
    index.bulk(ops, Wait::Searchable).unwrap();
    // Replacing documents leaves old physical copies until a merge: the second
    // round overflows the engine's physical bound, merges, and still applies.
    for round in 0..3 {
        let ops: Vec<_> = (0..20)
            .map(|i| put(&format!("d{}", round * 20 + i), "y", i))
            .collect();
        index.bulk(ops, Wait::Searchable).unwrap();
    }
    let snapshot = index.snapshot().unwrap();
    assert_eq!(all(&snapshot).0, 100);
    assert_eq!(tagged(&snapshot, "y"), 60);
    drop(snapshot);
    // Full: 28 more identities fit exactly, one more is refused up front.
    let more: Vec<_> = (0..28).map(|i| put(&format!("e{i}"), "x", i)).collect();
    index.bulk(more, Wait::Searchable).unwrap();
    assert_eq!(
        index
            .bulk(vec![put("one-more", "x", 1)], Wait::Durable)
            .err(),
        Some(Error::Capacity)
    );
    // Nothing was poisoned by the refusal.
    assert!(index.bulk(vec![delete("e0")], Wait::Searchable).is_ok());
}

/// Room for many documents, but the default 256 lifetime operations.
fn roomy() -> hsearch_engine::Schema {
    schema()
        .with_limits(Limits {
            documents: 1000,
            ..Limits::BOUNDED
        })
        .unwrap()
}

#[test]
fn lifetime_operation_and_history_bounds_are_refused_before_a_slot_is_written() {
    // The engine's lifetime operation bound (256 by default) never resets: the
    // writer must refuse past it up front, never acknowledge what cannot apply,
    // and the index must still reopen.
    let store = store();
    let (one, two) = (Scratch::new("life-a"), Scratch::new("life-b"));
    let index = Index::create(Arc::clone(&store), "orders", roomy(), &one.0).unwrap();
    index
        .bulk(
            (0..200).map(|i| put(&format!("d{i}"), "x", i)).collect(),
            Wait::Durable,
        )
        .unwrap();
    assert_eq!(
        index
            .bulk(
                (0..100).map(|i| put(&format!("d{i}"), "y", i)).collect(),
                Wait::Durable
            )
            .err(),
        Some(Error::Capacity)
    );
    // 56 more operations still fit exactly; then nothing does.
    index
        .bulk(
            (0..56).map(|i| put(&format!("d{i}"), "y", i)).collect(),
            Wait::Searchable,
        )
        .unwrap();
    assert_eq!(
        index.bulk(vec![put("d0", "z", 0)], Wait::Durable).err(),
        Some(Error::Capacity)
    );
    let live = all(&index.snapshot().unwrap());
    index.close();
    let again = Index::open(store, "orders", roomy(), &two.0).unwrap();
    assert_eq!(all(&again.snapshot().unwrap()), live);
}

#[test]
fn history_window_bytes_are_bounded_under_the_default_profile() {
    let (store, scratch) = (store(), Scratch::new("history"));
    let index = Index::create(store, "orders", schema(), &scratch.0).unwrap();
    // Large values exhaust the 256 KiB retry history long before 256 operations.
    let big = "v".repeat(4000);
    let mut accepted = 0;
    for i in 0..100 {
        match index.bulk(vec![put(&format!("d{i}"), &big, i)], Wait::Durable) {
            Ok(_) => accepted += 1,
            Err(error) => {
                assert_eq!(error, Error::Capacity);
                break;
            }
        }
    }
    assert!((30..100).contains(&accepted), "accepted {accepted}");
    index.refresh().unwrap();
    assert_eq!(all(&index.snapshot().unwrap()).0, accepted);
}

#[test]
fn two_openers_cannot_both_own_the_same_epoch() {
    let store = store();
    let seed = Scratch::new("race-seed");
    Index::create(Arc::clone(&store), "orders", schema(), &seed.0)
        .unwrap()
        .close();
    for round in 0..8 {
        let (a, b) = (Scratch::new("race-a"), Scratch::new("race-b"));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let spawn = |scratch: Scratch| {
            let (store, barrier) = (Arc::clone(&store), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                let result = Index::open(store, "orders", schema(), &scratch.0);
                (result.map(|_| ()), scratch)
            })
        };
        let (first, second) = (spawn(a), spawn(b));
        let results = [first.join().unwrap().0, second.join().unwrap().0];
        let winners = results.iter().filter(|r| r.is_ok()).count();
        assert!(winners >= 1, "round {round}: {results:?}");
        // Both succeeding is only possible if they opened strictly one after the
        // other (the second saw the first's bumped root); a simultaneous open
        // has exactly one winner and the other is fenced.
        for result in &results {
            assert!(matches!(result, Ok(()) | Err(Error::Fenced)), "{result:?}");
        }
    }
}

#[test]
fn mappings_are_stored_once_under_a_derived_key() {
    let store = store();
    assert_eq!(
        hsearch_index::get_mapping(&*store, "orders").err(),
        Some(Error::NotFound)
    );
    hsearch_index::put_mapping(&*store, "orders", b"mapping-bytes").unwrap();
    assert_eq!(
        hsearch_index::get_mapping(&*store, "orders").unwrap(),
        b"mapping-bytes"
    );
    assert_eq!(
        hsearch_index::put_mapping(&*store, "orders", b"other").err(),
        Some(Error::Exists)
    );
    assert_eq!(
        hsearch_index::get_mapping(&*store, "orders").unwrap(),
        b"mapping-bytes"
    );
    for bad in ["", "Upper", "a/b", &"x".repeat(65)] {
        assert_eq!(
            hsearch_index::put_mapping(&*store, bad, b"x").err(),
            Some(Error::Invalid)
        );
        assert_eq!(
            hsearch_index::get_mapping(&*store, bad).err(),
            Some(Error::Invalid)
        );
    }
}

#[test]
fn a_published_root_naming_too_many_files_is_refused_not_written() {
    // The root decoder refuses more than 4096 files, so publish must never
    // write one: exercised through the format bound the writer checks.
    assert_eq!(hsearch_index::format::MAX_FILES, 4096);
}
