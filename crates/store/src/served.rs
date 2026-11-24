//! An index served from the object store under its name: the lane that holds its write log, the
//! host that owns its engine, and the calls a node makes on it (create, open, bulk, evolve,
//! refresh, snapshot, read by id).
//!
//! Each index has a lane of its own until lanes are shared: the lane is derived from the index
//! name, so a node that restarts, or another that takes over, finds the same log by key, and
//! nothing is discovered by LIST.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hsearch_commit::{AnyDependencies as AnyDeps, Committer, Dependencies};
use hsearch_engine::{Hit, MergeMode, Schema, Snapshot};
use hsearch_format::{Digest, Durability, Id, KeyIdentity, Layout, PolicyTuple};
use hsearch_index::barrier::Gen;
use hsearch_index::{Error, ItemResult, Op, Result, Wait};
use hsearch_lane::batch::BatchPolicy;
use hsearch_lane::{
    Error as LaneError, LaneId, LaneWriter, block_on, create_head, create_lane, dedicated_group,
    read_head,
};
use hsearch_object::{Error as StoreError, LargeObjectWriter, MetadataStore};
use sha2::{Digest as _, Sha256};

use crate::host::{Config, Host, HostError, OpenArgs, OpenError, ResolvedItem, RetryKey};
use crate::payload::ObjectPayloads;
use crate::publish::{AnyDependencies, Publisher};
use crate::rebuild::RebuildError;
use crate::recover::{NotReady, ReplayLimits};
use hsearch_index::resolve::Request;

/// What the lane of a served index is for, so it differs from any other lane of the same index.
const ORIGIN: &[u8] = b"served";
/// Most members a dedicated lane admits: the one index.
const DEDICATED_CEILING: u32 = 1;

/// How indexes are served: where their objects live and how their writers behave.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The key space every object of the deployment lives under.
    pub layout: Layout,
    /// Which lane group a new index's lane belongs to.
    pub policy: PolicyTuple,
    /// Replay bounds when an index is opened.
    pub limits: ReplayLimits,
    /// How long a bulk waits for others to share its record.
    pub window: Duration,
    /// Publish a base after this many batches since the last; zero never publishes one.
    pub base_every_batches: u64,
    /// The byte size of one part of a published file.
    pub part_bytes: usize,
    /// The longest one store request of a base may take.
    pub deadline: Duration,
    /// How records are packed into slots.
    pub slots: BatchPolicy,
    /// How the engine's automatic merges run.
    pub merge: MergeMode,
}

impl Settings {
    /// The settings a single node uses under `layout`: object durability in one region.
    #[must_use]
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            policy: PolicyTuple {
                key: KeyIdentity::DeploymentDefault,
                residency: "local".into(),
                durability: Durability::Object,
                format_version: 1,
            },
            limits: ReplayLimits {
                slots: 1 << 20,
                records: 1 << 22,
                bytes: 8 << 30,
            },
            window: Duration::from_millis(2),
            base_every_batches: 4096,
            part_bytes: 8 << 20,
            deadline: Duration::from_secs(60),
            slots: BatchPolicy {
                max_records: 64,
                max_bytes: 4 << 20,
                max_wait: Duration::from_millis(1),
            },
            merge: MergeMode::Inline,
        }
    }
}

fn digest_of(label: &str, tail: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"hs-served\0");
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(tail);
    hasher.finalize().into()
}

fn id_of(label: &str, name: &str) -> Id {
    let mut id = [0u8; 16];
    id.copy_from_slice(
        digest_of(label, name.as_bytes())
            .get(..16)
            .unwrap_or(&[0; 16]),
    );
    id
}

/// The identity of the index called `name`.
#[must_use]
pub fn index_id(name: &str) -> Id {
    id_of("index", name)
}

