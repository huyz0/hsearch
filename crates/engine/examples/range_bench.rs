//! M02.03 benchmark: the cost `ObjectBlobStore` adds to one range request, with
//! the network removed (an in-memory backend), at the sizes the page cache sends.
//! Run `cargo run --release -p hsearch-engine --example range_bench`.
use hsearch_engine::{ManifestEntry, ObjectBlobStore};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Metadata, Mutation, ObjectBackend, ObjectKey, Payload,
    Precondition, RequestContext, RequestId,
};
use lucene_store::BlobStore;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

struct W(std::thread::Thread);
impl Wake for W {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn block_on<T>(f: impl Future<Output = T>) -> T {
    let mut f = std::pin::pin!(f);
    let w = Waker::from(Arc::new(W(std::thread::current())));
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut Context::from_waker(&w)) {
            return v;
        }
        std::thread::park();
    }
}

fn main() {
    const FILE: usize = 8 << 20;
    let data: Vec<u8> = (0..FILE)
        .map(|i| (i.wrapping_mul(2654435761) >> 7) as u8)
        .collect();
    let backend = MemoryStore::new(MemoryLimits {
        objects: 8,
        versions: 8,
        version_bytes: 64 << 20,
        identities: 8,
        identity_bytes: 64 << 20,
    })
    .unwrap();
    let ctx = RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: u64::MAX / 4,
            response_bytes: u64::MAX / 4,
            attempts: u32::MAX,
        },
    )
    .unwrap();
    let key = ObjectKey::new(b"f").unwrap();
    let receipt = block_on(backend.create(
        &ctx,
        &Mutation {
            request_id: RequestId::new(b"r").unwrap(),
            key: key.clone(),
            payload: Payload::new(&data).unwrap(),
            metadata: Metadata::new(&[]).unwrap(),
            precondition: Precondition::Absent,
        },
    ))
    .unwrap();
    let version = receipt.object.version;
    let entry = ManifestEntry {
        key: key.clone(),
        version: version.clone(),
        length: FILE as u64,
    };
    let store =
        ObjectBlobStore::new(backend, [("f".to_owned(), entry)], Duration::from_secs(30)).unwrap();
    println!("in-memory backend, {} MiB object (no network)", FILE >> 20);
    for size in [4usize << 10, 64 << 10, 256 << 10, 1 << 20] {
        let n = 20_000usize.min((1usize << 30) / size);
        let at = |i: usize| ((i * 7919 * size) % (FILE - size)) as u64;
        let t = Instant::now();
        for i in 0..n {
            let v = store.get_range("f", at(i), size).unwrap();
            std::hint::black_box(&v);
        }
        let wrapped = t.elapsed();
        // The same reads straight on the backend: the floor the wrapper sits on.
        let t = Instant::now();
        for i in 0..n {
            let range = ByteRange::new(at(i), size as u64).unwrap();
            let read = store.backend().read_range(&ctx, &key, &version, range);
            std::hint::black_box(&block_on(read).unwrap());
        }
        let direct = t.elapsed();
        println!(
            "{size:>8} B request: via BlobStore {:>8.0} ns ({:.1} GB/s), backend alone {:>8.0} ns, wrapper adds {:>6.0} ns",
            wrapped.as_nanos() as f64 / n as f64,
            n as f64 * size as f64 / wrapped.as_secs_f64() / 1e9,
            direct.as_nanos() as f64 / n as f64,
            (wrapped.as_nanos() as f64 - direct.as_nanos() as f64) / n as f64
        );
    }
}
