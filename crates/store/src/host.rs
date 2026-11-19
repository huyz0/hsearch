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
    Engine, Error as EngineError, MAX_BATCH_OPERATIONS, Mutation, Receipt, Schema, Snapshot, Value,
};
use hsearch_format::{CommitToken, Digest, Id};
use hsearch_index::barrier::{Gen, Readers};
use hsearch_index::{Field, ItemResult, Op, Wait};
use hsearch_lane::LaneId;
use hsearch_object::{LargeObjectWriter, MetadataStore};

use crate::pins::Pins;
use crate::publish::{self, Publisher};
use crate::rebuild::{Hold, RebuildError, recover_writer_held};
use crate::recover::{Expect, ReplayLimits, Target};
use crate::serve::{BaseTarget, RefreshError, ServeError, Serving};

/// Most operations in one bulk (the slot bound of the v0 log).
pub const MAX_BATCH_OPS: usize = 4096;
const VERSION_SHIFT: u32 = 20;

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
}

enum Command {
    Bulk(Vec<Op>, Wait, Sender<Result<Vec<ItemResult>, HostError>>),
    Refresh(Sender<Result<(), HostError>>),
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

fn open_error(e: RebuildError) -> String {
    format!("{e:?}")
}

impl Host {
    /// Recover the index from its lane and base and start serving it.
    ///
    /// # Errors
    /// The recovery error, as text, if the index could not be made ready; nothing is left
    /// running.
    pub fn open<S, V, D>(args: OpenArgs<S, V, D>, config: Config) -> Result<Self, String>
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
            } = args;
            let target = Target {
                lane: &lane,
                index: config.index,
                shard: config.shard,
                incarnation: config.incarnation,
                limits: config.limits,
                expect: Expect::default(),
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
            let outcome = recover_writer_held(
                &publisher,
                &target,
                &dependencies,
                &schema,
                &config.scratch,
                hold.as_ref(),
                |engine, local, rebuilt| {
                    let serving = Serving::recovered(
                        committer,
                        config.index,
                        &rebuilt.recovered,
                        config.deps,
                    );
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
                    };
                    // A recovered index with data is searchable at once.
                    let first = core.serving.refresh(engine);
                    if first.is_err() {
                        let _ =
                            ready.send(Err("the recovered index could not be refreshed".into()));
                        return;
                    }
                    let _ = ready.send(Ok(core.serving.readers()));
                    core.run(&inbox);
                },
            );
            if let Err(e) = outcome {
                let _ = ready.send(Err(open_error(e)));
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
                Err("the host thread ended while opening".into())
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
        self.send(|reply| Command::Bulk(ops, wait, reply))
    }

    /// Publish a reader over everything applied so far.
    ///
    /// # Errors
    /// [`HostError::Refresh`] if it could not be published; the serving reader is unchanged.
    pub fn refresh(&self) -> Result<(), HostError> {
        self.send(Command::Refresh)
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

struct Bulk {
    ops: Vec<Op>,
    wait: Wait,
    reply: Sender<Result<Vec<ItemResult>, HostError>>,
    results: Vec<Option<ItemResult>>,
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
                Command::Bulk(ops, wait, reply) => {
                    let mut group = vec![Bulk::new(ops, wait, reply)];
                    let mut count = group[0].ops.len();
                    // Others queued, or arriving inside the window, share this group's records.
                    // The window is one deadline for the group, not a wait after each arrival.
                    let deadline = std::time::Instant::now() + self.config.window;
                    while count < MAX_BATCH_OPS {
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        match inbox.recv_timeout(left) {
                            Ok(Command::Bulk(ops, wait, reply)) => {
                                count += ops.len();
                                group.push(Bulk::new(ops, wait, reply));
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
        self.serving
            .refresh(self.engine)
            .map(|_| ())
            .map_err(|e| match e {
                RefreshError::Cut => {
                    self.counters.broken.store(1, Ordering::Release);
                    HostError::Broken
                }
                RefreshError::Engine(_) | RefreshError::Refused(_) => HostError::Refresh,
            })
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
        for run in runs {
            // A bulk that has failed writes nothing more, so it leaves a durable prefix.
            let run: Vec<(usize, usize)> = run
                .into_iter()
                .filter(|(b, _)| group[*b].failed.is_none())
                .collect();
            if run.is_empty() {
                continue;
            }
            let result = self.write_split(&group, &run);
            match result {
                Ok(receipts) => {
                    for ((b, o), receipt) in run.iter().zip(receipts) {
                        let bulk = &mut group[*b];
                        bulk.results[*o] = Some(ItemResult {
                            id: receipt.id,
                            version: receipt.version,
                            outcome: receipt.outcome,
                        });
                    }
                }
                Err(e) => {
                    if matches!(e, HostError::Broken | HostError::Unknown) {
                        self.mark_broken();
                    }
                    // Every bulk with an operation in this run fails with the cause; a broken
                    // host fails everything after it the same way.
                    let broken = matches!(e, HostError::Broken | HostError::Unknown);
                    for (b, _) in &run {
                        group[*b].failed.get_or_insert_with(|| clone_error(&e));
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

    /// Write `run` as one record, or as halves if the engine finds it too large (a run of
    /// documents over its byte bound). Deterministic, so replay sees the same records.
    fn write_split(
        &mut self,
        group: &[Bulk],
        run: &[(usize, usize)],
    ) -> Result<Vec<Receipt>, HostError> {
        match self.write_run(group, run) {
            Err(HostError::Refused(EngineError::Limit)) if run.len() > 1 => {
                let (head, tail) = run.split_at(run.len() / 2);
                let mut receipts = self.write_split(group, head)?;
                receipts.extend(self.write_split(group, tail)?);
                Ok(receipts)
            }
            other => other,
        }
    }

    fn write_run(
        &mut self,
        group: &[Bulk],
        run: &[(usize, usize)],
    ) -> Result<Vec<Receipt>, HostError> {
        let sequence = self.serving.next_seq();
        let first =
            i64::try_from((sequence + 1) << VERSION_SHIFT).map_err(|_| HostError::Invalid)?;
        let names: Vec<&str> = self.schema.fields().map(|(n, _)| n).collect();
        let rows: Vec<Vec<(&str, Value<'_>)>> = run
            .iter()
            .map(|(b, o)| match &group[*b].ops[*o] {
                Op::Put { fields, .. } => names
                    .iter()
                    .copied()
                    .zip(fields.iter())
                    .filter_map(|(name, field)| Some((name, value(field)?)))
                    .collect(),
                Op::Delete { .. } => Vec::new(),
            })
            .collect();
        let mutations: Vec<Mutation<'_>> = run
            .iter()
            .zip(&rows)
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
            .collect();
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
    fn new(ops: Vec<Op>, wait: Wait, reply: Sender<Result<Vec<ItemResult>, HostError>>) -> Self {
        let results = vec![None; ops.len()];
        Self {
            ops,
            wait,
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
