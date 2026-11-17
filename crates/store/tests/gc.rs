//! Compaction, pins and collection against the in-memory store: a replaced base is reclaimed and a
//! live, pinned or in-flight one is not; stale publishers cannot name swept generations; a stopped
//! sweep resumes; pin registration races collection.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hsearch_engine::{Kind, LocalFiles, Mutation, Schema, Value, with_local_engine};
use hsearch_format::{
    CommitToken, Durability, Id, Layout, ManifestRoot, PinRecord, StructuralHead,
};
use hsearch_index::barrier::{Cut, Replay};
use hsearch_lane::{LaneId, block_on, create_head, read_head};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    ByteRange, Error as StoreError, LargeObjectWriter, MetadataStore, Mutation as Put,
    ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload, RequestContext, VersionToken,
    WriteReceipt,
};
use hsearch_store::compact::{Compactor, Error as CompactError};
use hsearch_store::pins::{Error as PinError, Pins};
use hsearch_store::publish::{
    AnyDependencies, BaseSpec, Error as PublishError, HeadRead, Published, Publisher,
};
use hsearch_store::sweep::{
    Config, Error as SweepError, Inventory, Listed, Mode, ShardRef, Sweeper,
};

const INDEX: Id = [7; 16];
const SHARD: Id = [8; 16];

/// The in-memory store, keeping the list of what it holds (the inventory a provider's LIST gives),
/// and able to refuse removals after a count.
struct Tracked {
    inner: MemoryStore,
    listing: Mutex<BTreeMap<String, (VersionToken, u64)>>,
    clock: AtomicU64,
    /// Removals allowed before every removal fails; negative for no limit.
    removals: AtomicI64,
    pending_upload: Mutex<Option<String>>,
}

impl Tracked {
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
            listing: Mutex::new(BTreeMap::new()),
            clock: AtomicU64::new(1_000),
            removals: AtomicI64::new(-1),
            pending_upload: Mutex::new(None),
        })
    }
    fn note(&self, key: &ObjectKey, version: &VersionToken) {
        let at = self.clock.load(Ordering::SeqCst);
        self.listing.lock().unwrap().insert(
            String::from_utf8(key.as_bytes().to_vec()).unwrap(),
            (version.clone(), at),
        );
    }
    fn keys(&self, prefix: &str) -> Vec<String> {
        self.listing
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect()
    }
    fn count(&self, prefix: &str) -> usize {
        self.keys(prefix).len()
    }
    /// Objects held, not counting the boundary (a sweep publishes it before it can refuse).
    fn held(&self) -> usize {
        self.keys("")
            .iter()
            .filter(|k| !k.ends_with("gc-boundary"))
            .count()
    }
}

impl ObjectBackend for Tracked {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            let receipt = self.inner.create(c, r).await?;
            self.note(&r.key, &receipt.object.version);
            Ok(receipt)
        })
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

impl MetadataStore for Tracked {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Put,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            let receipt = self.inner.compare_exchange(c, r).await?;
            self.note(&r.key, &receipt.object.version);
            Ok(receipt)
        })
    }
    fn resolve<'a>(&'a self, c: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        self.inner.resolve(c, r)
    }
    fn remove_version<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            let left = self.removals.load(Ordering::SeqCst);
            if left == 0 {
                return Err(StoreError::Unavailable);
            }
            if left > 0 {
                self.removals.store(left - 1, Ordering::SeqCst);
            }
            self.inner.remove_version(c, k, v).await?;
            self.listing
                .lock()
                .unwrap()
                .remove(&String::from_utf8(k.as_bytes().to_vec()).unwrap());
            Ok(())
        })
    }
}

impl LargeObjectWriter for Tracked {
    type Upload = <MemoryStore as LargeObjectWriter>::Upload;
    fn begin_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
    ) -> Operation<'a, Self::Upload> {
        *self.pending_upload.lock().unwrap() =
            Some(String::from_utf8(k.as_bytes().to_vec()).unwrap());
        self.inner.begin_upload(c, k)
    }
    fn put_part<'a>(
        &'a self,
        c: &'a RequestContext,
        u: &'a mut Self::Upload,
        b: &'a [u8],
    ) -> Operation<'a, ()> {
        self.inner.put_part(c, u, b)
    }
    fn complete_upload<'a>(
        &'a self,
        c: &'a RequestContext,
        u: &'a Self::Upload,
    ) -> Operation<'a, (VersionToken, u64)> {
        Box::pin(async move {
            let (version, length) = self.inner.complete_upload(c, u).await?;
            if let Some(key) = self.pending_upload.lock().unwrap().take() {
                let at = self.clock.load(Ordering::SeqCst);
                self.listing
                    .lock()
                    .unwrap()
                    .insert(key, (version.clone(), at));
            }
            Ok((version, length))
        })
    }
    fn abort_upload<'a>(&'a self, c: &'a RequestContext, u: &'a Self::Upload) -> Operation<'a, ()> {
        self.inner.abort_upload(c, u)
    }
}

