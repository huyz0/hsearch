//! M05.04 sealed object backend over the in-memory store and the isolated test KMS: what is on the
//! store is ciphertext, ranges decrypt correctly, a copied or damaged object is refused, retries resend
//! identical bytes, and the KMS is called once per object, not per read.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error, Metadata, Mutation, ObjectBackend, ObjectInfo,
    ObjectKey, Operation, Payload, Precondition, RequestContext, RequestId, VersionToken,
    WriteReceipt,
};
use hsearch_seal::{Scope, ScopeResolver, SealConfig, SealedBackend};
use hsearch_security::audit::MemoryAuditSink;
use hsearch_security::clock::ManualClock;
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
        Duration::from_secs(30),
        Cancellation::default(),
        Budget {
            request_bytes: 64 << 20,
            response_bytes: 64 << 20,
            attempts: 100_000,
        },
    )
    .unwrap()
}

fn store() -> MemoryStore {
    MemoryStore::new(MemoryLimits {
        objects: 4096,
        versions: 16384,
        version_bytes: 64 << 20,
        identities: 16384,
        identity_bytes: 64 << 20,
    })
    .unwrap()
}

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(name.as_bytes()).unwrap()
}

fn kref(name: &str) -> KeyRef {
    KeyRef::new(&format!("test/acct/{name}")).unwrap()
}

/// Keys under `orders/` belong to index `orders`, under `invoices/` to `invoices` with its own key.
struct Scopes;
impl ScopeResolver for Scopes {
    fn scope(&self, key: &ObjectKey) -> Result<Scope, Error> {
        let text = std::str::from_utf8(key.as_bytes()).map_err(|_| Error::InvalidInput)?;
        let (index, own) = if text.starts_with("orders/") {
            ("orders", None)
        } else if text.starts_with("invoices/") {
            ("invoices", Some(kref("invoices")))
        } else {
            return Err(Error::PermissionDenied);
        };
        Ok(Scope {
            index: IndexId::new(index).unwrap(),
            domain: [7; 16],
            key: own,
            generation: 1,
        })
    }
}

/// The backend the decorator wraps: a memory store that counts reads and can damage them.
struct Tap {
    inner: MemoryStore,
    reads: AtomicUsize,
    flip: Mutex<Option<usize>>,
    short: AtomicBool,
}

impl ObjectBackend for Tap {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        Box::pin(async move {
            let mut info = self.inner.head(c, k).await?;
            if self.short.load(Ordering::SeqCst) {
                info.length -= 1;
            }
            Ok(info)
        })
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        self.inner.create(c, r)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let got = self.inner.read_range(c, k, v, r).await?;
            if let Some(at) = *self.flip.lock().unwrap() {
                let start = r.offset() as usize;
                if at >= start && at < start + got.as_bytes().len() {
                    let mut bytes = got.as_bytes().to_vec();
                    bytes[at - start] ^= 0x40;
                    return Payload::new(&bytes);
                }
            }
            Ok(got)
        })
    }
}

/// A resolver the test can switch to refusing everything.
struct Switch(Arc<AtomicBool>);
impl ScopeResolver for Switch {
    fn scope(&self, key: &ObjectKey) -> Result<Scope, Error> {
        if self.0.load(Ordering::SeqCst) {
            return Err(Error::PermissionDenied);
        }
        Scopes.scope(key)
    }
}

struct Rig {
    kms: Arc<TestKms>,
    keys: Arc<KeyManager>,
    tap: Arc<Tap>,
    sealed: SealedBackend<Shared>,
}

#[derive(Clone)]
struct Shared(Arc<Tap>);

impl ObjectBackend for Shared {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        (*self.0).head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        (*self.0).create(c, r)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        (*self.0).read_range(c, k, v, r)
    }
}

fn rig_with(config: SealConfig) -> Rig {
    let kms = Arc::new(TestKms::new());
    kms.create_key(&kref("default")).unwrap();
    kms.create_key(&kref("invoices")).unwrap();
    let keys = Arc::new(
        KeyManager::new(
            Profile::Development,
            kms.clone(),
            KeyAllowlist::new(vec![("test".into(), "acct".into())]),
            kref("default"),
            CachePolicy::default(),
            Arc::new(MemoryAuditSink::new(10_000)),
            Arc::new(ManualClock::new()),
        )
        .unwrap(),
    );
    let tap = Arc::new(Tap {
        inner: store(),
        reads: AtomicUsize::new(0),
        flip: Mutex::new(None),
        short: AtomicBool::new(false),
    });
    let sealed =
        SealedBackend::new(Shared(tap.clone()), keys.clone(), Arc::new(Scopes), config).unwrap();
    Rig {
        kms,
        keys,
        tap,
        sealed,
    }
}

