//! `ObjectUploadSink`: bounded memory, one request for a small file, create-only, and no visible
//! or billable remains after a failure or a drop. Memory double for the rules, real RustFS for the
//! bytes and the request counts when `HSEARCH_S3_ENDPOINT` is set.
use hsearch_engine::ObjectUploadSink;
use hsearch_object::memory::{MemoryLimits, MemoryStore, MemoryUpload};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error, LargeObjectWriter, MIN_PART, Mutation, ObjectBackend,
    ObjectInfo, ObjectKey, Operation, Payload, RequestContext, VersionToken, WriteReceipt,
};
use lucene_store::OutputSink;
use std::future::Future;
use std::io::ErrorKind;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

struct Unpark(std::thread::Thread);
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn run<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park_timeout(Duration::from_millis(1));
    }
}

fn context() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(30),
        Cancellation::default(),
        Budget {
            request_bytes: 64 * 1024 * 1024,
            response_bytes: 64 * 1024 * 1024,
            attempts: 100,
        },
    )
    .unwrap()
}

fn store() -> Arc<MemoryStore> {
    Arc::new(
        MemoryStore::new(MemoryLimits {
            objects: 32,
            versions: 64,
            version_bytes: 48 * 1024 * 1024,
            identities: 64,
            identity_bytes: 48 * 1024 * 1024,
        })
        .unwrap(),
    )
}

fn key(name: &[u8]) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

fn sink<B: LargeObjectWriter>(store: &Arc<B>, name: &[u8]) -> Box<ObjectUploadSink<B>> {
    Box::new(
        ObjectUploadSink::new(
            Arc::clone(store),
            key(name),
            MIN_PART,
            Duration::from_secs(30),
        )
        .unwrap(),
    )
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

fn read_all(store: &MemoryStore, name: &[u8]) -> Vec<u8> {
    let ctx = context();
    let info = run(store.head(&ctx, &key(name))).unwrap();
    run(store.read_range(
        &ctx,
        &key(name),
        &info.version,
        ByteRange::new(0, info.length).unwrap(),
    ))
    .unwrap()
    .as_bytes()
    .to_vec()
}

#[test]
fn a_file_of_one_part_or_less_is_written_whole() {
    let store = store();
    let data = pattern(1024 * 1024);
    let mut sink = sink(&store, b"small");
    sink.write_all(&data).unwrap();
    sink.finish().unwrap();
    assert_eq!(read_all(&store, b"small"), data);

    // Exactly one part is still one request, not a one-part upload.
    let data = pattern(MIN_PART);
    let mut sink = sink_for(&store, b"exact");
    sink.write_all(&data).unwrap();
    assert_eq!(sink.peak_buffered(), MIN_PART);
    Box::new(sink).finish().unwrap();
    assert_eq!(read_all(&store, b"exact"), data);
}

fn sink_for(store: &Arc<MemoryStore>, name: &[u8]) -> ObjectUploadSink<MemoryStore> {
    ObjectUploadSink::new(
        Arc::clone(store),
        key(name),
        MIN_PART,
        Duration::from_secs(30),
    )
    .unwrap()
}

#[test]
fn a_large_file_streams_in_parts_and_never_holds_more_than_one() {
    let data = pattern(MIN_PART * 2 + 12345);
    // Many small writes, as Lucene produces them.
    let store_a = store();
    let mut a = sink_for(&store_a, b"chunks");
    for chunk in data.chunks(64 * 1024) {
        a.write_all(chunk).unwrap();
    }
    assert_eq!(a.peak_buffered(), MIN_PART, "capacity, not just length");
    Box::new(a).finish().unwrap();
    assert_eq!(read_all(&store_a, b"chunks"), data);

    // One enormous write holds no more than one part either.
    let store_b = store();
    let mut b = sink_for(&store_b, b"once");
    b.write_all(&data).unwrap();
    assert_eq!(b.peak_buffered(), MIN_PART);
    Box::new(b).finish().unwrap();
    assert_eq!(read_all(&store_b, b"once"), data);
}

#[test]
fn a_dropped_sink_leaves_nothing_visible() {
    let store = store();
    let mut dropped = sink(&store, b"dropped");
    dropped.write_all(&pattern(MIN_PART + 10)).unwrap();
    drop(dropped);
    assert_eq!(
        run(store.head(&context(), &key(b"dropped"))),
        Err(Error::NotFound)
    );
}

#[test]
fn an_existing_key_is_never_overwritten() {
    let store = store();
    let first = pattern(1000);
    let mut sink1 = sink(&store, b"taken");
    sink1.write_all(&first).unwrap();
    sink1.finish().unwrap();

    // Small and large both refuse different bytes, and the original stays. (The same bytes
    // again are a replay of the first create and succeed, by the contract's own rule.)
    for len in [1001usize, MIN_PART + 5] {
        let mut again = sink(&store, b"taken");
        again.write_all(&pattern(len)).unwrap();
        let error = again.finish().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::AlreadyExists, "{len}");
        assert_eq!(read_all(&store, b"taken"), first);
    }
}

