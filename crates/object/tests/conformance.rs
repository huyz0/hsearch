mod support;

use hsearch_object::memory::{Fault, MemoryLimits, MemoryStore};
use hsearch_object::{
    ByteRange, Error, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey, Operation,
    Payload, Precondition, RequestContext, RetryAction, VersionToken, WriteReceipt, retry_action,
};
use std::sync::{Arc, Barrier};
use support::{context, limited, mutation, run};

fn limits() -> MemoryLimits {
    MemoryLimits {
        objects: 32,
        versions: 64,
        version_bytes: 1024 * 1024,
        identities: 64,
        identity_bytes: 1024 * 1024,
    }
}
fn backend() -> MemoryStore {
    MemoryStore::new(limits()).unwrap()
}

#[test]
fn public_contract_history() {
    assert_eq!(support::conditional_history(&backend()), Ok(()));
}

#[test]
fn simultaneous_cas_has_one_winner() {
    let store = Arc::new(backend());
    let first = mutation(b"create", b"key", b"old", Precondition::Absent);
    let token = run(store.create(&context(), &first))
        .unwrap()
        .object
        .version;
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let store = store.clone();
            let token = token.clone();
            let barrier = barrier.clone();
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
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, result)| *result == Err(Error::Conflict))
            .count(),
        7
    );
    let (request, receipt) = results.iter().find(|(_, result)| result.is_ok()).unwrap();
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
fn uncertainty_and_late_abort_resolve_the_same_write() {
    for fault in [
        Fault::LoseResponseAfterCommit,
        Fault::CancelAfterCommit,
        Fault::ExpireAfterCommit,
    ] {
        let store = backend();
        let ctx = context();
        let request = mutation(b"identity", b"key", b"accepted", Precondition::Absent);
        store.inject(fault).unwrap();
        assert_eq!(
            run(store.create(&ctx, &request)),
            Err(Error::UnknownOutcome)
        );
        assert_eq!(
            retry_action(&request, Error::UnknownOutcome),
            RetryAction::Resolve(&request)
        );
        let resolved = run(store.resolve(&context(), &request)).unwrap();
        assert_eq!(resolved.request_id, request.request_id);
        assert_eq!(
            run(store.create(&context(), &request)),
            Ok(resolved.clone())
        );
        assert_eq!(
            run(store.read_range(
                &context(),
                &request.key,
                &resolved.object.version,
                ByteRange::new(0, 8).unwrap()
            )),
            Ok(request.payload.clone())
        );
        for changed in [
            Mutation {
                payload: Payload::new(b"changed").unwrap(),
                ..request.clone()
            },
            Mutation {
                key: ObjectKey::new(b"other").unwrap(),
                ..request.clone()
            },
            Mutation {
                metadata: hsearch_object::Metadata::new(b"changed").unwrap(),
                ..request.clone()
            },
            Mutation {
                precondition: Precondition::Matches(resolved.object.version.clone()),
                ..request.clone()
            },
        ] {
            assert_eq!(
                run(store.compare_exchange(&context(), &changed)),
                Err(Error::IdentityConflict)
            );
            assert_eq!(
                run(store.resolve(&context(), &changed)),
                Err(Error::IdentityConflict)
            );
        }
    }
}

#[test]
fn preacceptance_faults_never_create() {
    for (fault, error) in [
        (Fault::UnavailableBeforeAcceptance, Error::Unavailable),
        (Fault::CancelBeforeAcceptance, Error::Cancelled),
        (Fault::ExpireBeforeAcceptance, Error::DeadlineExceeded),
    ] {
        let store = backend();
        let request = mutation(b"id", b"key", b"data", Precondition::Absent);
        store.inject(fault).unwrap();
        assert_eq!(run(store.create(&context(), &request)), Err(error));
        assert_eq!(
            run(store.head(&context(), &request.key)),
            Err(Error::NotFound)
        );
        assert_eq!(
            run(store.resolve(&context(), &request)),
            Err(Error::ResolutionUnavailable)
        );
        assert!(matches!(
            retry_action(&request, error),
            RetryAction::Retry(_) | RetryAction::Stop
        ));
        assert!(run(store.create(&context(), &request)).is_ok());
    }
}

#[test]
fn ranges_are_exact_and_bound_to_key_and_backend() {
    let store = backend();
    let other = backend();
    let request = mutation(b"id", b"key", b"123456", Precondition::Absent);
    let version = run(store.create(&context(), &request))
        .unwrap()
        .object
        .version;
    assert_eq!(
        run(store.read_range(
            &context(),
            &request.key,
            &version,
            ByteRange::new(5, 1).unwrap()
        )),
        Ok(Payload::new(b"6").unwrap())
    );
    for range in [
        ByteRange::new(6, 1).unwrap(),
        ByteRange::new(1, 6).unwrap(),
        ByteRange::new(u64::MAX - 1, 1).unwrap(),
    ] {
        assert_eq!(
            run(store.read_range(&context(), &request.key, &version, range)),
            Err(Error::InvalidRange)
        );
    }
    assert_eq!(
        run(store.read_range(
            &context(),
            &ObjectKey::new(b"wrong").unwrap(),
            &version,
            ByteRange::new(0, 1).unwrap()
        )),
        Err(Error::NotFound)
    );
    assert_eq!(
        run(other.read_range(
            &context(),
            &request.key,
            &version,
            ByteRange::new(0, 1).unwrap()
        )),
        Err(Error::NotFound)
    );
    let wrong = mutation(b"wrong", b"key", b"new", Precondition::Matches(version));
    assert_eq!(
        run(other.compare_exchange(&context(), &wrong)),
        Err(Error::Conflict)
    );
    assert_eq!(
        run(store.create(&context(), &wrong)),
        Err(Error::InvalidInput)
    );
}