struct Inv(Arc<Tracked>);

impl Inventory for Inv {
    fn list(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Listed>, StoreError> {
        let map = self.0.listing.lock().unwrap();
        Ok(map
            .iter()
            .filter(|(k, _)| k.starts_with(prefix) && after.is_none_or(|a| k.as_str() > a))
            .take(limit)
            .map(|(k, (v, t))| Listed {
                key: k.clone(),
                version: v.clone(),
                modified_unix_ms: *t,
            })
            .collect())
    }
}

fn layout() -> Layout {
    Layout::new("test").unwrap()
}
fn lane() -> LaneId {
    LaneId::new([9; 16])
}
fn token(n: u8) -> CommitToken {
    CommitToken::new([n; 16]).unwrap()
}
fn cut(n: u64) -> Cut {
    let mut replay = Replay::new(0);
    for seq in 0..n {
        replay.begin(seq).unwrap();
        replay.done(seq).unwrap();
    }
    replay.cut().unwrap()
}
fn spec<'a>(covered: u64, attempt: u8, previous: Option<&'a ManifestRoot>) -> BaseSpec<'a> {
    BaseSpec {
        index: INDEX,
        shard: SHARD,
        incarnation: 1,
        engine_format: "lucene-test",
        mapping: [5; 32],
        crypto_envelope: [6; 16],
        cut: cut(covered),
        committed: covered.max(previous.map_or(0, |p| p.covered)),
        previous,
        attempt: token(attempt),
    }
}
fn shard_ref() -> ShardRef {
    ShardRef {
        shard: SHARD,
        incarnation: 1,
        lane: lane(),
    }
}
fn config(mode: Mode, now: u64) -> Config {
    Config {
        mode,
        now_unix_ms: now,
        page: 1000,
        max_tracked: 1 << 20,
        max_roots: 1 << 20,
        max_deletes: usize::MAX,
        orphan_grace_ms: None,
    }
}
fn head_of(store: &Arc<Tracked>) -> (StructuralHead, VersionToken) {
    block_on(read_head(&**store, &layout(), &lane())).unwrap()
}

