//! The structural head and the catalog (M03.05) against the in-memory store, with injected faults.
mod support;

use std::sync::Arc;

use hsearch_format::{Durability, Id};
use hsearch_lane::counting::Requests;
use hsearch_lane::{
    Advance, Change, Error, LaneId, Resolution, advance_head, block_on, create_catalog,
    create_head, open_catalog, read_head,
};
use hsearch_object::ObjectBackend as _;
use hsearch_object::memory::Fault;
use support::*;

fn ctx() -> hsearch_object::RequestContext {
    hsearch_object::RequestContext::new(
        std::time::Duration::from_secs(10),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 4,
        },
    )
    .unwrap()
}

fn lane() -> LaneId {
    LaneId::new([7; 16])
}

fn done(a: Advance) -> (hsearch_format::StructuralHead, hsearch_object::VersionToken) {
    match a {
        Advance::Done(h, v) => (h, v),
        Advance::Lost => panic!("CAS lost"),
    }
}

// ---- structural head ------------------------------------------------------------------------

#[test]
fn a_head_is_created_once_and_read_back() {
    let store = counted();
    let (head, _) = block_on(create_head(&*store, &layout(), &lane(), Durability::Object)).unwrap();
    assert_eq!((head.generation, head.writer_epoch, head.cover), (1, 0, 0));
    let (read, _) = block_on(read_head(&*store, &layout(), &lane())).unwrap();
    assert_eq!(read, head);
    assert_eq!(
        block_on(create_head(&*store, &layout(), &lane(), Durability::Object)).unwrap_err(),
        Error::Exists,
        "a head is never recreated while it exists"
    );
    assert_eq!(
        block_on(read_head(&*store, &layout(), &LaneId::new([8; 16]))).unwrap_err(),
        Error::NoHead
    );
}

#[test]
fn establishing_checkpointing_and_bumping_walk_the_head_forward() {
    let store = counted();
    let (h, v) = block_on(create_head(&*store, &layout(), &lane(), Durability::Object)).unwrap();
    let (h, v) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Establish {
                activation: 1,
                lane: 0,
                fence: 4,
            },
        ))
        .unwrap(),
    );
    assert_eq!((h.generation, h.writer_epoch, h.inventory.len()), (2, 1, 1));
    assert_eq!(h.tail.ordinal, 5, "a successor looks above the fence");
    let (h, v) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Checkpoint {
                cover: 3,
                root: [9; 16],
            },
        ))
        .unwrap(),
    );
    assert_eq!((h.cover, h.checkpoint), (3, Some([9; 16])));
    // A replacement root keeps the cover and raises the publication generation.
    let publication = h.publication_generation;
    let (h, v) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Replace { root: [8; 16] },
        ))
        .unwrap(),
    );
    assert_eq!((h.cover, h.checkpoint), (3, Some([8; 16])));
    assert_eq!(h.publication_generation, publication + 1);
    assert!(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Replace { root: [0; 16] },
        ))
        .is_err(),
        "the zero id is not a root"
    );
    let (h, v) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Schema,
        ))
        .unwrap(),
    );
    let (h, v) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Publication,
        ))
        .unwrap(),
    );
    let (h, _) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Membership,
        ))
        .unwrap(),
    );
    assert_eq!(
        (
            h.generation,
            h.schema_generation,
            h.publication_generation,
            h.membership_epoch
        ),
        (7, 2, 3, 1)
    );
    let (read, _) = block_on(read_head(&*store, &layout(), &lane())).unwrap();
    assert_eq!(read, h);
}

#[test]
fn a_stale_writer_loses_and_cannot_regress_the_head() {
    let store = counted();
    let (h, v) = block_on(create_head(&*store, &layout(), &lane(), Durability::Object)).unwrap();
    let establish = |fence| Change::Establish {
        activation: 1,
        lane: 0,
        fence,
    };
    // Two writers read generation 1; the first establishes, the second is stale.
    let (first, _) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            establish(1),
        ))
        .unwrap(),
    );
    let lost = block_on(advance_head(
        &*store,
        &layout(),
        &lane(),
        &h,
        &v,
        establish(2),
    ))
    .unwrap();
    assert!(matches!(lost, Advance::Lost));
    let (now, v2) = block_on(read_head(&*store, &layout(), &lane())).unwrap();
    assert_eq!(now, first, "the loser changed nothing");
    // Re-reading and trying again succeeds with the next epoch.
    let (second, _) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &now,
            &v2,
            establish(2),
        ))
        .unwrap(),
    );
    assert_eq!(second.writer_epoch, 2);
    assert_eq!(second.inventory.len(), 2);
}

