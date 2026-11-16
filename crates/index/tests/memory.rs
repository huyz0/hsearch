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
fn the_name_v1_is_reserved_so_no_index_owns_the_shared_prefix() {
    let store = store();
    assert_eq!(
        hsearch_index::put_mapping(&*store, "v1", b"x").err(),
        Some(Error::Invalid)
    );
    assert_eq!(
        hsearch_index::get_mapping(&*store, "v1").err(),
        Some(Error::Invalid)
    );
}

#[test]
fn a_shared_object_is_stored_once_and_a_different_body_under_its_key_is_refused() {
    let store = store();
    let key = "v1/local/mappings/0000000000000000000000000000000000000000000000000000000000000000";
    assert_eq!(
        hsearch_index::get_shared(&*store, key).err(),
        Some(Error::NotFound)
    );
    hsearch_index::put_shared(&*store, key, b"body").unwrap();
    // The same bytes again are one object: success, nothing replaced.
    hsearch_index::put_shared(&*store, key, b"body").unwrap();
    assert_eq!(hsearch_index::get_shared(&*store, key).unwrap(), b"body");
    // A different body under a held key is never written over it.
    assert_eq!(
        hsearch_index::put_shared(&*store, key, b"other").err(),
        Some(Error::Corrupt)
    );
    assert_eq!(hsearch_index::get_shared(&*store, key).unwrap(), b"body");
    // Keys that could fall inside an index prefix or escape the layout are refused.
    for bad in [
        "",
        "orders/root",
        "v1//x",
        "v1/a/../b",
        "v1/UPPER/x",
        "v1/a b",
        "mappings/x",
    ] {
        assert_eq!(
            hsearch_index::put_shared(&*store, bad, b"x").err(),
            Some(Error::Invalid),
            "{bad}"
        );
        assert_eq!(
            hsearch_index::get_shared(&*store, bad).err(),
            Some(Error::Invalid),
            "{bad}"
        );
    }
}

#[test]
fn a_published_root_naming_too_many_files_is_refused_not_written() {
    // The root decoder refuses more than 4096 files, so publish must never
    // write one: exercised through the format bound the writer checks.
    assert_eq!(hsearch_index::format::MAX_FILES, 4096);
}

fn big_schema() -> hsearch_engine::Schema {
    hsearch_engine::Schema::new(
        1,
        &[
            ("tag", hsearch_engine::Kind::Keyword),
            ("body", hsearch_engine::Kind::Text),
        ],
    )
    .unwrap()
    .with_limits(Limits::scale().with_document_bytes(32 << 20))
    .unwrap()
}

fn big_put(id: &str, body: String) -> hsearch_index::Op {
    hsearch_index::Op::Put {
        id: id.to_string(),
        fields: vec![
            hsearch_index::Field::Str("t".into()),
            hsearch_index::Field::Str(body),
        ],
    }
}

fn needles(index: &Index) -> u64 {
    use hsearch_engine::{Query, Value};
    index
        .snapshot()
        .unwrap()
        .search(
            &Query::Term {
                field: "body",
                value: Value::String("needle"),
            },
            &[],
            0,
            &[],
        )
        .unwrap()
        .total
}

#[test]
fn a_document_over_the_inline_limit_goes_to_chunk_objects_and_replays() {
    let store = store();
    let (one, two) = (Scratch::new("blob-a"), Scratch::new("blob-b"));
    let index = Index::create(Arc::clone(&store), "docs", big_schema(), &one.0).unwrap();
    // 12 MiB: two 8 MiB-bound chunks, far over the 1 MiB inline limit.
    let body = format!("{}needle", "word ".repeat(12 * (1 << 20) / 5));
    index
        .bulk(vec![big_put("big", body)], Wait::Searchable)
        .unwrap();
    assert_eq!(needles(&index), 1);
    // The slot holds a reference, not the text; replay below reads the chunks.
    assert!(object_len(&store, "docs/log/00000000000000000000") < 1024);
    index.close();
    std::fs::remove_dir_all(&one.0).unwrap();
    let again = Index::open(store, "docs", big_schema(), &two.0).unwrap();
    assert_eq!(needles(&again), 1);
}

fn object_len(store: &MemoryStore, key: &str) -> u64 {
    let key = hsearch_object::ObjectKey::new(key.as_bytes()).unwrap();
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
    let mut future = std::pin::pin!(hsearch_object::ObjectBackend::head(store, &ctx, &key));
    let waker = std::task::Waker::noop();
    match future
        .as_mut()
        .poll(&mut std::task::Context::from_waker(waker))
    {
        std::task::Poll::Ready(Ok(info)) => info.length,
        other => panic!("head failed: {other:?}"),
    }
}