/// One scratch commit, then `f` with its native files on disk.
fn with_commit<R>(label: &str, f: impl FnOnce(&LocalFiles, &[String]) -> R) -> R {
    let dir = std::env::temp_dir().join(format!("hsearch-gc-{label}-{}", std::process::id()));
    let schema = Schema::new(1, &[("tag", Kind::Keyword)]).unwrap();
    let fields = [("tag", Value::String("x"))];
    let result = with_local_engine(&dir, schema.clone(), |engine, local| {
        let puts: Vec<Mutation<'_>> = (0..4)
            .map(|i| Mutation::Put {
                id: ["a", "b", "c", "d"][i],
                version: 1,
                fields: &fields,
            })
            .collect();
        engine.apply(schema.prepare("r", &puts).unwrap()).unwrap();
        engine.commit().unwrap();
        f(local, &local.names().unwrap())
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

struct World {
    store: Arc<Tracked>,
    publisher: Publisher<Tracked>,
    compactor: Compactor<Tracked>,
}

impl World {
    fn new() -> Self {
        let store = Tracked::new();
        block_on(create_head(&*store, &layout(), &lane(), Durability::Object)).unwrap();
        let t = Duration::from_secs(30);
        Self {
            publisher: Publisher::new(Arc::clone(&store), layout(), 5 << 20, t),
            compactor: Compactor::new(Arc::clone(&store), layout(), 5 << 20, t),
            store,
        }
    }
    fn pins(&self) -> Pins<Tracked> {
        Pins::new(Arc::clone(&self.store), layout(), Duration::from_secs(30))
    }
    fn sweeper(&self) -> Sweeper<Tracked, Inv> {
        Sweeper::new(
            Arc::clone(&self.store),
            layout(),
            Inv(Arc::clone(&self.store)),
            Duration::from_secs(30),
        )
    }
    /// Publish and name a writer's base (the cover rises with it).
    fn write(
        &self,
        f: &LocalFiles,
        names: &[String],
        covered: u64,
        attempt: u8,
        prev: Option<&ManifestRoot>,
    ) -> Published {
        let s = spec(covered, attempt, prev);
        let p = self
            .publisher
            .publish(f, names, &s, &AnyDependencies)
            .unwrap();
        let (head, version) = head_of(&self.store);
        self.publisher
            .activate(
                &HeadRead {
                    lane: &lane(),
                    head: &head,
                    version: &version,
                },
                &p,
                &s,
                covered,
                &AnyDependencies,
            )
            .unwrap();
        p
    }
    /// Compact the base the head names (a rewrite of the same history).
    fn compact(
        &self,
        f: &LocalFiles,
        names: &[String],
        source: &ManifestRoot,
        attempt: u8,
    ) -> Result<Published, CompactError> {
        let (head, version) = head_of(&self.store);
        let s = spec(source.covered, attempt, Some(source));
        self.compactor
            .compact(
                f,
                names,
                &s,
                &HeadRead {
                    lane: &lane(),
                    head: &head,
                    version: &version,
                },
                &AnyDependencies,
            )
            .map(|(p, _, _)| p)
    }
    fn readable(&self, p: &Published) -> Result<ManifestRoot, PublishError> {
        self.publisher
            .verify_root(&INDEX, &SHARD, 1, &p.manifest, &AnyDependencies)
    }
    fn sweep(&self, mode: Mode, now: u64) -> Result<hsearch_store::sweep::Report, SweepError> {
        self.sweeper()
            .run(&INDEX, &[shard_ref()], &config(mode, now))
    }
}

fn pin_for(p: &Published, n: u8, expires: u64) -> PinRecord {
    PinRecord {
        pin: [n; 16],
        index: INDEX,
        manifest: p.manifest,
        manifest_generation: p.root.generation,
        expires_unix_ms: expires,
        token: token(100 + n),
    }
}

#[test]
fn a_replaced_base_is_reclaimed_and_the_live_one_is_untouched() {
    with_commit("reclaim", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let b2 = w.compact(f, names, &b1.root, 2).unwrap();
        assert_eq!(b2.root.generation, 2);
        assert_eq!(b2.root.covered, 3);
        let (head, _) = head_of(&w.store);
        assert_eq!(head.checkpoint, Some(b2.manifest));
        assert_eq!(head.cover, 3);
        assert_eq!(head.publication_generation, 2);
        let before = w.store.count("");
        // A dry run changes nothing and reports exactly what a sweep then does.
        let dry = w.sweep(Mode::DryRun, 2_000).unwrap();
        assert_eq!(w.store.count(""), before);
        assert!(dry.complete);
        assert_eq!((dry.roots_swept, dry.roots_kept), (1, 1));
        assert_eq!(dry.files_swept, names.len());
        assert_eq!(dry.pages_swept, 1);
        assert_eq!(dry.requests.deletes, 0);
        assert!(w.readable(&b1).is_ok());
        let started = std::time::Instant::now();
        let done = w.sweep(Mode::Sweep, 2_000).unwrap();
        eprintln!(
            "sweep of one replaced base ({} files): {:?} in {:?}",
            names.len(),
            done,
            started.elapsed()
        );
        assert_eq!(
            (done.roots_swept, done.pages_swept, done.files_swept),
            (dry.roots_swept, dry.pages_swept, dry.files_swept)
        );
        assert_eq!(done.requests.deletes as usize, 1 + 1 + names.len());
        assert!(w.readable(&b2).is_ok(), "the live root stays readable");
        assert!(w.readable(&b1).is_err(), "the replaced base is gone");
        assert_eq!(w.store.count(&layout().payload_prefix(&INDEX)), names.len());
        // Idempotent: a second sweep has nothing left.
        let again = w.sweep(Mode::Sweep, 2_000).unwrap();
        assert_eq!(
            (again.roots_swept, again.pages_swept, again.files_swept),
            (0, 0, 0)
        );
    });
}

#[test]
fn a_pin_holds_a_generation_until_it_expires() {
    with_commit("pin", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let pin = w
            .pins()
            .register(&pin_for(&b1, 1, 5_000), &SHARD, 1, 2_000)
            .unwrap();
        assert!(pin.usable(2_000, 100) && !pin.usable(4_950, 100));
        let b2 = w.compact(f, names, &b1.root, 2).unwrap();
        let held = w.sweep(Mode::Sweep, 2_000).unwrap();
        assert_eq!((held.roots_swept, held.pins_active), (0, 1));
        assert!(w.readable(&b1).is_ok(), "pinned base survives the sweep");
        // The instant a pin expires it stops protecting (strictly after its expiry).
        let at_expiry = w.sweep(Mode::DryRun, 5_000).unwrap();
        assert_eq!(at_expiry.roots_swept, 1);
        let before_expiry = w.sweep(Mode::DryRun, 4_999).unwrap();
        assert_eq!(before_expiry.roots_swept, 0);
        let freed = w.sweep(Mode::Sweep, 5_001).unwrap();
        assert_eq!(
            (freed.roots_swept, freed.pins_expired, freed.pins_active),
            (1, 1, 0)
        );
        assert!(w.readable(&b1).is_err());
        assert!(w.readable(&b2).is_ok());
        assert_eq!(w.store.count(&layout().pin_prefix(&INDEX)), 0);
    });
}

#[test]
fn a_pin_below_the_boundary_is_refused_and_leaves_nothing() {
    with_commit("late-pin", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let _b2 = w.compact(f, names, &b1.root, 2).unwrap();
        // The collector has published its boundary but not yet deleted b1's root (a budget of 0).
        let mut cfg = config(Mode::Sweep, 2_000);
        cfg.max_deletes = 0;
        w.sweeper().run(&INDEX, &[shard_ref()], &cfg).unwrap();
        // The reader read the head before the compaction and registers after that.
        let late = w.pins().register(&pin_for(&b1, 2, 9_000), &SHARD, 1, 2_100);
        assert!(
            matches!(late, Err(PinError::Swept { boundary: 2 })),
            "{late:?}"
        );
        assert_eq!(w.store.count(&layout().pin_prefix(&INDEX)), 0);
        // After the sweep the root is gone: a pin on it names nothing.
        w.sweep(Mode::Sweep, 2_000).unwrap();
        let gone = w.pins().register(&pin_for(&b1, 2, 9_000), &SHARD, 1, 2_100);
        assert!(matches!(gone, Err(PinError::Mismatch)), "{gone:?}");
        // A wrong shard, or a generation the root does not have, is refused before any write.
        let live = head_of(&w.store).0.checkpoint.unwrap();
        let mut rec = pin_for(&b1, 5, 9_000);
        rec.manifest = live;
        rec.manifest_generation = 2;
        assert!(matches!(
            w.pins().register(&rec, &[1; 16], 1, 2_100),
            Err(PinError::Mismatch)
        ));
        rec.manifest_generation = 9;
        assert!(matches!(
            w.pins().register(&rec, &SHARD, 1, 2_100),
            Err(PinError::Mismatch)
        ));
        assert_eq!(w.store.count(&layout().pin_prefix(&INDEX)), 0);
        // A pin already past its expiry is refused without a write.
        assert!(matches!(
            w.pins().register(&pin_for(&b1, 3, 100), &SHARD, 1, 2_100),
            Err(PinError::Expired)
        ));
        // Pinning the current generation works, and releasing twice is fine.
        let live = head_of(&w.store).0.checkpoint.unwrap();
        let mut rec = pin_for(&b1, 4, 9_000);
        rec.manifest = live;
        rec.manifest_generation = 2;
        w.pins().register(&rec, &SHARD, 1, 2_100).unwrap();
        w.pins().release(&INDEX, &rec.pin).unwrap();
        w.pins().release(&INDEX, &rec.pin).unwrap();
    });
}

