//! The same equality, with the segments stored in a real S3-compatible store.
//! Needs `HSEARCH_S3_ENDPOINT` and credentials (see `scripts/rustfs.sh`); without
//! them it prints SKIPPED unless `HSEARCH_S3_REQUIRED=1`.
#[path = "support/remote_fixture.rs"]
mod fixture;

use fixture::{Scratch, answers, schema, write_documents};
use hsearch_engine::{CachedDirectory, EngineDirectory, RemoteError, RemoteFiles, Snapshot};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Metadata, Mutation, ObjectBackend, ObjectKey, Payload,
    Precondition, RequestContext, RequestId,
};
use hsearch_s3::{S3Config, S3Store};
use lucene_store::FsDirectory;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

struct Unpark(std::thread::Thread);
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn block<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}
fn ctx() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 28,
            response_bytes: 1 << 28,
            attempts: 10_000,
        },
    )
    .unwrap()
}

struct S3Files {
    store: S3Store,
}
impl S3Files {
    fn key(name: &str) -> ObjectKey {
        ObjectKey::new(format!("segments/{name}").as_bytes()).unwrap()
    }
    fn upload(&self, name: &str, bytes: &[u8]) {
        let request = Mutation {
            request_id: RequestId::new(format!("upload-{name}").as_bytes()).unwrap(),
            key: Self::key(name),
            payload: Payload::new(bytes).unwrap(),
            metadata: Metadata::new(b"").unwrap(),
            precondition: Precondition::Absent,
        };
        block(self.store.create(&ctx(), &request)).unwrap();
    }
}
impl RemoteFiles for &S3Files {
    fn fetch(&self, name: &str) -> Result<Vec<u8>, RemoteError> {
        let key = S3Files::key(name);
        let info = block(self.store.head(&ctx(), &key)).map_err(|e| match e {
            hsearch_object::Error::NotFound => RemoteError::NotFound,
            _ => RemoteError::Unavailable,
        })?;
        let mut out = Vec::new();
        while (out.len() as u64) < info.length {
            let length = (info.length - out.len() as u64).min(1 << 20);
            let range = ByteRange::new(out.len() as u64, length).unwrap();
            let part = block(self.store.read_range(&ctx(), &key, &info.version, range))
                .map_err(|_| RemoteError::Unavailable)?;
            out.extend_from_slice(part.as_bytes());
        }
        Ok(out)
    }
}

#[test]
fn segments_in_a_real_store_answer_like_a_local_directory() {
    let Ok(endpoint) = std::env::var("HSEARCH_S3_ENDPOINT") else {
        assert!(
            std::env::var("HSEARCH_S3_REQUIRED").as_deref() != Ok("1"),
            "store required"
        );
        eprintln!(
            "SKIPPED: HSEARCH_S3_ENDPOINT is not set; nothing was checked against a real store"
        );
        return;
    };
    let store = S3Store::new(S3Config {
        endpoint,
        bucket: format!("hs-engine-{}", std::process::id()),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").unwrap(),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").unwrap(),
        region: "us-east-1".into(),
    })
    .unwrap();
    store.prepare_bucket(&ctx()).unwrap();
    let files = S3Files { store };

    let (written, documents) = write_documents();
    let names: Vec<String> = documents.iter().map(|(name, _)| name.clone()).collect();
    for (name, bytes) in &documents {
        files.upload(name, bytes);
    }
    let local = EngineDirectory::new(FsDirectory::open(&written.0));
    let expected = answers(&Snapshot::open_committed(&local, schema()).unwrap());
    assert_eq!(expected[0].0, 40);

    let cache = Scratch::new("rustfs-cache");
    let remote =
        EngineDirectory::new(CachedDirectory::new(&cache.0, &files, names, 64 << 20).unwrap());
    let before = files.store.counts();
    assert_eq!(
        answers(&Snapshot::open_committed(&remote, schema()).unwrap()),
        expected
    );
    remote.inner().evict_all().unwrap();
    assert_eq!(
        answers(&Snapshot::open_committed(&remote, schema()).unwrap()),
        expected
    );
    let after = files.store.counts();
    assert_eq!(after.list, before.list, "no LIST while opening segments");
    assert!(after.get > before.get && after.head > before.head);
    eprintln!(
        "engine open over RustFS (provisional): {} fetches, {} GET, {} HEAD",
        remote.inner().fetches(),
        after.get - before.get,
        after.head - before.head
    );
}