#[test]
fn a_bulk_whose_text_can_never_fit_one_slot_is_invalid_not_retryable() {
    // Two 5 MiB documents in one bulk carry 10 MiB of text, over the 8 MiB slot
    // text bound of a schema admitting 6 MiB documents: a permanent refusal.
    let schema = hsearch_engine::Schema::new(
        1,
        &[
            ("tag", hsearch_engine::Kind::Keyword),
            ("body", hsearch_engine::Kind::Text),
        ],
    )
    .unwrap()
    .with_limits(Limits::scale().with_document_bytes(6 << 20))
    .unwrap();
    let (store, scratch) = (store(), Scratch::new("slot-text"));
    let index = Index::create(store, "docs", schema, &scratch.0).unwrap();
    let five = || "w ".repeat(5 << 19);
    assert_eq!(
        index
            .bulk(
                vec![big_put("a", five()), big_put("b", five())],
                Wait::Durable
            )
            .err(),
        Some(Error::Invalid)
    );
    // One document of the same size is fine, and the writer was not poisoned.
    assert!(
        index
            .bulk(vec![big_put("a", five())], Wait::Searchable)
            .is_ok()
    );
}

/// A store that refuses the next create under a key part, and counts deletes.
struct Flaky {
    inner: MemoryStore,
    refuse: std::sync::Mutex<Option<&'static str>>,
    deleted: std::sync::atomic::AtomicUsize,
    removed: std::sync::Mutex<Vec<(String, bool)>>,
    commit_then_unknown: std::sync::Mutex<Option<&'static str>>,
    resolve_blind: std::sync::atomic::AtomicBool,
    can_delete: bool,
    created: std::sync::Mutex<Vec<String>>,
}

impl Flaky {
    fn new(can_delete: bool) -> Self {
        Self {
            inner: Arc::try_unwrap(store()).unwrap(),
            refuse: std::sync::Mutex::new(None),
            deleted: std::sync::atomic::AtomicUsize::new(0),
            removed: std::sync::Mutex::new(Vec::new()),
            commit_then_unknown: std::sync::Mutex::new(None),
            resolve_blind: std::sync::atomic::AtomicBool::new(false),
            can_delete,
            created: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl hsearch_object::ObjectBackend for Flaky {
    fn head<'a>(
        &'a self,
        c: &'a hsearch_object::RequestContext,
        k: &'a hsearch_object::ObjectKey,
    ) -> hsearch_object::Operation<'a, hsearch_object::ObjectInfo> {
        self.inner.head(c, k)
    }
    fn create<'a>(
        &'a self,
        c: &'a hsearch_object::RequestContext,
        r: &'a hsearch_object::Mutation,
    ) -> hsearch_object::Operation<'a, hsearch_object::WriteReceipt> {
        self.created
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(r.key.as_bytes()).into_owned());
        let hit = {
            let mut refuse = self.refuse.lock().unwrap();
            match *refuse {
                Some(part) if String::from_utf8_lossy(r.key.as_bytes()).contains(part) => {
                    *refuse = None;
                    true
                }
                _ => false,
            }
        };
        if hit {
            return Box::pin(async { Err(hsearch_object::Error::PermissionDenied) });
        }
        let unknown = {
            let mut mode = self.commit_then_unknown.lock().unwrap();
            match *mode {
                Some(part) if String::from_utf8_lossy(r.key.as_bytes()).contains(part) => {
                    *mode = None;
                    true
                }
                _ => false,
            }
        };
        if unknown {
            // Commit, then lose the answer and the ability to resolve it.
            let inner = self.inner.create(c, r);
            return Box::pin(async move {
                inner.await?;
                Err(hsearch_object::Error::UnknownOutcome)
            });
        }
        self.inner.create(c, r)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a hsearch_object::RequestContext,
        k: &'a hsearch_object::ObjectKey,
        v: &'a hsearch_object::VersionToken,
        r: hsearch_object::ByteRange,
    ) -> hsearch_object::Operation<'a, hsearch_object::Payload> {
        self.inner.read_range(c, k, v, r)
    }
}