#[test]
fn a_publisher_suspended_across_a_sweep_cannot_name_a_swept_generation() {
    with_commit("suspended", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        // A compactor reads the head, publishes its replacement, and is suspended before naming it.
        let (stale_head, stale_version) = head_of(&w.store);
        let stale_spec = spec(b1.root.covered, 10, Some(&b1.root));
        let stalled = w
            .publisher
            .publish(f, names, &stale_spec, &AnyDependencies)
            .unwrap();
        // Meanwhile another compaction wins and a third base follows; a sweep then runs.
        let b2 = w.compact(f, names, &b1.root, 2).unwrap();
        let b3 = w.compact(f, names, &b2.root, 3).unwrap();
        let report = w.sweep(Mode::Sweep, 2_000).unwrap();
        assert_eq!(
            report.roots_swept, 3,
            "b1, b2 and the stalled attempt's root are all below the boundary"
        );
        assert!(
            w.readable(&stalled).is_err(),
            "the stalled attempt's objects were swept"
        );
        // It resumes with its old head read: refused by the boundary before any write.
        let writes_before = w.store.count("");
        let resumed = w.compactor.finish(
            &HeadRead {
                lane: &lane(),
                head: &stale_head,
                version: &stale_version,
            },
            &stalled,
            &stale_spec,
            &AnyDependencies,
        );
        assert!(
            matches!(resumed, Err(CompactError::Swept { boundary: 3 })),
            "{resumed:?}"
        );
        assert_eq!(w.store.count(""), writes_before);
        assert_eq!(head_of(&w.store).0.checkpoint, Some(b3.manifest));
        assert!(w.readable(&b3).is_ok());
        // Publishing straight through the writer's path with the stale head fails on the swept
        // files, whatever the boundary says.
        let direct = w.publisher.replace(
            &HeadRead {
                lane: &lane(),
                head: &stale_head,
                version: &stale_version,
            },
            &stalled,
            &stale_spec,
            &AnyDependencies,
        );
        assert!(
            matches!(
                direct,
                Err(PublishError::Store(_) | PublishError::Broken(_))
            ),
            "{direct:?}"
        );
    });
}