#[test]
fn a_change_that_would_regress_is_refused_before_anything_is_written() {
    let store = counted();
    let (h, v) = block_on(create_head(&*store, &layout(), &lane(), Durability::Object)).unwrap();
    let (h, v) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Establish {
                activation: 1,
                lane: 0,
                fence: 4,
            },
        ))
        .unwrap(),
    );
    let (h, v) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Checkpoint {
                cover: 4,
                root: [9; 16],
            },
        ))
        .unwrap(),
    );
    let before = store.requests();
    for change in [
        Change::Checkpoint {
            cover: 3,
            root: [8; 16],
        },
        Change::Checkpoint {
            cover: 5,
            root: [0; 16],
        },
        Change::Establish {
            activation: 0,
            lane: 0,
            fence: 9,
        },
    ] {
        let e = block_on(advance_head(&*store, &layout(), &lane(), &h, &v, change)).unwrap_err();
        assert!(matches!(e, Error::Format(_) | Error::Invalid(_)), "{e:?}");
    }
    assert_eq!(
        store.requests().since(&before),
        Requests::default(),
        "refused locally: no request"
    );
}

#[test]
fn a_lost_reply_is_resolved_by_identity_not_guessed() {
    let store = counted();
    let (h, v) = block_on(create_head(&*store, &layout(), &lane(), Durability::Object)).unwrap();
    store
        .inner()
        .inject(Fault::LoseResponseAfterCommit)
        .unwrap();
    let (next, _) = done(
        block_on(advance_head(
            &*store,
            &layout(),
            &lane(),
            &h,
            &v,
            Change::Schema,
        ))
        .unwrap(),
    );
    assert_eq!(next.generation, 2);
    let (read, _) = block_on(read_head(&*store, &layout(), &lane())).unwrap();
    assert_eq!(
        read, next,
        "the write that lost its reply is the one reported"
    );
}

#[test]
fn an_unresolvable_reply_is_reported_unknown_and_never_as_success() {
    let chaos = Arc::new(Chaos::new(Arc::new(memory())));
    let (h, v) = block_on(create_head(&*chaos, &layout(), &lane(), Durability::Object)).unwrap();
    chaos.inner.inject(Fault::LoseResponseAfterCommit).unwrap();
    chaos
        .fail_resolve
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let e = block_on(advance_head(
        &*chaos,
        &layout(),
        &lane(),
        &h,
        &v,
        Change::Schema,
    ))
    .unwrap_err();
    assert_eq!(e, Error::Contended, "the caller must re-read before acting");
    chaos
        .fail_resolve
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (read, _) = block_on(read_head(&*chaos, &layout(), &lane())).unwrap();
    assert_eq!(
        read.generation, 2,
        "it did land; reading is how a caller learns"
    );
}

#[test]
fn concurrent_advances_never_lose_an_epoch_or_regress() {
    let store = Arc::new(memory());
    block_on(create_head(&*store, &layout(), &lane(), Durability::Object)).unwrap();
    let mut joins = Vec::new();
    for _ in 0..4u64 {
        let store = Arc::clone(&store);
        joins.push(std::thread::spawn(move || {
            let mut won = 0u64;
            for _ in 0..15u64 {
                loop {
                    let (h, v) = block_on(read_head(&*store, &layout(), &lane())).unwrap();
                    let change = Change::Establish {
                        activation: 1,
                        lane: 0,
                        fence: h.tail.ordinal,
                    };
                    match block_on(advance_head(&*store, &layout(), &lane(), &h, &v, change)) {
                        Ok(Advance::Done(..)) => {
                            won += 1;
                            break;
                        }
                        Ok(Advance::Lost) | Err(Error::Contended) => {}
                        Err(e) => panic!("{e:?}"),
                    }
                }
            }
            won
        }));
    }
    let total: u64 = joins.into_iter().map(|j| j.join().unwrap()).sum();
    let (head, _) = block_on(read_head(&*store, &layout(), &lane())).unwrap();
    assert_eq!(total, 60);
    assert_eq!(head.generation, 61, "exactly one generation per success");
    assert_eq!(head.writer_epoch, 60);
    let epochs: Vec<u64> = head.inventory.iter().map(|e| e.epoch).collect();
    assert_eq!(epochs, (1..=60).collect::<Vec<_>>());
}

#[test]
fn advancing_costs_one_put_and_nothing_else() {
    let store = counted();
    let (h, v) = block_on(create_head(&*store, &layout(), &lane(), Durability::Object)).unwrap();
    let before = store.requests();
    block_on(advance_head(
        &*store,
        &layout(),
        &lane(),
        &h,
        &v,
        Change::Schema,
    ))
    .unwrap();
    let used = store.requests().since(&before);
    assert_eq!(used.cas, 1);
    assert_eq!(
        used.total(),
        1,
        "the caller already holds the version: one PUT, nothing else"
    );
}