fn config() -> SealConfig {
    SealConfig {
        block_size: 64,
        retry_entries: 16,
        retry_bytes: 1 << 20,
        open_entries: 16,
    }
}

fn rig() -> Rig {
    rig_with(config())
}

fn plain(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 13 + 5) as u8).collect()
}

fn put(rig: &Rig, id: &str, name: &str, body: &[u8]) -> WriteReceipt {
    block_on(rig.sealed.create(&ctx(), &mutation(id, name, body))).unwrap()
}

fn mutation(id: &str, name: &str, body: &[u8]) -> Mutation {
    Mutation {
        request_id: RequestId::new(id.as_bytes()).unwrap(),
        key: key(name),
        payload: Payload::new(body).unwrap(),
        metadata: Metadata::new(b"meta").unwrap(),
        precondition: Precondition::Absent,
    }
}

fn read(
    rig: &Rig,
    name: &str,
    version: &VersionToken,
    offset: u64,
    len: u64,
) -> Result<Vec<u8>, Error> {
    block_on(rig.sealed.read_range(
        &ctx(),
        &key(name),
        version,
        ByteRange::new(offset, len).unwrap(),
    ))
    .map(|p| p.as_bytes().to_vec())
}

/// The raw bytes the store holds for an object.
fn stored(rig: &Rig, name: &str) -> Vec<u8> {
    let info = block_on(rig.tap.inner.head(&ctx(), &key(name))).unwrap();
    block_on(rig.tap.inner.read_range(
        &ctx(),
        &key(name),
        &info.version,
        ByteRange::new(0, info.length).unwrap(),
    ))
    .unwrap()
    .as_bytes()
    .to_vec()
}

#[test]
fn what_the_store_holds_is_ciphertext_and_head_reports_the_plaintext_length() {
    let r = rig();
    let body = b"SECRET-MARKER-SECRET-MARKER-SECRET-MARKER".repeat(20);
    let receipt = put(&r, "r1", "orders/a", &body);
    let raw = stored(&r, "orders/a");
    assert_eq!(&raw[..4], b"HSEB");
    assert!(!raw.windows(13).any(|w| w == b"SECRET-MARKER"));
    assert!(raw.len() > body.len());
    let info = block_on(r.sealed.head(&ctx(), &key("orders/a"))).unwrap();
    assert_eq!(info.length, body.len() as u64);
    assert_eq!(info.version, receipt.object.version);
    assert_eq!(info.metadata.as_bytes(), b"meta");
}

#[test]
fn ranges_decrypt_across_block_boundaries_and_match_the_plaintext() {
    let r = rig();
    let body = plain(1000);
    put(&r, "r1", "orders/a", &body);
    let v = block_on(r.sealed.head(&ctx(), &key("orders/a")))
        .unwrap()
        .version;
    for (offset, len) in [
        (0u64, 1000u64),
        (0, 1),
        (63, 2),
        (64, 64),
        (60, 200),
        (999, 1),
        (512, 488),
    ] {
        assert_eq!(
            read(&r, "orders/a", &v, offset, len).unwrap(),
            body[offset as usize..(offset + len) as usize],
            "{offset}+{len}"
        );
    }
    assert_eq!(read(&r, "orders/a", &v, 1000, 1), Err(Error::InvalidRange));
    assert_eq!(read(&r, "orders/a", &v, 990, 11), Err(Error::InvalidRange));
}

#[test]
fn an_empty_object_round_trips() {
    let r = rig();
    put(&r, "r1", "orders/e", b"");
    let info = block_on(r.sealed.head(&ctx(), &key("orders/e"))).unwrap();
    assert_eq!(info.length, 0);
}

#[test]
fn one_wrap_per_object_and_no_provider_call_per_read() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(500));
    assert_eq!(r.kms.wrap_calls(), 1);
    let v = block_on(r.sealed.head(&ctx(), &key("orders/a")))
        .unwrap()
        .version;
    let before = r.kms.total_calls();
    for i in 0..200u64 {
        read(&r, "orders/a", &v, i, 100).unwrap();
    }
    assert_eq!(r.kms.total_calls(), before);
    // A node that has never seen the object (new decorator, same cache-less manager state) unwraps once.
    let again = SealedBackend::new(
        Shared(r.tap.clone()),
        r.keys.clone(),
        Arc::new(Scopes),
        config(),
    )
    .unwrap();
    for _ in 0..5 {
        block_on(again.read_range(&ctx(), &key("orders/a"), &v, ByteRange::new(0, 10).unwrap()))
            .unwrap();
    }
    assert!(r.kms.unwrap_calls() <= 1);
}

