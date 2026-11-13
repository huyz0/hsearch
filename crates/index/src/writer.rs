//! The single writer of one index: it owns a scratch engine, assigns versions,
//! makes each group of bulks durable as one conditional-create log slot, applies
//! it, publishes segments and a root, and rebuilds itself from the store alone.
//!
//! Ordering is the whole design. A slot is created before it is applied, so a
//! definite store refusal leaves the engine untouched and the request
//! retryable, and an acknowledged write is in the store before anyone is told.
//! Recovery reads slots `0, 1, 2, ...` by derived key until the first
//! not-found and replays them into a fresh engine; no LIST anywhere.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use hsearch_engine::{
    Engine, LocalFiles, Mutation as EngineMutation, Outcome, Schema, Snapshot, Value,
};
use hsearch_object::{MetadataStore, Precondition, VersionToken};

use crate::format::{
    Field, MAX_SLOT_BYTES, MAX_SLOT_OPS, Op, Root, Slot, decode_root, decode_slot, encode_root,
    encode_slot,
};
use crate::store::{
    Written, block, context, key, mutation, read_all, read_version, root_key, segment_key,
    slot_key, write,
};
use crate::{Error, Result};

/// How long a bulk waits before it is acknowledged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Wait {
    /// Acknowledge once the write is durable in the store; searchable later.
    Durable,
    /// Acknowledge once the write is durable, applied and searchable.
    Searchable,
}

/// Per-operation result, in submission order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemResult {
    /// Document identity.
    pub id: String,
    /// The version the writer assigned.
    pub version: i64,
    /// What the operation did.
    pub outcome: Outcome,
}

/// Points at which a test harness may stop the process to simulate a crash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Point {
    /// A slot is built and validated; nothing is in the store yet.
    BeforeSlot,
    /// The slot is durable; nobody has been told.
    AfterSlot,
    /// A durable acknowledgement was sent; the slot is not applied yet.
    AfterAck,
    /// Segment files are uploaded; the root still names the previous ones.
    BeforeRoot,
}

/// Observer of [`Point`]s. The default does nothing.
pub trait Hooks: Send + Sync {
    /// Called at each point on the writer thread.
    fn at(&self, _point: Point) {}
}

/// The hooks of a production writer.
pub struct NoHooks;
impl Hooks for NoHooks {}

/// What a publish made visible to a search-only reader.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Published {
    /// Slots `0..covered` are in the published segments.
    pub covered: u64,
    /// Number of segment files named by the root.
    pub files: usize,
}

pub(crate) struct Bulk {
    pub(crate) ops: Vec<Op>,
    pub(crate) wait: Wait,
    pub(crate) reply: Sender<Result<Vec<ItemResult>>>,
}

pub(crate) enum Command {
    Bulk(Bulk),
    Refresh(Sender<Result<()>>),
    Publish(Sender<Result<Published>>),
    Stop,
}

pub(crate) type Shared = Arc<Mutex<Option<Arc<Snapshot>>>>;

fn value(field: &Field) -> Value<'_> {
    match field {
        Field::Str(s) => Value::String(s),
        Field::Int(n) => Value::Integer(*n),
        Field::Bool(b) => Value::Boolean(*b),
        Field::Date(n) => Value::Date(*n),
    }
}