// ---- catalog --------------------------------------------------------------------------------

fn cat_id() -> Id {
    [3; 16]
}

#[test]
fn a_catalog_resolves_creates_and_deletes_names() {
    let store = counted();
    let catalog = block_on(create_catalog(&*store, &layout(), cat_id(), 4)).unwrap();
    assert_eq!(
        block_on(catalog.resolve(&*store, &layout(), "orders")).unwrap(),
        Resolution::Absent
    );
    block_on(catalog.create(&*store, &layout(), "orders", id(1))).unwrap();
    assert_eq!(
        block_on(catalog.resolve(&*store, &layout(), "orders")).unwrap(),
        Resolution::Live {
            index: id(1),
            name_generation: 1
        }
    );
    assert_eq!(
        block_on(catalog.create(&*store, &layout(), "orders", id(2))).unwrap_err(),
        Error::Exists,
        "a live name is never rebound"
    );
    block_on(catalog.delete(&*store, &layout(), "orders", &id(1))).unwrap();
    assert_eq!(
        block_on(catalog.resolve(&*store, &layout(), "orders")).unwrap(),
        Resolution::Deleted {
            last: id(1),
            name_generation: 2
        }
    );
    // Reopening finds the same catalog without a LIST.
    let again = block_on(open_catalog(&*store, &layout(), cat_id())).unwrap();
    assert_eq!(again.bucket_bits(), 4);
    assert_eq!(
        block_on(open_catalog(&*store, &layout(), [9; 16])).unwrap_err(),
        Error::NoCatalog
    );
    assert_eq!(
        block_on(create_catalog(&*store, &layout(), cat_id(), 4)).unwrap_err(),
        Error::Exists
    );
}

#[test]
fn a_deleted_name_never_resurrects_its_old_identity() {
    let store = counted();
    let catalog = block_on(create_catalog(&*store, &layout(), cat_id(), 4)).unwrap();
    block_on(catalog.create(&*store, &layout(), "orders", id(1))).unwrap();
    block_on(catalog.delete(&*store, &layout(), "orders", &id(1))).unwrap();
    let e = block_on(catalog.create(&*store, &layout(), "orders", id(1))).unwrap_err();
    assert_eq!(
        e,
        Error::Invalid("a deleted name cannot resurrect its old identity")
    );
    // A fresh identity is allowed and the name moves to it with a higher generation.
    block_on(catalog.create(&*store, &layout(), "orders", id(2))).unwrap();
    assert_eq!(
        block_on(catalog.resolve(&*store, &layout(), "orders")).unwrap(),
        Resolution::Live {
            index: id(2),
            name_generation: 3
        }
    );
    // Deleting needs the identity the caller believes it is deleting: a stale delete is refused.
    assert_eq!(
        block_on(catalog.delete(&*store, &layout(), "orders", &id(1))).unwrap_err(),
        Error::NoName
    );
    assert_eq!(
        block_on(catalog.delete(&*store, &layout(), "never", &id(1))).unwrap_err(),
        Error::NoName
    );
}

#[test]
fn concurrent_create_of_one_name_has_exactly_one_winner() {
    let store = Arc::new(memory());
    let catalog = Arc::new(block_on(create_catalog(&*store, &layout(), cat_id(), 2)).unwrap());
    let mut joins = Vec::new();
    for t in 1..=6u32 {
        let (store, catalog) = (Arc::clone(&store), Arc::clone(&catalog));
        joins.push(std::thread::spawn(move || {
            block_on(catalog.create(&*store, &layout(), "same", id(t)))
        }));
    }
    let results: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter(|r| r.is_err())
            .all(|r| *r == Err(Error::Exists))
    );
    let winner = match block_on(catalog.resolve(&*store, &layout(), "same")).unwrap() {
        Resolution::Live { index, .. } => index,
        other => panic!("{other:?}"),
    };
    assert!((1..=6).any(|t| id(t) == winner));
}