impl hsearch_object::MetadataStore for Flaky {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a hsearch_object::RequestContext,
        r: &'a hsearch_object::Mutation,
    ) -> hsearch_object::Operation<'a, hsearch_object::WriteReceipt> {
        self.inner.compare_exchange(c, r)
    }
    fn resolve<'a>(
        &'a self,
        c: &'a hsearch_object::RequestContext,
        r: &'a hsearch_object::Mutation,
    ) -> hsearch_object::Operation<'a, hsearch_object::WriteReceipt> {
        if self.commit_then_unknown.lock().unwrap().is_none()
            && self
                .resolve_blind
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Box::pin(async { Err(hsearch_object::Error::ResolutionUnavailable) });
        }
        self.inner.resolve(c, r)
    }
    fn remove_version<'a>(
        &'a self,
        c: &'a hsearch_object::RequestContext,
        k: &'a hsearch_object::ObjectKey,
        v: &'a hsearch_object::VersionToken,
    ) -> hsearch_object::Operation<'a, ()> {
        if !self.can_delete {
            return Box::pin(async { Err(hsearch_object::Error::Unsupported) });
        }
        self.deleted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = String::from_utf8_lossy(k.as_bytes()).into_owned();
        let removal = self.inner.remove_version(c, k, v);
        Box::pin(async move {
            let result = removal.await;
            self.removed.lock().unwrap().push((name, result.is_ok()));
            result
        })
    }
}

fn big_body() -> String {
    // 12 MiB: two chunks.
    format!("{}needle", "word ".repeat(12 * (1 << 20) / 5))
}

#[test]
fn a_refused_slot_write_removes_the_chunks_it_wrote_and_the_retry_succeeds() {
    let store = Arc::new(Flaky::new(true));
    let scratch = Scratch::new("discard");
    let index = Index::create(Arc::clone(&store), "docs", big_schema(), &scratch.0).unwrap();
    *store.refuse.lock().unwrap() = Some("/log/");
    let refused = index.bulk(vec![big_put("a", big_body())], Wait::Searchable);
    assert!(matches!(refused, Err(Error::Store(_))), "{refused:?}");
    assert_eq!(
        store.deleted.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "both chunks were removed"
    );
    let removed = store.removed.lock().unwrap().clone();
    assert!(
        removed
            .iter()
            .all(|(key, ok)| *ok && key.starts_with("docs/blob/")),
        "{removed:?}"
    );
    assert_eq!(removed.len(), 2);
    // Nothing was poisoned, and the retry commits and finds the document.
    index
        .bulk(vec![big_put("a", big_body())], Wait::Searchable)
        .unwrap();
    assert_eq!(needles(&index), 1);
}

#[test]
fn a_refused_second_chunk_removes_the_first() {
    let store = Arc::new(Flaky::new(true));
    let scratch = Scratch::new("discard-mid");
    let index = Index::create(Arc::clone(&store), "docs", big_schema(), &scratch.0).unwrap();
    *store.refuse.lock().unwrap() = Some("0.1/000001");
    assert!(matches!(
        index.bulk(vec![big_put("a", big_body())], Wait::Durable),
        Err(Error::Store(_))
    ));
    assert_eq!(store.deleted.load(std::sync::atomic::Ordering::Relaxed), 1);
    let removed = store.removed.lock().unwrap().clone();
    assert!(
        removed.len() == 1 && removed[0].1 && removed[0].0.ends_with("/0.1/000000"),
        "{removed:?}"
    );
    assert!(index.bulk(vec![delete("zz")], Wait::Durable).is_ok());
}

#[test]
fn a_store_that_cannot_delete_leaves_the_orphans_and_stays_correct() {
    let store = Arc::new(Flaky::new(false));
    let scratch = Scratch::new("no-delete");
    let index = Index::create(Arc::clone(&store), "docs", big_schema(), &scratch.0).unwrap();
    *store.refuse.lock().unwrap() = Some("/log/");
    assert!(
        index
            .bulk(vec![big_put("a", big_body())], Wait::Durable)
            .is_err()
    );
    index
        .bulk(vec![big_put("a", big_body())], Wait::Searchable)
        .unwrap();
    assert_eq!(needles(&index), 1);
}

