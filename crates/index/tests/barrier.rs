//! M04.04: ordered apply and committed-reader barriers. Sans-I/O: positions are
//! slot prefix lengths (`n` means slots 0..n).
use hsearch_index::barrier::{Cut, Readers, Refuse, Refused, Replay, Step, Waiters};
use std::sync::Arc;

/// A proof that `n` slots are applied, from a cursor that applied them.
fn cut(n: u64) -> Cut {
    Replay::new(n).cut().unwrap()
}
fn why<T: std::fmt::Debug, R>(r: Result<T, Refused<R>>) -> Refuse {
    r.unwrap_err().reason
}

#[test]
fn replay_applies_a_contiguous_prefix_in_order() {
    let mut r = Replay::new(0);
    assert_eq!(r.begin(0), Ok(Step::Apply));
    r.done(0).unwrap();
    assert_eq!(r.begin(1), Ok(Step::Apply));
    r.done(1).unwrap();
    assert_eq!(r.applied(), 2);
}

#[test]
fn a_duplicate_is_skipped_and_a_gap_is_refused() {
    let mut r = Replay::new(0);
    r.begin(0).unwrap();
    r.done(0).unwrap();
    assert_eq!(
        r.begin(0),
        Ok(Step::Duplicate),
        "a replayed slot is not applied twice"
    );
    assert_eq!(
        r.begin(2),
        Ok(Step::Gap { expected: 1 }),
        "slot 1 is missing"
    );
    assert_eq!(r.applied(), 1, "a gap changes nothing");
}

#[test]
fn done_without_begin_or_out_of_order_is_refused() {
    let mut r = Replay::new(0);
    assert!(r.done(0).is_err());
    r.begin(0).unwrap();
    assert!(r.done(1).is_err());
    assert_eq!(r.applied(), 0);
}

#[test]
fn a_failed_apply_poisons_the_cursor_until_rebuilt_from_the_log() {
    let mut r = Replay::new(0);
    r.begin(0).unwrap();
    r.fail();
    assert!(r.begin(0).is_err());
    assert!(r.begin(1).is_err());
    // Recovery: a new cursor from the committed prefix replays what is durable.
    let mut again = Replay::new(0);
    assert_eq!(again.begin(0), Ok(Step::Apply));
}

#[test]
fn a_reader_cannot_cover_more_than_is_committed() {
    let readers: Readers<&str> = Readers::new(4);
    readers.commit(2).unwrap();
    assert!(why(readers.publish("r3", cut(3))) == Refuse::Uncommitted);
    assert!(
        readers.pin().is_none(),
        "nothing exposed by a refused publish"
    );
    let g = readers.publish("r2", cut(2)).unwrap();
    assert_eq!(g.covers, 2);
    assert_eq!(readers.pin().unwrap().covers, 2);
}

#[test]
fn the_commit_watermark_and_published_prefix_never_regress() {
    let readers: Readers<u32> = Readers::new(4);
    readers.commit(5).unwrap();
    assert!(readers.commit(4).is_err());
    readers.publish(1, cut(3)).unwrap();
    assert!(why(readers.publish(2, cut(2))) == Refuse::Regress);
    // The same prefix again is a refresh with no new data: allowed, new generation.
    let again = readers.publish(3, cut(3)).unwrap();
    assert_eq!(again.generation, 2);
}

#[test]
fn a_failed_persistence_leaves_the_published_reader_where_it_was() {
    let readers: Readers<&str> = Readers::new(4);
    readers.commit(1).unwrap();
    readers.publish("r1", cut(1)).unwrap();
    // Slot 1 was applied to the engine but its persistence failed: commit stays 1.
    let refused = readers
        .publish("r2-applied-uncommitted", cut(2))
        .unwrap_err();
    assert_eq!(refused.reason, Refuse::Uncommitted);
    assert_eq!(
        refused.reader, "r2-applied-uncommitted",
        "the reader comes back for a retry"
    );
    assert_eq!(readers.pin().unwrap().covers, 1);
    // After persistence succeeds the same reader may be published.
    readers.commit(2).unwrap();
    assert_eq!(readers.publish("r2", cut(2)).unwrap().covers, 2);
}

