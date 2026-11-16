//! M03.05: rebuilding a catalog page that is missing or unreadable, without letting a name come
//! back to an identity it lost.
mod support;

use std::sync::Arc;

use hsearch_format::{Id, bucket_of};
use hsearch_lane::{Catalog, Error, RepairClaim, Resolution, block_on, create_catalog};
use hsearch_object::{
    Metadata, MetadataStore as _, Mutation, ObjectBackend as _, ObjectKey, Payload, Precondition,
    RequestContext, RequestId,
};
use support::*;

const CATALOG: Id = [5; 16];
const BITS: u8 = 2;

fn ctx() -> RequestContext {
    RequestContext::new(
        std::time::Duration::from_secs(10),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 8,
        },
    )
    .unwrap()
}

/// `count` names that all hash to `bucket`.
fn names(bucket: u32, count: usize, tag: &str) -> Vec<String> {
    (0u32..)
        .map(|i| format!("{tag}{i}"))
        .filter(|n| bucket_of(&CATALOG, BITS, n) == bucket)
        .take(count)
        .collect()
}

fn catalog(store: &impl hsearch_object::MetadataStore) -> Catalog {
    block_on(create_catalog(store, &layout(), CATALOG, BITS)).unwrap()
}

fn page_key(bucket: u32) -> ObjectKey {
    ObjectKey::new(layout().catalog_page(&CATALOG, bucket).as_bytes()).unwrap()
}

/// Overwrite the page object with bytes that do not decode, as bit rot or a bad tool would.
fn corrupt(store: &impl hsearch_object::MetadataStore, bucket: u32) {
    corrupt_with(store, bucket, b"not a catalog page at all");
}

fn corrupt_with(store: &impl hsearch_object::MetadataStore, bucket: u32, bytes: &[u8]) {
    let k = page_key(bucket);
    let info = block_on(store.head(&ctx(), &k)).unwrap();
    let m = Mutation {
        request_id: RequestId::new(format!("rot-{bucket}").as_bytes()).unwrap(),
        key: k,
        payload: Payload::new(bytes).unwrap(),
        metadata: Metadata::new(b"").unwrap(),
        precondition: Precondition::Matches(info.version),
    };
    block_on(store.compare_exchange(&ctx(), &m)).unwrap();
}

fn claim(name: &str, index: u32, live: bool) -> RepairClaim {
    RepairClaim {
        name: name.to_owned(),
        index: id(index),
        live,
    }
}

/// A bucket holding a live name, a deleted one and a name rebound after a delete.
fn populated(store: &impl hsearch_object::MetadataStore, c: &Catalog) -> Vec<String> {
    let n = names(1, 3, "a");
    block_on(c.create(store, &layout(), &n[0], id(1))).unwrap();
    block_on(c.create(store, &layout(), &n[1], id(2))).unwrap();
    block_on(c.delete(store, &layout(), &n[1], &id(2))).unwrap();
    block_on(c.create(store, &layout(), &n[2], id(3))).unwrap();
    block_on(c.delete(store, &layout(), &n[2], &id(3))).unwrap();
    block_on(c.create(store, &layout(), &n[2], id(4))).unwrap();
    n
}

fn good_claims(n: &[String]) -> Vec<RepairClaim> {
    vec![
        claim(&n[0], 1, true),
        claim(&n[1], 2, false),
        claim(&n[2], 4, true),
    ]
}

