//! The example provider is judged by the same suite as the fake and the S3 adapter, and its
//! state survives reopening the directory.
use std::path::PathBuf;

use hsearch_example_provider::DirectoryStore;
use hsearch_object::{
    Budget, Cancellation, Error, Metadata, MetadataStore, Mutation, ObjectBackend, ObjectKey,
    Payload, Precondition, RequestContext, RequestId,
};
use hsearch_testkit::provider::{Capabilities, qualify};

fn dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("hsearch-example-{tag}-{nanos}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn ctx() -> RequestContext {
    RequestContext::new(
        std::time::Duration::from_secs(10),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 100,
        },
    )
    .unwrap()
}

fn block<T>(f: impl std::future::Future<Output = T>) -> T {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    struct W(std::thread::Thread);
    impl Wake for W {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(W(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut f = std::pin::pin!(f);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}

#[test]
fn the_directory_store_passes_the_shared_suite() {
    let root = dir("suite");
    let store = DirectoryStore::open(&root).unwrap();
    let report = qualify(
        "example-directory",
        &store,
        Capabilities {
            remove_version: true,
        },
    );
    assert!(report.passed(), "{:?}", report.failures());
    if let Ok(out) = std::env::var("HSEARCH_CONFORMANCE_DIR") {
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            PathBuf::from(out).join("example-directory.json"),
            report.to_json(),
        )
        .unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn accepted_writes_and_identities_survive_reopening() {
    let root = dir("reopen");
    let request = Mutation {
        request_id: RequestId::new(b"survivor").unwrap(),
        key: ObjectKey::new(b"k").unwrap(),
        payload: Payload::new(b"durable").unwrap(),
        metadata: Metadata::new(b"m").unwrap(),
        precondition: Precondition::Absent,
    };
    let receipt = block(
        DirectoryStore::open(&root)
            .unwrap()
            .create(&ctx(), &request),
    )
    .unwrap();
    let reopened = DirectoryStore::open(&root).unwrap();
    assert_eq!(
        block(reopened.head(&ctx(), &request.key)).unwrap(),
        receipt.object
    );
    assert_eq!(block(reopened.resolve(&ctx(), &request)).unwrap(), receipt);
    assert_eq!(block(reopened.create(&ctx(), &request)).unwrap(), receipt);
    let other = Mutation {
        request_id: RequestId::new(b"someone-else").unwrap(),
        ..request.clone()
    };
    assert_eq!(block(reopened.create(&ctx(), &other)), Err(Error::Conflict));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_key_too_long_for_a_directory_name_is_refused_not_truncated() {
    let root = dir("long");
    let store = DirectoryStore::open(&root).unwrap();
    let key = vec![b'k'; hsearch_example_provider::MAX_KEY + 1];
    let request = Mutation {
        request_id: RequestId::new(b"long").unwrap(),
        key: ObjectKey::new(&key).unwrap(),
        payload: Payload::new(b"x").unwrap(),
        metadata: Metadata::new(b"m").unwrap(),
        precondition: Precondition::Absent,
    };
    assert_eq!(
        block(store.create(&ctx(), &request)),
        Err(Error::InvalidInput)
    );
    std::fs::remove_dir_all(root).unwrap();
}
