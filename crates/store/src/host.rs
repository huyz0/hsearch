//! A serving host: one index's durable write path on the lane stack, behind a handle other
//! threads share.
//!
//! A thread owns the engine and the [`Serving`] writer. It is opened by [`recover_writer_held`]
//! (the base, if the head names one, and the committed tail), and then takes commands: a bulk with
//! a [`Wait`], a refresh, a schema evolution, a base publication and a stop. Searches do not go
//! through the thread: [`Host::reader`] pins the published reader directly.
//!
//! **Group commit.** A record carries at most [`hsearch_engine::MAX_BATCH_OPERATIONS`] operations
//! and a bounded number of bytes, so a large bulk is several records: requests scale with bytes.
//!
//! **Group commit, continued.** Bulks that queue while the thread is busy, or arrive inside the batching
//! window, become runs of distinct identities (the engine takes one version of an identity per
//! batch), each run one lane record. A bulk acknowledged with `Ok` is durable; with `Err` some of
//! its operations may be durable and the bulk may be sent again, because a put or delete of an
//! identity is idempotent once it carries a newer version.
//!
//! **Versions** are assigned here: the batch at index sequence `s` gives its operation `p` the
//! version `((s + 1) << 20) + p`. A recovered writer continues from the sequence recovery
//! applied, so no version goes backwards and nothing is stored to remember the counter. A batch
//! is bounded below 2^20 operations.
//!
//! **Bases** are published on a schedule of batches since the last one, so a restart replays a
//! bounded tail. A base that fails to publish is counted and tried again on the next batch; it
//! never fails a write.
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use hsearch_commit::{Committer, Dependencies, DependencyView, Reject};
use hsearch_engine::{
    Engine, Error as EngineError, MAX_BATCH_OPERATIONS, MergeMode, Mutation, Receipt, Schema,
    Snapshot, Value,
};
use hsearch_format::{CommitToken, Digest, Id};
use hsearch_index::barrier::{Gen, Readers};
use hsearch_index::resolve::{
    Committed, CommittedState, Lookup, Outcome as ResolvedOutcome, OutcomeStore, Reject as Refusal,
    Request, Resolver,
};
use hsearch_index::{Field, ItemResult, Op, Wait};
use hsearch_lane::LaneId;
use hsearch_object::{LargeObjectWriter, MetadataStore};

use crate::payload::PayloadSink;
use crate::pins::Pins;
use crate::pointer::BaseSource;
use crate::publish::{self, Publisher};
use crate::rebuild::{Hold, RebuildError, recover_writer_merging};
use crate::recover::{Expect, ReplayLimits, Target};
use crate::serve::{BaseTarget, ChainBatch, ChainStop, RefreshError, ServeError, Serving};

/// Most operations in one bulk (the slot bound of the v0 log).
pub const MAX_BATCH_OPS: usize = 4096;
const VERSION_SHIFT: u32 = 20;
/// Most runs written to the log in one commit.
const CHAIN_RUNS: usize = 64;

/// Why a host call did not complete.
#[derive(Debug, Eq, PartialEq)]
pub enum HostError {
    /// The request is empty or over the batch bound; nothing was written.
    Invalid,
    /// The schema or the engine refused it; nothing was written for this bulk's run.
    Refused(EngineError),
    /// The store certainly did not take a record. The bulk may be sent again.
    Rejected(Reject),
    /// A record may or may not be in the log. The host is broken until reopened.
    Unknown,
    /// The host must be reopened: a record is in the log that the engine did not take, or the
    /// lane was fenced.
    Broken,
    /// The write is durable but the reader could not be published.
    Refresh,
    /// The host has stopped.
    Stopped,
}

/// One request's result from [`Host::resolve`]: the id, the version and writer incarnation it
/// was written at (the committed ones for a request that wrote nothing), and what happened.
pub type ResolvedItem = hsearch_index::resolve::Item;

/// A retry key for [`Host::resolve`]: the client's request id and a fingerprint of its body.
#[derive(Clone, Debug)]
pub struct RetryKey {
    /// The client's request id.
    pub request: String,
    /// A digest of the request body, so reusing an id for another body is caught.
    pub fingerprint: u64,
}

/// What [`Host::resolve`] sends the writer thread.
pub struct ResolveCall {
    requests: Vec<Request>,
    key: Option<RetryKey>,
    wait: Wait,
}

/// What became of one operation of a bulk: its result, or the engine's refusal of it.
pub type Item = Result<ItemResult, EngineError>;

/// A pin held on the base while its files are fetched at open.
pub struct PinConfig {
    /// The pin's id.
    pub pin: Id,
    /// How long it lives, in milliseconds.
    pub ttl_ms: u64,
    /// Clock error allowed against the collector, in milliseconds.
    pub skew_ms: u64,
    /// Unique to this open.
    pub token: CommitToken,
    /// Deadline of each pin request.
    pub deadline: Duration,
}