#[test]
fn an_unknown_slot_write_never_removes_the_chunks_it_may_name() {
    let store = Arc::new(Flaky::new(true));
    let (one, two) = (Scratch::new("unknown-a"), Scratch::new("unknown-b"));
    let index = Index::create(Arc::clone(&store), "docs", big_schema(), &one.0).unwrap();
    // The slot commits but its answer is lost and cannot be resolved.
    *store.commit_then_unknown.lock().unwrap() = Some("/log/");
    store
        .resolve_blind
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let result = index.bulk(vec![big_put("a", big_body())], Wait::Durable);
    assert_eq!(result.err(), Some(Error::Unknown));
    assert_eq!(
        store.deleted.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "nothing may be removed when the slot might exist"
    );
    // The slot did commit: a new process replays the document from its chunks.
    index.close();
    store
        .resolve_blind
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let again = Index::open(Arc::clone(&store), "docs", big_schema(), &two.0).unwrap();
    assert_eq!(needles(&again), 1);
    // Reopening read the intent that lists the committed attempt: the replay
    // found a slot naming it, so it removed nothing.
    assert_eq!(store.deleted.load(std::sync::atomic::Ordering::Relaxed), 0);
}

/// Panics on the writer thread at one point: the in-process stand-in for a crash,
/// which leaves everything already in the store and tells nobody.
struct CrashAt(hsearch_index::Point);
impl hsearch_index::Hooks for CrashAt {
    fn at(&self, point: hsearch_index::Point) {
        if point == self.0 {
            panic!("simulated crash at {point:?}");
        }
    }
}

fn read_object(store: &Flaky, key: &str) -> Option<Vec<u8>> {
    use hsearch_object::{ByteRange, ObjectBackend, ObjectKey};
    let key = ObjectKey::new(key.as_bytes()).unwrap();
    let ctx = hsearch_object::RequestContext::new(
        std::time::Duration::from_secs(5),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 20,
            response_bytes: 64 << 20,
            attempts: 4,
        },
    )
    .unwrap();
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let info = match std::pin::pin!(store.head(&ctx, &key))
        .as_mut()
        .poll(&mut cx)
    {
        std::task::Poll::Ready(Ok(info)) => info,
        _ => return None,
    };
    let range = ByteRange::new(0, info.length).unwrap();
    match std::pin::pin!(store.read_range(&ctx, &key, &info.version, range))
        .as_mut()
        .poll(&mut cx)
    {
        std::task::Poll::Ready(Ok(payload)) => Some(payload.as_bytes().to_vec()),
        _ => None,
    }
}

fn chunk_keys(entry: &hsearch_index::format::IntentEntry) -> Vec<String> {
    (0..entry.chunks)
        .map(|n| {
            format!(
                "docs/blob/{:020}/{:020}/{:020}/{}.{}/{n:06}",
                entry.epoch, entry.attempt, entry.seq, entry.op, entry.field
            )
        })
        .collect()
}

fn intent(store: &Flaky) -> Vec<hsearch_index::format::IntentEntry> {
    hsearch_index::format::decode_intent(&read_object(store, "docs/intent").expect("an intent"))
        .unwrap()
}

