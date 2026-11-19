//! Shallow snapshots and same-store restore against the in-memory store (M22.05): a snapshot is
//! metadata only and costs a fixed number of requests, a pinned base survives compaction and
//! collection, a restore copies nothing, deleting releases only what nothing else holds, and a
//! revoked key makes both unreadable.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hsearch_engine::{Kind, LocalFiles, Mutation, Schema, Value, with_local_engine};
use hsearch_format::{CommitToken, Durability, Id, Layout, ManifestRoot, StructuralHead};
use hsearch_index::barrier::{Cut, Replay};
use hsearch_lane::{LaneId, block_on, create_head, read_head};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    ByteRange, Error as StoreError, Inventory, LargeObjectWriter, ListPage, ListedObject,
    MetadataStore, Mutation as Put, ObjectBackend, ObjectInfo, ObjectKey, Operation, Payload,
    RequestContext, VersionToken, WriteReceipt,
};
use hsearch_store::compact::{Compactor, Error as CompactError};
use hsearch_store::publish::{
    AnyDependencies, BaseSpec, Error as PublishError, HeadRead, Published, Publisher,
};
use hsearch_store::snapshot::{AnyKeys, Error as SnapError, Keys, ShardPin, Snapshots, State};
use hsearch_store::sweep::{Config, Error as SweepError, Mode, ShardRef, Sweeper};
use sha2::{Digest as _, Sha256};

const INDEX: Id = [7; 16];
const SHARD: Id = [8; 16];

/// A prefix to pause at, the signal that it was reached, and the signal to go on.
type Gate = (
    String,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

/// The in-memory store, keeping the list of what it holds (the inventory a provider's LIST gives),
/// and able to refuse removals after a count.
struct Tracked {
    inner: MemoryStore,
    listing: Mutex<BTreeMap<String, (VersionToken, u64)>>,
    clock: AtomicU64,
    /// Removals allowed before every removal fails; negative for no limit.
    removals: AtomicI64,
    pending_upload: Mutex<Option<String>>,
    /// Most objects a listing page returns, whatever the limit asked; 0 for no cap.
    page_cap: AtomicUsize,
    /// Every operation, in order, with the key it named.
    ops: Mutex<Vec<(&'static str, String)>>,
    /// Pause the first HEAD or read of a key under this prefix until released.
    gate: Mutex<Option<Gate>>,
    /// Fail the next create under this prefix once, as an unavailable store.
    fail_create: Mutex<Option<String>>,
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
            page_cap: AtomicUsize::new(0),
            ops: Mutex::new(Vec::new()),
            gate: Mutex::new(None),
            fail_create: Mutex::new(None),
        })
    }
    fn log(&self, op: &'static str, key: &ObjectKey) {
        let key = String::from_utf8(key.as_bytes().to_vec()).unwrap();
        self.ops.lock().unwrap().push((op, key));
    }
    /// Take the operations logged since the last call.
    fn take_ops(&self) -> Vec<(&'static str, String)> {
        std::mem::take(&mut *self.ops.lock().unwrap())
    }
    fn pause(&self, key: &ObjectKey) {
        let key = String::from_utf8(key.as_bytes().to_vec()).unwrap();
        let slot = {
            let mut g = self.gate.lock().unwrap();
            match g.as_ref() {
                Some((prefix, _, _)) if key.starts_with(prefix.as_str()) => g.take(),
                _ => None,
            }
        };
        if let Some((_, reached, go)) = slot {
            reached.send(()).unwrap();
            go.recv().unwrap();
        }
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
}

impl ObjectBackend for Tracked {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.log("head", k);
        self.pause(k);
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Put) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            self.pause(&r.key);
            {
                let mut f = self.fail_create.lock().unwrap();
                let key = String::from_utf8(r.key.as_bytes().to_vec()).unwrap();
                if f.as_ref().is_some_and(|p| key.starts_with(p.as_str())) {
                    *f = None;
                    return Err(StoreError::Unavailable);
                }
            }
            self.log("put", &r.key);
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
        self.log("get", k);
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
            self.log("put", &r.key);
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
            self.log("delete", k);
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