#[test]
fn a_stalled_compaction_resuming_on_a_fresh_head_cannot_replace_a_newer_base() {
    with_commit("fresh-resume", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        // The compaction publishes generation 2 from b1 and stalls.
        let stale_spec = spec(b1.root.covered, 10, Some(&b1.root));
        let stalled = w
            .publisher
            .publish(f, names, &stale_spec, &AnyDependencies)
            .unwrap();
        // A writer publishes generation 2 with more history, and a sweep sets the boundary to 2.
        let b2 = w.write(f, names, 5, 2, Some(&b1.root));
        w.sweep(Mode::Sweep, 2_000).unwrap();
        // Resuming with a head read taken now passes the boundary (2 is not below 2) and the swap
        // would win, but the head no longer names the source: refused, nothing changes.
        let (head, version) = head_of(&w.store);
        let at = HeadRead {
            lane: &lane(),
            head: &head,
            version: &version,
        };
        let r = w
            .compactor
            .finish(&at, &stalled, &stale_spec, &AnyDependencies);
        assert!(matches!(r, Err(CompactError::SourceMoved)), "{r:?}");
        assert_eq!(head_of(&w.store).0.checkpoint, Some(b2.manifest));
        // The head rule itself refuses a replacement that names the wrong source.
        let wrong = w
            .publisher
            .replace(&at, &stalled, &stale_spec, &AnyDependencies);
        assert!(wrong.is_err(), "{wrong:?}");
        assert_eq!(head_of(&w.store).0.checkpoint, Some(b2.manifest));
    });
}

#[test]
fn a_compaction_cannot_replace_a_base_that_moved_during_the_merge() {
    with_commit("moved", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let (stale_head, stale_version) = head_of(&w.store);
        // A writer publishes a newer base (more history: deletes and updates the merge never saw).
        let b2 = w.write(f, names, 5, 2, Some(&b1.root));
        let source = &b1.root;
        let s = spec(source.covered, 3, Some(source));
        // A compactor holding the old head read: the head moved, the swap loses, nothing is named.
        let lost = w.compactor.compact(
            f,
            names,
            &s,
            &HeadRead {
                lane: &lane(),
                head: &stale_head,
                version: &stale_version,
            },
            &AnyDependencies,
        );
        assert!(
            matches!(lost, Err(CompactError::Publish(PublishError::Lost))),
            "{lost:?}"
        );
        // One that reads the head afresh sees the source is not what the head names.
        let (head, version) = head_of(&w.store);
        let moved = w.compactor.compact(
            f,
            names,
            &s,
            &HeadRead {
                lane: &lane(),
                head: &head,
                version: &version,
            },
            &AnyDependencies,
        );
        assert!(matches!(moved, Err(CompactError::SourceMoved)), "{moved:?}");
        assert_eq!(head_of(&w.store).0.checkpoint, Some(b2.manifest));
    });
}