#[test]
fn query_and_fetch_share_one_pinned_generation_across_a_publish() {
    let readers: Readers<&str> = Readers::new(4);
    readers.commit(3).unwrap();
    readers.publish("old", cut(1)).unwrap();
    let query = readers.pin().unwrap();
    readers.publish("new", cut(3)).unwrap();
    let fetch = Arc::clone(&query); // the fetch round uses the query's pin
    assert_eq!((fetch.reader, fetch.covers), ("old", 1));
    assert_eq!(
        readers.pin().unwrap().reader,
        "new",
        "a new query sees the new one"
    );
}

#[test]
fn pinned_generations_are_bounded() {
    let readers: Readers<u32> = Readers::new(2);
    readers.commit(10).unwrap();
    let a = readers.publish(1, cut(1)).unwrap();
    let b = readers.publish(2, cut(2)).unwrap();
    assert!(
        why(readers.publish(3, cut(3))) == Refuse::Pinned,
        "a and b are held"
    );
    drop(a);
    let c = readers.publish(3, cut(3)).unwrap();
    assert_eq!(readers.live(), 2);
    drop((b, c));
}

#[test]
fn a_crash_between_apply_and_refresh_recovers_to_the_committed_prefix() {
    // Before the crash: slots 0..3 applied, 0..2 committed, reader at 2.
    let before: Readers<&str> = Readers::new(4);
    before.commit(2).unwrap();
    before.publish("r2", cut(2)).unwrap();
    drop(before); // process dies; nothing in memory survives
    // Recovery replays the durable prefix, then publishes; no reader exists
    // until it has, and it can never cover more than the durable prefix.
    let after: Readers<&str> = Readers::new(4);
    assert!(after.pin().is_none());
    let mut replay = Replay::new(0);
    for seq in 0..2 {
        assert_eq!(replay.begin(seq), Ok(Step::Apply));
        replay.done(seq).unwrap();
    }
    after.commit(2).unwrap();
    assert_eq!(
        why(after.publish("rebuilt-through-3", cut(3))),
        Refuse::Uncommitted
    );
    assert_eq!(
        after
            .publish("rebuilt", replay.cut().unwrap())
            .unwrap()
            .covers,
        2
    );
}

#[test]
fn a_concurrent_fetch_keeps_its_generation_while_publishing_continues() {
    let readers = Arc::new(Readers::<u64>::new(64));
    readers.commit(1000).unwrap();
    readers.publish(0, cut(1)).unwrap();
    let pinned = readers.pin().unwrap();
    let publisher = {
        let readers = Arc::clone(&readers);
        std::thread::spawn(move || {
            for n in 2..40u64 {
                loop {
                    match readers.publish(n, cut(n)) {
                        Ok(_) => break,
                        Err(e) if e.reason == Refuse::Pinned => std::thread::yield_now(),
                        Err(e) => panic!("{e:?}"),
                    }
                }
            }
        })
    };
    for _ in 0..1000 {
        assert_eq!((pinned.reader, pinned.covers), (0, 1));
        let latest = readers.pin().unwrap();
        assert!(latest.covers >= pinned.covers && latest.covers <= 1000);
        assert!(
            latest.reader == 0 || latest.reader == latest.covers,
            "a reader and the prefix it covers are published together"
        );
    }
    publisher.join().unwrap();
    assert_eq!(readers.pin().unwrap().covers, 39);
}

#[test]
fn waiters_are_released_exactly_when_a_published_reader_covers_them() {
    let mut w = Waiters::new(3);
    let a = w.add(2).unwrap();
    let b = w.add(5).unwrap();
    let c = w.add(2).unwrap();
    assert!(w.add(9).is_err(), "bounded");
    assert_eq!(w.release(1), Vec::<u64>::new());
    let mut got = w.release(2);
    got.sort_unstable();
    assert_eq!(got, vec![a, c]);
    assert_eq!(w.release(5), vec![b]);
    assert_eq!(w.len(), 0);
}