#[test]
fn a_read_after_head_costs_exactly_one_backend_read() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(1000));
    let v = block_on(r.sealed.head(&ctx(), &key("orders/a")))
        .unwrap()
        .version;
    let before = r.tap.reads.load(Ordering::SeqCst);
    read(&r, "orders/a", &v, 100, 300).unwrap();
    assert_eq!(r.tap.reads.load(Ordering::SeqCst), before + 1);
}

#[test]
fn an_object_copied_to_another_key_or_tenant_is_refused() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(300));
    // Write the very same stored bytes under another key of the same index, and under another index.
    let raw = stored(&r, "orders/a");
    for (id, name) in [("c1", "orders/copy"), ("c2", "invoices/copy")] {
        let m = Mutation {
            request_id: RequestId::new(id.as_bytes()).unwrap(),
            key: key(name),
            payload: Payload::new(&raw).unwrap(),
            metadata: Metadata::new(b"").unwrap(),
            precondition: Precondition::Absent,
        };
        block_on(r.tap.inner.create(&ctx(), &m)).unwrap();
        let err = block_on(r.sealed.head(&ctx(), &key(name))).unwrap_err();
        assert!(
            matches!(err, Error::Corrupt | Error::PermissionDenied),
            "{name}: {err:?}"
        );
    }
}

#[test]
fn every_damaged_byte_is_refused_never_returned() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(150));
    let len = stored(&r, "orders/a").len();
    for at in 0..len {
        let fresh = SealedBackend::new(
            Shared(r.tap.clone()),
            r.keys.clone(),
            Arc::new(Scopes),
            config(),
        )
        .unwrap();
        *r.tap.flip.lock().unwrap() = Some(at);
        let result = block_on(async {
            let info = fresh.head(&ctx(), &key("orders/a")).await?;
            fresh
                .read_range(
                    &ctx(),
                    &key("orders/a"),
                    &info.version,
                    ByteRange::new(0, 150).unwrap(),
                )
                .await
        });
        assert!(result.is_err(), "byte {at} of {len} was accepted");
    }
    *r.tap.flip.lock().unwrap() = None;
}

#[test]
fn a_length_that_disagrees_with_the_header_is_corruption() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(150));
    r.tap.short.store(true, Ordering::SeqCst);
    assert_eq!(
        block_on(r.sealed.head(&ctx(), &key("orders/a"))).unwrap_err(),
        Error::Corrupt
    );
}

#[test]
fn revoking_one_index_key_blocks_that_index_only() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(100));
    put(&r, "r2", "invoices/a", &plain(100));
    // A node with a cold cache reads after revocation.
    r.kms.revoke(&kref("invoices"));
    let cold = SealedBackend::new(
        Shared(r.tap.clone()),
        r.keys.clone(),
        Arc::new(Scopes),
        config(),
    )
    .unwrap();
    r.keys
        .invalidate_index(&IndexId::new("invoices").unwrap(), 2);
    let err = block_on(cold.head(&ctx(), &key("invoices/a"))).unwrap_err();
    assert_eq!(err, Error::PermissionDenied);
    assert!(block_on(cold.head(&ctx(), &key("orders/a"))).is_ok());
}

#[test]
fn a_retry_resends_identical_bytes_and_wraps_once() {
    let r = rig();
    let first = put(&r, "same", "orders/a", &plain(300));
    let second = put(&r, "same", "orders/a", &plain(300));
    assert_eq!(first, second);
    assert_eq!(r.kms.wrap_calls(), 1);
    // The same identity with different content is an identity conflict, not a second seal.
    let err = block_on(
        r.sealed
            .create(&ctx(), &mutation("same", "orders/a", &plain(301))),
    )
    .unwrap_err();
    assert_eq!(err, Error::IdentityConflict);
    assert_eq!(r.kms.wrap_calls(), 1);
}

#[test]
fn distinct_requests_get_distinct_data_keys() {
    let r = rig();
    put(&r, "a", "orders/a", &plain(100));
    put(&r, "b", "orders/b", &plain(100));
    assert_eq!(r.kms.wrap_calls(), 2);
    assert_ne!(stored(&r, "orders/a")[..], stored(&r, "orders/b")[..]);
}

#[test]
fn identical_plaintext_under_two_keys_shares_no_ciphertext() {
    let r = rig();
    put(&r, "a", "orders/a", &plain(256));
    put(&r, "b", "orders/b", &plain(256));
    let (a, b) = (stored(&r, "orders/a"), stored(&r, "orders/b"));
    let head = 140.min(a.len());
    assert!(
        a[head..]
            .iter()
            .zip(&b[head..])
            .filter(|(x, y)| x == y)
            .count()
            < a.len() / 8
    );
}