#[test]
fn a_compaction_must_keep_coverage_mapping_and_key() {
    with_commit("shape", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let (head, version) = head_of(&w.store);
        let at = HeadRead {
            lane: &lane(),
            head: &head,
            version: &version,
        };
        let writes = w.store.count("");
        let mut more = spec(4, 2, Some(&b1.root));
        more.committed = 4;
        let r = w.compactor.compact(f, names, &more, &at, &AnyDependencies);
        assert!(matches!(r, Err(CompactError::CoverageChanged)), "{r:?}");
        let mut other_mapping = spec(3, 3, Some(&b1.root));
        other_mapping.mapping = [9; 32];
        let r = w
            .compactor
            .compact(f, names, &other_mapping, &at, &AnyDependencies);
        assert!(matches!(r, Err(CompactError::Incompatible)), "{r:?}");
        let mut other_key = spec(3, 4, Some(&b1.root));
        other_key.crypto_envelope = [1; 16];
        let r = w
            .compactor
            .compact(f, names, &other_key, &at, &AnyDependencies);
        assert!(matches!(r, Err(CompactError::Incompatible)), "{r:?}");
        let none = spec(3, 5, None);
        let r = w.compactor.compact(f, names, &none, &at, &AnyDependencies);
        assert!(matches!(r, Err(CompactError::SourceMoved)), "{r:?}");
        assert_eq!(w.store.count(""), writes, "refusals write nothing");
        assert_eq!(head_of(&w.store).0.checkpoint, Some(b1.manifest));
    });
}

#[test]
fn a_sweep_stopped_anywhere_resumes_and_never_loses_the_live_root() {
    with_commit("restart", |f, names| {
        // Fail the Nth removal for every N until a sweep completes without failing.
        for n in 0..(names.len() as i64 + 6) {
            let w = World::new();
            let b1 = w.write(f, names, 3, 1, None);
            let b2 = w.compact(f, names, &b1.root, 2).unwrap();
            w.store.removals.store(n, Ordering::SeqCst);
            let first = w.sweep(Mode::Sweep, 2_000);
            w.store.removals.store(-1, Ordering::SeqCst);
            assert!(
                w.readable(&b2).is_ok(),
                "live root readable after a stop at removal {n}"
            );
            if let Err(e) = &first {
                assert!(matches!(e, SweepError::Store(_)), "{e:?}");
            }
            let second = w.sweep(Mode::Sweep, 2_000).unwrap();
            assert!(second.complete);
            assert_eq!(
                w.store.count(&layout().page_prefix(&INDEX, &SHARD, 1)),
                1,
                "stop {n}"
            );
            assert_eq!(
                w.store.count(&layout().manifest_prefix(&INDEX, &SHARD, 1)),
                1,
                "stop {n}"
            );
            assert_eq!(
                w.store.count(&layout().payload_prefix(&INDEX)),
                names.len(),
                "stop {n}"
            );
            assert!(w.readable(&b2).is_ok());
        }
    });
}

#[test]
fn a_delete_budget_splits_a_sweep_into_runs_that_converge() {
    with_commit("budget", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let b2 = w.compact(f, names, &b1.root, 2).unwrap();
        let mut runs = 0;
        loop {
            let mut cfg = config(Mode::Sweep, 2_000);
            cfg.max_deletes = 2;
            let r = w.sweeper().run(&INDEX, &[shard_ref()], &cfg).unwrap();
            runs += 1;
            assert!(w.readable(&b2).is_ok());
            assert!(r.requests.deletes <= 2);
            if r.complete {
                break;
            }
            assert!(runs < 20);
        }
        assert!(
            runs > 1,
            "a budget of 2 cannot finish {} objects in one run",
            names.len() + 2
        );
        assert!(w.readable(&b1).is_err());
        assert_eq!(
            w.store.count(&layout().manifest_prefix(&INDEX, &SHARD, 1)),
            1
        );
        // Roots go last: no run left the base's files or page behind by deleting its root early.
        assert_eq!(w.store.count(&layout().page_prefix(&INDEX, &SHARD, 1)), 1);
        assert_eq!(w.store.count(&layout().payload_prefix(&INDEX)), names.len());
    });
}