#[test]
fn an_empty_file_and_a_small_part_size_are_refused() {
    let store = store();
    let empty = sink(&store, b"empty");
    assert_eq!(empty.finish().unwrap_err().kind(), ErrorKind::InvalidInput);
    assert!(ObjectUploadSink::new(store, key(b"x"), MIN_PART - 1, Duration::from_secs(1)).is_err());
}

/// Fails the n-th part and counts aborts.
struct Flaky {
    inner: MemoryStore,
    parts: AtomicU32,
    fail_on: u32,
    aborts: AtomicU32,
}
impl ObjectBackend for Flaky {
    fn head<'a>(&'a self, cx: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.inner.head(cx, k)
    }
    fn create<'a>(
        &'a self,
        cx: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.inner.create(cx, r)
    }
    fn read_range<'a>(
        &'a self,
        cx: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        self.inner.read_range(cx, k, v, r)
    }
}
impl LargeObjectWriter for Flaky {
    type Upload = MemoryUpload;
    fn begin_upload<'a>(
        &'a self,
        cx: &'a RequestContext,
        k: &'a ObjectKey,
    ) -> Operation<'a, MemoryUpload> {
        self.inner.begin_upload(cx, k)
    }
    fn put_part<'a>(
        &'a self,
        cx: &'a RequestContext,
        u: &'a mut MemoryUpload,
        b: &'a [u8],
    ) -> Operation<'a, ()> {
        if self.parts.fetch_add(1, Ordering::Relaxed) + 1 == self.fail_on {
            return Box::pin(async { Err(Error::Unavailable) });
        }
        self.inner.put_part(cx, u, b)
    }
    fn complete_upload<'a>(
        &'a self,
        cx: &'a RequestContext,
        u: &'a MemoryUpload,
    ) -> Operation<'a, (VersionToken, u64)> {
        self.inner.complete_upload(cx, u)
    }
    fn abort_upload<'a>(
        &'a self,
        cx: &'a RequestContext,
        u: &'a MemoryUpload,
    ) -> Operation<'a, ()> {
        self.aborts.fetch_add(1, Ordering::Relaxed);
        self.inner.abort_upload(cx, u)
    }
}

#[test]
fn a_failed_part_aborts_once_latches_the_error_and_leaves_nothing() {
    let flaky = Arc::new(Flaky {
        inner: Arc::try_unwrap(store()).unwrap(),
        parts: AtomicU32::new(0),
        fail_on: 2,
        aborts: AtomicU32::new(0),
    });
    let mut sink = sink(&flaky, b"flaky");
    // Parts 1 and 2 flush when the third part's bytes arrive; the second fails.
    let result = sink.write_all(&pattern(MIN_PART * 2 + 1));
    assert_eq!(result.unwrap_err().kind(), ErrorKind::TimedOut);
    assert_eq!(flaky.aborts.load(Ordering::Relaxed), 1);
    assert!(sink.write_all(b"more").is_err(), "the error is latched");
    drop(sink);
    assert_eq!(
        flaky.aborts.load(Ordering::Relaxed),
        1,
        "an aborted upload is not aborted again"
    );
    assert_eq!(
        run(flaky.head(&context(), &key(b"flaky"))),
        Err(Error::NotFound)
    );
}