#[test]
fn concurrent_create_delete_recreate_keeps_every_name_consistent() {
    let store = Arc::new(memory());
    let catalog = Arc::new(block_on(create_catalog(&*store, &layout(), cat_id(), 2)).unwrap());
    let mut joins = Vec::new();
    for t in 0..4u32 {
        let (store, catalog) = (Arc::clone(&store), Arc::clone(&catalog));
        joins.push(std::thread::spawn(move || {
            // Each thread cycles its own identities through a shared set of names.
            for round in 0..10u32 {
                let name = format!("n{}", round % 3);
                let me = id(1000 * (t + 1) + round);
                match block_on(catalog.create(&*store, &layout(), &name, me)) {
                    Ok(()) => {
                        let _ = block_on(catalog.delete(&*store, &layout(), &name, &me));
                    }
                    Err(Error::Exists) => {}
                    Err(e) => panic!("{e:?}"),
                }
            }
        }));
    }
    for j in joins {
        j.join().unwrap();
    }
    let report = block_on(catalog.verify(&*store, &layout())).unwrap();
    assert_eq!(report.pages_bad, 0);
    for n in 0..3 {
        let r = block_on(catalog.resolve(&*store, &layout(), &format!("n{n}"))).unwrap();
        if let Resolution::Live { .. } | Resolution::Deleted { .. } = r {
        } else {
            panic!("a name that was created resolves to something: {r:?}");
        }
    }
}

#[test]
fn a_lost_create_reply_is_resolved_and_a_second_create_is_not_double_applied() {
    let store = counted();
    let catalog = block_on(create_catalog(&*store, &layout(), cat_id(), 3)).unwrap();
    store
        .inner()
        .inject(Fault::LoseResponseAfterCommit)
        .unwrap();
    block_on(catalog.create(&*store, &layout(), "orders", id(1))).unwrap();
    assert_eq!(
        block_on(catalog.resolve(&*store, &layout(), "orders")).unwrap(),
        Resolution::Live {
            index: id(1),
            name_generation: 1
        }
    );
}

#[test]
fn an_ambiguous_create_that_cannot_be_resolved_is_settled_by_reading() {
    let chaos = Arc::new(Chaos::new(Arc::new(memory())));
    let catalog = block_on(create_catalog(&*chaos, &layout(), cat_id(), 3)).unwrap();
    chaos.inner.inject(Fault::LoseResponseAfterCommit).unwrap();
    chaos
        .fail_resolve
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // The write landed, the reply and the resolve both failed: the catalog reads the page and
    // sees its own binding, so it reports success rather than an unknown.
    block_on(catalog.create(&*chaos, &layout(), "orders", id(1))).unwrap();
    // A second identity cannot take the name afterwards.
    assert_eq!(
        block_on(catalog.create(&*chaos, &layout(), "orders", id(2))).unwrap_err(),
        Error::Exists
    );
}