#[test]
fn an_unreadable_view_deletes_nothing() {
    with_commit("unreadable", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let b2 = w.compact(f, names, &b1.root, 2).unwrap();
        let all = w.store.held();
        // A pin that does not decode is an unknown reference.
        let key = layout().pin(&INDEX, &[1; 16]);
        let k = ObjectKey::new(key.as_bytes()).unwrap();
        let ctx = RequestContext::new(
            Duration::from_secs(5),
            hsearch_object::Cancellation::default(),
            hsearch_object::Budget {
                request_bytes: 4096,
                response_bytes: 4096,
                attempts: 4,
            },
        )
        .unwrap();
        let put = Put {
            request_id: hsearch_object::RequestId::new(b"junk-pin").unwrap(),
            key: k,
            payload: Payload::new(b"not a pin").unwrap(),
            metadata: hsearch_object::Metadata::new(b"").unwrap(),
            precondition: hsearch_object::Precondition::Absent,
        };
        block_on(w.store.create(&ctx, &put)).unwrap();
        let r = w.sweep(Mode::Sweep, 2_000);
        assert!(matches!(r, Err(SweepError::Format(_))), "{r:?}");
        assert_eq!(w.store.held(), all + 1, "nothing was deleted");
        assert!(w.readable(&b1).is_ok());
        // A head that names a root that is not there stops a sweep before it deletes anything.
        let (v, _) = {
            let root_key = layout().manifest_root(&INDEX, &SHARD, 1, &b2.manifest);
            let listing = w.store.listing.lock().unwrap();
            listing.get(&root_key).cloned().unwrap()
        };
        let root_key = layout().manifest_root(&INDEX, &SHARD, 1, &b2.manifest);
        w.store.removals.store(-1, Ordering::SeqCst);
        block_on(
            w.store
                .remove_version(&ctx, &ObjectKey::new(root_key.as_bytes()).unwrap(), &v),
        )
        .unwrap();
        w.store.listing.lock().unwrap().remove(&root_key);
        let before = w.store.held();
        let r = w.sweep(Mode::Sweep, 2_000);
        assert!(matches!(r, Err(SweepError::LiveRootMissing)), "{r:?}");
        assert_eq!(w.store.held(), before);
    });
}

#[test]
fn orphans_are_reclaimed_only_when_asked_and_old() {
    with_commit("orphans", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        // An attempt that uploaded files and pages but never wrote its root.
        w.store.clock.store(1_500, Ordering::SeqCst);
        let lost = w
            .publisher
            .publish(f, names, &spec(3, 7, Some(&b1.root)), &AnyDependencies)
            .unwrap();
        let root_key = layout().manifest_root(&INDEX, &SHARD, 1, &lost.manifest);
        let (v, _) = w
            .store
            .listing
            .lock()
            .unwrap()
            .get(&root_key)
            .cloned()
            .unwrap();
        let ctx = RequestContext::new(
            Duration::from_secs(5),
            hsearch_object::Cancellation::default(),
            hsearch_object::Budget {
                request_bytes: 4096,
                response_bytes: 4096,
                attempts: 4,
            },
        )
        .unwrap();
        block_on(
            w.store
                .remove_version(&ctx, &ObjectKey::new(root_key.as_bytes()).unwrap(), &v),
        )
        .unwrap();
        w.store.listing.lock().unwrap().remove(&root_key);
        let payloads = w.store.count(&layout().payload_prefix(&INDEX));
        assert_eq!(payloads, 2 * names.len());
        // Off by default: nothing is swept.
        assert_eq!(w.sweep(Mode::Sweep, 10_000_000).unwrap().orphan_files, 0);
        assert_eq!(w.store.count(&layout().payload_prefix(&INDEX)), payloads);
        // On, but the upload is younger than the grace: kept (it may be in flight).
        let mut cfg = config(Mode::Sweep, 1_600);
        cfg.orphan_grace_ms = Some(500);
        let young = w.sweeper().run(&INDEX, &[shard_ref()], &cfg).unwrap();
        assert_eq!((young.orphan_files, young.orphan_pages), (0, 0));
        // Old enough: the orphan files and page go, the live base stays whole.
        cfg.now_unix_ms = 2_100;
        let old = w.sweeper().run(&INDEX, &[shard_ref()], &cfg).unwrap();
        assert_eq!((old.orphan_files, old.orphan_pages), (names.len(), 1));
        assert!(w.readable(&b1).is_ok());
        assert_eq!(w.store.count(&layout().payload_prefix(&INDEX)), names.len());
    });
}

#[test]
fn a_sweep_that_would_track_too_much_refuses_and_deletes_nothing() {
    with_commit("bound", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let _ = w.compact(f, names, &b1.root, 2).unwrap();
        let all = w.store.held();
        let mut cfg = config(Mode::Sweep, 2_000);
        cfg.max_tracked = 2;
        let r = w.sweeper().run(&INDEX, &[shard_ref()], &cfg);
        assert!(matches!(r, Err(SweepError::Budget(_))), "{r:?}");
        assert_eq!(w.store.held(), all);
    });
}

