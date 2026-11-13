//! The M03.01 storage contract against a real S3-compatible store.
//!
//! Needs `HSEARCH_S3_ENDPOINT` (host:port), `HSEARCH_S3_ACCESS_KEY` and
//! `HSEARCH_S3_SECRET_KEY`: RustFS first (CI); MinIO is tracked as M03.10. Without
//! them each test prints SKIPPED and passes, unless `HSEARCH_S3_REQUIRED=1`,
//! which CI sets so a missing store is a failure rather than a silent skip.
#[path = "../../object/tests/support/mod.rs"]
mod support;

use hsearch_object::{
    Budget, ByteRange, Cancellation, Error, MetadataStore, ObjectBackend, ObjectKey, Precondition,
    RequestContext, VersionToken,
};
use hsearch_s3::{MAX_METADATA, RequestCounts, S3Config, S3Store, SCAN_LIMIT};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};
use support::{context, mutation, run};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A store on a fresh versioned bucket, or `None` when no endpoint is configured.
fn store() -> Option<S3Store> {
    let Ok(endpoint) = std::env::var("HSEARCH_S3_ENDPOINT") else {
        assert!(
            std::env::var("HSEARCH_S3_REQUIRED").as_deref() != Ok("1"),
            "HSEARCH_S3_REQUIRED=1 but HSEARCH_S3_ENDPOINT is unset"
        );
        eprintln!(
            "SKIPPED: HSEARCH_S3_ENDPOINT is not set; nothing was checked against a real store"
        );
        return None;
    };
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let store = S3Store::new(S3Config {
        endpoint,
        bucket: format!("hs-{nanos}-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .unwrap();
    store.prepare_bucket(&context()).expect("prepare bucket");
    Some(store)
}

#[test]
fn public_contract_history() {
    let Some(store) = store() else { return };
    assert_eq!(support::conditional_history(&store), Ok(()));
}

#[test]
fn simultaneous_cas_has_one_winner() {
    let Some(store) = store() else { return };
    let store = Arc::new(store);
    let first = mutation(b"create", b"key", b"old", Precondition::Absent);
    let token = run(store.create(&context(), &first))
        .unwrap()
        .object
        .version;
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let (store, token, barrier) = (store.clone(), token.clone(), barrier.clone());
            std::thread::spawn(move || {
                let id = format!("contender-{i}");
                let request = mutation(
                    id.as_bytes(),
                    b"key",
                    id.as_bytes(),
                    Precondition::Matches(token),
                );
                barrier.wait();
                let result = run(store.compare_exchange(&context(), &request));
                (request, result)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|(_, r)| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|(_, r)| *r == Err(Error::Conflict))
            .count(),
        7
    );
    let (request, receipt) = results.iter().find(|(_, r)| r.is_ok()).unwrap();
    let receipt = receipt.as_ref().unwrap();
    assert_eq!(
        run(store.head(&context(), &first.key)),
        Ok(receipt.object.clone())
    );
    assert_eq!(
        run(store.read_range(
            &context(),
            &first.key,
            &receipt.object.version,
            ByteRange::new(0, request.payload.len() as u64).unwrap()
        )),
        Ok(request.payload.clone())
    );
}

#[test]
fn lost_race_loser_resolves_to_conflict_and_winner_replays() {
    let Some(store) = store() else { return };
    let first = mutation(b"create", b"k", b"v0", Precondition::Absent);
    let base = run(store.create(&context(), &first))
        .unwrap()
        .object
        .version;
    let winner = mutation(b"w", b"k", b"v1", Precondition::Matches(base.clone()));
    let receipt = run(store.compare_exchange(&context(), &winner)).unwrap();
    // The winner's own retry, after its condition is stale, is its receipt.
    assert_eq!(
        run(store.compare_exchange(&context(), &winner)),
        Ok(receipt.clone())
    );
    assert_eq!(run(store.resolve(&context(), &winner)), Ok(receipt));
    let loser = mutation(b"l", b"k", b"v2", Precondition::Matches(base.clone()));
    assert_eq!(
        run(store.compare_exchange(&context(), &loser)),
        Err(Error::Conflict)
    );
    // Never accepted, so there is nothing to resolve; not a certified refusal.
    assert_eq!(
        run(store.resolve(&context(), &loser)),
        Err(Error::ResolutionUnavailable)
    );
}

#[test]
fn changed_content_under_an_accepted_identity_is_refused() {
    let Some(store) = store() else { return };
    let request = mutation(b"same-id", b"k", b"v0", Precondition::Absent);
    run(store.create(&context(), &request)).unwrap();
    let mut changed = request.clone();
    changed.payload = hsearch_object::Payload::new(b"different").unwrap();
    assert_eq!(
        run(store.resolve(&context(), &changed)),
        Err(Error::IdentityConflict)
    );
    assert_eq!(
        run(store.create(&context(), &changed)),
        Err(Error::IdentityConflict)
    );
}

#[test]
fn conditions_and_ranges_fail_cleanly() {
    let Some(store) = store() else { return };
    let missing = mutation(
        b"m",
        b"absent-key",
        b"x",
        Precondition::Matches(VersionToken::new(b"v\n\"e\"").unwrap()),
    );
    assert_eq!(
        run(store.compare_exchange(&context(), &missing)),
        Err(Error::Conflict)
    );
    assert_eq!(
        run(store.head(&context(), &missing.key)),
        Err(Error::NotFound)
    );
    let request = mutation(b"c", b"k", b"abcdef", Precondition::Absent);
    let created = run(store.create(&context(), &request)).unwrap();
    let version = &created.object.version;
    // Past the end, running over the end, and a version that never existed.
    assert_eq!(
        run(store.read_range(
            &context(),
            &request.key,
            version,
            ByteRange::new(100, 4).unwrap()
        )),
        Err(Error::InvalidRange)
    );
    assert_eq!(
        run(store.read_range(
            &context(),
            &request.key,
            version,
            ByteRange::new(4, 4).unwrap()
        )),
        Err(Error::InvalidRange)
    );
    let ghost = VersionToken::new(b"no-such-version\n\"e\"").unwrap();
    assert_eq!(
        run(store.read_range(
            &context(),
            &request.key,
            &ghost,
            ByteRange::new(0, 2).unwrap()
        )),
        Err(Error::NotFound)
    );
    // Non-absent create and malformed tokens never reach the network.
    let before = store.counts();
    let wrong = mutation(b"w", b"k2", b"x", Precondition::Matches(version.clone()));
    assert_eq!(
        run(store.create(&context(), &wrong)),
        Err(Error::InvalidInput)
    );
    let bad = mutation(
        b"b",
        b"k2",
        b"x",
        Precondition::Matches(VersionToken::new(b"no-separator").unwrap()),
    );
    assert_eq!(
        run(store.compare_exchange(&context(), &bad)),
        Err(Error::InvalidInput)
    );
    let big = hsearch_object::Metadata::new(&vec![1; MAX_METADATA + 1]).unwrap();
    let mut oversized = mutation(b"o", b"k3", b"x", Precondition::Absent);
    oversized.metadata = big;
    assert_eq!(
        run(store.create(&context(), &oversized)),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(store.counts(), before);
}

#[test]
fn budgets_cancellation_and_deadlines_stop_before_the_network() {
    let Some(store) = store() else { return };
    let request = mutation(b"b", b"k", b"data", Precondition::Absent);
    let before = store.counts();
    assert_eq!(
        run(store.create(&support::limited(1, 1 << 20, 10), &request)),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(
        run(store.create(&support::limited(1 << 20, 1, 10), &request)),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(
        run(store.create(&support::limited(1 << 20, 1 << 20, 0), &request)),
        Err(Error::BudgetExceeded)
    );
    let cancelled = context();
    cancelled.cancel();
    assert_eq!(
        run(store.create(&cancelled, &request)),
        Err(Error::Cancelled)
    );
    let expired = context();
    expired.expire();
    assert_eq!(
        run(store.create(&expired, &request)),
        Err(Error::DeadlineExceeded)
    );
    let zero = RequestContext::new(
        Duration::ZERO,
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 10,
        },
    )
    .unwrap();
    assert_eq!(
        run(store.head(&zero, &request.key)),
        Err(Error::DeadlineExceeded)
    );
    assert_eq!(store.counts(), before);
}

#[test]
fn every_request_is_counted_by_type() {
    let Some(store) = store() else { return };
    let after_setup = store.counts();
    assert_eq!(after_setup.list + after_setup.head + after_setup.delete, 0);
    let a = mutation(b"a", b"k", b"abcdef", Precondition::Absent);
    let created = run(store.create(&context(), &a)).unwrap();
    assert_eq!(
        store.counts().put,
        after_setup.put + 1,
        "a plain write is one PUT"
    );
    run(store.head(&context(), &a.key)).unwrap();
    run(store.read_range(
        &context(),
        &a.key,
        &created.object.version,
        ByteRange::new(0, 3).unwrap(),
    ))
    .unwrap();
    let counts = store.counts();
    assert_eq!(
        (counts.head, counts.get - after_setup.get, counts.list),
        (1, 1, 0)
    );
    // Resolving costs one LIST plus a HEAD per inspected version.
    run(store.resolve(&context(), &a)).unwrap();
    let resolved = store.counts();
    assert_eq!((resolved.list, resolved.head - counts.head), (1, 1));
    store
        .delete_version(&context(), &a.key, &created.object.version)
        .unwrap();
    assert_eq!(store.counts().delete, 1);
    assert_eq!(run(store.head(&context(), &a.key)), Err(Error::NotFound));
}

#[test]
fn resolution_is_bounded_not_guessed() {
    let Some(store) = store() else { return };
    let first = mutation(b"r0", b"k", b"v", Precondition::Absent);
    let mut latest = run(store.create(&context(), &first))
        .unwrap()
        .object
        .version;
    for i in 1..=SCAN_LIMIT + 2 {
        let id = format!("r{i}");
        let next = mutation(
            id.as_bytes(),
            b"k",
            id.as_bytes(),
            Precondition::Matches(latest),
        );
        latest = run(store.compare_exchange(&context(), &next))
            .unwrap()
            .object
            .version;
    }
    // The first write is older than the scan window: unavailable, never "refused".
    assert_eq!(
        run(store.resolve(&context(), &first)),
        Err(Error::ResolutionUnavailable)
    );
    assert_eq!(
        run(store.create(
            &context(),
            &mutation(b"x", b"k", b"y", Precondition::Absent)
        )),
        Err(Error::ResolutionUnavailable)
    );
    let _ = ObjectKey::new(b"k");
}

#[test]
fn an_unreachable_store_is_unavailable_not_unknown() {
    let store = S3Store::new(S3Config {
        endpoint: "127.0.0.1:1".into(),
        bucket: "b".into(),
        access_key: "a".into(),
        secret_key: "s".into(),
        region: "us-east-1".into(),
    })
    .unwrap();
    let request = mutation(b"u", b"k", b"v", Precondition::Absent);
    assert_eq!(
        run(store.create(&context(), &request)),
        Err(Error::Unavailable)
    );
    assert_eq!(
        store.counts(),
        RequestCounts {
            put: 1,
            ..RequestCounts::default()
        }
    );
    assert_eq!(
        S3Store::new(S3Config {
            endpoint: "a/b".into(),
            bucket: "b".into(),
            access_key: String::new(),
            secret_key: String::new(),
            region: String::new()
        })
        .err(),
        Some(Error::InvalidInput)
    );
}

/// Serve one canned raw HTTP response per connection, then close.
fn canned(responses: Vec<&'static str>) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for response in responses {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0u8; 8192];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    address
}

fn offline(endpoint: String) -> S3Store {
    S3Store::new(S3Config {
        endpoint,
        bucket: "b".into(),
        access_key: "a".into(),
        secret_key: "s".into(),
        region: "us-east-1".into(),
    })
    .unwrap()
}

#[test]
fn provider_statuses_map_to_typed_errors() {
    let key = ObjectKey::new(b"k").unwrap();
    let cases = [
        (
            "HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n",
            Error::PermissionDenied,
        ),
        (
            "HTTP/1.1 503 Slow Down\r\ncontent-length: 0\r\n\r\n",
            Error::Throttled,
        ),
        (
            "HTTP/1.1 429 Too Many\r\ncontent-length: 0\r\n\r\n",
            Error::Throttled,
        ),
        (
            "HTTP/1.1 501 Not Implemented\r\ncontent-length: 0\r\n\r\n",
            Error::Unsupported,
        ),
        (
            "HTTP/1.1 400 Bad\r\ncontent-length: 0\r\n\r\n",
            Error::InvalidInput,
        ),
        (
            "HTTP/1.1 404 No\r\ncontent-length: 0\r\n\r\n",
            Error::NotFound,
        ),
        (
            "HTTP/1.1 500 Oops\r\ncontent-length: 0\r\n\r\n",
            Error::Unavailable,
        ),
    ];
    for (response, expected) in cases {
        let store = offline(canned(vec![response]));
        assert_eq!(
            run(store.head(&context(), &key)),
            Err(expected),
            "{response}"
        );
    }
    // A 500 on a mutation may have committed: never a refusal.
    let store = offline(canned(vec![
        "HTTP/1.1 500 Oops\r\ncontent-length: 0\r\n\r\n",
    ]));
    let request = mutation(b"m", b"k", b"v", Precondition::Absent);
    assert_eq!(
        run(store.create(&context(), &request)),
        Err(Error::UnknownOutcome)
    );
    // An accepted write whose receipt cannot be built is unknown, never a refusal.
    let store = offline(canned(vec![
        "HTTP/1.1 200 OK\r\netag: \"x\"\r\ncontent-length: 0\r\n\r\n",
    ]));
    assert_eq!(
        run(store.create(&context(), &request)),
        Err(Error::UnknownOutcome)
    );
    let store = offline(canned(vec!["HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n"]));
    assert_eq!(store.verify_versioning(&context()), Err(Error::Unsupported));
}

#[test]
fn a_dropped_connection_is_unknown_for_writes_and_unavailable_for_reads() {
    let key = ObjectKey::new(b"k").unwrap();
    let store = offline(canned(vec![""]));
    assert_eq!(run(store.head(&context(), &key)), Err(Error::Unavailable));
    let store = offline(canned(vec![""]));
    let request = mutation(b"m", b"k", b"v", Precondition::Absent);
    assert_eq!(
        run(store.create(&context(), &request)),
        Err(Error::UnknownOutcome)
    );
}

#[test]
fn tight_budgets_survive_a_replayed_accept_and_siblings_do_not_hide_conflicts() {
    let Some(store) = store() else { return };
    let request = mutation(b"r", b"k", b"v", Precondition::Absent);
    run(store.create(&context(), &request)).unwrap();
    // Retry with room for exactly one receipt: the replay must not be a budget refusal.
    let one_receipt = support::limited(1 << 20, 1 + 128 + 8 + 15 + 1, 10);
    assert!(run(store.create(&one_receipt, &request)).is_ok());
    // Many sibling keys sharing the prefix must not turn a plain conflict into "unavailable".
    for i in 0..20 {
        let id = format!("s{i}");
        let key = format!("k{i}");
        run(store.create(
            &context(),
            &mutation(id.as_bytes(), key.as_bytes(), b"x", Precondition::Absent),
        ))
        .unwrap();
    }
    let other = mutation(b"other", b"k", b"w", Precondition::Absent);
    assert_eq!(run(store.create(&context(), &other)), Err(Error::Conflict));
}

#[test]
fn control_bytes_in_a_token_never_reach_the_network() {
    let Some(store) = store() else { return };
    let before = store.counts();
    let bad = mutation(
        b"b",
        b"k",
        b"x",
        Precondition::Matches(VersionToken::new(b"v\n\"e\r\nX: y\"").unwrap()),
    );
    assert_eq!(
        run(store.compare_exchange(&context(), &bad)),
        Err(Error::InvalidInput)
    );
    let huge = ByteRange::new(0, 16 * 1024 * 1024 + 1).unwrap();
    let key = ObjectKey::new(b"k").unwrap();
    let token = VersionToken::new(b"v\n\"e\"").unwrap();
    assert_eq!(
        run(store.read_range(&context(), &key, &token, huge)),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(store.counts(), before);
}