#[test]
fn finishing_after_a_failed_part_is_an_error_and_publishes_nothing() {
    let flaky = Arc::new(Flaky {
        inner: Arc::try_unwrap(store()).unwrap(),
        parts: AtomicU32::new(0),
        fail_on: 2,
        aborts: AtomicU32::new(0),
    });
    let mut sink = sink(&flaky, b"truncated");
    assert!(sink.write_all(&pattern(MIN_PART * 2 + 1)).is_err());
    // A caller that ignores the error must not turn the leftover buffer into an object.
    assert!(sink.finish().is_err());
    assert_eq!(
        run(flaky.head(&context(), &key(b"truncated"))),
        Err(Error::NotFound)
    );
}

#[test]
fn an_unknown_completion_is_not_aborted() {
    struct Unknown(Flaky);
    impl ObjectBackend for Unknown {
        fn head<'a>(
            &'a self,
            c: &'a RequestContext,
            k: &'a ObjectKey,
        ) -> Operation<'a, ObjectInfo> {
            self.0.head(c, k)
        }
        fn create<'a>(
            &'a self,
            c: &'a RequestContext,
            r: &'a Mutation,
        ) -> Operation<'a, WriteReceipt> {
            self.0.create(c, r)
        }
        fn read_range<'a>(
            &'a self,
            c: &'a RequestContext,
            k: &'a ObjectKey,
            v: &'a VersionToken,
            r: ByteRange,
        ) -> Operation<'a, Payload> {
            self.0.read_range(c, k, v, r)
        }
    }
    impl LargeObjectWriter for Unknown {
        type Upload = MemoryUpload;
        fn begin_upload<'a>(
            &'a self,
            c: &'a RequestContext,
            k: &'a ObjectKey,
        ) -> Operation<'a, MemoryUpload> {
            self.0.begin_upload(c, k)
        }
        fn put_part<'a>(
            &'a self,
            c: &'a RequestContext,
            u: &'a mut MemoryUpload,
            b: &'a [u8],
        ) -> Operation<'a, ()> {
            self.0.put_part(c, u, b)
        }
        fn complete_upload<'a>(
            &'a self,
            _c: &'a RequestContext,
            _u: &'a MemoryUpload,
        ) -> Operation<'a, (VersionToken, u64)> {
            Box::pin(async { Err(Error::UnknownOutcome) })
        }
        fn abort_upload<'a>(
            &'a self,
            c: &'a RequestContext,
            u: &'a MemoryUpload,
        ) -> Operation<'a, ()> {
            self.0.abort_upload(c, u)
        }
    }
    let unknown = Arc::new(Unknown(Flaky {
        inner: Arc::try_unwrap(store()).unwrap(),
        parts: AtomicU32::new(0),
        fail_on: 0,
        aborts: AtomicU32::new(0),
    }));
    let mut sink = sink(&unknown, b"maybe");
    sink.write_all(&pattern(MIN_PART + 1)).unwrap();
    assert!(sink.finish().is_err());
    assert_eq!(
        unknown.0.aborts.load(Ordering::Relaxed),
        0,
        "the object may exist; an abort could race its completion"
    );
}

#[test]
fn a_failed_completion_aborts_the_upload() {
    let flaky = Arc::new(Flaky {
        inner: Arc::try_unwrap(store()).unwrap(),
        parts: AtomicU32::new(0),
        fail_on: 3, // the last part, sent by finish
        aborts: AtomicU32::new(0),
    });
    let mut sink = sink(&flaky, b"late");
    sink.write_all(&pattern(MIN_PART * 2 + 7)).unwrap();
    assert!(sink.finish().is_err());
    assert_eq!(flaky.aborts.load(Ordering::Relaxed), 1);
    assert_eq!(
        run(flaky.head(&context(), &key(b"late"))),
        Err(Error::NotFound)
    );
}