/// Where the host lives and how it behaves.
pub struct Config {
    /// The index.
    pub index: Id,
    /// The shard of the index's manifests.
    pub shard: Id,
    /// The shard incarnation.
    pub incarnation: u64,
    /// Local scratch directory for the engine; emptied at open.
    pub scratch: PathBuf,
    /// Replay bounds at open.
    pub limits: ReplayLimits,
    /// What every record is prepared against.
    pub deps: Dependencies,
    /// The native format label recorded in a base's root.
    pub engine_format: String,
    /// The mapping digest the files are written under.
    pub mapping: Digest,
    /// The key envelope the files are protected under.
    pub crypto_envelope: Id,
    /// How long a bulk waits for others to share its record.
    pub window: Duration,
    /// Publish a base after this many batches since the last; zero never publishes one.
    pub base_every_batches: u64,
    /// Unique to this open: commit tokens are this followed by a counter.
    pub token_seed: [u8; 8],
    /// Hold the base with a pin while it is fetched, if given.
    pub pin: Option<PinConfig>,
    /// How the engine's automatic merges run: [`MergeMode::Background`] keeps them off the
    /// writer's refresh.
    pub merge: MergeMode,
    /// Publish a reader over an index with nothing in it at open, so it can be searched and
    /// answers with no hits; otherwise there is no reader until something is searchable.
    pub searchable_when_empty: bool,
    /// Where the base comes from at open and where a published base is named: the lane head's
    /// checkpoint, or the shard's own pointer (a lane shared by several indexes).
    pub base: BaseSource,
}

/// Everything `open` needs besides the configuration.
pub struct OpenArgs<S: MetadataStore, V: DependencyView, D> {
    /// The store the lane and the bases are in.
    pub store: Arc<S>,
    /// Publishes bases and reads the head.
    pub publisher: Publisher<S>,
    /// The committer for the index's lane, opened (and so sealing) before recovery.
    pub committer: Committer<S, V>,
    /// The lane the index belongs to.
    pub lane: LaneId,
    /// What a base is checked against.
    pub dependencies: D,
    /// The index's schema.
    pub schema: Schema,
    /// Where a batch too large for a lane record is written first; without one it is refused.
    pub payloads: Option<Box<dyn PayloadSink>>,
}

enum Command {
    Bulk(Vec<Op>, Wait, bool, Sender<Result<Vec<Item>, HostError>>),
    Refresh(Sender<Result<(), HostError>>),
    Resolve(
        Box<ResolveCall>,
        Sender<Result<Vec<ResolvedItem>, HostError>>,
    ),
    Read(
        Vec<String>,
        Vec<String>,
        Sender<Result<Vec<Option<hsearch_engine::Hit>>, HostError>>,
    ),
    Publish(Sender<Result<publish::Published, HostError>>),
    Evolve(Box<Schema>, Dependencies, Sender<Result<(), HostError>>),
    Stop,
}

/// What the host has done, for tests and the operator.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct HostStats {
    /// Lane records written.
    pub records: u64,
    /// Bases published.
    pub bases: u64,
    /// Scheduled bases that failed.
    pub base_failures: u64,
}

#[derive(Default)]
struct Counters {
    records: AtomicU64,
    bases: AtomicU64,
    base_failures: AtomicU64,
    broken: AtomicU64,
}

/// The handle to one index's host. Cheap to share by reference; every method may be called from
/// any thread.
pub struct Host {
    commands: Mutex<Sender<Command>>,
    readers: Arc<Readers<Snapshot>>,
    counters: Arc<Counters>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// Why a host could not be opened.
#[derive(Debug)]
pub enum OpenError {
    /// Recovery from the lane and base was refused or failed.
    Recovery(RebuildError),
    /// The recovered index could not publish its first reader.
    Refresh,
    /// The host thread ended before it was ready.
    Ended,
}

impl Host {
    /// Recover the index from its lane and base and start serving it.
    ///
    /// # Errors
    /// [`OpenError`] if the index could not be made ready; nothing is left running.
    pub fn open<S, V, D>(args: OpenArgs<S, V, D>, config: Config) -> Result<Self, OpenError>
    where
        S: MetadataStore + LargeObjectWriter + 'static,
        V: DependencyView + Send + 'static,
        D: publish::Dependencies + Send + 'static,
    {
        let (commands, inbox) = channel();
        let (ready, started) = channel();
        let counters = Arc::new(Counters::default());
        let seen = Arc::clone(&counters);
        let thread = std::thread::spawn(move || {
            let OpenArgs {
                store,
                publisher,
                committer,
                lane,
                dependencies,
                schema,
                payloads,
            } = args;
            let target = Target {
                lane: &lane,
                index: config.index,
                shard: config.shard,
                incarnation: config.incarnation,
                limits: config.limits,
                expect: Expect::default(),
                base: config.base,
            };
            let pins = config
                .pin
                .as_ref()
                .map(|p| Pins::new(Arc::clone(&store), publisher.layout().clone(), p.deadline));
            let now = || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            };
            let hold = pins
                .as_ref()
                .zip(config.pin.as_ref())
                .map(|(pins, p)| Hold {
                    pins,
                    pin: p.pin,
                    token: p.token,
                    ttl_ms: p.ttl_ms,
                    skew_ms: p.skew_ms,
                    now: &now,
                });
            let outcome = recover_writer_merging(
                &publisher,
                &target,
                &dependencies,
                &schema,
                &config.scratch,
                hold.as_ref(),
                config.merge,
                |engine, local, rebuilt| {
                    let serving = Serving::recovered(
                        committer,
                        config.index,
                        &rebuilt.recovered,
                        config.deps,
                    );
                    let serving = match payloads {
                        Some(sink) => serving.with_payloads(sink),
                        None => serving,
                    };
                    let mut core = Core {
                        serving,
                        engine,
                        local,
                        publisher: &publisher,
                        dependencies: &dependencies,
                        lane: &lane,
                        schema: schema.clone(),
                        config: &config,
                        counters: &seen,
                        batches_since_base: 0,
                        attempts: 0,
                        schema_unpublished: false,
                        outcomes: OutcomeStore::new(OUTCOME_REQUESTS, OUTCOME_BYTES),
                    };
                    // A recovered index with data is searchable at once.
                    let first = core.serving.refresh(engine).map(|_| ()).and_then(|()| {
                        if config.searchable_when_empty {
                            core.serving.publish_empty(engine)
                        } else {
                            Ok(())
                        }
                    });
                    if first.is_err() {
                        let _ = ready.send(Err(OpenError::Refresh));
                        return;
                    }
                    let _ = ready.send(Ok(core.serving.readers()));
                    core.run(&inbox);
                },
            );
            if let Err(e) = outcome {
                let _ = ready.send(Err(OpenError::Recovery(e)));
            }
        });
        match started.recv() {
            Ok(Ok(readers)) => Ok(Self {
                commands: Mutex::new(commands),
                readers,
                counters,
                thread: Mutex::new(Some(thread)),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(OpenError::Ended)
            }
        }
    }