#[test]
fn an_unreadable_page_is_rebuilt_and_no_name_comes_back_to_an_identity_it_lost() {
    let store = counted();
    let c = catalog(&*store);
    let n = populated(&*store, &c);
    let before: Vec<_> = n
        .iter()
        .map(|x| block_on(c.resolve(&*store, &layout(), x)).unwrap())
        .collect();
    corrupt(&*store, 1);
    assert_eq!(block_on(c.verify(&*store, &layout())).unwrap().pages_bad, 1);
    assert!(block_on(c.resolve(&*store, &layout(), &n[0])).is_err());
    let done = block_on(c.repair_page(&*store, &layout(), 1, &good_claims(&n))).unwrap();
    assert_eq!((done.bucket, done.names, done.replaced), (1, 3, true));
    let after: Vec<_> = n
        .iter()
        .map(|x| block_on(c.resolve(&*store, &layout(), x)).unwrap())
        .collect();
    // Same identities, states and name generations: a tombstone keeps the generation it had.
    assert_eq!(before, after);
    assert_eq!(block_on(c.verify(&*store, &layout())).unwrap().pages_bad, 0);
    // The tombstone survived the rebuild: the lost identity is refused, a new one is accepted.
    assert!(matches!(
        block_on(c.create(&*store, &layout(), &n[1], id(2))),
        Err(Error::Invalid(_))
    ));
    block_on(c.create(&*store, &layout(), &n[1], id(20))).unwrap();
    // And the identity a name held before the rebind (3) is still refused after the rebuild.
    block_on(c.delete(&*store, &layout(), &n[2], &id(4))).unwrap();
    assert!(matches!(
        block_on(c.create(&*store, &layout(), &n[2], id(3))),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn a_missing_page_is_created_and_a_healthy_one_is_never_rewritten() {
    let store = counted();
    let c = catalog(&*store);
    let n = populated(&*store, &c);
    // Healthy: refused, untouched.
    let before = block_on(store.head(&ctx(), &page_key(1))).unwrap().version;
    assert!(matches!(
        block_on(c.repair_page(&*store, &layout(), 1, &good_claims(&n))),
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        block_on(store.head(&ctx(), &page_key(1))).unwrap().version,
        before
    );
    // Absent: a bucket that was never written takes a page from its claims (only a bound
    // identity counts).
    let m = names(2, 1, "b");
    block_on(c.create(&*store, &layout(), &m[0], id(30))).unwrap();
    let k = page_key(2);
    let info = block_on(store.head(&ctx(), &k)).unwrap();
    block_on(store.remove_version(&ctx(), &k, &info.version)).unwrap();
    let done = block_on(c.repair_page(&*store, &layout(), 2, &[claim(&m[0], 30, true)])).unwrap();
    assert_eq!((done.names, done.replaced), (1, false));
    assert_eq!(
        block_on(c.resolve(&*store, &layout(), &m[0])).unwrap(),
        Resolution::Live {
            index: id(30),
            name_generation: 1
        }
    );
}

#[test]
fn an_invented_or_misplaced_claim_is_refused_and_writes_nothing() {
    let store = counted();
    let c = catalog(&*store);
    let n = populated(&*store, &c);
    corrupt(&*store, 1);
    let version = block_on(store.head(&ctx(), &page_key(1))).unwrap().version;
    let other = names(2, 1, "z");
    block_on(c.create(&*store, &layout(), &other[0], id(31))).unwrap();
    let refused = [
        // An identity that was never bound.
        vec![claim(&n[0], 99, true)],
        // An identity bound to another name.
        vec![claim(&n[0], 2, true)],
        // A name that belongs to another bucket.
        vec![claim(&other[0], 31, true)],
        vec![],
    ];
    for claims in refused {
        assert!(
            matches!(
                block_on(c.repair_page(&*store, &layout(), 1, &claims)),
                Err(Error::Invalid(_))
            ),
            "{claims:?}"
        );
    }
    assert!(block_on(c.repair_page(&*store, &layout(), 9, &good_claims(&n))).is_err());
    assert_eq!(
        block_on(store.head(&ctx(), &page_key(1))).unwrap().version,
        version,
        "a refused repair wrote nothing"
    );
}

#[test]
fn two_repairs_racing_leave_exactly_one_page() {
    let store = counted();
    let c = catalog(&*store);
    let n = populated(&*store, &c);
    corrupt(&*store, 1);
    let claims = good_claims(&n);
    let results: Vec<_> = (0..4)
        .map(|_| {
            let (s, c, claims) = (Arc::clone(&store), c.clone(), claims.clone());
            std::thread::spawn(move || block_on(c.repair_page(&*s, &layout(), 1, &claims)))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    let won = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(won, 1, "{results:?}");
    for r in results.iter().filter(|r| r.is_err()) {
        // A loser either lost the swap or arrived after the page was healthy again.
        assert!(
            matches!(r, Err(Error::Contended | Error::Invalid(_))),
            "{r:?}"
        );
    }
    let report = block_on(c.verify(&*store, &layout())).unwrap();
    assert_eq!((report.pages_bad, report.live, report.deleted), (0, 2, 1));
}

#[test]
fn an_oversized_page_is_repaired_once_and_a_second_repairer_never_overwrites_the_first() {
    let store = counted();
    let c = catalog(&*store);
    let n = populated(&*store, &c);
    // Longer than any page may be: it is never read, only replaced at the version that was judged.
    corrupt_with(&*store, 1, &vec![7u8; 700 * 1024]);
    let claims = good_claims(&n);
    let results: Vec<_> = (0..4)
        .map(|_| {
            let (s, c, claims) = (Arc::clone(&store), c.clone(), claims.clone());
            std::thread::spawn(move || block_on(c.repair_page(&*s, &layout(), 1, &claims)))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    // Repairing the repaired page is refused, not redone.
    assert!(matches!(
        block_on(c.repair_page(&*store, &layout(), 1, &claims)),
        Err(Error::Invalid(_))
    ));
    let report = block_on(c.verify(&*store, &layout())).unwrap();
    assert_eq!((report.pages_bad, report.live, report.deleted), (0, 2, 1));
}