#[test]
fn unknown_scope_a_precondition_and_oversize_are_refused_before_any_key_call() {
    let r = rig();
    let err = block_on(
        r.sealed
            .create(&ctx(), &mutation("x", "elsewhere/a", &plain(10))),
    )
    .unwrap_err();
    assert_eq!(err, Error::PermissionDenied);
    assert_eq!(r.kms.total_calls(), 0);
    let mut m = mutation("y", "orders/a", &plain(10));
    m.precondition = Precondition::Matches(VersionToken::new(b"v").unwrap());
    assert_eq!(
        block_on(r.sealed.create(&ctx(), &m)).unwrap_err(),
        Error::InvalidInput
    );
    assert_eq!(r.kms.total_calls(), 0);
}

#[test]
fn a_zero_block_size_and_empty_caches_are_refused_at_construction() {
    let bad = |c: SealConfig| {
        let r = rig();
        SealedBackend::new(Shared(r.tap.clone()), r.keys.clone(), Arc::new(Scopes), c).is_err()
    };
    assert!(bad(SealConfig {
        block_size: 0,
        ..config()
    }));
    assert!(bad(SealConfig {
        block_size: (1 << 20) + 1,
        ..config()
    }));
    assert!(bad(SealConfig {
        open_entries: 0,
        ..config()
    }));
    assert!(bad(SealConfig {
        retry_bytes: 0,
        ..config()
    }));
    assert!(bad(SealConfig {
        retry_entries: 0,
        ..config()
    }));
}

#[test]
fn a_create_receipt_reports_the_plaintext_length() {
    let r = rig();
    assert_eq!(put(&r, "r1", "orders/a", &plain(300)).object.length, 300);
}

#[test]
fn a_cached_object_is_rechecked_against_the_stored_length() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(150));
    assert!(block_on(r.sealed.head(&ctx(), &key("orders/a"))).is_ok());
    r.tap.short.store(true, Ordering::SeqCst);
    assert_eq!(
        block_on(r.sealed.head(&ctx(), &key("orders/a"))).unwrap_err(),
        Error::Corrupt
    );
}

#[test]
fn a_read_at_a_version_that_is_not_the_stored_one_never_reads_it() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(150));
    let wrong = VersionToken::new(b"not-the-version").unwrap();
    assert_eq!(read(&r, "orders/a", &wrong, 0, 10), Err(Error::NotFound));
}

#[test]
fn an_object_shorter_than_its_own_header_is_corruption_not_a_panic() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(150));
    let raw = stored(&r, "orders/a");
    for cut in [0usize, 1, 20, 57, 58, 70, 100, 140] {
        let name = format!("orders/cut{cut}");
        let m = Mutation {
            request_id: RequestId::new(name.as_bytes()).unwrap(),
            key: key(&name),
            payload: Payload::new(&raw[..cut]).unwrap(),
            metadata: Metadata::new(b"").unwrap(),
            precondition: Precondition::Absent,
        };
        block_on(r.tap.inner.create(&ctx(), &m)).unwrap();
        assert_eq!(
            block_on(r.sealed.head(&ctx(), &key(&name))).unwrap_err(),
            Error::Corrupt,
            "cut {cut}"
        );
    }
}

#[test]
fn a_range_larger_than_one_response_is_refused_up_front() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(150));
    let v = block_on(r.sealed.head(&ctx(), &key("orders/a")))
        .unwrap()
        .version;
    assert_eq!(
        read(&r, "orders/a", &v, 0, (16 << 20) + 1),
        Err(Error::InvalidInput)
    );
}

#[test]
fn the_retry_memory_is_bounded_and_an_evicted_retry_fails_closed() {
    // One remembered request: the second evicts the first, so a retry of the first seals under a new key
    // and the store refuses it. It is never accepted as a second ciphertext of the same identity.
    let r = rig_with(SealConfig {
        retry_entries: 1,
        ..config()
    });
    put(&r, "a", "orders/a", &plain(100));
    put(&r, "b", "orders/b", &plain(100));
    assert_eq!(r.kms.wrap_calls(), 2);
    let err = block_on(
        r.sealed
            .create(&ctx(), &mutation("a", "orders/a", &plain(100))),
    )
    .unwrap_err();
    assert_eq!(err, Error::IdentityConflict);
    assert_eq!(r.kms.wrap_calls(), 3);
    // The sealed form just made is the one remembered now: another retry seals nothing new.
    let err = block_on(
        r.sealed
            .create(&ctx(), &mutation("a", "orders/a", &plain(100))),
    )
    .unwrap_err();
    assert_eq!(err, Error::IdentityConflict);
    assert_eq!(r.kms.wrap_calls(), 3);
}

