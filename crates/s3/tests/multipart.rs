//! The adapter's multipart upload against a real S3-compatible store (RustFS in CI).
//! Without `HSEARCH_S3_ENDPOINT` each test prints SKIPPED and passes unless
//! `HSEARCH_S3_REQUIRED=1`, as in `rustfs.rs`.
#[path = "../../object/tests/support/mod.rs"]
mod support;

use hsearch_object::{ByteRange, Error, ObjectBackend, ObjectKey};
use hsearch_s3::{MIN_PART, Outcome, Role, S3Config, S3Store, Verb};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;
use support::run;

/// A context whose request budget holds a 5 MiB part; the default 4 MiB one cannot.
fn context() -> hsearch_object::RequestContext {
    support::limited(64 * 1024 * 1024, 4 * 1024 * 1024, 100)
}

static NEXT: AtomicU32 = AtomicU32::new(0);

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
        bucket: format!("hm-{nanos}-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .unwrap();
    store.prepare_bucket(&context()).expect("prepare bucket");
    Some(store)
}

fn key(name: &[u8]) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

#[test]
fn a_completed_upload_is_one_object_and_every_request_is_attributed() {
    let Some(store) = store() else { return };
    let ctx = context();
    let object = key(b"segment");
    // Bucket setup already sent PUTs of its own; count only what the upload adds.
    let setup_puts = store.ledger().attempts_by_verb(Verb::Put);
    let first = vec![3u8; MIN_PART];
    let tail = b"tail-bytes".to_vec();

    let mut upload = store.begin_multipart(&ctx, &object).unwrap();
    assert_eq!(
        run(store.head(&ctx, &object)),
        Err(Error::NotFound),
        "invisible before completion"
    );
    upload.put_part(&ctx, &first).unwrap();
    upload.put_part(&ctx, &tail).unwrap();
    assert_eq!((upload.parts(), upload.length()), (2, MIN_PART as u64 + 10));
    let (version, length) = upload.complete(&ctx).unwrap();
    assert_eq!(length, MIN_PART as u64 + 10);

    let info = run(store.head(&ctx, &object)).unwrap();
    assert_eq!(
        (info.version.clone(), info.length),
        (version.clone(), length)
    );
    let read = run(store.read_range(
        &ctx,
        &object,
        &version,
        ByteRange::new(MIN_PART as u64 - 2, 12).unwrap(),
    ))
    .unwrap();
    assert_eq!(read.as_bytes(), [&[3u8, 3][..], &tail].concat());

    // begin + complete are POSTs, the two parts are PUTs; every one of the four is a logical
    // call and a wire attempt, and the part bytes are in the request bytes.
    let ledger = store.ledger();
    assert_eq!(ledger.attempts_by_verb(Verb::Post), 2);
    assert_eq!(ledger.attempts_by_verb(Verb::Put) - setup_puts, 2);
    assert_eq!(
        ledger.cell(Role::Payload, Verb::Put, Outcome::Ok).attempts,
        2
    );
    assert!(
        ledger
            .cell(Role::Payload, Verb::Put, Outcome::Ok)
            .request_bytes
            >= length
    );
    assert_eq!(ledger.retries(), 0);
}

#[test]
fn completing_onto_an_existing_key_is_a_conflict_and_leaves_it_untouched() {
    let Some(store) = store() else { return };
    let ctx = context();
    let object = key(b"once");
    let mut first = store.begin_multipart(&ctx, &object).unwrap();
    first.put_part(&ctx, b"original").unwrap();
    let (original, _) = first.complete(&ctx).unwrap();

    let mut second = store.begin_multipart(&ctx, &object).unwrap();
    second.put_part(&ctx, b"replacement").unwrap();
    assert_eq!(second.complete(&ctx), Err(Error::Conflict));
    // The failed completion left the upload open, so its parts can still be discarded.
    second.abort(&ctx).unwrap();
    assert_eq!(run(store.head(&ctx, &object)).unwrap().version, original);
    assert_eq!(store.ledger().attempts_with(Outcome::Conflict), 1);
}

#[test]
fn an_aborted_upload_leaves_nothing_and_the_abort_is_attributed() {
    let Some(store) = store() else { return };
    let ctx = context();
    let object = key(b"gone");
    let mut upload = store.begin_multipart(&ctx, &object).unwrap();
    upload.put_part(&ctx, &vec![1u8; MIN_PART]).unwrap();
    upload.abort(&ctx).unwrap();
    assert_eq!(run(store.head(&ctx, &object)), Err(Error::NotFound));
    assert_eq!(store.ledger().attempts_by_verb(Verb::Delete), 1);
}

#[test]
fn part_rules_are_enforced_before_the_wire() {
    let Some(store) = store() else { return };
    let ctx = context();
    let mut upload = store.begin_multipart(&ctx, &key(b"rules")).unwrap();
    let before = store.ledger().attempts(Role::Payload);

    assert_eq!(upload.put_part(&ctx, b""), Err(Error::InvalidInput));
    upload.put_part(&ctx, b"short").unwrap();
    let sent = store.ledger().attempts(Role::Payload);
    // Only the last part may be short, so nothing may follow one.
    assert_eq!(upload.put_part(&ctx, b"more"), Err(Error::InvalidInput));
    assert_eq!(
        store.ledger().attempts(Role::Payload),
        sent,
        "refused without a request"
    );
    assert_eq!(sent, before + 1);
    upload.abort(&ctx).unwrap();

    let empty = store.begin_multipart(&ctx, &key(b"empty")).unwrap();
    let sent = store.ledger().attempts(Role::Payload);
    assert_eq!(empty.complete(&ctx), Err(Error::InvalidInput));
    assert_eq!(store.ledger().attempts(Role::Payload), sent);
    empty.abort(&ctx).unwrap();
}

#[test]
fn an_upload_spends_the_callers_budget() {
    let Some(store) = store() else { return };
    let ctx = support::limited(MIN_PART as u64 / 2, 4 * 1024 * 1024, 100);
    let mut upload = store.begin_multipart(&context(), &key(b"budget")).unwrap();
    assert_eq!(
        upload.put_part(&ctx, &vec![0u8; MIN_PART]),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(upload.parts(), 0);
    upload.abort(&context()).unwrap();
}

#[test]
fn aborting_an_upload_that_is_already_gone_succeeds() {
    let Some(store) = store() else { return };
    let ctx = context();
    let object = key(b"twice");
    let upload = store.begin_multipart(&ctx, &object).unwrap();
    // Another party (or an earlier attempt whose reply was lost) already aborted it.
    let query = format!("uploadId={}", upload.id());
    let (status, _, _) = store
        .probe_request(&ctx, "DELETE", &object, &query, &[], &[])
        .unwrap();
    assert!((200..300).contains(&status));
    assert_eq!(upload.abort(&ctx), Ok(()));
}

#[test]
fn begin_spends_the_callers_budget_too() {
    let Some(store) = store() else { return };
    let ctx = support::limited(1, 4 * 1024 * 1024, 100);
    assert!(store.begin_multipart(&ctx, &key(b"tiny")).is_err());
    assert_eq!(store.ledger().attempts_by_verb(Verb::Post), 0);
}