/// A fresh versioned bucket on the configured S3-compatible store, or `None` when none is set.
fn s3() -> Option<Arc<hsearch_s3::S3Store>> {
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
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let store = hsearch_s3::S3Store::new(hsearch_s3::S3Config {
        endpoint,
        bucket: format!("hu-{nanos}"),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .unwrap();
    store.prepare_bucket(&context()).expect("prepare bucket");
    Some(Arc::new(store))
}

#[test]
fn a_forty_mib_file_reaches_real_storage_through_one_part_of_memory() {
    use hsearch_s3::Verb;
    let Some(store) = s3() else { return };
    let setup_puts = store.ledger().attempts_by_verb(Verb::Put);
    let setup_posts = store.ledger().attempts_by_verb(Verb::Post);
    let data = pattern(40 * 1024 * 1024 + 1000);
    let mut sink = ObjectUploadSink::new(
        Arc::clone(&store),
        key(b"forty"),
        MIN_PART,
        Duration::from_secs(60),
    )
    .unwrap();
    for chunk in data.chunks(256 * 1024) {
        sink.write_all(chunk).unwrap();
    }
    assert_eq!(
        sink.peak_buffered(),
        MIN_PART,
        "memory is one part, not 40 MiB"
    );
    Box::new(sink).finish().unwrap();

    // 8 full parts flushed as more bytes arrived, the 1000-byte tail sent by finish: 9 parts,
    // begin and complete: 2 control requests. Each is attributed by the store's own ledger.
    let ledger = store.ledger();
    assert_eq!(ledger.attempts_by_verb(Verb::Put) - setup_puts, 9);
    assert_eq!(ledger.attempts_by_verb(Verb::Post) - setup_posts, 2);
    assert_eq!(ledger.retries(), 0);

    let ctx = context();
    let info = run(store.head(&ctx, &key(b"forty"))).unwrap();
    assert_eq!(info.length, data.len() as u64);
    // Bytes either side of every part boundary, and the end.
    let mut offsets: Vec<u64> = (1..9).map(|n| (n * MIN_PART) as u64 - 8).collect();
    offsets.push(data.len() as u64 - 16);
    for offset in offsets {
        let got = run(store.read_range(
            &ctx,
            &key(b"forty"),
            &info.version,
            ByteRange::new(offset, 16).unwrap(),
        ))
        .unwrap();
        assert_eq!(got.as_bytes(), &data[offset as usize..offset as usize + 16]);
    }
}

#[test]
fn a_dropped_sink_aborts_its_open_upload_exactly_once() {
    let flaky = Arc::new(Flaky {
        inner: Arc::try_unwrap(store()).unwrap(),
        parts: AtomicU32::new(0),
        fail_on: 0,
        aborts: AtomicU32::new(0),
    });
    // No part flushed yet: nothing was begun, so nothing to abort.
    let mut small = sink(&flaky, b"small");
    small.write_all(b"tiny").unwrap();
    drop(small);
    assert_eq!(flaky.aborts.load(Ordering::Relaxed), 0);
    // A part was flushed: an upload is open, and dropping the sink aborts it.
    let mut big = sink(&flaky, b"big");
    big.write_all(&pattern(MIN_PART + 1)).unwrap();
    drop(big);
    assert_eq!(flaky.aborts.load(Ordering::Relaxed), 1);
    // A finished sink has nothing left to abort.
    let mut done = sink(&flaky, b"done");
    done.write_all(&pattern(MIN_PART + 1)).unwrap();
    done.finish().unwrap();
    assert_eq!(flaky.aborts.load(Ordering::Relaxed), 1);
}

#[test]
fn on_real_storage_an_existing_key_is_refused_small_or_large_and_the_same_small_file_replays() {
    let Some(store) = s3() else { return };
    let write = |bytes: &[u8]| {
        let mut sink = Box::new(
            ObjectUploadSink::new(
                Arc::clone(&store),
                key(b"occupied"),
                MIN_PART,
                Duration::from_secs(60),
            )
            .unwrap(),
        );
        sink.write_all(bytes).unwrap();
        sink.finish()
    };
    let first = pattern(4096);
    write(&first).unwrap();
    // The same bytes again are the first write replayed.
    write(&first).unwrap();
    // Different bytes, small or multipart, never replace it.
    assert_eq!(
        write(&pattern(4097)).unwrap_err().kind(),
        ErrorKind::AlreadyExists
    );
    assert_eq!(
        write(&pattern(MIN_PART + 3)).unwrap_err().kind(),
        ErrorKind::AlreadyExists
    );
    let ctx = context();
    let info = run(store.head(&ctx, &key(b"occupied"))).unwrap();
    assert_eq!(info.length, first.len() as u64);
}
