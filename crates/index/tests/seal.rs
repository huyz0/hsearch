//! The recovery rule from the M03.04 authority model: a successor that opens an
//! index seals the log at its first absent slot, so a late write from a
//! suspended predecessor is refused, never acknowledged and then lost to a
//! reader that already served; the fence is retained, and recovery replays past
//! it. Over the in-memory store with failure injection; the same crash matrix
//! against RustFS is in `rustfs.rs`.
mod support;

use hsearch_index::{Error, Hooks, Index, Point, Wait};
use hsearch_object::memory::{Fault, MemoryLimits, MemoryStore};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
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

/// Blocks the writer thread at `BeforeSlot` until released, once armed.
struct Gate {
    armed: Mutex<bool>,
    reached: Sender<()>,
    release: Mutex<Receiver<()>>,
}
impl Hooks for Gate {
    fn at(&self, point: Point) {
        if point == Point::BeforeSlot && std::mem::replace(&mut *self.armed.lock().unwrap(), false)
        {
            self.reached.send(()).unwrap();
            let _ = self.release.lock().unwrap().recv();
        }
    }
}

/// An index created and closed, then opened by `hooks`: the predecessor.
fn predecessor(store: &Arc<MemoryStore>, scratch: &Scratch, hooks: Arc<Gate>) -> Arc<Index> {
    Index::create(Arc::clone(store), "orders", schema(), &scratch.0)
        .unwrap()
        .close();
    Arc::new(Index::open_with(Arc::clone(store), "orders", schema(), &scratch.0, hooks).unwrap())
}

fn gate() -> (Arc<Gate>, Receiver<()>, Sender<()>) {
    let (reached, wait) = channel();
    let (release, gate) = channel();
    (
        Arc::new(Gate {
            armed: Mutex::new(false),
            reached,
            release: Mutex::new(gate),
        }),
        wait,
        release,
    )
}

#[test]
fn a_late_write_from_a_suspended_predecessor_is_refused_not_acknowledged() {
    let store = store();
    let (one, two, three) = (
        Scratch::new("seal-a"),
        Scratch::new("seal-b"),
        Scratch::new("seal-c"),
    );
    let (hooks, reached, release) = gate();
    let old = predecessor(&store, &one, Arc::clone(&hooks));
    // The predecessor builds a slot and is suspended just before its PUT.
    *hooks.armed.lock().unwrap() = true;
    let suspended = {
        let old = Arc::clone(&old);
        std::thread::spawn(move || old.bulk(vec![put("late", "x", 1)], Wait::Durable))
    };
    reached.recv().unwrap();
    // The successor opens and starts serving while that PUT is still in flight.
    let new = Index::open(Arc::clone(&store), "orders", schema(), &two.0).unwrap();
    assert_eq!(all(&new.snapshot().unwrap()).0, 0, "the successor serves");
    release.send(()).unwrap();
    // The late PUT must not be acknowledged: the successor has already served.
    assert_eq!(suspended.join().unwrap().err(), Some(Error::Fenced));
    new.bulk(vec![put("n", "x", 2)], Wait::Searchable).unwrap();
    new.close();
    let again = Index::open(store, "orders", schema(), &three.0).unwrap();
    assert_eq!(all(&again.snapshot().unwrap()), (1, vec!["n".to_string()]));
}

#[test]
fn recovery_replays_past_fences_and_each_open_seals_once() {
    let store = store();
    let scratch = Scratch::new("seal-chain");
    Index::create(Arc::clone(&store), "orders", schema(), &scratch.0)
        .unwrap()
        .close();
    let mut expect = Vec::new();
    for round in 0..4 {
        let dir = Scratch::new("seal-chain-n");
        let index = Index::open(Arc::clone(&store), "orders", schema(), &dir.0).unwrap();
        // Everything acknowledged by earlier opens survives their fences.
        assert_eq!(all(&index.snapshot().unwrap()).1, expect, "round {round}");
        let id = format!("d{round}");
        index
            .bulk(vec![put(&id, "x", round)], Wait::Searchable)
            .unwrap();
        expect.push(id);
        index.close();
    }
}

#[test]
fn a_crash_after_sealing_before_serving_leaves_a_log_the_next_open_reads() {
    let store = store();
    let scratch = Scratch::new("seal-crash");
    Index::create(Arc::clone(&store), "orders", schema(), &scratch.0)
        .unwrap()
        .close();
    for _ in 0..3 {
        // Open and die at once: the fence is written, nothing is served.
        let dir = Scratch::new("seal-crash-n");
        Index::open(Arc::clone(&store), "orders", schema(), &dir.0)
            .unwrap()
            .close();
    }
    let dir = Scratch::new("seal-crash-last");
    let index = Index::open(store, "orders", schema(), &dir.0).unwrap();
    index
        .bulk(vec![put("a", "x", 1)], Wait::Searchable)
        .unwrap();
    assert_eq!(all(&index.snapshot().unwrap()).0, 1);
}

#[test]
fn a_lost_or_refused_fence_write_is_resolved_or_fails_the_open_cleanly() {
    let store = store();
    let scratch = Scratch::new("seal-faults");
    Index::create(Arc::clone(&store), "orders", schema(), &scratch.0)
        .unwrap()
        .close();
    // The fence commits but its response is lost: resolved by identity.
    store.inject(Fault::LoseResponseAfterCommit).unwrap();
    let dir = Scratch::new("seal-faults-a");
    let index = Index::open(Arc::clone(&store), "orders", schema(), &dir.0).unwrap();
    index.bulk(vec![put("a", "x", 1)], Wait::Durable).unwrap();
    index.close();
    // Refused before acceptance: retried inside the open.
    store.inject(Fault::UnavailableBeforeAcceptance).unwrap();
    let dir = Scratch::new("seal-faults-b");
    let index = Index::open(Arc::clone(&store), "orders", schema(), &dir.0).unwrap();
    assert_eq!(all(&index.snapshot().unwrap()).0, 1);
    index.close();
}