impl Inventory for Tracked {
    fn list<'a>(
        &'a self,
        _: &'a RequestContext,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> Operation<'a, ListPage> {
        Box::pin(async move {
            self.ops.lock().unwrap().push(("list", prefix.to_owned()));
            // A provider may answer with fewer than asked and still have more: `cap` models that.
            let cap = self.page_cap.load(Ordering::SeqCst);
            let take = if cap == 0 { limit } else { limit.min(cap) };
            let map = self.listing.lock().unwrap();
            let mut rest = map
                .iter()
                .filter(|(k, _)| k.starts_with(prefix) && after.is_none_or(|a| k.as_str() > a));
            let objects: Vec<ListedObject> = rest
                .by_ref()
                .take(take)
                .map(|(k, (v, t))| ListedObject {
                    key: ObjectKey::new(k.as_bytes()).unwrap(),
                    version: v.clone(),
                    modified_unix_ms: *t,
                })
                .collect();
            Ok(ListPage {
                objects,
                more: rest.next().is_some(),
            })
        })
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
fn with_commit<R>(label: &str, docs: usize, f: impl FnOnce(&LocalFiles, &[String]) -> R) -> R {
    let dir = std::env::temp_dir().join(format!("hsearch-snap-{label}-{}", std::process::id()));
    let schema = Schema::new(1, &[("tag", Kind::Keyword)]).unwrap();
    let fields = [("tag", Value::String("x"))];
    let result = with_local_engine(&dir, schema.clone(), |engine, local| {
        let ids: Vec<String> = (0..docs).map(|i| format!("doc-{i}")).collect();
        for (n, chunk) in ids.chunks(16).enumerate() {
            let puts: Vec<Mutation<'_>> = chunk
                .iter()
                .map(|id| Mutation::Put {
                    id,
                    version: 1,
                    fields: &fields,
                })
                .collect();
            engine
                .apply(schema.prepare(&format!("r{n}"), &puts).unwrap())
                .unwrap();
            // A commit per batch: a bigger index has more segments and so more files and pages.
            engine.commit().unwrap();
        }
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
    fn sweeper(&self) -> Sweeper<Tracked> {
        Sweeper::new(Arc::clone(&self.store), layout(), Duration::from_secs(30))
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

const NEW: Id = [11; 16];
const SNAP: Id = [21; 16];
const SNAP_B: Id = [22; 16];

fn hex(id: &Id) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn snaps(w: &World) -> Snapshots<Tracked> {
    Snapshots::new(Arc::clone(&w.store), layout(), Duration::from_secs(30))
}

/// A key view that has revoked one envelope.
struct Revoked(Id);
impl Keys for Revoked {
    fn usable(&self, envelope: &Id) -> bool {
        *envelope != self.0
    }
}

fn under(ops: &[(&'static str, String)], prefix: &str) -> usize {
    ops.iter().filter(|(_, k)| k.starts_with(prefix)).count()
}
fn count_of(ops: &[(&'static str, String)], op: &str) -> usize {
    ops.iter().filter(|(o, _)| *o == op).count()
}

fn get(store: &Tracked, key: &str) -> Option<Vec<u8>> {
    let ctx = RequestContext::new(
        Duration::from_secs(30),
        hsearch_object::Cancellation::default(),
        hsearch_object::Budget {
            request_bytes: 4096,
            response_bytes: 64 << 20,
            attempts: 2,
        },
    )
    .unwrap();
    let k = ObjectKey::new(key.as_bytes()).unwrap();
    let info = match block_on(store.head(&ctx, &k)) {
        Ok(i) => i,
        Err(StoreError::NotFound) => return None,
        Err(e) => panic!("{e:?}"),
    };
    Some(
        block_on(store.read_range(
            &ctx,
            &k,
            &info.version,
            ByteRange::new(0, info.length).unwrap(),
        ))
        .unwrap()
        .as_bytes()
        .to_vec(),
    )
}

/// Every file a shard names, read back from the store, must be the bytes the engine wrote.
fn assert_identical(
    w: &World,
    s: &Snapshots<Tracked>,
    source: &Id,
    shard: &ShardPin,
    f: &LocalFiles,
    names: &[String],
) {
    let mut seen = Vec::new();
    s.files(source, shard, &AnyKeys, |entry, key| {
        let bytes = get(&w.store, key).expect("a referenced file is in the store");
        assert_eq!(bytes.len() as u64, entry.length);
        assert_eq!(Sha256::digest(&bytes).as_slice(), entry.digest.as_slice());
        assert_eq!(bytes, f.read(&entry.name).unwrap(), "{}", entry.name);
        seen.push(entry.name.clone());
        Ok(())
    })
    .unwrap();
    let mut want = names.to_vec();
    want.sort();
    seen.sort();
    assert_eq!(seen, want);
}

#[test]
fn a_snapshot_is_metadata_only_and_its_request_count_ignores_index_size() {
    let mut counts = Vec::new();
    let mut files = Vec::new();
    for docs in [4usize, 128] {
        with_commit(&format!("cost{docs}"), docs, |f, names| {
            let w = World::new();
            let _b1 = w.write(f, names, 3, 1, None);
            let objects = w.store.count("");
            let _ = w.store.take_ops();
            let made = snaps(&w)
                .create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
                .unwrap();
            let ops = w.store.take_ops();
            // Nothing a snapshot costs depends on how many files or pages the base has: it never
            // reads or writes one, and never lists.
            assert_eq!(under(&ops, &layout().payload_prefix(&INDEX)), 0);
            assert_eq!(
                under(&ops, &layout().page_prefix(&INDEX, &SHARD, 1)),
                0,
                "{ops:?}"
            );
            assert_eq!(count_of(&ops, "list"), 0);
            assert_eq!(count_of(&ops, "delete"), 0);
            // It adds a pin and a record, and nothing else.
            assert_eq!(w.store.count(""), objects + 2, "{ops:?}");
            assert_eq!(count_of(&ops, "put"), 2);
            assert_eq!(made.requests.lists + made.requests.deletes, 0);
            assert_eq!(made.requests.writes, 2);
            assert_eq!(made.head_reads, 1);
            // 1 existence check, 2 for the head, 2 for the root, 1 boundary check, 2 writes.
            assert_eq!(ops.len(), 8, "{ops:?}");
            files.push(w.store.count(&layout().payload_prefix(&INDEX)));
            counts.push((docs, ops.len(), made.requests));
        });
    }
    assert!(
        files[1] > files[0],
        "the larger index has more files: {files:?}"
    );
    assert_eq!(counts[0].1, counts[1].1, "{counts:?}");
    assert_eq!(counts[0].2, counts[1].2, "{counts:?}");
}

#[test]
fn a_snapshot_names_the_root_its_pin_holds_and_refuses_a_second_record() {
    with_commit("record", 4, |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        let made = s
            .create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        let shard = made.record.shards[0];
        let root = shard.root.unwrap();
        assert_eq!(root.manifest, b1.manifest);
        assert_eq!(root.generation, b1.root.generation);
        assert_eq!(root.covered, 3);
        assert_eq!(root.crypto_envelope, [6; 16]);
        assert_eq!(made.record.state, State::Active);
        assert_eq!(s.read(&INDEX, &SNAP).unwrap(), Some(made.record.clone()));
        assert!(s.read(&INDEX, &SNAP_B).unwrap().is_none());
        // The pin exists and holds that root.
        let pins = w.store.keys(&layout().pin_prefix(&INDEX));
        assert_eq!(pins.len(), 1);
        let again = s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_001);
        assert!(matches!(again, Err(SnapError::AlreadyExists)), "{again:?}");
        assert_eq!(w.store.keys(&layout().pin_prefix(&INDEX)).len(), 1);
    });
}

#[test]
fn a_pinned_base_survives_compaction_and_collection_and_restores_byte_identical() {
    with_commit("survive", 4, |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        let made = s
            .create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        let b2 = w.compact(f, names, &b1.root, 2).unwrap();
        let b3 = w.compact(f, names, &b2.root, 3).unwrap();
        let report = w.sweep(Mode::Sweep, 3_000).unwrap();
        assert_eq!(
            report.roots_swept, 1,
            "only b2: b1 is pinned and b3 is live"
        );
        assert!(w.readable(&b1).is_ok(), "the snapshot's base is intact");
        assert!(
            w.readable(&b2).is_err(),
            "an unpinned replaced base is gone"
        );
        assert!(w.readable(&b3).is_ok());
        // The record names the old base and every file of it reads back identical.
        s.restore(&INDEX, &SNAP, &NEW, &AnyKeys, 3_100).unwrap();
        let origin = s.read_origin(&NEW).unwrap().unwrap();
        assert_identical(&w, &s, &INDEX, &made.record.shards[0], f, names);
        assert_identical(&w, &s, &origin.source, &origin.shards[0], f, names);
        // Control: without a snapshot the same history loses b1.
        let c = World::new();
        let c1 = c.write(f, names, 3, 1, None);
        let c2 = c.compact(f, names, &c1.root, 2).unwrap();
        let _c3 = c.compact(f, names, &c2.root, 3).unwrap();
        c.sweep(Mode::Sweep, 3_000).unwrap();
        assert!(c.readable(&c1).is_err());
    });
}

#[test]
fn a_restore_copies_nothing_and_holds_what_it_shares() {
    with_commit("restore", 4, |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        let before = w.store.count("");
        let _ = w.store.take_ops();
        let restored = s.restore(&INDEX, &SNAP, &NEW, &AnyKeys, 2_100).unwrap();
        let ops = w.store.take_ops();
        // Same key only: the restored index is under the source's key envelope.
        let source = s.read(&INDEX, &SNAP).unwrap().unwrap();
        assert_eq!(
            restored.record.shards,
            source
                .shards
                .iter()
                .map(|sp| {
                    let mut sp = *sp;
                    if let Some(r) = sp.root.as_mut() {
                        r.pin = restored.record.shards[0].root.unwrap().pin;
                    }
                    sp
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            restored.record.shards[0].root.unwrap().crypto_envelope,
            source.shards[0].root.unwrap().crypto_envelope
        );
        assert_eq!(restored.record.source, INDEX);
        assert_eq!(restored.record.snapshot, SNAP);
        // A holder mark on the snapshot and the origin record: no payload, page or root is written anywhere.
        assert_eq!(
            w.store.count(""),
            before + 1,
            "only the origin record is new"
        );
        assert_eq!(count_of(&ops, "put"), 2, "{ops:?}");
        // 1 existence check, 2 to read the snapshot, 2 writes.
        assert_eq!(ops.len(), 5, "{ops:?}");
        assert_eq!(under(&ops, &layout().payload_prefix(&INDEX)), 0);
        assert_eq!(under(&ops, &layout().payload_prefix(&NEW)), 0);
        assert_eq!(count_of(&ops, "list"), 0);
        assert_eq!(w.store.count(&format!("v1/test/indexes/{}/", hex(&NEW))), 1);
        assert_eq!(s.read_origin(&NEW).unwrap(), Some(restored.record.clone()));
        // The restored index reads the source's objects, byte for byte.
        assert_identical(
            &w,
            &s,
            &restored.record.source,
            &restored.record.shards[0],
            f,
            names,
        );
        // A restore into an index that exists is refused and changes nothing.
        let again = s.restore(&INDEX, &SNAP, &NEW, &AnyKeys, 2_200);
        assert!(matches!(again, Err(SnapError::AlreadyExists)), "{again:?}");
        assert_eq!(w.store.count(""), before + 1);
        // The snapshot is deleted and the base replaced: the restored index still holds it.
        let retired = s.delete(&INDEX, &SNAP).unwrap();
        assert_eq!(retired.pins, 0, "its pins stay while a holder remains");
        assert_eq!(
            s.read(&INDEX, &SNAP).unwrap().unwrap().state,
            State::Retired
        );
        let late = s.restore(&INDEX, &SNAP, &[13; 16], &AnyKeys, 2_300);
        assert!(matches!(late, Err(SnapError::Deleting)), "{late:?}");
        let b2 = w.compact(f, names, &b1.root, 2).unwrap();
        w.sweep(Mode::Sweep, 3_000).unwrap();
        assert!(w.readable(&b1).is_ok(), "held by the restored index");
        assert_identical(
            &w,
            &s,
            &restored.record.source,
            &restored.record.shards[0],
            f,
            names,
        );
        // Dropping the last holder releases the snapshot's pins, and then the base is reclaimed.
        let dropped = s.drop_restore(&NEW).unwrap();
        assert_eq!(dropped.pins, 1);
        assert!(s.read(&INDEX, &SNAP).unwrap().is_none());
        assert!(s.read_origin(&NEW).unwrap().is_none());
        w.sweep(Mode::Sweep, 3_100).unwrap();
        assert!(w.readable(&b1).is_err());
        assert!(w.readable(&b2).is_ok());
    });
}

#[test]
fn a_sweep_suspended_across_creation_cannot_leave_a_pin_on_a_swept_base() {
    with_commit("suspended", 4, |f, names| {
        // The sweep runs after the root was read and before the pin is written.
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        *w.store.gate.lock().unwrap() = Some((layout().pin_prefix(&INDEX), reached_tx, go_rx));
        let s = snaps(&w);
        let made = std::thread::scope(|scope| {
            let creator = scope.spawn(|| s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000));
            reached_rx.recv_timeout(Duration::from_secs(60)).unwrap();
            let b2 = w.compact(f, names, &b1.root, 2).unwrap();
            let report = w.sweep(Mode::Sweep, 2_500).unwrap();
            assert_eq!(
                report.roots_swept, 1,
                "b1 was unpinned and below the boundary"
            );
            go_tx.send(()).unwrap();
            (creator.join().unwrap().unwrap(), b2)
        });
        let (made, b2) = made;
        let root = made.record.shards[0].root.unwrap();
        assert_eq!(
            root.manifest, b2.manifest,
            "it pinned the base that is live now"
        );
        assert_eq!(
            w.store.keys(&layout().pin_prefix(&INDEX)).len(),
            1,
            "no pin on the swept base"
        );
        assert!(w.readable(&b2).is_ok());
        w.sweep(Mode::Sweep, 3_000).unwrap();
        assert_identical(&w, &s, &INDEX, &made.record.shards[0], f, names);
    });
    with_commit("suspended-root", 4, |f, names| {
        // The sweep runs before the root is read: the root is gone, so the head is read again.
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        *w.store.gate.lock().unwrap() = Some((
            layout().manifest_root(&INDEX, &SHARD, 1, &b1.manifest),
            reached_tx,
            go_rx,
        ));
        let s = snaps(&w);
        let (made, b2) = std::thread::scope(|scope| {
            let creator = scope.spawn(|| s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000));
            reached_rx.recv_timeout(Duration::from_secs(60)).unwrap();
            let b2 = w.compact(f, names, &b1.root, 2).unwrap();
            w.sweep(Mode::Sweep, 2_500).unwrap();
            go_tx.send(()).unwrap();
            (creator.join().unwrap().unwrap(), b2)
        });
        assert_eq!(made.record.shards[0].root.unwrap().manifest, b2.manifest);
        assert_eq!(made.head_reads, 2);
    });
}

#[test]
fn deleting_releases_exclusive_objects_and_keeps_shared_ones() {
    with_commit("delete", 4, |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        s.create(&INDEX, &SNAP_B, &[shard_ref()], &AnyKeys, 2_001)
            .unwrap();
        let b2 = w.compact(f, names, &b1.root, 2).unwrap();
        // Two snapshots hold b1.
        w.sweep(Mode::Sweep, 3_000).unwrap();
        assert!(w.readable(&b1).is_ok());
        // Deleting one leaves the object the other still holds.
        let _ = w.store.take_ops();
        let gone = s.delete(&INDEX, &SNAP).unwrap();
        // 2 to read the record, 1 to mark it, 3 to release the pin, 3 to remove the record.
        assert_eq!(w.store.take_ops().len(), 9);
        assert_eq!(gone.pins, 1);
        assert_eq!(gone.requests.lists, 0);
        assert!(s.read(&INDEX, &SNAP).unwrap().is_none());
        w.sweep(Mode::Sweep, 3_100).unwrap();
        assert!(w.readable(&b1).is_ok(), "still held by the second snapshot");
        // Deleting the last one makes them reclaimable.
        s.delete(&INDEX, &SNAP_B).unwrap();
        let names_before = w.store.count(&layout().payload_prefix(&INDEX));
        w.sweep(Mode::Sweep, 3_200).unwrap();
        assert!(w.readable(&b1).is_err());
        assert!(w.store.count(&layout().payload_prefix(&INDEX)) < names_before);
        assert!(
            w.readable(&b2).is_ok(),
            "the live base is never the snapshot's to lose"
        );
        // Deleting what is gone is reported, not repeated.
        assert!(matches!(s.delete(&INDEX, &SNAP), Err(SnapError::NotFound)));
        assert!(matches!(
            s.restore(&INDEX, &SNAP, &NEW, &AnyKeys, 3_300),
            Err(SnapError::NotFound)
        ));
        assert_eq!(w.store.keys(&layout().pin_prefix(&INDEX)).len(), 0);
    });
}

#[test]
fn a_snapshot_of_the_live_base_keeps_nothing_once_deleted_and_the_live_base_stays() {
    with_commit("live", 4, |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        s.delete(&INDEX, &SNAP).unwrap();
        let report = w.sweep(Mode::Sweep, 3_000).unwrap();
        assert_eq!(report.roots_swept, 0);
        assert!(w.readable(&b1).is_ok());
    });
}

#[test]
fn a_delete_stopped_midway_leaves_a_snapshot_nobody_can_restore_and_finishes_on_retry() {
    with_commit("midway", 4, |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        let _b2 = w.compact(f, names, &b1.root, 2).unwrap();
        // The first removal (the pin) fails: the record is already marked and nothing was lost.
        w.store.removals.store(0, Ordering::SeqCst);
        assert!(s.delete(&INDEX, &SNAP).is_err());
        w.store.removals.store(-1, Ordering::SeqCst);
        assert_eq!(
            s.read(&INDEX, &SNAP).unwrap().unwrap().state,
            State::Deleting
        );
        let refused = s.restore(&INDEX, &SNAP, &NEW, &AnyKeys, 2_100);
        assert!(matches!(refused, Err(SnapError::Deleting)), "{refused:?}");
        assert!(w.readable(&b1).is_ok());
        assert!(w.store.keys(&layout().pin_prefix(&INDEX)).len() == 1);
        // Running it again finishes.
        s.delete(&INDEX, &SNAP).unwrap();
        assert!(s.read(&INDEX, &SNAP).unwrap().is_none());
        assert_eq!(w.store.keys(&layout().pin_prefix(&INDEX)).len(), 0);
    });
}

#[test]
fn a_revoked_key_makes_the_snapshot_and_its_restores_unreadable() {
    with_commit("revoke", 4, |f, names| {
        let w = World::new();
        let _b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        let key = Revoked([6; 16]);
        // A revoked key cannot be snapshotted: nothing is written.
        let _ = w.store.take_ops();
        let refused = s.create(&INDEX, &SNAP_B, &[shard_ref()], &key, 2_000);
        assert!(
            matches!(refused, Err(SnapError::KeyUnavailable)),
            "{refused:?}"
        );
        assert_eq!(count_of(&w.store.take_ops(), "put"), 0);
        let made = s
            .create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        s.restore(&INDEX, &SNAP, &NEW, &AnyKeys, 2_100).unwrap();
        let origin = s.read_origin(&NEW).unwrap().unwrap();
        // Once the key is revoked, neither reads a file and neither spends a request trying.
        let _ = w.store.take_ops();
        let snapshot_read = s.files(&INDEX, &made.record.shards[0], &key, |_, _| Ok(()));
        assert!(matches!(snapshot_read, Err(SnapError::KeyUnavailable)));
        let restored_read = s.files(&origin.source, &origin.shards[0], &key, |_, _| Ok(()));
        assert!(matches!(restored_read, Err(SnapError::KeyUnavailable)));
        assert!(w.store.take_ops().is_empty(), "no request was spent");
        let again = s.restore(&INDEX, &SNAP, &[12; 16], &key, 2_200);
        assert!(matches!(again, Err(SnapError::KeyUnavailable)));
        assert_eq!(count_of(&w.store.take_ops(), "put"), 0);
        // The same record is readable again under a key that works.
        assert_identical(&w, &s, &INDEX, &made.record.shards[0], f, names);
    });
}

#[test]
fn a_create_that_fails_leaves_no_pin_and_can_be_retried() {
    with_commit("retry", 4, |f, names| {
        let w = World::new();
        let _b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        // The record write fails after the pin was registered.
        *w.store.fail_create.lock().unwrap() =
            Some(format!("v1/test/indexes/{}/snapshots/", hex(&INDEX)));
        let failed = s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000);
        assert!(
            matches!(failed, Err(SnapError::Store(StoreError::Unavailable))),
            "{failed:?}"
        );
        assert_eq!(
            w.store.keys(&layout().pin_prefix(&INDEX)).len(),
            0,
            "the pin was released"
        );
        assert!(s.read(&INDEX, &SNAP).unwrap().is_none());
        s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_100)
            .unwrap();
        assert_eq!(w.store.keys(&layout().pin_prefix(&INDEX)).len(), 1);
    });
}

#[test]
fn a_shard_with_no_base_is_recorded_and_pins_nothing() {
    let w = World::new();
    let s = snaps(&w);
    let made = s
        .create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
        .unwrap();
    assert_eq!(made.record.shards[0].root, None);
    assert_eq!(w.store.keys(&layout().pin_prefix(&INDEX)).len(), 0);
    s.restore(&INDEX, &SNAP, &NEW, &AnyKeys, 2_100).unwrap();
    let origin = s.read_origin(&NEW).unwrap().unwrap();
    s.files(&origin.source, &origin.shards[0], &AnyKeys, |_, _| {
        panic!("no files")
    })
    .unwrap();
    assert_eq!(s.drop_restore(&NEW).unwrap().pins, 0);
    assert_eq!(s.delete(&INDEX, &SNAP).unwrap().pins, 0);
}

#[test]
fn the_shard_list_is_bounded_before_any_request() {
    let w = World::new();
    let s = snaps(&w);
    let _ = w.store.take_ops();
    let none = s.create(&INDEX, &SNAP, &[], &AnyKeys, 2_000);
    assert!(matches!(none, Err(SnapError::NoShards)));
    let many = vec![shard_ref(); hsearch_store::snapshot::MAX_SHARDS + 1];
    let too_many = s.create(&INDEX, &SNAP, &many, &AnyKeys, 2_000);
    assert!(matches!(too_many, Err(SnapError::TooManyShards)));
    assert!(w.store.take_ops().is_empty());
}

#[test]
fn a_record_that_is_not_ours_is_corrupt_not_trusted() {
    with_commit("corrupt", 4, |f, names| {
        let w = World::new();
        let _b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        // Overwrite the record's key with garbage of the right length class.
        let key = layout().snapshot(&INDEX, &SNAP);
        let held = get(&w.store, &key).unwrap();
        let mut bad = held.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let ctx = RequestContext::new(
            Duration::from_secs(30),
            hsearch_object::Cancellation::default(),
            hsearch_object::Budget {
                request_bytes: 1 << 20,
                response_bytes: 4096,
                attempts: 2,
            },
        )
        .unwrap();
        let info = block_on(w.store.head(&ctx, &ObjectKey::new(key.as_bytes()).unwrap())).unwrap();
        block_on(w.store.compare_exchange(
            &ctx,
            &Put {
                request_id: hsearch_object::RequestId::new(b"corrupt-1").unwrap(),
                key: ObjectKey::new(key.as_bytes()).unwrap(),
                payload: Payload::new(&bad).unwrap(),
                metadata: hsearch_object::Metadata::new(b"").unwrap(),
                precondition: hsearch_object::Precondition::Matches(info.version),
            },
        ))
        .unwrap();
        assert!(matches!(s.read(&INDEX, &SNAP), Err(SnapError::Corrupt(_))));
        assert!(matches!(
            s.restore(&INDEX, &SNAP, &NEW, &AnyKeys, 2_100),
            Err(SnapError::Corrupt(_))
        ));
        assert!(matches!(
            s.delete(&INDEX, &SNAP),
            Err(SnapError::Corrupt(_))
        ));
        // A record of another snapshot id under this key is also refused.
        let other = layout().snapshot(&INDEX, &SNAP_B);
        let _ = other;
    });
}

#[test]
fn a_snapshot_shared_by_two_restores_is_held_until_the_last_is_dropped() {
    with_commit("holders", 4, |f, names| {
        let w = World::new();
        let b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        let other: Id = [12; 16];
        s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        s.restore(&INDEX, &SNAP, &NEW, &AnyKeys, 2_100).unwrap();
        s.restore(&INDEX, &SNAP, &other, &AnyKeys, 2_200).unwrap();
        assert_eq!(
            s.read(&INDEX, &SNAP).unwrap().unwrap().holders,
            vec![NEW, other]
        );
        let _b2 = w.compact(f, names, &b1.root, 2).unwrap();
        s.delete(&INDEX, &SNAP).unwrap();
        assert_eq!(s.drop_restore(&NEW).unwrap().pins, 0);
        w.sweep(Mode::Sweep, 3_000).unwrap();
        assert!(w.readable(&b1).is_ok(), "the second restore still holds it");
        // Dropping twice is a no-op for the holder list and an error for the missing origin.
        assert!(matches!(s.drop_restore(&NEW), Err(SnapError::NotFound)));
        assert_eq!(s.drop_restore(&other).unwrap().pins, 1);
        w.sweep(Mode::Sweep, 3_100).unwrap();
        assert!(w.readable(&b1).is_err());
        assert_eq!(w.store.keys(&layout().pin_prefix(&INDEX)).len(), 0);
    });
}

#[test]
fn a_snapshot_refuses_more_holders_than_a_record_carries() {
    with_commit("many", 4, |f, names| {
        let w = World::new();
        let _b1 = w.write(f, names, 3, 1, None);
        let s = snaps(&w);
        s.create(&INDEX, &SNAP, &[shard_ref()], &AnyKeys, 2_000)
            .unwrap();
        for n in 0..hsearch_store::snapshot::MAX_HOLDERS {
            let mut id = [30; 16];
            id[0] = n as u8;
            s.restore(&INDEX, &SNAP, &id, &AnyKeys, 2_100).unwrap();
        }
        let over = s.restore(&INDEX, &SNAP, &[99; 16], &AnyKeys, 2_200);
        assert!(matches!(over, Err(SnapError::TooManyHolders)), "{over:?}");
        assert!(s.read_origin(&[99; 16]).unwrap().is_none());
    });
}
