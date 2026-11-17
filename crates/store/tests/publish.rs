//! Publishing a base against the in-memory store: verification before upload, the root naming only
//! complete objects, the head naming only a verified root, and a crash at every write boundary.
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use hsearch_engine::{Kind, LocalFiles, Mutation, Schema, Value, with_local_engine};
use hsearch_format::{CommitToken, Durability, Id, Layout, StructuralHead};
use hsearch_index::barrier::{Cut, Replay};
use hsearch_lane::{LaneId, block_on, create_head, read_head};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    ByteRange, Error as StoreError, LargeObjectWriter, MetadataStore, Mutation as Put,
    ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload, RequestContext, VersionToken,
    WriteReceipt,
};
use hsearch_store::publish::{AnyDependencies, BaseSpec, Dependencies, Error, HeadRead, Publisher};
use sha2::{Digest, Sha256};

const INDEX: Id = [7; 16];
const SHARD: Id = [8; 16];

/// A store that refuses every write (create, compare-exchange, upload step) once its allowance is
/// spent, and counts the writes it let through.
struct Crash {
    inner: MemoryStore,
    allow: AtomicI64,
    writes: AtomicUsize,
    /// The write (counted from 0) whose reply is lost after it commits; negative for none.
    lose: AtomicI64,
    fail_resolve: std::sync::atomic::AtomicBool,
}

impl Crash {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(MemoryLimits {
                objects: 4096,
                versions: 16384,
                version_bytes: 64 << 20,
                identities: 16384,
                identity_bytes: 64 << 20,
            })
            .unwrap(),
            allow: AtomicI64::new(i64::MAX),
            writes: AtomicUsize::new(0),
            lose: AtomicI64::new(-1),
            fail_resolve: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn spend(&self) -> bool {
        if self.allow.fetch_sub(1, Ordering::SeqCst) > 0 {
            self.writes.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
    /// True when this write's reply is to be lost after it commits.
    fn lost(&self) -> bool {
        self.writes.load(Ordering::SeqCst) as i64 - 1 == self.lose.load(Ordering::SeqCst)
    }
    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
}

impl ObjectBackend for Crash {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        if self.lost() {
            return Box::pin(async move {
                self.inner.create(c, r).await?;
                Err(StoreError::UnknownOutcome)
            });
        }
        self.inner.create(c, r)
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        self.inner.read_range(c, k, v, r)
    }
}

impl MetadataStore for Crash {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Put,
    ) -> Operation<'a, WriteReceipt> {
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        if self.lost() {
            return Box::pin(async move {
                self.inner.compare_exchange(c, r).await?;
                Err(StoreError::UnknownOutcome)
            });
        }
        self.inner.compare_exchange(c, r)
    }
    fn remove_version<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
    ) -> Operation<'a, ()> {
        self.inner.remove_version(c, k, v)
    }
    fn resolve<'a>(&'a self, c: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        if self.fail_resolve.load(Ordering::SeqCst) {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        self.inner.resolve(c, r)
    }
}

impl LargeObjectWriter for Crash {
    type Upload = <MemoryStore as LargeObjectWriter>::Upload;
    fn begin_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
    ) -> Operation<'a, Self::Upload> {
        self.inner.begin_upload(c, k)
    }
    fn put_part<'a>(
        &'a self,
        c: &'a RequestContext,
        u: &'a mut Self::Upload,
        b: &'a [u8],
    ) -> Operation<'a, ()> {
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        self.inner.put_part(c, u, b)
    }
    fn complete_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        u: &'a Self::Upload,
    ) -> Operation<'a, (VersionToken, u64)> {
        if !self.spend() {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        self.inner.complete_upload(c, u)
    }
    fn abort_upload<'a>(&'a self, c: &'a RequestContext, u: &'a Self::Upload) -> Operation<'a, ()> {
        self.inner.abort_upload(c, u)
    }
}

fn layout() -> Layout {
    Layout::new("test").unwrap()
}

fn publisher(store: &Arc<Crash>) -> Publisher<Crash> {
    Publisher::new(
        Arc::clone(store),
        layout(),
        5 << 20,
        Duration::from_secs(30),
    )
}

