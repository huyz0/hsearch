//! Reusable public-trait conformance probes. No private memory map is inspected.
//! Include this module from a provider's integration tests and pass a fresh,
//! empty factory. Fault-specific tests use a separate explicit control seam.
#![allow(dead_code)]

use hsearch_object::{
    Budget, ByteRange, Cancellation, Error, Metadata, MetadataStore, Mutation, ObjectBackend,
    ObjectKey, Payload, Precondition, RequestContext, RequestId,
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