#[test]
fn budgets_refuse_before_effect_and_response_copy() {
    let store = backend();
    let request = mutation(b"id", b"key", b"payload", Precondition::Absent);
    for ctx in [limited(0, 100, 1), limited(100, 0, 1), limited(100, 100, 0)] {
        assert_eq!(
            run(store.create(&ctx, &request)),
            Err(Error::BudgetExceeded)
        );
        assert_eq!(
            run(store.head(&context(), &request.key)),
            Err(Error::NotFound)
        );
    }
    let ctx = limited(1000, 1000, 3);
    let receipt = run(store.create(&ctx, &request)).unwrap();
    assert_eq!(
        run(store.create(&limited(100, 0, 1), &request)),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(
        run(store.resolve(&limited(100, 0, 1), &request)),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(
        run(store.head(&limited(100, 0, 1), &request.key)),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(
        run(store.read_range(
            &limited(100, 6, 1),
            &request.key,
            &receipt.object.version,
            ByteRange::new(0, 7).unwrap()
        )),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(
        run(store.head(&ctx, &request.key)),
        Ok(receipt.object.clone())
    );
    assert_eq!(run(store.create(&ctx, &request)), Ok(receipt));
    assert_eq!(ctx.usage().unwrap().attempts, 3);
    assert_eq!(
        run(store.head(&ctx, &request.key)),
        Err(Error::BudgetExceeded)
    );
    assert_eq!(ctx.usage().unwrap().attempts, 3);
    assert!(ctx.usage().unwrap().request_bytes > 0 && ctx.usage().unwrap().response_bytes > 0);
}

#[test]
fn no_silent_eviction_on_capacity_or_identity_exhaustion() {
    for constrained in [
        MemoryLimits {
            objects: 1,
            ..limits()
        },
        MemoryLimits {
            versions: 1,
            ..limits()
        },
        MemoryLimits {
            identities: 1,
            ..limits()
        },
        MemoryLimits {
            version_bytes: 1,
            ..limits()
        },
        MemoryLimits {
            identity_bytes: 1,
            ..limits()
        },
    ] {
        let store = MemoryStore::new(constrained).unwrap();
        let first = mutation(b"id", b"key", b"first", Precondition::Absent);
        let first_result = run(store.create(&context(), &first));
        if let Ok(receipt) = first_result {
            let (key, condition) = if constrained.objects == 1 {
                (b"other".as_slice(), Precondition::Absent)
            } else {
                (
                    b"key".as_slice(),
                    Precondition::Matches(receipt.object.version.clone()),
                )
            };
            let second = mutation(b"second", key, b"second", condition);
            assert_eq!(
                run(store.compare_exchange(&context(), &second)),
                Err(Error::BudgetExceeded)
            );
            assert_eq!(run(store.resolve(&context(), &first)), Ok(receipt.clone()));
            assert_eq!(
                run(store.read_range(
                    &context(),
                    &first.key,
                    &receipt.object.version,
                    ByteRange::new(0, 5).unwrap()
                )),
                Ok(first.payload)
            );
        } else {
            assert_eq!(first_result, Err(Error::BudgetExceeded));
            assert_eq!(
                run(store.head(&context(), &first.key)),
                Err(Error::NotFound)
            );
        }
    }
    let store = backend();
    for _ in 0..32 {
        store.inject(Fault::UnavailableBeforeAcceptance).unwrap();
    }
    assert_eq!(
        store.inject(Fault::LoseResponseAfterCommit),
        Err(Error::BudgetExceeded)
    );
}

/// Deliberately violates CAS by internally replacing the expected token with the
/// latest one. The same public-trait conformance predicate must reject it.
struct Unconditional(MemoryStore);
impl ObjectBackend for Unconditional {
    fn head<'a>(&'a self, cx: &'a RequestContext, key: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.0.head(cx, key)
    }
    fn create<'a>(
        &'a self,
        cx: &'a RequestContext,
        req: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.0.create(cx, req)
    }
    fn read_range<'a>(
        &'a self,
        cx: &'a RequestContext,
        key: &'a ObjectKey,
        version: &'a VersionToken,
        range: ByteRange,
    ) -> Operation<'a, Payload> {
        self.0.read_range(cx, key, version, range)
    }
}
impl MetadataStore for Unconditional {
    fn compare_exchange<'a>(
        &'a self,
        cx: &'a RequestContext,
        req: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            let mut changed = req.clone();
            if matches!(changed.precondition, Precondition::Matches(_)) {
                changed.precondition =
                    Precondition::Matches(self.0.head(cx, &req.key).await?.version);
            }
            self.0.compare_exchange(cx, &changed).await
        })
    }
    fn resolve<'a>(
        &'a self,
        cx: &'a RequestContext,
        req: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.0.resolve(cx, req)
    }
}

#[test]
fn same_conformance_predicate_refuses_unconditional_overwrite() {
    assert_eq!(
        support::conditional_history(&Unconditional(backend())),
        Err("stale CAS overwrote")
    );
}