    fn send<T>(
        &self,
        command: impl FnOnce(Sender<Result<T, HostError>>) -> Command,
    ) -> Result<T, HostError> {
        let (reply, answer) = channel();
        {
            let commands = self
                .commands
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            commands
                .send(command(reply))
                .map_err(|_| HostError::Stopped)?;
        }
        answer.recv().map_err(|_| HostError::Stopped)?
    }

    /// Write `ops` and wait as `wait` says. Results are in submission order.
    ///
    /// # Errors
    /// [`HostError`]; see its variants for what is certain about the write.
    pub fn bulk(&self, ops: Vec<Op>, wait: Wait) -> Result<Vec<ItemResult>, HostError> {
        self.send(|reply| Command::Bulk(ops, wait, true, reply))?
            .into_iter()
            .map(|item| item.map_err(HostError::Refused))
            .collect()
    }

    /// [`Host::bulk`], with an operation the engine refuses answered alone: every other operation
    /// is written as if it had been sent without it, and the refused one carries the refusal.
    ///
    /// # Errors
    /// [`HostError`]; see its variants for what is certain about the write.
    pub fn bulk_items(&self, ops: Vec<Op>, wait: Wait) -> Result<Vec<Item>, HostError> {
        self.send(|reply| Command::Bulk(ops, wait, false, reply))
    }

    /// Publish a reader over everything applied so far.
    ///
    /// # Errors
    /// [`HostError::Refresh`] if it could not be published; the serving reader is unchanged.
    pub fn refresh(&self) -> Result<(), HostError> {
        self.send(Command::Refresh)
    }

    /// Resolve `requests` (updates, upserts, creates, conditions, no-op detection) once against
    /// the committed documents and write the resolved whole-document operations, so the log holds
    /// only puts and deletes and replay never resolves anything. A request refused or that changed
    /// nothing writes nothing and says so in its item. With a `key`, a retry of a request already
    /// answered gets the original items back instead of being resolved again; the answers are
    /// remembered in memory, so a retry after a restart is resolved again.
    ///
    /// # Errors
    /// [`HostError::Invalid`] for an empty or oversize batch, a reused request id with another
    /// body, an id named twice in one batch, or a partial update on an index with a vector field (the committed view carries no
    /// vectors to merge into); otherwise as [`Host::bulk`].
    pub fn resolve(
        &self,
        requests: Vec<Request>,
        key: Option<RetryKey>,
        wait: Wait,
    ) -> Result<Vec<ResolvedItem>, HostError> {
        let call = Box::new(ResolveCall {
            requests,
            key,
            wait,
        });
        self.send(|reply| Command::Resolve(call, reply))
    }

    /// Read documents as of every write acknowledged before the call, searchable or not, without
    /// publishing a reader: what `refresh` would make searchable stays unsearchable.
    ///
    /// # Errors
    /// [`HostError::Refresh`] if a view of the engine could not be taken, or [`HostError::Broken`].
    pub fn read_latest(
        &self,
        ids: &[&str],
        projection: &[&str],
    ) -> Result<Vec<Option<hsearch_engine::Hit>>, HostError> {
        let ids = ids.iter().map(|s| (*s).to_owned()).collect();
        let projection = projection.iter().map(|s| (*s).to_owned()).collect();
        self.send(|reply| Command::Read(ids, projection, reply))
    }