#[test]
fn a_sealed_request_larger_than_the_retry_byte_bound_is_not_remembered() {
    let r = rig_with(SealConfig {
        retry_bytes: 100,
        ..config()
    });
    put(&r, "a", "orders/a", &plain(300));
    let err = block_on(
        r.sealed
            .create(&ctx(), &mutation("a", "orders/a", &plain(300))),
    )
    .unwrap_err();
    assert_eq!(err, Error::IdentityConflict);
    assert_eq!(r.kms.wrap_calls(), 2);
}

#[test]
fn a_warm_object_is_rechecked_against_the_key_and_the_scope_on_every_use() {
    let r = rig();
    put(&r, "r1", "invoices/a", &plain(300));
    put(&r, "r2", "orders/a", &plain(300));
    let v = block_on(r.sealed.head(&ctx(), &key("invoices/a")))
        .unwrap()
        .version;
    let vo = block_on(r.sealed.head(&ctx(), &key("orders/a")))
        .unwrap()
        .version;
    assert!(read(&r, "invoices/a", &v, 0, 10).is_ok());
    // The owner revokes the key and the node is told: the same decorator, warm, now refuses that index.
    r.kms.revoke(&kref("invoices"));
    r.keys
        .invalidate_index(&IndexId::new("invoices").unwrap(), 2);
    assert_eq!(
        read(&r, "invoices/a", &v, 0, 10),
        Err(Error::PermissionDenied)
    );
    assert_eq!(
        block_on(r.sealed.head(&ctx(), &key("invoices/a"))).unwrap_err(),
        Error::PermissionDenied
    );
    // Another index is untouched.
    assert!(read(&r, "orders/a", &vo, 0, 10).is_ok());
    // A scope the resolver now refuses is refused even though the object is warm.
    let deny = Arc::new(AtomicBool::new(false));
    let warm = SealedBackend::new(
        Shared(r.tap.clone()),
        r.keys.clone(),
        Arc::new(Switch(deny.clone())),
        config(),
    )
    .unwrap();
    let range = || ByteRange::new(0, 10).unwrap();
    block_on(warm.head(&ctx(), &key("orders/a"))).unwrap();
    assert!(block_on(warm.read_range(&ctx(), &key("orders/a"), &vo, range())).is_ok());
    deny.store(true, Ordering::SeqCst);
    assert_eq!(
        block_on(warm.read_range(&ctx(), &key("orders/a"), &vo, range())),
        Err(Error::PermissionDenied)
    );
    assert_eq!(
        block_on(warm.head(&ctx(), &key("orders/a"))).unwrap_err(),
        Error::PermissionDenied
    );
}

#[test]
fn a_flipped_byte_anywhere_in_a_multi_block_ranged_read_is_refused() {
    let r = rig();
    put(&r, "r1", "orders/a", &plain(300));
    let len = stored(&r, "orders/a").len();
    let v = block_on(r.sealed.head(&ctx(), &key("orders/a")))
        .unwrap()
        .version;
    for at in 0..len {
        *r.tap.flip.lock().unwrap() = Some(at);
        // A fresh decorator each time so the header is re-read through the damage too.
        let fresh = SealedBackend::new(
            Shared(r.tap.clone()),
            r.keys.clone(),
            Arc::new(Scopes),
            config(),
        )
        .unwrap();
        let result = block_on(async {
            fresh
                .read_range(
                    &ctx(),
                    &key("orders/a"),
                    &v,
                    ByteRange::new(10, 280).unwrap(),
                )
                .await
        });
        // Bytes the range does not touch (the first 10 plaintext bytes' block is read whole, the tail of
        // the last block too) are all authenticated, so any flip inside the object fails the read.
        assert!(result.is_err(), "byte {at} of {len} was accepted");
    }
    *r.tap.flip.lock().unwrap() = None;
}

#[test]
fn a_payload_that_cannot_fit_once_sealed_is_refused_before_any_key_is_made() {
    let r = rig();
    let big = vec![1u8; (16 << 20) - 100];
    let err = block_on(
        r.sealed
            .create(&ctx(), &mutation("big", "orders/big", &big)),
    )
    .unwrap_err();
    assert_eq!(err, Error::InvalidInput);
    assert_eq!(r.kms.total_calls(), 0);
}