fn prepare(
    schema: &Schema,
    request: &str,
    ops: &[Op],
    first_version: i64,
) -> Result<hsearch_engine::PreparedBatch> {
    let names: Vec<&str> = schema.fields().map(|(n, _)| n).collect();
    let mut rows: Vec<Vec<(&str, Value<'_>)>> = Vec::with_capacity(ops.len());
    for op in ops {
        rows.push(match op {
            Op::Put { fields, .. } => {
                if fields.len() != names.len() {
                    return Err(Error::Invalid);
                }
                names
                    .iter()
                    .copied()
                    .zip(fields.iter().map(value))
                    .collect()
            }
            Op::Delete { .. } => Vec::new(),
        });
    }
    let mutations: Vec<EngineMutation<'_>> = ops
        .iter()
        .zip(&rows)
        .enumerate()
        .map(|(i, (op, row))| {
            let version = first_version + i as i64;
            match op {
                Op::Put { id, .. } => EngineMutation::Put {
                    id,
                    version,
                    fields: row,
                },
                Op::Delete { id } => EngineMutation::Delete { id, version },
            }
        })
        .collect();
    schema.prepare(request, &mutations).map_err(Error::from)
}

/// Split into engine-sized runs of distinct identities, shrinking a run the
/// engine finds too large. Deterministic, so replay chunks exactly as live.
/// Upper bound of the bytes the engine charges to its retry history for `ops`.
fn cost(schema: &Schema, ops: &[Op]) -> usize {
    let names: usize = schema.fields().map(|(n, _)| n.len()).sum();
    ops.iter()
        .map(|op| {
            let body = match op {
                Op::Put { fields, .. } => {
                    names
                        + fields
                            .iter()
                            .map(|f| match f {
                                Field::Str(s) => s.len(),
                                _ => 8,
                            })
                            .sum::<usize>()
                }
                Op::Delete { .. } => 0,
            };
            // The request name is charged per chunk; this covers it per operation.
            op.id().len() + body + 32
        })
        .sum()
}

fn chunks(schema: &Schema, ops: &[Op], first_version: i64) -> Result<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < ops.len() {
        let mut seen = HashSet::new();
        let mut n = 0;
        while start + n < ops.len() && n < 16 && seen.insert(ops[start + n].id()) {
            n += 1;
        }
        loop {
            match prepare(
                schema,
                "check",
                &ops[start..start + n],
                first_version + start as i64,
            ) {
                Ok(_) => break,
                Err(Error::Engine(hsearch_engine::Error::Limit)) if n > 1 => n = (n / 2).max(1),
                Err(error) => return Err(error),
            }
        }
        ranges.push((start, start + n));
        start += n;
    }
    Ok(ranges)
}

struct Core<'e, 'd, S> {
    store: Arc<S>,
    name: String,
    schema: Schema,
    engine: &'e Engine<'d>,
    files: &'e LocalFiles,
    hooks: Arc<dyn Hooks>,
    shared: Shared,
    seq: u64,
    next_version: i64,
    ids: HashMap<String, bool>,
    live: usize,
    /// Lifetime operations and retry-history bytes the engine has admitted:
    /// bounds it enforces forever and that no merge or commit reduces.
    operations: usize,
    history: usize,
    epoch: u64,
    root_token: VersionToken,
    uploaded: BTreeMap<String, String>,
    published: Option<u64>,
    poisoned: bool,
}