    /// Publish a base now.
    ///
    /// # Errors
    /// [`HostError::Refused`] or [`HostError::Broken`] with what stopped it.
    pub fn publish(&self) -> Result<publish::Published, HostError> {
        self.send(Command::Publish)
    }

    /// Extend the schema and prepare later records against `deps`.
    ///
    /// # Errors
    /// [`HostError::Refused`] if the engine refuses the schema.
    pub fn evolve(&self, schema: Schema, deps: Dependencies) -> Result<(), HostError> {
        self.send(|reply| Command::Evolve(Box::new(schema), deps, reply))
    }

    /// The published reader, pinned until the handle is dropped. `None` before any data is
    /// searchable.
    #[must_use]
    pub fn reader(&self) -> Option<Arc<Gen<Snapshot>>> {
        self.readers.pin()
    }

    /// Whether the host must be reopened.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        self.counters.broken.load(Ordering::Acquire) != 0
    }

    /// What the host has done.
    #[must_use]
    pub fn stats(&self) -> HostStats {
        HostStats {
            records: self.counters.records.load(Ordering::Relaxed),
            bases: self.counters.bases.load(Ordering::Relaxed),
            base_failures: self.counters.base_failures.load(Ordering::Relaxed),
        }
    }

    /// Stop the thread after the commands already queued. The engine is closed without
    /// committing: what is durable is the lane and the bases.
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self
            .commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(Command::Stop);
        let handle = self
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

/// Requests whose answers are remembered for retries, and the bytes they may hold.
const OUTCOME_REQUESTS: usize = 4096;
const OUTCOME_BYTES: usize = 4 << 20;

/// The committed documents as the writer thread sees them: the engine's view, whole documents in
/// schema order. A read the engine could not answer is remembered, because `get` cannot say it.
struct Committing<'a> {
    view: Snapshot,
    names: &'a [&'a str],
    head: u64,
    term: u64,
    failed: std::cell::Cell<bool>,
}

impl CommittedState for Committing<'_> {
    fn head(&self) -> u64 {
        self.head
    }

    fn get(&self, id: &str) -> Option<Committed> {
        let hit = match self.view.get(id, self.names) {
            Ok(hit) => hit?,
            Err(_) => {
                self.failed.set(true);
                return None;
            }
        };
        let fields = self
            .names
            .iter()
            .map(|name| {
                hit.fields
                    .iter()
                    .find(|(n, _)| n == name)
                    .map_or(Field::Absent, |(_, v)| match v {
                        hsearch_engine::StoredValue::String(s) => Field::Str(Arc::new(s.clone())),
                        hsearch_engine::StoredValue::Integer(n) => Field::Int(*n),
                        hsearch_engine::StoredValue::Boolean(b) => Field::Bool(*b),
                        hsearch_engine::StoredValue::Date(n) => Field::Date(*n),
                    })
            })
            .collect();
        Some(Committed {
            seq_no: u64::try_from(hit.version).unwrap_or(0),
            primary_term: self.term,
            fields,
        })
    }
}

struct Bulk {
    ops: Vec<Op>,
    wait: Wait,
    /// A refused operation stops this bulk: nothing after it is written, so a durable prefix is
    /// all that is left.
    strict: bool,
    reply: Sender<Result<Vec<Item>, HostError>>,
    results: Vec<Option<Item>>,
    failed: Option<HostError>,
}

struct Core<'a, 'e, S: MetadataStore, V: DependencyView, D> {
    serving: Serving<S, V>,
    engine: &'a Engine<'e>,
    local: &'a hsearch_engine::LocalFiles,
    publisher: &'a Publisher<S>,
    dependencies: &'a D,
    lane: &'a LaneId,
    schema: Schema,
    config: &'a Config,
    counters: &'a Counters,
    batches_since_base: u64,
    attempts: u64,
    /// A schema change took effect but the reader under it could not be published yet.
    schema_unpublished: bool,
    outcomes: OutcomeStore,
}

fn value(field: &Field) -> Option<Value<'_>> {
    Some(match field {
        Field::Str(s) => Value::Shared(s),
        Field::Int(n) => Value::Integer(*n),
        Field::Bool(b) => Value::Boolean(*b),
        Field::Date(n) => Value::Date(*n),
        Field::Vector(v) => Value::Vector(v),
        Field::Absent => return None,
    })
}

/// The version of the first operation of the record at `sequence`; later operations of the record
/// rise by one. Rises with the sequence, so a replayed log never puts a version backwards (the
/// sequence cannot reach the point where this saturates).
fn record_version(sequence: u64) -> i64 {
    i64::try_from((sequence + 1) << VERSION_SHIFT).unwrap_or(i64::MAX)
}