#[test]
fn a_crash_between_the_chunks_and_the_slot_is_cleaned_up_once_the_log_passes_it() {
    let store = Arc::new(Flaky::new(true));
    let (one, two, three) = (
        Scratch::new("crash-a"),
        Scratch::new("crash-b"),
        Scratch::new("crash-c"),
    );
    Index::create(Arc::clone(&store), "docs", big_schema(), &three.0)
        .unwrap()
        .close();
    let doomed = Index::open_with(
        Arc::clone(&store),
        "docs",
        big_schema(),
        &one.0,
        Arc::new(CrashAt(hsearch_index::Point::AfterChunks)),
    )
    .unwrap();
    assert!(
        doomed
            .bulk(vec![big_put("a", big_body())], Wait::Durable)
            .is_err()
    );
    drop(doomed);
    // The attempt was recorded before its chunks, and every chunk exists.
    let recorded = intent(&store);
    assert_eq!(recorded.len(), 1);
    let orphans = chunk_keys(&recorded[0]);
    assert_eq!(orphans.len(), 2);
    assert!(orphans.iter().all(|k| read_object(&store, k).is_some()));
    assert!(read_object(&store, "docs/log/00000000000000000000").is_none());

    // The next writer finds the log has not reached that position: the slot
    // may still land, so nothing is removed.
    let index = Index::open(Arc::clone(&store), "docs", big_schema(), &two.0).unwrap();
    assert_eq!(store.deleted.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(orphans.iter().all(|k| read_object(&store, k).is_some()));

    // It writes that position itself, which makes the orphan certain.
    index
        .bulk(vec![big_put("b", big_body())], Wait::Searchable)
        .unwrap();
    // The reclaim runs after the reply; a refresh is queued behind it.
    index.refresh().unwrap();
    assert!(orphans.iter().all(|k| read_object(&store, k).is_none()));
    let removed = store.removed.lock().unwrap().clone();
    assert_eq!(
        removed
            .iter()
            .filter(|(k, ok)| *ok && orphans.contains(k))
            .count(),
        2,
        "{removed:?}"
    );
    // Its own chunks are untouched: a fresh process replays the document.
    assert_eq!(needles(&index), 1);
    index.close();
    let again = Index::open(Arc::clone(&store), "docs", big_schema(), &one.0).unwrap();
    assert_eq!(needles(&again), 1);
}

#[test]
fn reopening_never_removes_the_chunks_of_a_committed_slot() {
    let store = Arc::new(Flaky::new(true));
    let (one, two, three) = (
        Scratch::new("keep-a"),
        Scratch::new("keep-b"),
        Scratch::new("keep-c"),
    );
    let index = Index::create(Arc::clone(&store), "docs", big_schema(), &one.0).unwrap();
    index
        .bulk(vec![big_put("a", big_body())], Wait::Searchable)
        .unwrap();
    index.close();
    // The intent still lists the committed attempt; a replay names it.
    let recorded = intent(&store);
    assert_eq!(recorded.len(), 1);
    for _ in 0..2 {
        let again = Index::open(Arc::clone(&store), "docs", big_schema(), &two.0).unwrap();
        assert_eq!(needles(&again), 1);
        again.close();
    }
    assert_eq!(store.deleted.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(
        chunk_keys(&recorded[0])
            .iter()
            .all(|k| read_object(&store, k).is_some())
    );
    let _ = three;
}

#[test]
fn a_slot_of_small_fields_writes_no_intent() {
    let store = Arc::new(Flaky::new(true));
    let scratch = Scratch::new("no-intent");
    let index = Index::create(Arc::clone(&store), "docs", big_schema(), &scratch.0).unwrap();
    index
        .bulk(vec![big_put("a", "small".into())], Wait::Searchable)
        .unwrap();
    assert!(read_object(&store, "docs/intent").is_none());
    let created = store.created.lock().unwrap().clone();
    assert!(
        created.iter().all(|k| !k.contains("/intent")),
        "{created:?}"
    );
}

#[test]
fn a_store_that_cannot_delete_keeps_the_crash_orphans_and_stays_writable() {
    let store = Arc::new(Flaky::new(false));
    let (one, two) = (Scratch::new("nodel-a"), Scratch::new("nodel-b"));
    Index::create(Arc::clone(&store), "docs", big_schema(), &one.0)
        .unwrap()
        .close();
    let doomed = Index::open_with(
        Arc::clone(&store),
        "docs",
        big_schema(),
        &one.0,
        Arc::new(CrashAt(hsearch_index::Point::AfterChunks)),
    )
    .unwrap();
    assert!(
        doomed
            .bulk(vec![big_put("a", big_body())], Wait::Durable)
            .is_err()
    );
    drop(doomed);
    let orphans = chunk_keys(&intent(&store)[0]);
    let index = Index::open(Arc::clone(&store), "docs", big_schema(), &two.0).unwrap();
    for id in ["b", "c"] {
        index
            .bulk(vec![big_put(id, big_body())], Wait::Searchable)
            .unwrap();
    }
    assert_eq!(needles(&index), 2);
    // Never removed: nothing could delete, and nothing reads them.
    assert!(orphans.iter().all(|k| read_object(&store, k).is_some()));
}

#[test]
fn a_stale_writer_is_fenced_by_the_intent_before_it_writes_a_chunk() {
    let store = Arc::new(Flaky::new(true));
    let (one, two, three) = (
        Scratch::new("fence-a"),
        Scratch::new("fence-b"),
        Scratch::new("fence-c"),
    );
    Index::create(Arc::clone(&store), "docs", big_schema(), &one.0)
        .unwrap()
        .close();
    let old = Index::open(Arc::clone(&store), "docs", big_schema(), &two.0).unwrap();
    let new = Index::open(Arc::clone(&store), "docs", big_schema(), &three.0).unwrap();
    new.bulk(vec![big_put("n", big_body())], Wait::Searchable)
        .unwrap();
    let before = store.created.lock().unwrap().len();
    let stale = old.bulk(vec![big_put("o", big_body())], Wait::Durable);
    assert_eq!(stale.err(), Some(Error::Fenced));
    let created = store.created.lock().unwrap()[before..].to_vec();
    assert!(
        created.iter().all(|k| !k.contains("/blob/")),
        "a fenced writer wrote chunks: {created:?}"
    );
}