impl<S: MetadataStore + 'static> Core<'_, '_, S> {
    fn track(&mut self, ops: &[Op]) {
        for op in ops {
            let after = matches!(op, Op::Put { .. });
            let before = self.ids.insert(op.id().to_string(), after).unwrap_or(false);
            match (before, after) {
                (false, true) => self.live += 1,
                (true, false) => self.live -= 1,
                _ => {}
            }
        }
    }

    fn results(&self, ops: &[Op], first_version: i64) -> Vec<ItemResult> {
        let mut overlay: HashMap<&str, bool> = HashMap::new();
        ops.iter()
            .enumerate()
            .map(|(i, op)| {
                let id = op.id();
                let before = overlay
                    .get(id)
                    .copied()
                    .or_else(|| self.ids.get(id).copied())
                    .unwrap_or(false);
                let put = matches!(op, Op::Put { .. });
                overlay.insert(id, put);
                ItemResult {
                    id: id.to_string(),
                    version: first_version + i as i64,
                    outcome: match (put, before) {
                        (true, false) => Outcome::Created,
                        (true, true) => Outcome::Updated,
                        (false, true) => Outcome::Deleted,
                        (false, false) => Outcome::NotFound,
                    },
                }
            })
            .collect()
    }

    /// Apply a durable slot. A full engine is merged once and retried.
    fn apply(&mut self, slot: &Slot) -> Result<()> {
        let ranges = chunks(&self.schema, &slot.ops, slot.first_version)?;
        for (k, (start, end)) in ranges.into_iter().enumerate() {
            let request = format!("{}.{k}", slot.seq);
            let ops = &slot.ops[start..end];
            let version = slot.first_version + start as i64;
            let first = self
                .engine
                .apply(prepare(&self.schema, &request, ops, version)?);
            match first {
                Ok(_) => {}
                Err(hsearch_engine::Error::Limit) => {
                    // A merge only compacts what a commit has flushed: without
                    // it deleted copies stay counted and the cap is still hit.
                    self.engine.commit()?;
                    self.engine.merge()?;
                    self.engine
                        .apply(prepare(&self.schema, &request, ops, version)?)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        self.track(&slot.ops);
        self.operations += slot.ops.len();
        self.history += cost(&self.schema, &slot.ops);
        self.seq = slot.seq + 1;
        self.next_version = self
            .next_version
            .max(slot.first_version + slot.ops.len() as i64);
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        let mut shared = self.shared.lock().map_err(|_| Error::Poisoned)?;
        // The previous snapshot's slot must be free before a new one is taken.
        shared.take();
        let snapshot = self.engine.refresh().map_err(|error| match error {
            hsearch_engine::Error::Limit => Error::Busy,
            other => Error::Engine(other),
        })?;
        *shared = Some(Arc::new(snapshot));
        Ok(())
    }

    fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Validate a request alone, so one bad bulk cannot fail a group.
    fn validate(&self, ops: &[Op]) -> Result<()> {
        if ops.is_empty() || ops.len() > MAX_SLOT_OPS {
            return Err(Error::Invalid);
        }
        if ops
            .iter()
            .any(|op| op.id().is_empty() || op.id().len() > 256)
        {
            return Err(Error::Invalid);
        }
        let probe = Slot {
            seq: 0,
            first_version: 1,
            ops: ops.to_vec(),
        };
        if encode_slot(&probe).len() > MAX_SLOT_BYTES / 2 {
            return Err(Error::Invalid);
        }
        chunks(&self.schema, ops, 1).map(|_| ())
    }

    fn handle_group(&mut self, group: Vec<Bulk>) {
        if self.poisoned {
            for bulk in group {
                let _ = bulk.reply.send(Err(Error::Poisoned));
            }
            return;
        }
        let limit = self.schema.limits().documents;
        let mut accepted: Vec<Bulk> = Vec::new();
        let mut ops: Vec<Op> = Vec::new();
        let mut bytes = 0usize;
        let mut new_ids: HashSet<String> = HashSet::new();
        let mut puts = 0usize;
        let mut planned_ops = 0usize;
        let mut planned_history = 0usize;
        let window = self.schema.limits().window;
        let (max_ops, max_history) = (
            self.schema.limits().operations,
            self.schema.limits().history_bytes,
        );
        for bulk in group {
            let verdict = self.validate(&bulk.ops).and_then(|()| {
                let size = encode_slot(&Slot {
                    seq: 0,
                    first_version: 1,
                    ops: bulk.ops.clone(),
                })
                .len();
                let added: HashSet<&str> = bulk
                    .ops
                    .iter()
                    .map(Op::id)
                    .filter(|id| !self.ids.contains_key(*id) && !new_ids.contains(*id))
                    .collect();
                let more_puts = bulk
                    .ops
                    .iter()
                    .filter(|op| matches!(op, Op::Put { .. }))
                    .count();
                if ops.len() + bulk.ops.len() > MAX_SLOT_OPS
                    || bytes + size > MAX_SLOT_BYTES
                    || self.ids.len() + new_ids.len() + added.len() > limit
                    || self.live + puts + more_puts > limit
                    || self.operations + planned_ops + bulk.ops.len() > max_ops
                    || (!window
                        && self.history + planned_history + cost(&self.schema, &bulk.ops)
                            > max_history)
                {
                    return Err(Error::Capacity);
                }
                bytes += size;
                puts += more_puts;
                planned_ops += bulk.ops.len();
                planned_history += cost(&self.schema, &bulk.ops);
                new_ids.extend(added.into_iter().map(str::to_string));
                Ok(())
            });
            match verdict {
                Ok(()) => {
                    ops.extend(bulk.ops.iter().cloned());
                    accepted.push(bulk);
                }
                Err(error) => {
                    let _ = bulk.reply.send(Err(error));
                }
            }
        }
        if accepted.is_empty() {
            return;
        }
        let slot = Slot {
            seq: self.seq,
            first_version: self.next_version,
            ops,
        };
        let results = self.results(&slot.ops, slot.first_version);
        self.hooks.at(Point::BeforeSlot);
        let outcome = self.write_slot(&slot);
        if let Err(error) = outcome {
            for bulk in accepted {
                let _ = bulk.reply.send(Err(error));
            }
            return;
        }
        self.hooks.at(Point::AfterSlot);
        // Durable. Acknowledge the ones that only wanted durability.
        let mut offset = 0;
        let mut searchable = Vec::new();
        for bulk in accepted {
            let n = bulk.ops.len();
            let mine = results[offset..offset + n].to_vec();
            offset += n;
            match bulk.wait {
                Wait::Durable => {
                    let _ = bulk.reply.send(Ok(mine));
                }
                Wait::Searchable => searchable.push((bulk, mine)),
            }
        }
        self.hooks.at(Point::AfterAck);
        let applied = self.apply(&slot);
        if applied.is_err() {
            self.poison();
        }
        if searchable.is_empty() {
            return;
        }
        let visible = applied.and_then(|()| self.refresh());
        for (bulk, mine) in searchable {
            let _ = bulk.reply.send(match visible {
                Ok(()) => Ok(mine),
                Err(Error::Busy) => Err(Error::Busy),
                Err(_) => Err(Error::AppliedFailed),
            });
        }
    }

    fn write_slot(&mut self, slot: &Slot) -> Result<()> {
        let request = mutation(
            &format!("{}/slot/{}/{}", self.name, slot.seq, self.epoch),
            &slot_key(&self.name, slot.seq),
            &encode_slot(slot),
            Precondition::Absent,
        )?;
        match write(&*self.store, &request, false) {
            Written::Done(_) => Ok(()),
            Written::Refused(error) => Err(Error::Store(error)),
            Written::Occupied => {
                self.poison();
                Err(Error::Fenced)
            }
            Written::Unknown => {
                self.poison();
                Err(Error::Unknown)
            }
        }
    }

    fn publish(&mut self) -> Result<Published> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        if self.published == Some(self.seq) {
            return Ok(Published {
                covered: self.seq,
                files: self.uploaded.len(),
            });
        }
        self.engine.commit()?;
        let names = self.files.names()?;
        for name in &names {
            if self.uploaded.contains_key(name) {
                continue;
            }
            let bytes = self.files.read(name)?;
            let object = segment_key(&self.name, self.epoch, name);
            let request = mutation(&object, &object, &bytes, Precondition::Absent)
                .map_err(|_| Error::Capacity)?;
            match write(&*self.store, &request, false) {
                Written::Done(_) => {
                    self.uploaded.insert(name.clone(), object);
                }
                Written::Refused(error) => return Err(Error::Store(error)),
                Written::Occupied => {
                    self.poison();
                    return Err(Error::Fenced);
                }
                Written::Unknown => return Err(Error::Unknown),
            }
        }
        if names.len() > crate::format::MAX_FILES {
            return Err(Error::Capacity);
        }
        self.hooks.at(Point::BeforeRoot);
        let root = Root {
            epoch: self.epoch,
            covered: self.seq,
            files: names
                .iter()
                .map(|n| (n.clone(), self.uploaded[n].clone()))
                .collect(),
        };
        self.swap_root(
            &root,
            &format!("{}/root/{}/{}", self.name, self.epoch, self.seq),
        )?;
        self.published = Some(self.seq);
        Ok(Published {
            covered: root.covered,
            files: root.files.len(),
        })
    }

    fn swap_root(&mut self, root: &Root, request: &str) -> Result<()> {
        let request = mutation(
            request,
            &root_key(&self.name),
            &encode_root(root),
            Precondition::Matches(self.root_token.clone()),
        )?;
        match write(&*self.store, &request, true) {
            Written::Done(receipt) => {
                self.root_token = receipt.object.version;
                Ok(())
            }
            Written::Refused(error) => Err(Error::Store(error)),
            Written::Occupied => {
                self.poison();
                Err(Error::Fenced)
            }
            Written::Unknown => {
                self.poison();
                Err(Error::Unknown)
            }
        }
    }
}

/// Unique per call and process, for identities that must not be shared.
fn nonce() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

pub(crate) fn put_mapping<S: MetadataStore>(store: &S, name: &str, bytes: &[u8]) -> Result<()> {
    let request = mutation(
        &format!("{name}/mapping/{}", nonce()),
        &format!("{name}/mapping"),
        bytes,
        Precondition::Absent,
    )?;
    match write(store, &request, false) {
        Written::Done(_) => Ok(()),
        Written::Occupied => Err(Error::Exists),
        Written::Refused(error) => Err(Error::Store(error)),
        Written::Unknown => Err(Error::Unknown),
    }
}

pub(crate) fn get_mapping<S: MetadataStore>(store: &S, name: &str) -> Result<Vec<u8>> {
    let object = key(&format!("{name}/mapping"))?;
    match read_all(store, &object) {
        Err(Error::Store(hsearch_object::Error::NotFound)) => Err(Error::NotFound),
        other => other,
    }
}

/// Create the root of a new index. Fails if it exists.
pub(crate) fn create_root<S: MetadataStore>(store: &S, name: &str) -> Result<()> {
    // A fresh identity per attempt: a replayed create must not read as success
    // for an index that already existed.
    let request = mutation(
        &format!("{name}/root/create/{}", nonce()),
        &root_key(name),
        &encode_root(&Root {
            epoch: 0,
            covered: 0,
            files: Vec::new(),
        }),
        Precondition::Absent,
    )?;
    match write(store, &request, false) {
        Written::Done(_) => Ok(()),
        Written::Occupied => Err(Error::Exists),
        Written::Refused(error) => Err(Error::Store(error)),
        Written::Unknown => Err(Error::Unknown),
    }
}

/// Read the published root and the version it is at.
pub(crate) fn read_root<S: MetadataStore>(store: &S, name: &str) -> Result<(Root, VersionToken)> {
    let root_key = key(&root_key(name))?;
    let info = block(store.head(&context(), &root_key)).map_err(|error| match error {
        hsearch_object::Error::NotFound => Error::NotFound,
        other => Error::Store(other),
    })?;
    let bytes = read_version(store, &root_key, &info.version, info.length)?;
    Ok((decode_root(&bytes)?, info.version))
}

/// Rebuild from the store, then serve commands until stopped. Sends exactly
/// one message on `init` unless the engine itself could not open.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run<S: MetadataStore + 'static>(
    store: Arc<S>,
    name: String,
    schema: Schema,
    scratch: std::path::PathBuf,
    hooks: Arc<dyn Hooks>,
    shared: Shared,
    init: Sender<Result<()>>,
    commands: Receiver<Command>,
) {
    let failure = init.clone();
    let result = hsearch_engine::with_local_engine(&scratch, schema.clone(), |engine, files| {
        let mut core = match start(store, name, schema, engine, files, hooks, shared) {
            Ok(core) => core,
            Err(error) => {
                let _ = init.send(Err(error));
                return;
            }
        };
        let _ = init.send(Ok(()));
        serve(&mut core, &commands);
    });
    if let Err(error) = result {
        let _ = failure.send(Err(Error::Engine(error)));
    }
}