/// The field values of each operation of `run`, in schema order.
fn run_rows<'a>(
    names: &[&'a str],
    group: &'a [Bulk],
    run: &[(usize, usize)],
) -> Vec<Vec<(&'a str, Value<'a>)>> {
    run.iter()
        .map(|(b, o)| match &group[*b].ops[*o] {
            Op::Put { fields, .. } => names
                .iter()
                .copied()
                .zip(fields.iter())
                .filter_map(|(name, field)| Some((name, value(field)?)))
                .collect(),
            Op::Delete { .. } => Vec::new(),
        })
        .collect()
}

/// The mutations of `run`, versioned from `first`.
fn run_mutations<'a>(
    group: &'a [Bulk],
    run: &[(usize, usize)],
    rows: &'a [Vec<(&'a str, Value<'a>)>],
    first: i64,
) -> Vec<Mutation<'a>> {
    run.iter()
        .zip(rows)
        .enumerate()
        .map(|(p, ((b, o), row))| {
            let version = first + p as i64;
            match &group[*b].ops[*o] {
                Op::Put { id, .. } => Mutation::Put {
                    id,
                    version,
                    fields: row,
                },
                Op::Delete { id } => Mutation::Delete { id, version },
            }
        })
        .collect()
}

fn serve_error(e: ServeError) -> HostError {
    match e {
        ServeError::Refused(e) => HostError::Refused(e),
        ServeError::Rejected(r) => HostError::Rejected(r),
        ServeError::Unknown { .. } => HostError::Unknown,
        ServeError::Broken => HostError::Broken,
    }
}