#[test]
fn enumeration_is_paged_complete_and_needs_no_list() {
    let store = counted();
    let catalog = block_on(create_catalog(&*store, &layout(), cat_id(), 3)).unwrap();
    let names: Vec<String> = (0..40).map(|i| format!("idx-{i:02}")).collect();
    for (i, n) in names.iter().enumerate() {
        block_on(catalog.create(&*store, &layout(), n, id(i as u32 + 1))).unwrap();
    }
    block_on(catalog.delete(&*store, &layout(), "idx-05", &id(6))).unwrap();
    let before = store.requests();
    let mut seen = Vec::new();
    let mut cursor = None;
    let mut calls = 0;
    loop {
        let (page, next) = block_on(catalog.enumerate(&*store, &layout(), cursor, 7)).unwrap();
        assert!(page.len() <= 7);
        seen.extend(page);
        calls += 1;
        match next {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    let mut got: Vec<_> = seen.iter().map(|e| e.name.clone()).collect();
    got.sort();
    let mut expect = names.clone();
    expect.retain(|n| n != "idx-05");
    assert_eq!(got, expect, "every live name once, tombstones skipped");
    assert!(calls >= 6);
    let used = store.requests().since(&before);
    // One HEAD and one read per bucket (8 buckets), plus one re-read of the bucket a page
    // stopped inside, for each call after the first.
    assert!(used.head <= 8 + calls && used.read <= 8 + calls, "{used:?}");
}

#[test]
fn verify_finds_a_page_that_does_not_belong() {
    let store = counted();
    let catalog = block_on(create_catalog(&*store, &layout(), cat_id(), 2)).unwrap();
    block_on(catalog.create(&*store, &layout(), "a", id(1))).unwrap();
    let report = block_on(catalog.verify(&*store, &layout())).unwrap();
    assert_eq!((report.pages_bad, report.live), (0, 1));
    // Copy one bucket's page over another's key: decode_at refuses, verify reports it.
    let home = hsearch_format::bucket_of(&cat_id(), 2, "a");
    let other = (home + 1) % 4;
    let src = layout().catalog_page(&cat_id(), home);
    let dst = layout().catalog_page(&cat_id(), other);
    let bytes = block_on(async {
        let k = hsearch_object::ObjectKey::new(src.as_bytes()).unwrap();
        let info = store.head(&ctx(), &k).await.unwrap();
        let range = hsearch_object::ByteRange::new(0, info.length).unwrap();
        store
            .read_range(&ctx(), &k, &info.version, range)
            .await
            .unwrap()
    });
    let mutation = hsearch_object::Mutation {
        request_id: hsearch_object::RequestId::new(b"copy").unwrap(),
        key: hsearch_object::ObjectKey::new(dst.as_bytes()).unwrap(),
        payload: bytes,
        metadata: hsearch_object::Metadata::new(b"").unwrap(),
        precondition: hsearch_object::Precondition::Absent,
    };
    block_on(store.create(&ctx(), &mutation)).unwrap();
    let report = block_on(catalog.verify(&*store, &layout())).unwrap();
    assert_eq!(report.pages_bad, 1);
}

#[test]
fn a_resolve_costs_one_read_and_a_first_create_one_head_and_two_creates() {
    let store = counted();
    let catalog = block_on(create_catalog(&*store, &layout(), cat_id(), 8)).unwrap();
    let before = store.requests();
    block_on(catalog.create(&*store, &layout(), "orders", id(1))).unwrap();
    let used = store.requests().since(&before);
    assert_eq!(
        (used.head, used.create, used.cas, used.total()),
        (1, 2, 0, 3),
        "an empty bucket: one HEAD finds it absent, one create binds the identity, one writes the page"
    );
    let before = store.requests();
    block_on(catalog.resolve(&*store, &layout(), "orders")).unwrap();
    let r = store.requests().since(&before);
    assert_eq!((r.head, r.read, r.total()), (1, 1, 2));
}

#[test]
fn no_older_identity_comes_back_even_after_other_identities_held_the_name() {
    let store = counted();
    let catalog = block_on(create_catalog(&*store, &layout(), cat_id(), 4)).unwrap();
    block_on(catalog.create(&*store, &layout(), "orders", id(1))).unwrap();
    block_on(catalog.delete(&*store, &layout(), "orders", &id(1))).unwrap();
    block_on(catalog.create(&*store, &layout(), "orders", id(2))).unwrap();
    block_on(catalog.delete(&*store, &layout(), "orders", &id(2))).unwrap();
    // The tombstone now holds identity 2; identity 1 is older and must still be refused.
    let e = block_on(catalog.create(&*store, &layout(), "orders", id(1))).unwrap_err();
    assert!(matches!(e, Error::Invalid(_)), "{e:?}");
    // It is refused under any other name too: an identity is bound once, ever.
    let e = block_on(catalog.create(&*store, &layout(), "other", id(1))).unwrap_err();
    assert!(matches!(e, Error::Invalid(_)), "{e:?}");
    assert_eq!(
        block_on(catalog.resolve(&*store, &layout(), "orders")).unwrap(),
        Resolution::Deleted {
            last: id(2),
            name_generation: 4
        }
    );
    block_on(catalog.create(&*store, &layout(), "orders", id(3))).unwrap();
}

#[test]
fn a_create_interrupted_between_its_binding_and_its_page_can_be_retried() {
    let chaos = Arc::new(Chaos::new(Arc::new(memory())));
    let catalog = block_on(create_catalog(&*chaos, &layout(), cat_id(), 3)).unwrap();
    // One mutation allowed: the binding lands, the page write is refused.
    chaos.allow(1);
    let e = block_on(catalog.create(&*chaos, &layout(), "orders", id(1))).unwrap_err();
    assert!(matches!(e, Error::Store(_)), "{e:?}");
    assert_eq!(
        block_on(catalog.resolve(&*chaos, &layout(), "orders")).unwrap(),
        Resolution::Absent
    );
    chaos.allow(i64::MAX);
    block_on(catalog.create(&*chaos, &layout(), "orders", id(1))).unwrap();
    assert!(matches!(
        block_on(catalog.resolve(&*chaos, &layout(), "orders")).unwrap(),
        Resolution::Live { .. }
    ));
}

#[test]
fn a_create_costs_two_puts_one_for_the_binding_and_one_for_the_page() {
    let store = counted();
    let catalog = block_on(create_catalog(&*store, &layout(), cat_id(), 8)).unwrap();
    let before = store.requests();
    block_on(catalog.create(&*store, &layout(), "orders", id(1))).unwrap();
    let used = store.requests().since(&before);
    assert_eq!((used.create, used.cas, used.head), (2, 0, 1), "{used:?}");
}