fn start<'e, 'd, S: MetadataStore + 'static>(
    store: Arc<S>,
    name: String,
    schema: Schema,
    engine: &'e Engine<'d>,
    files: &'e LocalFiles,
    hooks: Arc<dyn Hooks>,
    shared: Shared,
) -> Result<Core<'e, 'd, S>> {
    let (root, token) = read_root(&*store, &name)?;
    let mut core = Core {
        store,
        name,
        schema,
        engine,
        files,
        hooks,
        shared,
        seq: 0,
        next_version: 1,
        ids: HashMap::new(),
        live: 0,
        operations: 0,
        history: 0,
        epoch: root.epoch,
        root_token: token,
        uploaded: BTreeMap::new(),
        published: None,
        poisoned: false,
    };
    // Replay every slot until the first that is not there. A store error other
    // than not-found is never read as the end of the log.
    loop {
        let object = key(&slot_key(&core.name, core.seq))?;
        match block(core.store.head(&context(), &object)) {
            Ok(info) => {
                let bytes = read_version(&*core.store, &object, &info.version, info.length)?;
                let slot = decode_slot(&bytes)?;
                if slot.seq != core.seq {
                    return Err(Error::Corrupt);
                }
                core.apply(&slot)?;
            }
            Err(hsearch_object::Error::NotFound) => break,
            Err(error) => return Err(Error::Store(error)),
        }
    }
    if root.covered > core.seq {
        // The root claims slots the log does not have: never guess.
        return Err(Error::Corrupt);
    }
    // Opening fences every earlier writer: their root publishes now mismatch.
    core.epoch = root.epoch + 1;
    let bumped = Root {
        epoch: core.epoch,
        covered: root.covered,
        files: root.files,
    };
    // A per-open nonce: two openers building the same bump must not resolve to
    // each other's receipt and both believe they own the epoch.
    let request = format!("{}/root/open/{}/{}", core.name, core.epoch, nonce());
    core.swap_root(&bumped, &request)?;
    core.refresh()?;
    Ok(core)
}

fn serve<S: MetadataStore + 'static>(core: &mut Core<'_, '_, S>, commands: &Receiver<Command>) {
    let mut stash: Option<Command> = None;
    loop {
        let command = match stash.take() {
            Some(command) => command,
            None => match commands.recv() {
                Ok(command) => command,
                Err(_) => return,
            },
        };
        match command {
            Command::Stop => return,
            Command::Refresh(reply) => {
                let _ = reply.send(if core.poisoned {
                    Err(Error::Poisoned)
                } else {
                    core.refresh()
                });
            }
            Command::Publish(reply) => {
                let _ = reply.send(core.publish());
            }
            Command::Bulk(first) => {
                // Group commit: everything already queued shares one slot.
                let mut group = vec![first];
                let mut ops = group[0].ops.len();
                while ops < MAX_SLOT_OPS {
                    match commands.try_recv() {
                        Ok(Command::Bulk(next)) => {
                            ops += next.ops.len();
                            group.push(next);
                        }
                        Ok(other) => {
                            stash = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                core.handle_group(group);
            }
        }
    }
}