impl<S, V, D> Core<'_, '_, S, V, D>
where
    S: MetadataStore + LargeObjectWriter + 'static,
    V: DependencyView,
    D: publish::Dependencies,
{
    fn run(&mut self, inbox: &Receiver<Command>) {
        let mut pending: Option<Command> = None;
        loop {
            let command = match pending.take() {
                Some(c) => c,
                None => match inbox.recv() {
                    Ok(c) => c,
                    Err(_) => return,
                },
            };
            match command {
                Command::Stop => return,
                Command::Bulk(ops, wait, strict, reply) => {
                    let mut group = vec![Bulk::new(ops, wait, strict, reply)];
                    let mut count = group[0].ops.len();
                    // Others queued, or arriving inside the window, share this group's records.
                    // The window is one deadline for the group, not a wait after each arrival.
                    let deadline = std::time::Instant::now() + self.config.window;
                    while count < MAX_BATCH_OPS {
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        match inbox.recv_timeout(left) {
                            Ok(Command::Bulk(ops, wait, strict, reply)) => {
                                count += ops.len();
                                group.push(Bulk::new(ops, wait, strict, reply));
                            }
                            Ok(other) => {
                                pending = Some(other);
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                    self.group(group);
                    self.scheduled_base();
                }
                Command::Refresh(reply) => {
                    let _ = reply.send(self.refresh());
                }
                Command::Read(ids, projection, reply) => {
                    if self.serving.is_broken() {
                        let _ = reply.send(Err(HostError::Broken));
                        continue;
                    }
                    let projection: Vec<&str> = projection.iter().map(String::as_str).collect();
                    let answer = self
                        .engine
                        .refresh()
                        .map_err(|_| HostError::Refresh)
                        .and_then(|view| {
                            ids.iter()
                                .map(|id| view.get(id, &projection).map_err(|_| HostError::Refresh))
                                .collect()
                        });
                    let _ = reply.send(answer);
                }
                Command::Resolve(call, reply) => {
                    let _ = reply.send(self.resolve(*call));
                }
                Command::Publish(reply) => {
                    let _ = reply.send(self.publish_base());
                }
                Command::Evolve(schema, deps, reply) => {
                    let result = self
                        .engine
                        .evolve_schema((*schema).clone())
                        .map_err(HostError::Refused);
                    if result.is_ok() {
                        self.schema = *schema;
                        self.serving.set_deps(deps);
                        // Readers published from here on carry the new schema; a refresh that
                        // finds nothing new would keep serving the old one.
                        // The change has taken effect either way; a reader that could not be
                        // published now is published by the next refresh.
                        self.schema_unpublished = self.serving.republish(self.engine).is_err();
                    }
                    let _ = reply.send(result);
                }
            }
        }
    }

    fn mark_broken(&self) {
        self.counters.broken.store(1, Ordering::Release);
    }

    fn refresh(&mut self) -> Result<(), HostError> {
        let published = if self.schema_unpublished {
            self.serving.republish(self.engine)
        } else {
            self.serving.refresh(self.engine).map(|_| ())
        };
        if published.is_ok() {
            self.schema_unpublished = false;
        }
        published.map_err(|e| match e {
            RefreshError::Cut => {
                self.counters.broken.store(1, Ordering::Release);
                HostError::Broken
            }
            RefreshError::Engine(_) | RefreshError::Refused(_) => HostError::Refresh,
        })
    }

    fn resolve(&mut self, call: ResolveCall) -> Result<Vec<ResolvedItem>, HostError> {
        let ResolveCall {
            requests,
            key,
            wait,
        } = call;
        if self.serving.is_broken() {
            return Err(HostError::Broken);
        }
        if let Some(key) = &key {
            match self.outcomes.get(&key.request, key.fingerprint) {
                Lookup::Replay(items) => {
                    let items = items.to_vec();
                    // The retry's own wait still holds: what it asks to be searchable is.
                    if wait == Wait::Searchable {
                        self.refresh()?;
                    }
                    return Ok(items);
                }
                Lookup::Mismatch => return Err(HostError::Invalid),
                Lookup::Unknown => {}
            }
        }
        let vectors = self
            .schema
            .fields()
            .any(|(_, kind)| matches!(kind, hsearch_engine::Kind::Vector(_)));
        // One request per id: a later request on an id would be resolved against a version and a
        // document the resolver only assumed, before the engine had taken or refused them.
        let mut named = HashSet::new();
        let repeated = !requests.iter().all(|r| {
            named.insert(match r {
                Request::Index { id, .. }
                | Request::Create { id, .. }
                | Request::Delete { id, .. }
                | Request::Update { id, .. } => id.as_str(),
            })
        });
        if repeated
            || requests.is_empty()
            || requests.len() > MAX_BATCH_OPS
            || (vectors && requests.iter().any(|r| matches!(r, Request::Update { .. })))
        {
            return Err(HostError::Invalid);
        }
        let fields: Vec<(&str, hsearch_engine::Kind)> = self.schema.fields().collect();
        let names: Vec<&str> = fields.iter().map(|(n, _)| *n).collect();
        let resolver = Resolver::new(
            self.config.incarnation,
            &fields,
            self.schema.limits().document_bytes,
        );
        // The view holds one of the engine's few reader reservations: it is dropped before the
        // write, whose refresh needs one.
        let prepared = {
            let state = Committing {
                view: self.engine.refresh().map_err(|_| HostError::Refresh)?,
                names: &names,
                head: self.serving.next_seq(),
                term: self.config.incarnation,
                failed: std::cell::Cell::new(false),
            };
            let prepared = resolver
                .prepare(&state, 1, &requests)
                .map_err(|_| HostError::Invalid)?;
            if state.failed.get() {
                return Err(HostError::Refresh);
            }
            prepared
        };
        let mut items = prepared.items().to_vec();
        if !prepared.ops().is_empty() {
            let (reply, answer) = channel();
            self.group(vec![Bulk::new(prepared.ops().to_vec(), wait, false, reply)]);
            let written = answer.recv().map_err(|_| HostError::Stopped)??;
            let mut written = written.into_iter();
            for item in &mut items {
                if !matches!(
                    item.outcome,
                    ResolvedOutcome::Created | ResolvedOutcome::Updated | ResolvedOutcome::Deleted
                ) {
                    continue;
                }
                match written.next() {
                    Some(Ok(done)) => {
                        item.seq_no = u64::try_from(done.version).unwrap_or(0);
                        item.primary_term = self.config.incarnation;
                    }
                    Some(Err(refusal)) => {
                        item.seq_no = 0;
                        item.primary_term = 0;
                        item.outcome = ResolvedOutcome::Rejected(match refusal {
                            EngineError::Limit => Refusal::Limit,
                            _ => Refusal::Invalid,
                        });
                    }
                    None => return Err(HostError::Broken),
                }
            }
        }
        if let Some(key) = key {
            self.outcomes
                .record(&key.request, key.fingerprint, items.clone());
        }
        Ok(items)
    }

    fn token(&mut self) -> CommitToken {
        self.attempts += 1;
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.config.token_seed);
        bytes[8..].copy_from_slice(&self.attempts.to_le_bytes());
        // The counter starts at one, so the token is never all zero.
        CommitToken::new(bytes).unwrap_or_else(|_| unreachable!("a counter from one is nonzero"))
    }

    fn publish_base(&mut self) -> Result<publish::Published, HostError> {
        let attempt = self.token();
        let target = BaseTarget {
            lane: self.lane,
            shard: self.config.shard,
            incarnation: self.config.incarnation,
            engine_format: &self.config.engine_format,
            mapping: self.config.mapping,
            crypto_envelope: self.config.crypto_envelope,
            base: self.config.base,
        };
        let published = self
            .serving
            .publish_base(
                self.engine,
                self.local,
                self.publisher,
                &target,
                attempt,
                self.dependencies,
            )
            .map_err(|e| match e {
                crate::serve::BaseError::Broken => HostError::Broken,
                crate::serve::BaseError::Engine(e) => HostError::Refused(e),
                _ => HostError::Refresh,
            })?;
        self.batches_since_base = 0;
        self.counters.bases.fetch_add(1, Ordering::Relaxed);
        Ok(published)
    }

    fn scheduled_base(&mut self) {
        let every = self.config.base_every_batches;
        if every == 0 || self.batches_since_base < every || self.serving.is_broken() {
            return;
        }
        if self.publish_base().is_err() {
            self.counters.base_failures.fetch_add(1, Ordering::Relaxed);
            // The next attempt waits for another `every` batches: a base that cannot be published
            // is retried on the schedule, not once per write.
            self.batches_since_base = 0;
        }
    }

    /// Cut the group into runs of distinct identities, write each as one record, and answer.
    fn group(&mut self, mut group: Vec<Bulk>) {
        let fields = self.schema.fields().count();
        let required = self.schema.required_fields();
        for bulk in &mut group {
            // A put has no more values than the schema has fields and no fewer than it requires
            // (documents written before a field was added have the trailing ones absent).
            let shaped = bulk.ops.iter().all(|op| match op {
                Op::Put { fields: values, .. } => {
                    values.len() <= fields && values.len() >= required
                }
                Op::Delete { .. } => true,
            });
            if bulk.ops.is_empty() || bulk.ops.len() > MAX_BATCH_OPS || !shaped {
                bulk.failed = Some(HostError::Invalid);
            }
        }
        let mut run: Vec<(usize, usize)> = Vec::new();
        let mut ids: HashSet<&str> = HashSet::new();
        let mut runs: Vec<Vec<(usize, usize)>> = Vec::new();
        for (b, bulk) in group.iter().enumerate() {
            if bulk.failed.is_some() {
                continue;
            }
            for (o, op) in bulk.ops.iter().enumerate() {
                let id = match op {
                    Op::Put { id, .. } | Op::Delete { id } => id.as_str(),
                };
                if run.len() == MAX_BATCH_OPERATIONS || ids.contains(id) {
                    runs.push(std::mem::take(&mut run));
                    ids.clear();
                }
                ids.insert(id);
                run.push((b, o));
            }
        }
        if !run.is_empty() {
            runs.push(run);
        }
        let mut queue: std::collections::VecDeque<Vec<(usize, usize)>> = runs.into();
        while !queue.is_empty() {
            // Up to a slot's worth of runs go to the log together; a bulk that has failed writes
            // nothing more, so it leaves a durable prefix.
            let mut chunk: Vec<Vec<(usize, usize)>> = Vec::new();
            while chunk.len() < CHAIN_RUNS {
                let Some(run) = queue.pop_front() else { break };
                let run: Vec<(usize, usize)> = run
                    .into_iter()
                    .filter(|(b, _)| group[*b].failed.is_none())
                    .collect();
                if !run.is_empty() {
                    chunk.push(run);
                }
            }
            if chunk.is_empty() {
                continue;
            }
            let (done, stop) = self.write_chain(&group, &chunk);
            let written = done.len();
            for (run, receipts) in chunk.iter().zip(done) {
                Self::settle(&mut group, run, receipts.into_iter().map(Ok).collect());
            }
            match stop {
                None => {}
                Some(ChainStop::Refused(_) | ChainStop::Recognised | ChainStop::Alone) => {
                    // The run the log stopped at is written alone, split until the operations the
                    // engine refuses are told apart from the ones it takes; the rest go round again.
                    let run = chunk[written].clone();
                    match self.write_split(&group, &run) {
                        Ok(items) => Self::settle(&mut group, &run, items),
                        Err(e) => {
                            if matches!(e, HostError::Broken | HostError::Unknown) {
                                self.mark_broken();
                                for bulk in &mut group {
                                    bulk.failed.get_or_insert(HostError::Broken);
                                }
                                break;
                            }
                            for (b, _) in &run {
                                group[*b].failed.get_or_insert_with(|| clone_error(&e));
                            }
                        }
                    }
                    for rest in chunk.drain(written + 1..).rev() {
                        queue.push_front(rest);
                    }
                }
                Some(ChainStop::Failed(e)) => {
                    let e = serve_error(e);
                    let broken = matches!(e, HostError::Broken | HostError::Unknown);
                    if broken {
                        self.mark_broken();
                    }
                    // Every bulk with an operation in the runs that were not written fails with
                    // the cause; a broken host fails everything after it the same way.
                    for run in &chunk[written..] {
                        for (b, _) in run {
                            group[*b].failed.get_or_insert_with(|| clone_error(&e));
                        }
                    }
                    if broken {
                        for bulk in &mut group {
                            bulk.failed.get_or_insert(HostError::Broken);
                        }
                        break;
                    }
                }
            }
        }
        let searchable = group
            .iter()
            .any(|b| b.failed.is_none() && b.wait == Wait::Searchable);
        let refreshed = if searchable { self.refresh() } else { Ok(()) };
        for bulk in group {
            let answer = match bulk.failed {
                Some(e) => Err(e),
                None if bulk.wait == Wait::Searchable && refreshed.is_err() => Err(clone_error(
                    refreshed.as_ref().err().unwrap_or(&HostError::Refresh),
                )),
                None => Ok(bulk.results.into_iter().flatten().collect()),
            };
            let _ = bulk.reply.send(answer);
        }
    }

    /// Record what became of each operation of `run`.
    fn settle(
        group: &mut [Bulk],
        run: &[(usize, usize)],
        items: Vec<Result<Receipt, EngineError>>,
    ) {
        for ((b, o), receipt) in run.iter().zip(items) {
            let bulk = &mut group[*b];
            if bulk.strict
                && let Err(refusal) = &receipt
            {
                bulk.failed.get_or_insert(HostError::Refused(*refusal));
            }
            bulk.results[*o] = Some(receipt.map(|receipt| ItemResult {
                id: receipt.id,
                version: receipt.version,
                outcome: receipt.outcome,
            }));
        }
    }

    /// Write `chunk`, a run of runs, as records of one commit: the leading runs the log took,
    /// and why it stopped before the rest if it did.
    fn write_chain(
        &mut self,
        group: &[Bulk],
        chunk: &[Vec<(usize, usize)>],
    ) -> (Vec<Vec<Receipt>>, Option<ChainStop>) {
        let first_sequence = self.serving.next_seq();
        let names: Vec<&str> = self.schema.fields().map(|(n, _)| n).collect();
        let rows: Vec<Vec<Vec<(&str, Value<'_>)>>> = chunk
            .iter()
            .map(|run| run_rows(&names, group, run))
            .collect();
        let mutations: Vec<Vec<Mutation<'_>>> = chunk
            .iter()
            .zip(&rows)
            .enumerate()
            .map(|(s, (run, rows))| {
                run_mutations(group, run, rows, record_version(first_sequence + s as u64))
            })
            .collect();
        let requests: Vec<String> = (0..chunk.len())
            .map(|s| format!("h{}", first_sequence + s as u64))
            .collect();
        let batches: Vec<ChainBatch<'_>> = requests
            .iter()
            .zip(&mutations)
            .map(|(request, mutations)| ChainBatch { request, mutations })
            .collect();
        let schema = self.schema.clone();
        let chained = self.serving.write_chain(&schema, self.engine, &batches);
        let records = chained.receipts.len() as u64;
        self.counters.records.fetch_add(records, Ordering::Relaxed);
        self.batches_since_base += records;
        (chained.receipts, chained.stop)
    }

    /// Write `run` as one record. A run the engine refuses is written as halves, down to a single
    /// operation, so the operations it refuses are told apart from the ones it takes: a refused
    /// operation was never written and carries its refusal. Deterministic, so replay sees the
    /// same records.
    fn write_split(
        &mut self,
        group: &[Bulk],
        run: &[(usize, usize)],
    ) -> Result<Vec<Result<Receipt, EngineError>>, HostError> {
        match self.write_run(group, run) {
            Ok(receipts) => Ok(receipts.into_iter().map(Ok).collect()),
            Err(HostError::Refused(_)) if run.len() > 1 => {
                let (head, tail) = run.split_at(run.len() / 2);
                let mut items = self.write_split(group, head)?;
                // A strict bulk stops at its first refusal: nothing of it after the refused
                // operation is written, whichever half it falls in.
                let stopped: std::collections::HashMap<usize, EngineError> = head
                    .iter()
                    .zip(&items)
                    .filter_map(|((b, _), item)| match item {
                        Err(e) if group[*b].strict => Some((*b, *e)),
                        _ => None,
                    })
                    .collect();
                let rest: Vec<(usize, usize)> = tail
                    .iter()
                    .copied()
                    .filter(|(b, _)| !stopped.contains_key(b))
                    .collect();
                let mut written = if rest.is_empty() {
                    Vec::new().into_iter()
                } else {
                    self.write_split(group, &rest)?.into_iter()
                };
                for (b, _) in tail {
                    match stopped.get(b) {
                        Some(e) => items.push(Err(*e)),
                        None => items.extend(written.next()),
                    }
                }
                Ok(items)
            }
            Err(HostError::Refused(refusal)) => Ok(vec![Err(refusal)]),
            Err(other) => Err(other),
        }
    }

    fn write_run(
        &mut self,
        group: &[Bulk],
        run: &[(usize, usize)],
    ) -> Result<Vec<Receipt>, HostError> {
        let sequence = self.serving.next_seq();
        let names: Vec<&str> = self.schema.fields().map(|(n, _)| n).collect();
        let rows = run_rows(&names, group, run);
        let mutations = run_mutations(group, run, &rows, record_version(sequence));
        let request = format!("h{sequence}");
        let schema = self.schema.clone();
        let written = self
            .serving
            .write(&schema, self.engine, &request, &mutations)
            .map_err(serve_error)?;
        if written.committed.is_some() {
            self.counters.records.fetch_add(1, Ordering::Relaxed);
            self.batches_since_base += 1;
        }
        Ok(written.receipts)
    }
}

impl Bulk {
    fn new(
        ops: Vec<Op>,
        wait: Wait,
        strict: bool,
        reply: Sender<Result<Vec<Item>, HostError>>,
    ) -> Self {
        let results = vec![None; ops.len()];
        Self {
            ops,
            wait,
            strict,
            reply,
            results,
            failed: None,
        }
    }
}

fn clone_error(e: &HostError) -> HostError {
    match e {
        HostError::Invalid => HostError::Invalid,
        HostError::Refused(e) => HostError::Refused(*e),
        HostError::Rejected(r) => HostError::Rejected(*r),
        HostError::Unknown => HostError::Unknown,
        HostError::Broken => HostError::Broken,
        HostError::Refresh => HostError::Refresh,
        HostError::Stopped => HostError::Stopped,
    }
}