fn token(n: u8) -> CommitToken {
    CommitToken::new([n; 16]).unwrap()
}

/// A cut covering the first `n` slots, as a replay barrier produces it.
fn cut(n: u64) -> Cut {
    let mut replay = Replay::new(0);
    for seq in 0..n {
        replay.begin(seq).unwrap();
        replay.done(seq).unwrap();
    }
    replay.cut().unwrap()
}

/// Commit a few documents to a scratch engine and run `f` while its native files are on disk.
fn with_commit<R>(label: &str, f: impl FnOnce(&LocalFiles, &std::path::Path, &[String]) -> R) -> R {
    let dir = std::env::temp_dir().join(format!("hsearch-store-{label}-{}", std::process::id()));
    let schema = Schema::new(1, &[("tag", Kind::Keyword)]).unwrap();
    let fields = [("tag", Value::String("x"))];
    let result = with_local_engine(&dir, schema.clone(), |engine, local| {
        let puts: Vec<Mutation<'_>> = (0..5)
            .map(|i| Mutation::Put {
                id: ["a", "b", "c", "d", "e"][i],
                version: 1,
                fields: &fields,
            })
            .collect();
        engine.apply(schema.prepare("r", &puts).unwrap()).unwrap();
        engine.commit().unwrap();
        let names = local.names().unwrap();
        f(local, &dir, &names)
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn spec<'a>(cut: Cut, committed: u64, attempt: u8) -> BaseSpec<'a> {
    BaseSpec {
        index: INDEX,
        shard: SHARD,
        incarnation: 1,
        engine_format: "lucene-test",
        mapping: [5; 32],
        crypto_envelope: [6; 16],
        cut,
        committed,
        previous: None,
        attempt: token(attempt),
    }
}

fn lane() -> LaneId {
    LaneId::new([9; 16])
}

fn head_of(store: &Arc<Crash>) -> (StructuralHead, VersionToken) {
    block_on(read_head(store.inner_ref(), &layout(), &lane())).unwrap()
}

impl Crash {
    fn inner_ref(&self) -> &MemoryStore {
        &self.inner
    }
}

fn fresh_head(store: &Arc<Crash>) {
    block_on(create_head(
        store.inner_ref(),
        &layout(),
        &lane(),
        Durability::Object,
    ))
    .unwrap();
}

fn read_all(store: &Arc<Crash>, key: &str) -> Vec<u8> {
    let key = ObjectKey::new(key.as_bytes()).unwrap();
    let ctx = || {
        RequestContext::new(
            Duration::from_secs(30),
            hsearch_object::Cancellation::default(),
            hsearch_object::Budget {
                request_bytes: 4096,
                response_bytes: 64 << 20,
                attempts: 8,
            },
        )
        .unwrap()
    };
    let info = block_on(store.inner.head(&ctx(), &key)).unwrap();
    block_on(store.inner.read_range(
        &ctx(),
        &key,
        &info.version,
        ByteRange::new(0, info.length).unwrap(),
    ))
    .unwrap()
    .as_bytes()
    .to_vec()
}

#[test]
fn a_published_base_names_complete_files_and_the_head_names_the_base() {
    let store = Crash::new();
    fresh_head(&store);
    let (head, version) = head_of(&store);
    with_commit("happy", |local, dir, names| {
        let p = publisher(&store);
        let s = spec(cut(3), 3, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        assert_eq!(published.root.generation, 1);
        assert_eq!(published.root.covered, 3);
        assert_eq!(published.files, names.len());
        // Every file object is exactly the local file: the digest the root records matches the
        // bytes the store holds, byte for byte.
        let root = p
            .verify_root(&INDEX, &SHARD, 1, &published.manifest, &AnyDependencies)
            .unwrap();
        for (i, page_ref) in root.pages.iter().enumerate() {
            let key = layout().manifest_page(&INDEX, &SHARD, 1, &page_ref.page);
            let page = root.verify_page(i, &read_all(&store, &key)).unwrap();
            for e in &page.entries {
                let key = layout().payload(&INDEX, &e.object);
                let stored = read_all(&store, &key);
                assert_eq!(stored, std::fs::read(dir.join(&e.name)).unwrap());
                assert_eq!(Sha256::digest(&stored).as_slice(), e.digest);
            }
        }
        // The head still names nothing until activation.
        assert_eq!(head_of(&store).0.checkpoint, None);
        let (next, _) = p
            .activate(
                &at(&lane(), &head, &version),
                &published,
                &s,
                3,
                &AnyDependencies,
            )
            .unwrap();
        assert_eq!(next.checkpoint, Some(published.manifest));
        assert_eq!(next.cover, 3);
        assert_eq!(head_of(&store).0, next);
    });
}

#[test]
fn a_later_base_must_not_regress_and_never_passes_what_was_committed() {
    let store = Crash::new();
    with_commit("coords", |local, _, names| {
        let p = publisher(&store);
        let first = p
            .publish(local, names, &spec(cut(4), 4, 1), &AnyDependencies)
            .unwrap();
        let before = store.writes();
        // Speculative: applied 5 but only 4 acknowledged.
        let err = p
            .publish(local, names, &spec(cut(5), 4, 2), &AnyDependencies)
            .unwrap_err();
        assert!(matches!(err, Error::Speculative));
        // Regression: covered 3 below the previous base's 4.
        let mut s = spec(cut(3), 9, 3);
        s.previous = Some(&first.root);
        assert!(matches!(
            p.publish(local, names, &s, &AnyDependencies).unwrap_err(),
            Error::Regression
        ));
        assert_eq!(store.writes(), before, "a refused base writes nothing");
        // A forward base takes the next generation.
        let mut s = spec(cut(6), 9, 4);
        s.previous = Some(&first.root);
        let second = p.publish(local, names, &s, &AnyDependencies).unwrap();
        assert_eq!(second.root.generation, 2);
        // The same covered prefix again is allowed (a rewritten base), still a new generation.
        let mut s = spec(cut(6), 9, 5);
        s.previous = Some(&second.root);
        assert_eq!(
            p.publish(local, names, &s, &AnyDependencies)
                .unwrap()
                .root
                .generation,
            3
        );
    });
}

#[test]
fn a_damaged_file_uploads_nothing() {
    let store = Crash::new();
    with_commit("damage", |local, dir, names| {
        // Flip one byte in the middle of the largest file: it still opens, but its checksum fails.
        let victim = names
            .iter()
            .max_by_key(|n| std::fs::metadata(dir.join(n)).unwrap().len())
            .unwrap();
        let mut bytes = std::fs::read(dir.join(victim)).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x40;
        std::fs::write(dir.join(victim), bytes).unwrap();
        let err = publisher(&store)
            .publish(local, names, &spec(cut(3), 3, 1), &AnyDependencies)
            .unwrap_err();
        assert!(matches!(err, Error::Unverified));
        assert_eq!(store.writes(), 0);
    });
}

struct Flips(AtomicUsize, usize);
impl Dependencies for Flips {
    fn current(&self, _: &[u8; 32], _: &Id) -> bool {
        self.0.fetch_add(1, Ordering::SeqCst) < self.1
    }
}

#[test]
fn a_stale_dependency_stops_a_base_before_and_after_the_upload() {
    let store = Crash::new();
    with_commit("stale", |local, _, names| {
        let p = publisher(&store);
        // Stale at once: nothing written.
        let err = p
            .publish(
                local,
                names,
                &spec(cut(3), 3, 1),
                &Flips(AtomicUsize::new(0), 0),
            )
            .unwrap_err();
        assert!(matches!(err, Error::Stale));
        assert_eq!(store.writes(), 0);
        // Current at the start, stale once the files are up: the files are orphans, but no page
        // or root was written, so nothing can name them.
        let err = p
            .publish(
                local,
                names,
                &spec(cut(3), 3, 2),
                &Flips(AtomicUsize::new(0), 1),
            )
            .unwrap_err();
        assert!(matches!(err, Error::Stale));
        // Only the files went up: no page and no root.
        assert_eq!(store.writes(), names.len());
    });
}

#[test]
fn a_crash_at_every_write_boundary_leaves_the_head_unnamed_or_naming_a_verified_base() {
    with_commit("crash", |local, _, names| {
        // Count the writes of a clean publish and activation.
        let probe = Crash::new();
        fresh_head(&probe);
        let (head, version) = head_of(&probe);
        let base = probe.writes();
        let p = publisher(&probe);
        let s = spec(cut(3), 3, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        p.activate(
            &at(&lane(), &head, &version),
            &published,
            &s,
            3,
            &AnyDependencies,
        )
        .unwrap();
        let total = probe.writes() - base;
        assert!(total > names.len());

        for k in 0..total {
            let store = Crash::new();
            fresh_head(&store);
            let (head, version) = head_of(&store);
            store.allow.store(k as i64, Ordering::SeqCst);
            let p = publisher(&store);
            let s = spec(cut(3), 3, 1);
            let outcome = p
                .publish(local, names, &s, &AnyDependencies)
                .and_then(|published| {
                    p.activate(
                        &at(&lane(), &head, &version),
                        &published,
                        &s,
                        3,
                        &AnyDependencies,
                    )
                    .map(|_| published)
                });
            assert!(outcome.is_err(), "boundary {k} must fail");
            // A root that exists lists only objects that exist: it is written last.
            let root_key = layout().manifest_root(
                &INDEX,
                &SHARD,
                1,
                &hsearch_store::publish::manifest_id(&INDEX, &SHARD, &token(1)),
            );
            let exists = block_on(
                store.inner.head(
                    &RequestContext::new(
                        Duration::from_secs(30),
                        hsearch_object::Cancellation::default(),
                        hsearch_object::Budget {
                            request_bytes: 4096,
                            response_bytes: 4096,
                            attempts: 8,
                        },
                    )
                    .unwrap(),
                    &ObjectKey::new(root_key.as_bytes()).unwrap(),
                ),
            )
            .is_ok();
            if exists {
                let id = hsearch_store::publish::manifest_id(&INDEX, &SHARD, &token(1));
                p.verify_root(&INDEX, &SHARD, 1, &id, &AnyDependencies)
                    .unwrap();
            }
            // The head is exactly as it was: no crash names a half-built base.
            let (after, v) = head_of(&store);
            assert_eq!((after, v), (head.clone(), version.clone()), "boundary {k}");
            // A retry on a healthy store under a new attempt succeeds, and what the head then
            // names verifies, whatever orphans the crash left behind.
            store.allow.store(i64::MAX, Ordering::SeqCst);
            let s = spec(cut(3), 3, 2);
            let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
            let (next, _) = p
                .activate(
                    &at(&lane(), &head, &version),
                    &published,
                    &s,
                    3,
                    &AnyDependencies,
                )
                .unwrap();
            let named = next.checkpoint.unwrap();
            p.verify_root(&INDEX, &SHARD, 1, &named, &AnyDependencies)
                .unwrap();
        }
    });
}

#[test]
fn verification_refuses_a_missing_file_a_tampered_page_and_a_stale_head() {
    let store = Crash::new();
    fresh_head(&store);
    let (head, version) = head_of(&store);
    with_commit("verify", |local, _, names| {
        let p = publisher(&store);
        let s = spec(cut(3), 3, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        // A tampered page: replaced in place by different bytes of the same kind.
        let page_id = published.root.pages[0].page;
        let key = ObjectKey::new(
            layout()
                .manifest_page(&INDEX, &SHARD, 1, &page_id)
                .as_bytes(),
        )
        .unwrap();
        let ctx = RequestContext::new(
            Duration::from_secs(30),
            hsearch_object::Cancellation::default(),
            hsearch_object::Budget {
                request_bytes: 1 << 20,
                response_bytes: 1 << 20,
                attempts: 8,
            },
        )
        .unwrap();
        let info = block_on(store.inner.head(&ctx, &key)).unwrap();
        let mut bad = read_all(&store, &layout().manifest_page(&INDEX, &SHARD, 1, &page_id));
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let replace = Put {
            request_id: hsearch_object::RequestId::new(b"tamper").unwrap(),
            key: key.clone(),
            payload: Payload::new(&bad).unwrap(),
            metadata: hsearch_object::Metadata::new(b"").unwrap(),
            precondition: hsearch_object::Precondition::Matches(info.version.clone()),
        };
        block_on(store.inner.compare_exchange(&ctx, &replace)).unwrap();
        assert!(
            p.verify_root(&INDEX, &SHARD, 1, &published.manifest, &AnyDependencies)
                .is_err()
        );
        // Activation verifies first, so the head is untouched.
        assert!(
            p.activate(
                &at(&lane(), &head, &version),
                &published,
                &s,
                3,
                &AnyDependencies
            )
            .is_err()
        );
        assert_eq!(head_of(&store).0.checkpoint, None);

        // A second, intact base; delete one of its files: verification refuses it.
        let s2 = spec(cut(3), 3, 2);
        let intact = p.publish(local, names, &s2, &AnyDependencies).unwrap();
        p.verify_root(&INDEX, &SHARD, 1, &intact.manifest, &AnyDependencies)
            .unwrap();
        let page_key = layout().manifest_page(&INDEX, &SHARD, 1, &intact.root.pages[0].page);
        let page = intact
            .root
            .verify_page(0, &read_all(&store, &page_key))
            .unwrap();
        let file_key =
            ObjectKey::new(layout().payload(&INDEX, &page.entries[0].object).as_bytes()).unwrap();
        let file = block_on(store.inner.head(&ctx, &file_key)).unwrap();
        block_on(store.inner.remove_version(&ctx, &file_key, &file.version)).unwrap();
        assert!(
            p.verify_root(&INDEX, &SHARD, 1, &intact.manifest, &AnyDependencies)
                .is_err()
        );

        // A head read before another transition won is refused as lost, changing nothing.
        let s3 = spec(cut(3), 3, 3);
        let third = p.publish(local, names, &s3, &AnyDependencies).unwrap();
        let (moved, _) = p
            .activate(
                &at(&lane(), &head, &version),
                &third,
                &s3,
                3,
                &AnyDependencies,
            )
            .unwrap();
        let s4 = spec(cut(3), 3, 4);
        let fourth = p.publish(local, names, &s4, &AnyDependencies).unwrap();
        let err = p
            .activate(
                &at(&lane(), &head, &version),
                &fourth,
                &s4,
                3,
                &AnyDependencies,
            )
            .unwrap_err();
        assert!(matches!(err, Error::Lost));
        assert_eq!(head_of(&store).0, moved);
    });
}

#[test]
fn a_base_of_many_files_spans_pages_and_still_verifies() {
    let store = Crash::new();
    with_commit("pages", |local, dir, names| {
        // 2,100 copies of one valid file under distinct names: more than one page of entries.
        let source = names[0].clone();
        let mut many: Vec<String> = names.to_vec();
        for i in 0..2100 {
            let name = format!("copy-{i:05}");
            std::fs::copy(dir.join(&source), dir.join(&name)).unwrap();
            many.push(name);
        }
        let p = publisher(&store);
        let published = p
            .publish(local, &many, &spec(cut(3), 3, 1), &AnyDependencies)
            .unwrap();
        assert_eq!(published.files, many.len());
        assert!(published.root.pages.len() >= 2);
        p.verify_root(&INDEX, &SHARD, 1, &published.manifest, &AnyDependencies)
            .unwrap();
    });
}

#[test]
fn a_lost_reply_at_any_write_is_resolved_and_the_base_still_verifies() {
    with_commit("lost", |local, _, names| {
        let probe = Crash::new();
        fresh_head(&probe);
        let (head, version) = head_of(&probe);
        let p = publisher(&probe);
        let s = spec(cut(3), 3, 1);
        let base = probe.writes();
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        p.activate(
            &at(&lane(), &head, &version),
            &published,
            &s,
            3,
            &AnyDependencies,
        )
        .unwrap();
        let total = probe.writes() - base;
        for k in 0..total {
            let store = Crash::new();
            fresh_head(&store);
            let (head, version) = head_of(&store);
            let first = store.writes() as i64;
            store.lose.store(first + k as i64, Ordering::SeqCst);
            let p = publisher(&store);
            let published = match p.publish(local, names, &s, &AnyDependencies) {
                Ok(published) => published,
                // A file's create is not resolved by the sink: the publish reports it, and the same
                // attempt replays the one-part files that already landed.
                Err(Error::Upload(_)) if k < names.len() => p
                    .publish(local, names, &s, &AnyDependencies)
                    .unwrap_or_else(|e| panic!("retry at {k}: {e:?}")),
                Err(e) => panic!("write {k}: {e:?}"),
            };
            let (next, _) = p
                .activate(
                    &at(&lane(), &head, &version),
                    &published,
                    &s,
                    3,
                    &AnyDependencies,
                )
                .unwrap();
            p.verify_root(
                &INDEX,
                &SHARD,
                1,
                &next.checkpoint.unwrap(),
                &AnyDependencies,
            )
            .unwrap();
        }
    });
}

#[test]
fn an_unresolvable_lost_reply_on_the_head_is_unknown_not_lost() {
    with_commit("unknown", |local, _, names| {
        let store = Crash::new();
        fresh_head(&store);
        let (head, version) = head_of(&store);
        let p = publisher(&store);
        let s = spec(cut(3), 3, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        // The next write is the head transition: it commits, the reply is lost, resolution fails.
        store.lose.store(store.writes() as i64, Ordering::SeqCst);
        store.fail_resolve.store(true, Ordering::SeqCst);
        let err = p
            .activate(
                &at(&lane(), &head, &version),
                &published,
                &s,
                3,
                &AnyDependencies,
            )
            .unwrap_err();
        assert!(matches!(err, Error::Unknown), "{err:?}");
        // The caller re-reads and finds the transition landed.
        assert_eq!(head_of(&store).0.checkpoint, Some(published.manifest));
    });
}

struct StaleNow;
impl Dependencies for StaleNow {
    fn current(&self, _: &[u8; 32], _: &Id) -> bool {
        false
    }
}

#[test]
fn activation_rechecks_dependencies_and_ties_the_spec_to_the_published_root() {
    with_commit("tie", |local, _, names| {
        let store = Crash::new();
        fresh_head(&store);
        let (head, version) = head_of(&store);
        let p = publisher(&store);
        let s = spec(cut(3), 3, 1);
        let published = p.publish(local, names, &s, &AnyDependencies).unwrap();
        // A dependency that went stale between publish and activate.
        let err = p
            .activate(&at(&lane(), &head, &version), &published, &s, 3, &StaleNow)
            .unwrap_err();
        assert!(matches!(err, Error::Stale));
        // A spec for a different cut or index than the root was built for.
        let other_cut = spec(cut(2), 3, 1);
        assert!(matches!(
            p.activate(
                &at(&lane(), &head, &version),
                &published,
                &other_cut,
                3,
                &AnyDependencies
            )
            .unwrap_err(),
            Error::Mismatch
        ));
        let mut other_index = spec(cut(3), 3, 1);
        other_index.index = [1; 16];
        assert!(matches!(
            p.activate(
                &at(&lane(), &head, &version),
                &published,
                &other_index,
                3,
                &AnyDependencies
            )
            .unwrap_err(),
            Error::Mismatch
        ));
        assert_eq!(head_of(&store).0.checkpoint, None);
    });
}

#[test]
fn a_retry_under_the_same_attempt_replays_one_part_files_and_new_attempts_stand_alone() {
    with_commit("retry", |local, _, names| {
        let store = Crash::new();
        let p = publisher(&store);
        let first = p
            .publish(local, names, &spec(cut(3), 3, 1), &AnyDependencies)
            .unwrap();
        // One-part files replay by an identity derived from their key, and pages and the root
        // resolve by identity: the same attempt published again names the same manifest.
        let again = p
            .publish(local, names, &spec(cut(3), 3, 1), &AnyDependencies)
            .unwrap();
        assert_eq!(first, again);
        let other = p
            .publish(local, names, &spec(cut(3), 3, 2), &AnyDependencies)
            .unwrap();
        assert_ne!(other.manifest, first.manifest);
        assert!(matches!(
            p.publish(local, &[], &spec(cut(3), 3, 3), &AnyDependencies)
                .unwrap_err(),
            Error::NoFiles
        ));
    });
}

fn at<'a>(lane: &'a LaneId, head: &'a StructuralHead, version: &'a VersionToken) -> HeadRead<'a> {
    HeadRead {
        lane,
        head,
        version,
    }
}
