mod support;

use hsearch_object::{Budget, ByteRange, Cancellation, Error, Metadata, ObjectKey, Payload,
    Precondition, RequestContext, RequestId, RetryAction, VersionToken, retry_action};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use std::time::Duration;
use support::{context, limited, mutation};

#[test]
fn bounded_inputs_reject_before_copy_and_debug_redacts() {
    macro_rules! bounded_cases {
        ($ty:ty, $limit:expr, $empty:expr) => {{
            let body = vec![b'x'; $limit];
            let value = <$ty>::new(&body).unwrap();
            assert_eq!(value.len(), $limit);
            assert!(!value.is_empty());
            assert_eq!(value.as_bytes(), body);
            assert_eq!(value.clone(), value);
            let debug = format!("{value:?}");
            assert!(debug.contains("bytes"));
            assert!(!debug.contains("xxx"));
            assert_eq!(<$ty>::new(&vec![0; $limit + 1]), Err(Error::InvalidInput));
            if $empty { assert!(<$ty>::new(&[]).unwrap().is_empty()); }
            else { assert_eq!(<$ty>::new(&[]), Err(Error::InvalidInput)); }
        }};
    }
    bounded_cases!(ObjectKey, 1024, false);
    bounded_cases!(RequestId, 128, false);
    bounded_cases!(VersionToken, 256, false);
    bounded_cases!(Metadata, 8192, true);
    bounded_cases!(Payload, 16 * 1024 * 1024, true);
    let a = VersionToken::new(b"a").unwrap();
    assert_ne!(a, VersionToken::new(b"b").unwrap());
    let mut set = std::collections::HashSet::new();
    assert!(set.insert(a.clone()));
    assert!(!set.insert(a));
}

#[test]
fn checked_ranges_have_no_wrapping_or_empty_success() {
    assert_eq!(ByteRange::new(0, 0), Err(Error::InvalidRange));
    assert_eq!(ByteRange::new(u64::MAX, 1), Err(Error::InvalidRange));
    let range = ByteRange::new(u64::MAX - 1, 1).unwrap();
    assert_eq!(range.offset(), u64::MAX - 1);
    assert_eq!(range.length(), 1);
    assert_eq!(range.validate(u64::MAX), Ok(()));
    assert_eq!(range.validate(u64::MAX - 1), Err(Error::InvalidRange));
}

#[test]
fn deadlines_cancellation_and_shared_accounting_are_finite() {
    let flag = Cancellation::default();
    let ctx = RequestContext::new(Duration::from_secs(1), flag.clone(), Budget { request_bytes: 5, response_bytes: 7, attempts: 2 }).unwrap();
    assert!(!flag.is_cancelled());
    ctx.charge_request(2).unwrap();
    assert_eq!(ctx.charge_request(4), Err(Error::BudgetExceeded));
    assert_eq!(ctx.usage().unwrap().attempts, 1);
    ctx.charge_request(3).unwrap();
    assert_eq!(ctx.charge_request(0), Err(Error::BudgetExceeded));
    ctx.charge_response(7).unwrap();
    assert_eq!(ctx.charge_response(1), Err(Error::BudgetExceeded));
    assert_eq!(ctx.usage().unwrap(), hsearch_object::Usage { request_bytes: 5, response_bytes: 7, attempts: 2 });
    flag.cancel();
    assert!(flag.is_cancelled());
    assert_eq!(ctx.check(), Err(Error::Cancelled));
    // Returning an already admitted known outcome remains legal after cancel.
    assert_eq!(ctx.charge_response(0), Ok(()));
    let expired = RequestContext::new(Duration::ZERO, Cancellation::default(), Budget { request_bytes: 0, response_bytes: 0, attempts: 0 }).unwrap();
    assert_eq!(expired.check(), Err(Error::DeadlineExceeded));
    assert_eq!(RequestContext::new(Duration::from_secs(301), Cancellation::default(), Budget { request_bytes: 1, response_bytes: 1, attempts: 1 }).unwrap_err(), Error::InvalidInput);
    let expired = context(); expired.expire();
    assert_eq!(expired.charge_request(0), Err(Error::DeadlineExceeded));
    let cancelled = context(); cancelled.cancel();
    assert_eq!(cancelled.charge_request(0), Err(Error::Cancelled));
    let overflow = limited(u64::MAX, u64::MAX, 3);
    overflow.charge_request(u64::MAX).unwrap();
    assert_eq!(overflow.charge_request(1), Err(Error::BudgetExceeded));
    overflow.charge_response(u64::MAX).unwrap();
    assert_eq!(overflow.charge_response(1), Err(Error::BudgetExceeded));
}

#[test]
fn every_retry_action_preserves_the_envelope_and_errors_are_bounded() {
    let req = mutation(b"identity", b"private-key", b"secret-payload", Precondition::Absent);
    let errors = [Error::InvalidInput, Error::NotFound, Error::Conflict, Error::UnknownOutcome,
        Error::Unavailable, Error::PermissionDenied, Error::Throttled, Error::Unsupported,
        Error::Corrupt, Error::Cancelled, Error::DeadlineExceeded, Error::BudgetExceeded,
        Error::InvalidRange, Error::IdentityConflict, Error::ResolutionUnavailable];
    for error in errors {
        let text = error.to_string();
        assert!(text.len() <= 32 && !text.contains("secret"));
        assert_eq!(format!("{error:?}"), text);
        let _: &dyn std::error::Error = &error;
        match retry_action(&req, error) {
            RetryAction::Resolve(original) => { assert_eq!(error, Error::UnknownOutcome); assert!(std::ptr::eq(original, &req)); }
            RetryAction::Retry(original) => { assert!(matches!(error, Error::Unavailable | Error::Throttled)); assert!(std::ptr::eq(original, &req)); }
            RetryAction::Stop => assert!(!matches!(error, Error::UnknownOutcome | Error::Unavailable | Error::Throttled)),
        }
    }
    assert!(!format!("{req:?}").contains("secret"));
}

#[test]
fn invalid_memory_limits_never_construct_a_backend() {
    let normal = MemoryLimits { objects: 2, versions: 4, version_bytes: 4096, identities: 4, identity_bytes: 8192 };
    for invalid in [MemoryLimits { objects: 0, ..normal }, MemoryLimits { objects: 4097, ..normal },
        MemoryLimits { versions: 0, ..normal }, MemoryLimits { versions: 16385, ..normal },
        MemoryLimits { identities: 0, ..normal }, MemoryLimits { identities: 16385, ..normal },
        MemoryLimits { version_bytes: 0, ..normal }, MemoryLimits { version_bytes: 64 * 1024 * 1024 + 1, ..normal },
        MemoryLimits { identity_bytes: 0, ..normal }, MemoryLimits { identity_bytes: 64 * 1024 * 1024 + 1, ..normal }] {
        assert_eq!(MemoryStore::new(invalid).unwrap_err(), Error::InvalidInput);
    }
    assert!(format!("{:?}", MemoryStore::new(normal).unwrap()).contains("limits"));
}