#[test]
fn requests_scale_with_objects_not_with_time() {
    with_commit("cost", |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let _b2 = w.compact(f, names, &b1.root, 2).unwrap();
        // With a page of 1, every key costs a LIST page of its own: the count is exact.
        let mut cfg = config(Mode::DryRun, 2_000);
        cfg.page = 1;
        let r = w.sweeper().run(&INDEX, &[shard_ref()], &cfg).unwrap();
        let payloads = w.store.count(&layout().payload_prefix(&INDEX));
        let expected = 1 /* pins: none */ + 3 /* roots: 2 listed, 1 empty tail */ + (payloads + 1) + 3 /* pages */;
        assert_eq!(r.requests.lists as usize, expected);
        assert_eq!(r.requests.writes, 0);
        assert_eq!(r.requests.deletes, 0);
        // A second idle run does the same work: no hidden growth.
        let again = w.sweeper().run(&INDEX, &[shard_ref()], &cfg).unwrap();
        assert_eq!(again.requests, r.requests);
    });
}

/// The race the `pin_gc` model searched: readers pin the generation they read while a compactor and
/// a collector run. A pin that is accepted must keep its base readable until released.
#[test]
fn readers_pinning_while_compaction_and_sweeps_run_never_lose_their_base() {
    with_commit("race", |f, names| {
        let w = Arc::new(World::new());
        let first = w.write(f, names, 3, 1, None);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let accepted = Arc::new(AtomicU64::new(0));
        let refused = Arc::new(AtomicU64::new(0));
        let mut readers = Vec::new();
        for r in 0..3u8 {
            let (w, stop, accepted, refused) = (
                Arc::clone(&w),
                Arc::clone(&stop),
                Arc::clone(&accepted),
                Arc::clone(&refused),
            );
            readers.push(std::thread::spawn(move || {
                let mut n = 0u8;
                while !stop.load(Ordering::SeqCst)
                    && accepted.load(Ordering::SeqCst) + refused.load(Ordering::SeqCst) < 1500
                {
                    n = n.wrapping_add(1);
                    let (head, _) = head_of(&w.store);
                    let Some(manifest) = head.checkpoint else {
                        continue;
                    };
                    let Ok(root) =
                        w.publisher
                            .verify_root(&INDEX, &SHARD, 1, &manifest, &AnyDependencies)
                    else {
                        continue; // read the head, then it was swept: read again
                    };
                    let rec = PinRecord {
                        pin: [50 + r, n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                        index: INDEX,
                        manifest,
                        manifest_generation: root.generation,
                        expires_unix_ms: u64::MAX,
                        token: token(1 + (n % 200)),
                    };
                    match w.pins().register(&rec, &SHARD, 1, 0) {
                        Ok(_) => {
                            accepted.fetch_add(1, Ordering::SeqCst);
                            for _ in 0..3 {
                                assert!(
                                    w.publisher
                                        .verify_root(&INDEX, &SHARD, 1, &manifest, &AnyDependencies)
                                        .is_ok(),
                                    "an accepted pin lost its base (generation {})",
                                    root.generation
                                );
                                std::thread::yield_now();
                            }
                            w.pins().release(&INDEX, &rec.pin).unwrap();
                        }
                        Err(PinError::Swept { .. } | PinError::Mismatch) => {
                            refused.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) => panic!("{e:?}"),
                    }
                }
            }));
        }
        let mut root = first.root;
        for round in 0..14u8 {
            let p = w.compact(f, names, &root, 2 + round).unwrap();
            root = p.root;
            let r = w.sweep(Mode::Sweep, 1).unwrap();
            assert!(r.complete);
            std::thread::yield_now();
        }
        stop.store(true, Ordering::SeqCst);
        for t in readers {
            t.join().unwrap();
        }
        let live = head_of(&w.store).0.checkpoint.unwrap();
        assert!(
            w.publisher
                .verify_root(&INDEX, &SHARD, 1, &live, &AnyDependencies)
                .is_ok()
        );
        eprintln!(
            "accepted {} refused {}",
            accepted.load(Ordering::SeqCst),
            refused.load(Ordering::SeqCst)
        );
        assert!(
            accepted.load(Ordering::SeqCst) > 0,
            "the race never exercised an accepted pin"
        );
    });
}