/// The identity of the index called `name` in its `incarnation`, which the catalog binds at create
/// and records durably: a name reused after a delete has another identity, so it never finds the
/// old pointer, base or lane records.
#[must_use]
pub fn index_id_in(name: &str, incarnation: u64) -> Id {
    let mut tagged = Vec::with_capacity(name.len() + 9);
    tagged.extend_from_slice(&incarnation.to_le_bytes());
    tagged.push(0);
    tagged.extend_from_slice(name.as_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(
        digest_of("index-incarnation", &tagged)
            .get(..16)
            .unwrap_or(&[0; 16]),
    );
    id
}

fn lane_of(settings: &Settings, name: &str) -> (LaneId, Id) {
    let (domain, group) = dedicated_group(&settings.policy, &index_id(name), ORIGIN);
    (LaneId::new(domain), group)
}

/// What every record of an index with this mapping is prepared against.
#[must_use]
pub fn dependencies(mapping: &Digest) -> Dependencies {
    Dependencies {
        schema: *mapping,
        crypto: digest_of("crypto", b"deployment-default"),
        resolved: digest_of("resolved", b"none"),
    }
}

fn host_error(error: HostError) -> Error {
    match error {
        HostError::Invalid => Error::Invalid,
        HostError::Refused(e) => Error::Engine(e),
        HostError::Rejected(_) => Error::Store(StoreError::Unavailable),
        HostError::Unknown => Error::Unknown,
        HostError::Broken => Error::AppliedFailed,
        HostError::Refresh => Error::Busy,
        HostError::Stopped => Error::Closed,
    }
}

fn open_error(error: OpenError) -> Error {
    match error {
        OpenError::Recovery(RebuildError::NotReady(NotReady::NoHead)) => Error::NotFound,
        OpenError::Recovery(RebuildError::NotReady(NotReady::Corrupt)) => Error::Corrupt,
        OpenError::Recovery(RebuildError::Engine { error, .. } | RebuildError::Open(error)) => {
            Error::Engine(error)
        }
        OpenError::Recovery(_) | OpenError::Refresh | OpenError::Ended => {
            Error::Store(StoreError::Unavailable)
        }
    }
}

fn lane_error(error: LaneError) -> Error {
    match error {
        LaneError::Exists => Error::Exists,
        LaneError::NoLane | LaneError::NoHead => Error::NotFound,
        LaneError::Fenced => Error::Fenced,
        LaneError::Poisoned => Error::Poisoned,
        LaneError::Unknown { .. } => Error::Unknown,
        LaneError::Format(_) => Error::Corrupt,
        _ => Error::Store(StoreError::Unavailable),
    }
}

/// One open index: its writer thread and the readers it publishes.
pub struct ServedIndex {
    host: Host,
}

impl ServedIndex {
    /// Create the index called `name` and open it. [`Error::Exists`] if it is already there; a
    /// creation that stopped between its lane and its head is finished instead.
    ///
    /// # Errors
    /// [`Error`] naming what stopped it; the index may be partly created, and creating it again
    /// finishes the job.
    pub fn create<S>(
        store: &Arc<S>,
        settings: &Settings,
        name: &str,
        schema: Schema,
        mapping: &Digest,
        scratch: &Path,
    ) -> Result<Self>
    where
        S: MetadataStore + LargeObjectWriter + 'static,
    {
        let (lane, group) = lane_of(settings, name);
        let created = block_on(create_lane(
            &**store,
            &settings.layout,
            &lane,
            &settings.policy,
            group,
            DEDICATED_CEILING,
            &[index_id(name)],
        ));
        let lane_existed = match created {
            Ok(_) => false,
            Err(LaneError::Exists) => true,
            Err(e) => return Err(lane_error(e)),
        };
        match block_on(create_head(
            &**store,
            &settings.layout,
            &lane,
            settings.policy.durability,
        )) {
            Ok(_) => {}
            // A head with its lane was a complete index; a head without a lane cannot happen.
            Err(LaneError::Exists) if lane_existed => return Err(Error::Exists),
            Err(e) => return Err(lane_error(e)),
        }
        Self::open(store, settings, name, schema, mapping, scratch)
    }

    /// Recover the index called `name` from the store and serve it.
    ///
    /// # Errors
    /// [`Error::NotFound`] if it was never created; otherwise what stopped the recovery.
    pub fn open<S>(
        store: &Arc<S>,
        settings: &Settings,
        name: &str,
        schema: Schema,
        mapping: &Digest,
        scratch: &Path,
    ) -> Result<Self>
    where
        S: MetadataStore + LargeObjectWriter + 'static,
    {
        let (lane, _) = lane_of(settings, name);
        // The scratch directory is rebuilt at open, but its parent is the caller's to keep.
        if let Some(parent) = scratch.parent() {
            std::fs::create_dir_all(parent).map_err(|_| Error::Store(StoreError::Unavailable))?;
        }
        // A head that is not there is an index that was never created; say so before fencing.
        block_on(read_head(&**store, &settings.layout, &lane)).map_err(lane_error)?;
        let (writer, _) = block_on(LaneWriter::open_with_head(
            Arc::clone(store),
            settings.layout.clone(),
            lane,
            |_| Ok(()),
        ))
        .map_err(lane_error)?;
        let committer =
            Committer::new(writer, AnyDeps, settings.slots).map_err(|_| Error::Invalid)?;
        let publisher = Publisher::new(
            Arc::clone(store),
            settings.layout.clone(),
            settings.part_bytes,
            settings.deadline,
        );
        let config = Config {
            index: index_id(name),
            shard: id_of("shard", name),
            incarnation: 1,
            scratch: PathBuf::from(scratch),
            limits: settings.limits,
            deps: dependencies(mapping),
            engine_format: "lucene-rust".into(),
            mapping: *mapping,
            crypto_envelope: id_of("envelope", "deployment-default"),
            window: settings.window,
            base_every_batches: settings.base_every_batches,
            token_seed: token_seed(name),
            pin: None,
            merge: settings.merge,
            searchable_when_empty: true,
            base: crate::pointer::BaseSource::Head,
            cover: None,
        };
        let host = Host::open(
            OpenArgs {
                store: Arc::clone(store),
                publisher,
                committer,
                lane,
                dependencies: AnyDependencies,
                schema,
                payloads: Some(Box::new(ObjectPayloads::new(
                    Arc::clone(store),
                    settings.layout.clone(),
                    settings.deadline,
                ))),
            },
            config,
        )
        .map_err(open_error)?;
        Ok(Self { host })
    }

    /// Write `ops` and wait as `wait` says. Results are in submission order; an operation the
    /// engine refuses carries its refusal alone and the others are written.
    ///
    /// # Errors
    /// [`Error`] when the write as a whole failed: [`Error::Unknown`] means it may or may not be
    /// durable.
    pub fn bulk(&self, ops: Vec<Op>, wait: Wait) -> Result<Vec<Result<ItemResult>>> {
        Ok(self
            .host
            .bulk_items(ops, wait)
            .map_err(host_error)?
            .into_iter()
            .map(|item| item.map_err(Error::Engine))
            .collect())
    }

    /// Resolve and write `requests` (updates, upserts, creates, conditions); see
    /// [`Host::resolve`]. One result per request, a refusal in its own item.
    ///
    /// # Errors
    /// [`Error`] if the request is malformed as a whole, the writer stopped or the store failed.
    pub fn resolve(
        &self,
        requests: Vec<Request>,
        key: Option<RetryKey>,
        wait: Wait,
    ) -> Result<Vec<ResolvedItem>> {
        self.host.resolve(requests, key, wait).map_err(host_error)
    }

    /// Extend the schema; later writes are prepared against `mapping`.
    ///
    /// # Errors
    /// [`Error::Engine`] if the engine refuses the schema.
    pub fn evolve(&self, schema: Schema, mapping: &Digest) -> Result<()> {
        self.host
            .evolve(schema, dependencies(mapping))
            .map_err(host_error)
    }

    /// Make everything acknowledged so far searchable.
    ///
    /// # Errors
    /// [`Error::Busy`] if the reader could not be published.
    pub fn refresh(&self) -> Result<()> {
        self.host.refresh().map_err(host_error)
    }

    /// The latest searchable snapshot, held until dropped.
    ///
    /// # Errors
    /// [`Error::Busy`] before anything is searchable.
    pub fn snapshot(&self) -> Result<Arc<Gen<Snapshot>>> {
        self.host.reader().ok_or(Error::Busy)
    }

    /// Read documents by id, one answer per id in the order asked, `None` for a document that
    /// does not exist. With `realtime`, each answer includes every write acknowledged before the
    /// call, searchable or not; without it, the searchable snapshot's.
    ///
    /// # Errors
    /// [`Error`] if the writer stopped or the snapshot cannot answer.
    pub fn get_many(
        &self,
        ids: &[&str],
        projection: &[&str],
        realtime: bool,
    ) -> Result<Vec<Option<Hit>>> {
        if realtime {
            return self.host.read_latest(ids, projection).map_err(host_error);
        }
        let Some(reader) = self.host.reader() else {
            return Ok(vec![None; ids.len()]);
        };
        ids.iter()
            .map(|id| reader.reader.get(id, projection).map_err(Error::from))
            .collect()
    }

    /// Whether the index must be reopened.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        self.host.is_broken()
    }

    /// Stop the writer without publishing.
    pub fn close(self) {
        drop(self);
    }
}

/// Unique to this open, so a commit token of one open is never another's.
fn token_seed(name: &str) -> [u8; 8] {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut tail = name.as_bytes().to_vec();
    tail.extend_from_slice(&now.to_le_bytes());
    tail.extend_from_slice(&std::process::id().to_le_bytes());
    let mut seed = [0u8; 8];
    seed.copy_from_slice(digest_of("token", &tail).get(..8).unwrap_or(&[0; 8]));
    seed
}
