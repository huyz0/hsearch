//! Sealed versus plain object reads (M05.04): `cargo run --release -p hsearch-seal --example seal_bench`.
//! In-memory store and in-memory test KMS, so this isolates CPU and request-count cost, not network.
//! Provisional and relative: shared 4-vCPU VM, x86_64.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error, Metadata, Mutation, ObjectBackend, ObjectKey, Payload,
    Precondition, RequestContext, RequestId,
};
use hsearch_seal::{Scope, ScopeResolver, SealConfig, SealedBackend};
use hsearch_security::audit::MemoryAuditSink;
use hsearch_security::clock::SystemClock;
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{KeyRef, Profile};
use hsearch_security::manager::{CachePolicy, KeyAllowlist, KeyManager};
use hsearch_security::test_kms::TestKms;

struct Notify(std::thread::Thread);
impl Wake for Notify {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<T>(f: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(Notify(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

fn ctx() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 30,
            response_bytes: 1 << 30,
            attempts: u32::MAX,
        },
    )
    .unwrap()
}

fn memory() -> MemoryStore {
    MemoryStore::new(MemoryLimits {
        objects: 4096,
        versions: 16384,
        version_bytes: 64 << 20,
        identities: 16384,
        identity_bytes: 64 << 20,
    })
    .unwrap()
}

struct One;
impl ScopeResolver for One {
    fn scope(&self, _key: &ObjectKey) -> Result<Scope, Error> {
        Ok(Scope {
            index: IndexId::new("bench").unwrap(),
            domain: [1; 16],
            key: None,
            generation: 1,
        })
    }
}

fn put<B: ObjectBackend>(b: &B, name: &str, body: &[u8]) {
    let m = Mutation {
        request_id: RequestId::new(name.as_bytes()).unwrap(),
        key: ObjectKey::new(name.as_bytes()).unwrap(),
        payload: Payload::new(body).unwrap(),
        metadata: Metadata::new(b"").unwrap(),
        precondition: Precondition::Absent,
    };
    block_on(b.create(&ctx(), &m)).unwrap();
}

fn per_read<B: ObjectBackend>(b: &B, name: &str, len: u64, n: u64, span: u64) -> f64 {
    let key = ObjectKey::new(name.as_bytes()).unwrap();
    let info = block_on(b.head(&ctx(), &key)).unwrap();
    let start = Instant::now();
    for i in 0..n {
        let offset = (i * 7919 * 4096) % (info.length - span);
        std::hint::black_box(
            block_on(b.read_range(
                &ctx(),
                &key,
                &info.version,
                ByteRange::new(offset, len).unwrap(),
            ))
            .unwrap(),
        );
    }
    start.elapsed().as_secs_f64() / n as f64 * 1e6
}

fn main() {
    let kms = Arc::new(TestKms::new());
    let kref = KeyRef::new("test/acct/bench").unwrap();
    kms.create_key(&kref).unwrap();
    let keys = Arc::new(
        KeyManager::new(
            Profile::Development,
            kms.clone(),
            KeyAllowlist::new(vec![("test".into(), "acct".into())]),
            kref,
            CachePolicy::default(),
            Arc::new(MemoryAuditSink::new(16)),
            Arc::new(SystemClock::default()),
        )
        .unwrap(),
    );
    let body = vec![0x5Au8; 8 << 20];
    println!("| Block size | Read | Plain (us) | Sealed (us) | Sealed / plain |");
    println!("|---|---|---|---|---|");
    for block in [4u32 << 10, 64 << 10] {
        let plain = memory();
        put(&plain, "o", &body);
        let sealed = SealedBackend::new(
            memory(),
            keys.clone(),
            Arc::new(One),
            SealConfig {
                block_size: block,
                retry_entries: 4,
                retry_bytes: 32 << 20,
                open_entries: 64,
            },
        )
        .unwrap();
        put(&sealed, "o", &body);
        for (label, len) in [("4 KiB", 4096u64), ("256 KiB", 256 << 10)] {
            let p = per_read(&plain, "o", len, 2000, len);
            let s = per_read(&sealed, "o", len, 2000, len);
            println!(
                "| {} KiB | {label} | {p:.1} | {s:.1} | {:.2}x |",
                block >> 10,
                s / p
            );
        }
    }
    println!(
        "\nProvider calls: wrap {}, unwrap {}",
        kms.wrap_calls(),
        kms.unwrap_calls()
    );
}
