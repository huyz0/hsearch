//! Reusable public-trait conformance probes. No private memory map is inspected.
//! Include this module from a provider's integration tests and pass a fresh,
//! empty factory. Fault-specific tests use a separate explicit control seam.
#![allow(dead_code)]

use hsearch_object::{
    Budget, ByteRange, Cancellation, Error, Metadata, MetadataStore, Mutation, ObjectKey, Payload,
    Precondition, RequestContext, RequestId,
};
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

struct Notify(std::thread::Thread);
impl Wake for Notify {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

pub fn run<T: Future>(future: T) -> T::Output {
    let waker = Waker::from(Arc::new(Notify(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => {
                assert!(
                    Instant::now() < deadline,
                    "bounded conformance future timed out"
                );
                std::thread::park_timeout(Duration::from_millis(1));
            }
        }
    }
}

pub fn context() -> RequestContext {
    limited(4 * 1024 * 1024, 4 * 1024 * 1024, 100)
}

pub fn limited(request_bytes: u64, response_bytes: u64, attempts: u32) -> RequestContext {
    RequestContext::new(
        Duration::from_secs(30),
        Cancellation::default(),
        Budget {
            request_bytes,
            response_bytes,
            attempts,
        },
    )
    .unwrap()
}

pub fn mutation(id: &[u8], key: &[u8], payload: &[u8], precondition: Precondition) -> Mutation {
    Mutation {
        request_id: RequestId::new(id).unwrap(),
        key: ObjectKey::new(key).unwrap(),
        payload: Payload::new(payload).unwrap(),
        metadata: Metadata::new(b"opaque-metadata").unwrap(),
        precondition,
    }
}

/// Return a failure rather than panic so deliberately broken providers can be
/// rejected by exactly the same predicate as the healthy reference backend.
pub fn conditional_history<B: MetadataStore>(backend: &B) -> Result<(), &'static str> {
    let ctx = context();
    let create = mutation(b"first", b"object", b"abcdef", Precondition::Absent);
    let original = run(backend.create(&ctx, &create)).map_err(|_| "create failed")?;
    if run(backend.create(&ctx, &create)) != Ok(original.clone()) {
        return Err("retry changed receipt");
    }
    let other = mutation(b"duplicate", b"object", b"other", Precondition::Absent);
    if run(backend.create(&ctx, &other)) != Err(Error::Conflict) {
        return Err("create overwrote");
    }
    let update = mutation(
        b"second",
        b"object",
        b"new-data",
        Precondition::Matches(original.object.version.clone()),
    );
    let updated = run(backend.compare_exchange(&ctx, &update)).map_err(|_| "CAS failed")?;
    if updated.object.version == original.object.version {
        return Err("version did not change");
    }
    let stale = mutation(
        b"stale",
        b"object",
        b"wrong",
        Precondition::Matches(original.object.version.clone()),
    );
    if run(backend.compare_exchange(&ctx, &stale)) != Err(Error::Conflict) {
        return Err("stale CAS overwrote");
    }
    if run(backend.head(&ctx, &create.key)) != Ok(updated.object.clone()) {
        return Err("failed CAS changed state");
    }
    if run(backend.read_range(
        &ctx,
        &create.key,
        &original.object.version,
        ByteRange::new(1, 3).unwrap(),
    ))
    .map(|value| value.as_bytes().to_vec())
        != Ok(b"bcd".to_vec())
    {
        return Err("range mixed versions");
    }
    if run(backend.read_range(
        &ctx,
        &create.key,
        &updated.object.version,
        ByteRange::new(0, 8).unwrap(),
    ))
    .map(|value| value.as_bytes().to_vec())
        != Ok(b"new-data".to_vec())
    {
        return Err("current range differs");
    }
    if run(backend.resolve(&ctx, &create)) != Ok(original) {
        return Err("old identity lost");
    }
    if updated.object.metadata != update.metadata || updated.request_id != update.request_id {
        return Err("receipt lost identity/metadata");
    }
    Ok(())
}

/// A conditional update from a stale version is never accepted. The first update rewrites the
/// same bytes: a backend may refuse it (`InvalidInput` or `Conflict`) or accept it as a new,
/// distinct version, but if the object advanced, a second writer holding the old version must
/// then be refused, with identical bytes and with different bytes. A store that fences by
/// content hash accepts the second writer, because the new version has the old version's hash.
pub fn stale_update_history<B: MetadataStore>(backend: &B) -> Result<(), &'static str> {
    let ctx = context();
    let create = mutation(b"fence-create", b"fenced", b"AAAA", Precondition::Absent);
    let v1 = run(backend.create(&ctx, &create))
        .map_err(|_| "create failed")?
        .object
        .version;
    let same = mutation(
        b"fence-same",
        b"fenced",
        b"AAAA",
        Precondition::Matches(v1.clone()),
    );
    match run(backend.compare_exchange(&ctx, &same)) {
        Ok(receipt) if receipt.object.version == v1 => {
            return Err("identical update did not change the version");
        }
        Ok(_) | Err(Error::InvalidInput | Error::Conflict) => {}
        Err(_) => return Err("identical update failed unexpectedly"),
    }
    let advanced = run(backend.head(&ctx, &create.key))
        .map_err(|_| "head failed")?
        .version
        != v1;
    for (id, bytes) in [
        (&b"fence-stale-same"[..], &b"AAAA"[..]),
        (b"fence-stale-diff", b"CCCC"),
    ] {
        let stale = mutation(id, b"fenced", bytes, Precondition::Matches(v1.clone()));
        if advanced && run(backend.compare_exchange(&ctx, &stale)).is_ok() {
            return Err("stale update accepted after the object advanced");
        }
        // A backend that refused the identical update never advanced, so the old version is
        // still current and a different-bytes update from it is legitimate; identical bytes
        // must still not be accepted as a second version.
        if !advanced && bytes == b"AAAA" && run(backend.compare_exchange(&ctx, &stale)).is_ok() {
            return Err("identical update accepted");
        }
    }
    Ok(())
}