#[test]
fn refresh_is_due_only_when_commit_and_apply_both_run_ahead_of_the_reader() {
    let readers: Readers<u32> = Readers::new(4);
    assert!(!readers.refresh_due(0));
    readers.commit(3).unwrap();
    assert!(!readers.refresh_due(0), "nothing applied yet");
    assert!(
        readers.refresh_due(2),
        "2 applied and committed, none published"
    );
    readers.publish(0, cut(2)).unwrap();
    assert!(!readers.refresh_due(2));
    assert!(readers.refresh_due(3));
    // Applied ahead of commit does not make a refresh due beyond the commit.
    let r2: Readers<u32> = Readers::new(4);
    r2.commit(1).unwrap();
    r2.publish(0, cut(1)).unwrap();
    assert!(
        !r2.refresh_due(5),
        "only slot 0 is committed and it is published"
    );
}

#[test]
fn a_cut_exists_only_between_applies_and_never_after_a_failure() {
    let mut r = Replay::new(0);
    assert_eq!(r.cut().unwrap().covers(), 0);
    r.begin(0).unwrap();
    assert!(
        r.cut().is_err(),
        "a slot is being applied: the engine is between prefixes"
    );
    r.done(0).unwrap();
    assert_eq!(r.cut().unwrap().covers(), 1);
    r.fail();
    assert!(r.cut().is_err());
}

#[test]
fn a_reader_opened_on_applied_but_uncommitted_slots_cannot_be_published_as_older() {
    // Ten slots applied, five committed: the only cut the cursor can issue is 10.
    let mut replay = Replay::new(0);
    for seq in 0..10 {
        replay.begin(seq).unwrap();
        replay.done(seq).unwrap();
    }
    let readers: Readers<&str> = Readers::new(4);
    readers.commit(5).unwrap();
    assert_eq!(
        why(readers.publish("live-engine", replay.cut().unwrap())),
        Refuse::Uncommitted
    );
    assert!(readers.pin().is_none());
}

#[test]
fn an_unpinned_published_generation_does_not_count_against_a_tight_bound() {
    let readers: Readers<u32> = Readers::new(1);
    readers.commit(100).unwrap();
    for n in 1..20u64 {
        // The returned handle is dropped at once: only the manager holds it.
        readers.publish(n as u32, cut(n)).unwrap();
    }
    assert_eq!(readers.live(), 1);
    let held = readers.pin().unwrap();
    assert_eq!(
        why(readers.publish(99, cut(50))),
        Refuse::Pinned,
        "one holder, bound one"
    );
    drop(held);
    assert!(readers.publish(99, cut(50)).is_ok());
}

#[test]
fn a_burst_of_pins_and_drops_never_lets_the_bound_be_exceeded() {
    let readers = Arc::new(Readers::<u64>::new(3));
    readers.commit(u64::MAX / 2).unwrap();
    let mut handles = Vec::new();
    for t in 0..4u64 {
        let readers = Arc::clone(&readers);
        handles.push(std::thread::spawn(move || {
            for i in 0..2000u64 {
                let n = t * 10_000 + i + 1;
                // Publishing may be refused as stale or pinned; it must never
                // leave more than 3 generations held by callers plus the new one.
                let _ = readers.publish(n, cut(0));
                let pin = readers.pin();
                assert!(readers.live() <= 3 + 4 + 1, "live {}", readers.live());
                drop(pin);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert!(readers.live() <= 1 + 3);
}

#[test]
fn replay_begin_twice_in_flight_is_idempotent_and_done_after_duplicate_is_refused() {
    let mut r = Replay::new(0);
    assert_eq!(r.begin(0), Ok(Step::Apply));
    assert_eq!(r.begin(0), Ok(Step::Apply));
    r.done(0).unwrap();
    assert_eq!(r.begin(0), Ok(Step::Duplicate));
    assert!(
        r.done(0).is_err(),
        "a duplicate has nothing in flight to finish"
    );
    assert_eq!(r.applied(), 1);
}
