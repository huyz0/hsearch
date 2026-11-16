//! M04.02 (condition C3): what `ObjectBlobStore::get_range` costs on a real S3
//! store (RustFS), against reading the same range straight from the backend, one
//! thread and many. `get_range` blocks its calling thread and copies the payload
//! into the `Vec` the `BlobStore` trait returns; this measures both.
//!
//!   HSEARCH_S3_ENDPOINT=127.0.0.1:9000 HSEARCH_S3_ACCESS_KEY=.. HSEARCH_S3_SECRET_KEY=.. \
//!     cargo run --release -p hsearch-engine --example s3_range_bench
//!
//! Numbers are provisional: one host, RustFS on loopback, no provider network.
use hsearch_engine::{ManifestEntry, ObjectBlobStore};
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

fn ctx(response: u64) -> RequestContext {
    RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 20,
            response_bytes: response,
            attempts: 8,
        },
    )
    .unwrap()
}

fn pct(v: &mut [Duration], p: f64) -> Duration {
    v.sort();
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

fn main() {
    let open = || {
        hsearch_s3::S3Store::new(hsearch_s3::S3Config {
            endpoint: std::env::var("HSEARCH_S3_ENDPOINT").expect("HSEARCH_S3_ENDPOINT"),
            bucket: format!("hs-rangebench-{}", std::process::id()),
            access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").unwrap(),
            secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").unwrap(),
            region: "us-east-1".into(),
        })
        .unwrap()
    };
    let s3 = open();
    s3.prepare_bucket(&ctx(1 << 20)).unwrap();
    let size = 8usize << 20;
    let bytes: Vec<u8> = (0..size).map(|i| (i * 131 % 251) as u8).collect();
    let key = ObjectKey::new(b"file.bin").unwrap();
    let write = RequestContext::new(
        Duration::from_secs(120),
        Cancellation::default(),
        Budget {
            request_bytes: u64::MAX / 4,
            response_bytes: 1 << 20,
            attempts: 8,
        },
    )
    .unwrap();
    let receipt = block_on(s3.create(
        &write,
        &Mutation {
            request_id: RequestId::new(b"up").unwrap(),
            key: key.clone(),
            payload: Payload::new(&bytes).unwrap(),
            metadata: Metadata::new(&[]).unwrap(),
            precondition: Precondition::Absent,
        },
    ))
    .unwrap();
    let version = receipt.object.version;
    let store = Arc::new(
        ObjectBlobStore::new(
            open(),
            [(
                "file.bin".to_string(),
                ManifestEntry {
                    key: key.clone(),
                    version: version.clone(),
                    length: size as u64,
                },
            )],
            Duration::from_secs(60),
        )
        .unwrap(),
    );
    println!("RustFS, {} MiB object, provisional", size >> 20);
    for len in [256usize << 10, 1 << 20, 4 << 20] {
        for threads in [1usize, 8, 32] {
            let per_thread = (256usize << 20).div_ceil(len * threads).max(8);
            let run = |wrapped: bool| {
                let t = Instant::now();
                let lat: Vec<Vec<Duration>> = std::thread::scope(|sc| {
                    (0..threads)
                        .map(|th| {
                            let (store, s3, key, version) = (&store, &s3, &key, &version);
                            sc.spawn(move || {
                                let mut lats = Vec::with_capacity(per_thread);
                                for i in 0..per_thread {
                                    let off = (((th * per_thread + i) * len) % (size - len)) as u64;
                                    let t0 = Instant::now();
                                    if wrapped {
                                        let v = store.get_range("file.bin", off, len).unwrap();
                                        assert_eq!(v.len(), len);
                                    } else {
                                        let c = ctx(len as u64);
                                        let p = block_on(s3.read_range(
                                            &c,
                                            key,
                                            version,
                                            ByteRange::new(off, len as u64).unwrap(),
                                        ))
                                        .unwrap();
                                        assert_eq!(p.len(), len);
                                    }
                                    lats.push(t0.elapsed());
                                }
                                lats
                            })
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|h| h.join().unwrap())
                        .collect()
                });
                let wall = t.elapsed();
                let mut all: Vec<Duration> = lat.into_iter().flatten().collect();
                let n = all.len();
                (
                    n as f64 * len as f64 / wall.as_secs_f64() / 1048576.0,
                    pct(&mut all, 0.5),
                    pct(&mut all, 0.99),
                )
            };
            let (dm, d50, d99) = run(false);
            let (wm, w50, w99) = run(true);
            println!(
                "{:>4} KiB x {:>2} threads: backend {:>7.0} MiB/s p50 {:>8.2?} p99 {:>8.2?} | get_range {:>7.0} MiB/s p50 {:>8.2?} p99 {:>8.2?}",
                len >> 10,
                threads,
                dm,
                d50,
                d99,
                wm,
                w50,
                w99
            );
        }
    }
}
