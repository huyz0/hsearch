//! Real raft-rs Ready lifecycle with explicitly persisted research scratch.
use super::common::{Batch, Context, Disk, Operation};
use super::{budget::Budget, workload};
use protobuf::Message as ProtobufMessage;
use raft::eraftpb::{
    ConfChange, ConfChangeType, ConfChangeV2, ConfState, Entry, EntryType, HardState, Message,
    MessageType, Snapshot,
};
use raft::storage::{GetEntriesContext, MemStorage, RaftState, Storage};
use raft::{Config, RawNode, Ready, SnapshotStatus, StateRole};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Store {
    memory: MemStorage,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
}

impl Storage for Store {
    fn initial_state(&self) -> raft::Result<RaftState> {
        self.memory.initial_state()
    }
    fn entries(
        &self,
        low: u64,
        high: u64,
        max_size: impl Into<Option<u64>>,
        context: GetEntriesContext,
    ) -> raft::Result<Vec<Entry>> {
        self.memory.entries(low, high, max_size, context)
    }
    fn term(&self, index: u64) -> raft::Result<u64> {
        self.memory.term(index)
    }
    fn first_index(&self) -> raft::Result<u64> {
        self.memory.first_index()
    }
    fn last_index(&self) -> raft::Result<u64> {
        self.memory.last_index()
    }
    fn snapshot(&self, request_index: u64, _to: u64) -> raft::Result<Snapshot> {
        match self.snapshot.lock().unwrap().as_ref() {
            Some(snapshot) if snapshot.get_metadata().index >= request_index => {
                Ok(snapshot.clone())
            }
            _ => Err(raft::Error::Store(
                raft::StorageError::SnapshotTemporarilyUnavailable,
            )),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Application {
    applied: u64,
    operations: BTreeMap<String, Operation>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Data {
    Utf8(String),
    Binary(Vec<u8>),
}
impl Data {
    fn encode(raw: &[u8]) -> Self {
        match std::str::from_utf8(raw) {
            Ok(text) => Self::Utf8(text.into()),
            Err(_) => Self::Binary(raw.into()),
        }
    }
    fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Utf8(text) => text.as_bytes().to_vec(),
            Self::Binary(raw) => raw.clone(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEntry {
    kind: u64,
    term: u64,
    index: u64,
    data: Data,
    context: Vec<u8>,
    sync_log: bool,
}
impl StoredEntry {
    fn encode(entry: &Entry) -> Self {
        Self::try_encode(entry).expect("Pinned research entry codec refused fields")
    }
    fn try_encode(entry: &Entry) -> io::Result<Self> {
        let stored = Self {
            kind: entry.get_entry_type() as u64,
            term: entry.term,
            index: entry.index,
            data: Data::encode(&entry.data),
            context: entry.context.to_vec(),
            sync_log: entry.sync_log,
        };
        if stored.try_proto()?.write_to_bytes().unwrap() != entry.write_to_bytes().unwrap() {
            return Err(io::Error::other(
                "Research codec cannot preserve pinned protobuf fields",
            ));
        }
        Ok(stored)
    }
    fn proto(&self) -> Entry {
        self.try_proto().expect("Invalid research entry kind")
    }
    fn try_proto(&self) -> io::Result<Entry> {
        let mut entry = Entry::default();
        entry.set_entry_type(match self.kind {
            0 => EntryType::EntryNormal,
            1 => EntryType::EntryConfChange,
            2 => EntryType::EntryConfChangeV2,
            _ => return Err(io::Error::other("Unknown research entry kind")),
        });
        entry.term = self.term;
        entry.index = self.index;
        entry.set_data(self.data.bytes().into());
        entry.set_context(self.context.clone().into());
        entry.sync_log = self.sync_log;
        Ok(entry)
    }
}

pub(super) fn codec_controls() {
    for data in [
        vec![],
        b"{\"operations\":[]}".to_vec(),
        vec![0, 255, 128, 1],
    ] {
        let mut entry = Entry {
            term: 17,
            index: 129,
            sync_log: true,
            ..Entry::default()
        };
        entry.set_context(vec![0, 255].into());
        entry.set_data(data.into());
        let stored = StoredEntry::encode(&entry);
        let bytes = serde_json::to_vec(&stored).unwrap();
        let decoded: StoredEntry = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            entry.write_to_bytes().unwrap(),
            decoded.proto().write_to_bytes().unwrap()
        );
    }
    let mut entry = Entry::default();
    entry.mut_unknown_fields().add_varint(99, 1);
    assert!(StoredEntry::try_encode(&entry).is_err());
    let mut stored = StoredEntry::encode(&Entry::default());
    stored.kind = 99;
    assert!(stored.try_proto().is_err());
    atomic_application_storage_control();
    membership_reopen_control();
}

fn atomic_application_storage_control() {
    let context = Context::new("raft-rs");
    let mut node = Node::create(1, context.root.join("node-1"), &context);
    let mut entry = Entry {
        term: 1,
        index: 1,
        ..Entry::default()
    };
    entry.set_data(
        serde_json::to_vec(&Batch::one("atomic-control"))
            .unwrap()
            .into(),
    );
    let mut next = node.disk.state.clone();
    next.entries.push(StoredEntry::encode(&entry));
    next.hard_state = HardState {
        term: 1,
        commit: 1,
        ..HardState::default()
    }
    .write_to_bytes()
    .unwrap();
    let before = context.counts.lock().unwrap().clone();
    let staged = node.stage_application(&mut next, vec![entry]);
    assert_eq!(node.disk.state.application.applied, 0);
    assert!(node.disk.state.application.operations.is_empty());
    assert_eq!(
        context.counts.lock().unwrap().file_write_calls,
        before.file_write_calls
    );
    node.disk.commit(next).unwrap();
    let after = context.counts.lock().unwrap().clone();
    assert_eq!(after.file_write_calls - before.file_write_calls, 3);
    assert_eq!(after.sync_calls - before.sync_calls, 2);
    let persisted: Persisted = Disk::read(&context.root.join("node-1")).unwrap();
    assert_eq!(persisted.application.applied, 1);
    assert_eq!(persisted.entries.len(), 1);
    assert_eq!(
        HardState::parse_from_bytes(&persisted.hard_state)
            .unwrap()
            .commit,
        1
    );
    assert_eq!(
        persisted.application.operations["atomic-control"],
        Operation::fixture("atomic-control")
    );
    node.record_applied(staged);
    context.finish();
}

fn membership_reopen_control() {
    let context = Context::new("raft-rs");
    let mut group = Group::new(&context);
    group.nodes.get_mut(&1).unwrap().raw.campaign().unwrap();
    group.pump();
    group.propose(1, Batch::one("membership-reopen-base"));
    group.membership(ConfChangeType::AddLearnerNode, 4, "control-learner");
    group.nodes.get_mut(&1).unwrap().snapshot();
    group.nodes.insert(
        4,
        Node::create_with_conf(
            4,
            context.root.join("node-4"),
            &context,
            ConfState::default(),
        ),
    );
    group.heartbeat();
    group.membership(ConfChangeType::AddNode, 4, "control-voter");
    let saved = &group.nodes[&1].disk.state;
    let snapshot = Snapshot::parse_from_bytes(&saved.snapshot).unwrap();
    assert!(snapshot.get_metadata().index < saved.application.applied);
    assert!(
        snapshot
            .get_metadata()
            .get_conf_state()
            .learners
            .contains(&4)
    );
    assert!(!snapshot.get_metadata().get_conf_state().voters.contains(&4));
    let expected = ConfState::parse_from_bytes(&saved.conf_state).unwrap();
    assert_eq!(
        BTreeSet::from_iter(expected.voters.clone()),
        BTreeSet::from([1, 2, 3, 4])
    );
    assert!(expected.learners.is_empty());
    drop(group.nodes.remove(&1).unwrap());
    let reopened = Node::reopen(1, context.root.join("node-1"), &context).unwrap();
    let actual = reopened.raw.raft.prs().conf().to_conf_state();
    assert_eq!(
        BTreeSet::from_iter(actual.voters),
        BTreeSet::from([1, 2, 3, 4])
    );
    assert!(
        actual.learners.is_empty()
            && actual.learners_next.is_empty()
            && actual.voters_outgoing.is_empty()
    );
    assert!(reopened.raw.raft.prs().get(4).is_some());
    assert!(!reopened.raw.raft.prs().conf().learners().contains(&4));
    group.discard_inbound_for_shutdown();
    context.finish();
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Persisted {
    schema_version: u64,
    hard_state: Vec<u8>,
    entries: Vec<StoredEntry>,
    conf_state: Vec<u8>,
    snapshot: Vec<u8>,
    application: Application,
}

struct Node {
    id: u64,
    raw: RawNode<Store>,
    disk: Disk<Persisted>,
    context: Context,
    held: Option<Ready>,
    hold: bool,
    hold_seen: bool,
    failed: bool,
}

impl Node {
    fn create(id: u64, path: PathBuf, context: &Context) -> Self {
        let conf = ConfState::from((vec![1, 2, 3], Vec::<u64>::new()));
        Self::create_with_conf(id, path, context, conf)
    }
    fn create_with_conf(id: u64, path: PathBuf, context: &Context, conf: ConfState) -> Self {
        let state = Persisted {
            schema_version: 2,
            hard_state: HardState::default().write_to_bytes().unwrap(),
            entries: vec![],
            conf_state: conf.write_to_bytes().unwrap(),
            snapshot: vec![],
            application: Application::default(),
        };
        let disk = Disk::create(path, state, context).unwrap();
        Self::from_disk(id, disk, context)
    }
    fn reopen(id: u64, path: PathBuf, context: &Context) -> io::Result<Self> {
        let disk: Disk<Persisted> = Disk::reopen(path, context)?;
        Self::validate_snapshot(&disk.state)?;
        Ok(Self::from_disk(id, disk, context))
    }
    fn validate_snapshot(state: &Persisted) -> io::Result<()> {
        if state.schema_version != 2 {
            return Err(io::Error::other("Research scratch version refused"));
        }
        if !state.snapshot.is_empty() {
            let snapshot = Snapshot::parse_from_bytes(&state.snapshot).map_err(io::Error::other)?;
            let application: Application =
                serde_json::from_slice(snapshot.get_data()).map_err(io::Error::other)?;
            if snapshot.is_empty()
                || application.applied != snapshot.get_metadata().index
                || application.applied > state.application.applied
            {
                return Err(io::Error::other("Research snapshot prefix refused"));
            }
        }
        Ok(())
    }
    fn from_disk(id: u64, disk: Disk<Persisted>, context: &Context) -> Self {
        let state = &disk.state;
        assert_eq!(
            state.schema_version, 2,
            "Research scratch version mismatch; no fresh-cluster fallback"
        );
        let conf = ConfState::parse_from_bytes(&state.conf_state).unwrap();
        let store = Store {
            memory: MemStorage::new_with_conf_state(conf),
            snapshot: Arc::new(Mutex::new(None)),
        };
        let mut hard = HardState::parse_from_bytes(&state.hard_state).unwrap();
        let mut last_snapshot = 0;
        if !state.snapshot.is_empty() {
            let snapshot = Snapshot::parse_from_bytes(&state.snapshot).unwrap();
            last_snapshot = snapshot.get_metadata().index;
            let application: Application = serde_json::from_slice(snapshot.get_data()).unwrap();
            assert_eq!(application.applied, last_snapshot);
            assert!(application.applied <= state.application.applied);
            store.memory.wl().apply_snapshot(snapshot.clone()).unwrap();
            *store.snapshot.lock().unwrap() = Some(snapshot);
        }
        let entries = state
            .entries
            .iter()
            .map(StoredEntry::proto)
            .collect::<Vec<_>>();
        let mut previous = last_snapshot;
        for entry in &entries {
            assert_eq!(entry.index, previous + 1);
            previous = entry.index;
        }
        assert!(state.application.applied <= previous);
        store.memory.wl().append(&entries).unwrap();
        // A persisted application tuple establishes the committed/applied prefix.
        // LightReady.commit_index alone is not claimed to require another fsync.
        hard.commit = hard.commit.max(state.application.applied);
        assert!(hard.commit <= previous);
        store.memory.wl().set_hardstate(hard);
        // apply_snapshot restores its older ConfState. Config.applied below
        // skips through the persisted application prefix, whose membership
        // must therefore be restored after that snapshot, before RawNode.
        store
            .memory
            .wl()
            .set_conf_state(ConfState::parse_from_bytes(&state.conf_state).unwrap());
        let config = Config {
            id,
            election_tick: 10,
            heartbeat_tick: 1,
            max_size_per_msg: 1024 * 1024,
            max_inflight_msgs: 8,
            applied: state.application.applied,
            check_quorum: true,
            pre_vote: true,
            ..Config::default()
        };
        config.validate().unwrap();
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        let raw = RawNode::new(&config, store, &logger).unwrap();
        Self {
            id,
            raw,
            disk,
            context: context.clone(),
            held: None,
            hold: false,
            hold_seen: false,
            failed: false,
        }
    }
    fn persist_ready(&mut self, ready: &Ready, committed: Vec<Entry>) -> io::Result<()> {
        assert!(committed.len() <= 256 && ready.entries().len() <= 256);
        let committed_entries = committed
            .iter()
            .map(|entry| (entry.index, entry.term))
            .collect::<Vec<_>>();
        if !committed_entries.is_empty() {
            self.context.event(
                json!({"kind":"committed_apply_received","node":self.id,"api":"Ready",
                "entries":committed_entries}),
            );
        }
        let mut next = self.disk.state.clone();
        if !ready.snapshot().is_empty() {
            let snapshot = ready.snapshot();
            let application: Application =
                serde_json::from_slice(snapshot.get_data()).map_err(io::Error::other)?;
            if application.applied != snapshot.get_metadata().index {
                return Err(io::Error::other("Snapshot application prefix mismatch"));
            }
            next.application = application;
            next.conf_state = snapshot
                .get_metadata()
                .get_conf_state()
                .write_to_bytes()
                .unwrap();
            next.snapshot = snapshot.write_to_bytes().unwrap();
            next.entries
                .retain(|raw| raw.index > snapshot.get_metadata().index);
        }
        if let Some(first) = ready.entries().first() {
            next.entries.retain(|raw| raw.index < first.index);
            next.entries
                .extend(ready.entries().iter().map(StoredEntry::encode));
        }
        if let Some(hard) = ready.hs() {
            next.hard_state = hard.write_to_bytes().unwrap();
        }
        // Ready.committed_entries are already committed in stable storage.
        // Normal entries only stage our pure application map. Configuration
        // changes mutate RawNode and retain the existing post-Ready path.
        // Messages, applied milestones and advance still follow actual sync.
        let (atomic, deferred) = if committed
            .iter()
            .all(|entry| entry.get_entry_type() == EntryType::EntryNormal)
        {
            (committed, vec![])
        } else {
            (vec![], committed)
        };
        let applied = self.stage_application(&mut next, atomic);
        if !ready.snapshot().is_empty()
            || !ready.entries().is_empty()
            || ready.hs().is_some()
            || !applied.is_empty()
        {
            self.disk.commit(next)?;
        }
        let mut memory = self.raw.mut_store().memory.wl();
        if !ready.snapshot().is_empty() {
            memory.apply_snapshot(ready.snapshot().clone()).unwrap();
        }
        memory.append(ready.entries()).unwrap();
        if let Some(hard) = ready.hs() {
            memory.set_hardstate(hard.clone());
        }
        drop(memory);
        if !ready.snapshot().is_empty() {
            *self.raw.mut_store().snapshot.lock().unwrap() = Some(ready.snapshot().clone());
        }
        let persisted_snapshot = self
            .raw
            .mut_store()
            .snapshot
            .lock()
            .unwrap()
            .as_ref()
            .map(|snapshot| (snapshot.get_metadata().index, snapshot.get_metadata().term));
        self.context.event(json!({"kind":"ready_persisted", "node":self.id, "entries":ready.entries().iter().map(|e|e.index).collect::<Vec<_>>(),
                                  "entry_terms":ready.entries().iter().map(|e|e.term).collect::<Vec<_>>(),
                                  "committed_entries":committed_entries,"persisted_snapshot":persisted_snapshot,
                                  "hard_state":ready.hs().is_some(), "snapshot":!ready.snapshot().is_empty(), "sync_complete":true,"transaction":self.disk.transaction(),
                                  "atomic_application_indices":applied.iter().map(|(index,_,_)|*index).collect::<Vec<_>>()}));
        if !applied.is_empty() {
            self.record_applied(applied);
        }
        self.apply(deferred)?;
        Ok(())
    }
    fn stage_application(
        &mut self,
        next: &mut Persisted,
        entries: Vec<Entry>,
    ) -> Vec<(u64, u64, Vec<String>)> {
        let mut applied = vec![];
        for entry in entries {
            assert_eq!(entry.index, next.application.applied + 1);
            let mut identities = vec![];
            match entry.get_entry_type() {
                EntryType::EntryNormal if !entry.get_data().is_empty() => {
                    let batch: Batch = serde_json::from_slice(entry.get_data()).unwrap();
                    batch.validate();
                    for operation in batch.operations {
                        identities.push(operation.id.clone());
                        if let Some(existing) = next.application.operations.get(&operation.id) {
                            assert_eq!(existing, &operation);
                        } else {
                            next.application
                                .operations
                                .insert(operation.id.clone(), operation);
                        }
                    }
                    assert!(next.application.operations.len() <= 128);
                }
                EntryType::EntryConfChange => {
                    let change = ConfChange::parse_from_bytes(entry.get_data()).unwrap();
                    next.conf_state = self
                        .raw
                        .apply_conf_change(&change)
                        .unwrap()
                        .write_to_bytes()
                        .unwrap();
                }
                EntryType::EntryConfChangeV2 => {
                    let change = ConfChangeV2::parse_from_bytes(entry.get_data()).unwrap();
                    next.conf_state = self
                        .raw
                        .apply_conf_change(&change)
                        .unwrap()
                        .write_to_bytes()
                        .unwrap();
                }
                EntryType::EntryNormal => {}
            }
            next.application.applied = entry.index;
            applied.push((entry.index, entry.term, identities));
        }
        applied
    }
    fn record_applied(&mut self, applied: Vec<(u64, u64, Vec<String>)>) {
        let conf = ConfState::parse_from_bytes(&self.disk.state.conf_state).unwrap();
        self.raw.mut_store().memory.wl().set_conf_state(conf);
        for (index, term, identities) in applied {
            self.context.event(
                json!({"kind":"applied", "node":self.id, "index":index, "term":term,"ids":identities,
                                      "application_synced":true, "candidate_committed":true,"transaction":self.disk.transaction()}),
            );
        }
    }
    fn apply(&mut self, entries: Vec<Entry>) -> io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut next = self.disk.state.clone();
        let applied = self.stage_application(&mut next, entries);
        self.disk.commit(next)?;
        self.record_applied(applied);
        Ok(())
    }
    fn ready(&mut self) -> io::Result<(Vec<Message>, bool)> {
        if self.failed || (self.held.is_none() && !self.raw.has_ready()) {
            return Ok((vec![], false));
        }
        let mut ready = self.held.take().unwrap_or_else(|| self.raw.ready());
        if self.hold && !ready.entries().is_empty() {
            if !self.hold_seen {
                self.context
                    .event(json!({"kind":"persistence_barrier_reached", "node":self.id,
                    "indices":ready.entries().iter().map(|entry|entry.index).collect::<Vec<_>>()}));
            }
            self.hold_seen = true;
            self.held = Some(ready);
            return Ok((vec![], false));
        }
        let committed = ready.take_committed_entries();
        self.persist_ready(&ready, committed)?;
        let mut messages = ready.take_messages();
        messages.extend(ready.take_persisted_messages());
        let mut light = self.raw.advance_append(ready);
        let committed = light.take_committed_entries();
        self.context.event(
            json!({"kind":"advance_append", "node":self.id, "light_commit":light.commit_index(),
                   "light_committed_entries":committed.iter().map(|entry|(entry.index,entry.term)).collect::<Vec<_>>()}),
        );
        messages.extend(light.take_messages());
        self.apply(committed)?;
        self.raw
            .advance_apply_to(self.disk.state.application.applied);
        self.context.event(json!({"kind":"advance_apply", "node":self.id, "actual_applied":self.disk.state.application.applied}));
        Ok((messages, true))
    }
    fn snapshot(&mut self) {
        let mut next = self.disk.state.clone();
        let index = next.application.applied;
        assert!(index > 0 && self.held.is_none());
        let mut snapshot = Snapshot::default();
        snapshot.mut_metadata().index = index;
        snapshot.mut_metadata().term = self.raw.store().term(index).unwrap();
        snapshot
            .mut_metadata()
            .set_conf_state(ConfState::parse_from_bytes(&next.conf_state).unwrap());
        snapshot.set_data(serde_json::to_vec(&next.application).unwrap().into());
        next.snapshot = snapshot.write_to_bytes().unwrap();
        next.entries.retain(|raw| raw.index > index);
        self.disk.commit(next).unwrap();
        self.raw
            .mut_store()
            .memory
            .wl()
            .apply_snapshot(snapshot.clone())
            .unwrap();
        self.raw
            .mut_store()
            .memory
            .wl()
            .set_hardstate(HardState::parse_from_bytes(&self.disk.state.hard_state).unwrap());
        let term = snapshot.get_metadata().term;
        *self.raw.mut_store().snapshot.lock().unwrap() = Some(snapshot);
        self.context
            .event(json!({"kind":"snapshot_persisted", "node":self.id, "index":index,"term":term,"transaction":self.disk.transaction()}));
    }
}

struct Group {
    nodes: BTreeMap<u64, Node>,
    blocked: BTreeSet<(u64, u64)>,
    inbound: BTreeMap<u64, VecDeque<(Message, u64)>>,
    snapshot_reports: Vec<(u64, u64, u64)>,
    deferred: u64,
    context: Context,
}

impl Group {
    fn new(context: &Context) -> Self {
        let nodes = (1..=3)
            .map(|id| {
                (
                    id,
                    Node::create(id, context.root.join(format!("node-{id}")), context),
                )
            })
            .collect();
        Self {
            nodes,
            blocked: BTreeSet::new(),
            inbound: BTreeMap::new(),
            snapshot_reports: vec![],
            deferred: 0,
            context: context.clone(),
        }
    }
    fn cycle(&mut self) -> bool {
        self.context.deadline();
        let mut progress = false;
        let mut messages = vec![];
        for node in self.nodes.values_mut() {
            match node.ready() {
                Ok((outgoing, worked)) => {
                    progress |= worked;
                    messages.extend(outgoing);
                }
                Err(_) => {
                    node.failed = true;
                    self.context.event(json!({"kind":"node_io_error", "node":node.id, "poisoned_until_reload":true}));
                }
            }
        }
        for (source, target, index) in std::mem::take(&mut self.snapshot_reports) {
            let receiver = &self.nodes[&target];
            let persisted = !receiver.disk.state.snapshot.is_empty()
                && Snapshot::parse_from_bytes(&receiver.disk.state.snapshot)
                    .unwrap()
                    .get_metadata()
                    .index
                    >= index;
            if receiver.failed || persisted {
                self.nodes.get_mut(&source).unwrap().raw.report_snapshot(
                    target,
                    if persisted {
                        SnapshotStatus::Finish
                    } else {
                        SnapshotStatus::Failure
                    },
                );
                self.context.event(json!({"kind":"snapshot_report", "source":source, "target":target,
                                          "snapshot_index":index, "delivered_and_persisted":persisted}));
                progress = true;
            } else {
                self.snapshot_reports.push((source, target, index));
            }
        }
        assert!(messages.len() <= 4096);
        for message in messages {
            let source = message.from;
            let target = message.to;
            let dropped = self.blocked.contains(&(source, target))
                || !self.nodes.contains_key(&target)
                || self.nodes[&target].failed;
            let raw = message.write_to_bytes().unwrap();
            let edge = self.context.wire(
                source,
                target,
                &format!("{:?}", message.get_msg_type()),
                &raw,
                dropped,
            );
            if !dropped {
                let decoded = Message::parse_from_bytes(&raw).unwrap();
                let queue = self.inbound.entry(target).or_default();
                assert!(queue.len() < 64);
                queue.push_back((decoded, edge));
                assert!(self.inbound.values().map(VecDeque::len).sum::<usize>() <= 256);
                if self.nodes[&target].held.is_some() {
                    self.deferred += 1;
                    self.context.event(
                        json!({"kind":"receive_deferred_for_ready", "node":target, "edge":edge}),
                    );
                }
            } else if message.get_msg_type() == MessageType::MsgSnapshot {
                self.nodes
                    .get_mut(&source)
                    .unwrap()
                    .raw
                    .report_snapshot(target, SnapshotStatus::Failure);
                self.context.event(json!({"kind":"snapshot_report", "source":source, "target":target, "delivered":false}));
            }
        }
        for (id, node) in &mut self.nodes {
            if node.held.is_some() || node.failed {
                continue;
            }
            let received = self.inbound.entry(*id).or_default().pop_front();
            if let Some((message, edge)) = received {
                let source = message.from;
                let snapshot = message.get_msg_type() == MessageType::MsgSnapshot;
                let snapshot_index = if snapshot {
                    message.get_snapshot().get_metadata().index
                } else {
                    0
                };
                self.context.event(json!({"kind":"step_received", "node":id, "edge":edge, "prior_ready_advanced":true}));
                let kind = message.get_msg_type();
                match node.raw.step(message) {
                    Ok(()) => {}
                    Err(raft::Error::StepPeerNotFound) => {
                        assert!(node.raw.raft.prs().get(source).is_none());
                        self.context.event(json!({"kind":"step_peer_not_found","node":id,"source":source,"edge":edge,"message":format!("{kind:?}"),"source_absent_from_progress":true}));
                    }
                    Err(error) => panic!("Unexpected research step failure: {error}"),
                }
                progress = true;
                if snapshot {
                    self.snapshot_reports.push((source, *id, snapshot_index));
                    // Report only after this receiver's Ready has persisted/applied the snapshot.
                    self.context.event(json!({"kind":"snapshot_received_pending_persistence", "source":source, "target":id}));
                }
            }
        }
        progress
    }
    fn pump(&mut self) {
        while self.cycle() {}
    }
    fn submit(&mut self, leader: u64, batch: Batch) {
        batch.validate();
        self.nodes
            .get_mut(&leader)
            .unwrap()
            .raw
            .propose(vec![], serde_json::to_vec(&batch).unwrap())
            .unwrap();
    }
    fn propose(&mut self, leader: u64, batch: Batch) {
        self.submit(leader, batch);
        self.pump();
    }
    fn discard_inbound_for_shutdown(&mut self) {
        assert!(self.snapshot_reports.is_empty());
        for (node, queue) in std::mem::take(&mut self.inbound) {
            for (_, edge) in queue {
                self.context.event(json!({"kind":"received_abandoned", "node":node, "edge":edge, "reason":"graceful_transport_shutdown"}));
            }
        }
    }
    fn persisted_copies(&self, id: &str) -> Vec<u64> {
        self.nodes
            .keys()
            .filter_map(|node| {
                let state: Persisted =
                    Disk::read(&self.context.root.join(format!("node-{node}"))).unwrap();
                let in_log = state
                    .entries
                    .iter()
                    .map(StoredEntry::proto)
                    .filter(|e| e.get_entry_type() == EntryType::EntryNormal && !e.data.is_empty())
                    .any(|entry| {
                        serde_json::from_slice::<Batch>(&entry.data)
                            .unwrap()
                            .operations
                            .iter()
                            .any(|op| op.id == id)
                    });
                if in_log || state.application.operations.contains_key(id) {
                    Some(*node)
                } else {
                    None
                }
            })
            .collect()
    }
    fn acknowledge(&self, leader: u64, id: &str) {
        assert!(
            self.nodes[&leader]
                .disk
                .state
                .application
                .operations
                .contains_key(id)
        );
        let copies = self.persisted_copies(id);
        let conf = ConfState::parse_from_bytes(&self.nodes[&leader].disk.state.conf_state).unwrap();
        let voter_sets = [conf.voters.clone(), conf.voters_outgoing.clone()]
            .into_iter()
            .filter(|set| !set.is_empty())
            .collect::<Vec<_>>();
        assert!(!voter_sets.is_empty());
        for voters in &voter_sets {
            assert!(voters.iter().filter(|node| copies.contains(node)).count() > voters.len() / 2);
        }
        self.context.event(
            json!({"kind":"client_ack", "leader":leader, "id":id, "persisted_nodes":copies,
                                  "voter_sets":voter_sets,"applied":self.nodes[&leader].disk.state.application.applied}),
        );
    }
    fn isolate(&mut self, node: u64, blocked: bool) {
        for peer in self.nodes.keys().copied().filter(|peer| *peer != node) {
            for edge in [(node, peer), (peer, node)] {
                if blocked {
                    self.blocked.insert(edge);
                } else {
                    self.blocked.remove(&edge);
                }
            }
        }
        self.context
            .event(json!({"kind":"partition_changed","node":node,"isolated":blocked}));
    }
    fn witness(&self, label: &str) {
        for id in self.nodes.keys() {
            self.context.state_witness(&format!("node-{id}"), label);
        }
    }
    fn heartbeat(&mut self) {
        self.nodes.get_mut(&1).unwrap().raw.ping();
        self.pump();
    }
    fn membership(&mut self, kind: ConfChangeType, target: u64, label: &str) {
        let mut change = ConfChange {
            node_id: target,
            ..ConfChange::default()
        };
        change.set_change_type(kind);
        self.context.event(json!({"kind":"membership_api_proposed","api":"propose_conf_change","change":format!("{kind:?}"),"target":target,"label":label}));
        self.nodes
            .get_mut(&1)
            .unwrap()
            .raw
            .propose_conf_change(vec![], change)
            .unwrap();
        self.pump();
        let conf = ConfState::parse_from_bytes(&self.nodes[&1].disk.state.conf_state).unwrap();
        self.witness(label);
        self.context.event(json!({"kind":"membership_transition","label":label,"voters":conf.voters,"outgoing":conf.voters_outgoing,"learners":conf.learners,
            "index":self.nodes[&1].disk.state.application.applied,"persisted":true}));
    }
}

pub(super) fn ready_persistence_probe() {
    let context = Context::new("raft-rs");
    let mut group = Group::new(&context);
    group.nodes.get_mut(&1).unwrap().raw.campaign().unwrap();
    group.pump();
    assert_eq!(group.nodes[&1].raw.raft.state, StateRole::Leader);
    group.propose(1, Batch::one("healthy"));
    group.acknowledge(1, "healthy");
    for id in [2, 3] {
        group.nodes.get_mut(&id).unwrap().hold = true;
    }
    group.propose(1, Batch::one("held"));
    assert!(group.nodes[&2].hold_seen && group.nodes[&3].hold_seen);
    assert!(
        !group.nodes[&1]
            .disk
            .state
            .application
            .operations
            .contains_key("held")
    );
    assert_eq!(group.persisted_copies("held"), vec![1]);
    group.nodes.get_mut(&2).unwrap().hold = false;
    group.pump();
    group.acknowledge(1, "held");
    group.nodes.get_mut(&3).unwrap().hold = false;
    group.pump();
    assert!(
        group.deferred > 0,
        "Regression history must actually queue inbound messages behind a held Ready"
    );
    for target in [2, 3] {
        group.blocked.insert((1, target));
        group.blocked.insert((target, 1));
    }
    group.propose(1, Batch::one("isolated-unknown"));
    assert!(
        !group.nodes[&1]
            .disk
            .state
            .application
            .operations
            .contains_key("isolated-unknown")
    );
    assert_eq!(group.persisted_copies("isolated-unknown"), vec![1]);
    group.blocked.clear();
    group.nodes.get_mut(&1).unwrap().raw.tick();
    group.pump();
    group.acknowledge(1, "isolated-unknown");
    group.nodes.get_mut(&3).unwrap().disk.fail_next = true;
    group.propose(1, Batch::one("io-error-majority"));
    group.acknowledge(1, "io-error-majority");
    assert!(group.nodes[&3].failed);
    let state = group.nodes[&3].disk.state.clone();
    assert!(group.nodes.get_mut(&3).unwrap().disk.commit(state).is_err());
    group.discard_inbound_for_shutdown();
    group.nodes.clear();
    for id in 1..=3 {
        let disk = Disk::reopen(context.root.join(format!("node-{id}")), &context).unwrap();
        group.nodes.insert(id, Node::from_disk(id, disk, &context));
    }
    group.nodes.get_mut(&1).unwrap().raw.campaign().unwrap();
    group.pump();
    group.propose(1, Batch::one("after-reopen"));
    group.acknowledge(1, "after-reopen");
    for node in group.nodes.values_mut() {
        node.snapshot();
    }
    group.discard_inbound_for_shutdown();
    group.nodes.clear();
    for id in 1..=3 {
        let disk = Disk::reopen(context.root.join(format!("node-{id}")), &context).unwrap();
        group.nodes.insert(id, Node::from_disk(id, disk, &context));
    }
    assert!(group.nodes.values().all(|node| {
        node.disk
            .state
            .application
            .operations
            .contains_key("after-reopen")
    }));
    context.event(
        json!({"kind":"graceful_reopen_verified", "snapshot":true, "power_loss_qualified":false}),
    );
    println!("DURABILITY_ADAPTER_PROBE: {}", context.finish());
}

impl workload::Driver for Group {
    fn submit(&mut self, _ticket: usize, batch: &Batch) {
        self.submit(1, batch.clone());
    }
    fn advance(&mut self) {
        self.cycle();
    }
    fn finished(&mut self, ticket: usize, batch: &Batch) -> bool {
        if !batch.operations.iter().all(|operation| {
            self.nodes[&1]
                .disk
                .state
                .application
                .operations
                .contains_key(&operation.id)
        }) {
            return false;
        }
        self.context.event(json!({"kind":"adapter_response_observed","ticket":ticket,"ids":batch.operations.iter().map(|op|&op.id).collect::<Vec<_>>()}));
        for operation in &batch.operations {
            self.acknowledge(1, &operation.id);
        }
        true
    }
    fn heartbeat(&mut self) {
        for node in self.nodes.values_mut() {
            node.raw.tick();
        }
    }
    fn stop(mut self) {
        self.pump();
        assert!(self.nodes.values().all(|node| !node.failed));
        self.discard_inbound_for_shutdown();
        self.nodes.clear();
        self.context
            .event(json!({"kind":"driver_stopped","native_protocol":"raft-rs"}));
    }
}

pub(super) fn matched_workload_experiment() {
    let runtime = super::common::sample_runtime("raft-rs");
    let budget = Budget::sample();
    let workloads = matched_histories(&budget);
    let failure = failure_history(&budget);
    let scales = scale_histories(&budget);
    budget.check().unwrap();
    println!(
        "NATIVE_DURABILITY_SAMPLE: {}",
        json!({"sample_schema":2,"runtime":runtime,"candidate":"raft-rs","profile":if cfg!(debug_assertions) {"dev"} else {"release"},
        "workloads":workloads,"failure":failure,"scales":scales,"aggregate_counts":budget.counts(),"elapsed_us":budget.elapsed_us(),
        "scope":"One complete candidate sample; native/profile/repetition/source/coverage/review/CI acceptance remains separate"})
    );
}

fn matched_histories(budget: &Budget) -> Vec<serde_json::Value> {
    let mut cases = vec![];
    for trace in workload::fixtures() {
        for wait in [50000, 250000] {
            budget.check().unwrap();
            let context = Context::for_sample("raft-rs", budget);
            let mut group = Group::new(&context);
            group.nodes.get_mut(&1).unwrap().raw.campaign().unwrap();
            group.pump();
            assert_eq!(group.nodes[&1].raw.raft.state, StateRole::Leader);
            let measurement = workload::run_case(group, &context, &trace, wait);
            let raw = context.finish();
            cases.push(json!({"case":measurement,"raw":raw}));
        }
    }
    budget.check().unwrap();
    assert_eq!(cases.len(), 18);
    cases
}

pub(super) fn failure_component() {
    let budget = Budget::sample();
    let result = failure_history(&budget);
    budget.check().unwrap();
    println!(
        "NATIVE_FAILURE_COMPONENT: {}",
        json!({"candidate":"raft-rs","case":result,"aggregate_counts":budget.counts(),"scope":"Failure/membership/snapshot component; full sample/ARM/release/coverage/review remains"})
    );
}

fn failure_history(budget: &Budget) -> serde_json::Value {
    let context = Context::for_sample("raft-rs", budget);
    let mut group = Group::new(&context);
    context.event(json!({"kind":"failure_begin","history":"fixed-failures-membership-v1"}));
    group.nodes.get_mut(&1).unwrap().raw.campaign().unwrap();
    group.pump();
    assert_eq!(group.nodes[&1].raw.raft.state, StateRole::Leader);
    group.propose(1, Batch::one("failure-healthy"));
    group.acknowledge(1, "failure-healthy");
    group.isolate(3, true);
    group.propose(1, Batch::one("failure-minority-offline"));
    group.acknowledge(1, "failure-minority-offline");
    assert!(
        !group.nodes[&3]
            .disk
            .state
            .application
            .operations
            .contains_key("failure-minority-offline")
    );
    group.witness("minority-offline");
    context.event(json!({"kind":"minority_offline_verified","id":"failure-minority-offline","offline":3,"applied":false}));
    group.isolate(3, false);
    group.heartbeat();
    assert!(
        group.nodes[&3]
            .disk
            .state
            .application
            .operations
            .contains_key("failure-minority-offline")
    );
    group.witness("minority-recovered");
    context.event(json!({"kind":"minority_recovered","node":3,"id":"failure-minority-offline"}));
    group.isolate(1, true);
    group.propose(1, Batch::one("failure-leader-unknown"));
    assert_eq!(group.persisted_copies("failure-leader-unknown"), vec![1]);
    assert!(
        !group.nodes[&1]
            .disk
            .state
            .application
            .operations
            .contains_key("failure-leader-unknown")
    );
    group.witness("unreplicated");
    context.event(json!({"kind":"unreplicated_barrier","id":"failure-leader-unknown","persisted_nodes":[1],"applied":false,"successful_reply":false}));
    group.isolate(1, false);
    group.heartbeat();
    group.acknowledge(1, "failure-leader-unknown");
    group.witness("leader-healed");
    context.event(json!({"kind":"identity_resolved","id":"failure-leader-unknown","resubmitted":false,"outcome":"published"}));
    for id in [2, 3] {
        group.nodes.get_mut(&id).unwrap().hold = true;
    }
    group.propose(1, Batch::one("failure-held"));
    assert!(group.nodes[&2].hold_seen && group.nodes[&3].hold_seen);
    assert_eq!(group.persisted_copies("failure-held"), vec![1]);
    assert!(
        !group.nodes[&1]
            .disk
            .state
            .application
            .operations
            .contains_key("failure-held")
    );
    group.witness("held");
    context.event(json!({"kind":"held_majority_barrier","id":"failure-held","followers":[2,3],"successful_reply":false,"applied":false}));
    group.nodes.get_mut(&2).unwrap().hold = false;
    group.pump();
    group.acknowledge(1, "failure-held");
    group.nodes.get_mut(&3).unwrap().hold = false;
    group.pump();
    assert!(group.deferred > 0);
    group.propose(1, Batch::one("failure-suppressed"));
    assert!(
        group.nodes[&1]
            .disk
            .state
            .application
            .operations
            .contains_key("failure-suppressed")
    );
    group.witness("suppressed");
    context.event(json!({"kind":"client_reply_suppressed","id":"failure-suppressed","observed_adapter_commit":true,"outcome":"unknown","injection":"wrapper_suppression_not_transport_loss"}));
    let copies = group.persisted_copies("failure-suppressed");
    assert!(copies.len() >= 2);
    context.event(json!({"kind":"identity_resolved","id":"failure-suppressed","resubmitted":false,"outcome":"published","persisted_nodes":copies}));
    group.acknowledge(1, "failure-suppressed");
    group.nodes.get_mut(&3).unwrap().disk.fail_next = true;
    group.propose(1, Batch::one("failure-io-majority"));
    group.acknowledge(1, "failure-io-majority");
    assert!(group.nodes[&3].failed);
    let unchanged = group.nodes[&3].disk.state.clone();
    assert!(
        group
            .nodes
            .get_mut(&3)
            .unwrap()
            .disk
            .commit(unchanged)
            .is_err()
    );
    context.event(json!({"kind":"poisoned_node_refused","node":3}));
    group.witness("poisoned");
    let old = group.nodes.remove(&3).unwrap();
    drop(old);
    if let Some(queue) = group.inbound.remove(&3) {
        for (_, edge) in queue {
            context.event(json!({"kind":"received_abandoned","node":3,"edge":edge,"reason":"explicit_failed_node_reload"}));
        }
    }
    group.nodes.insert(
        3,
        Node::reopen(3, context.root.join("node-3"), &context).unwrap(),
    );
    group.heartbeat();
    assert!(
        group.nodes[&3]
            .disk
            .state
            .application
            .operations
            .contains_key("failure-io-majority")
    );
    group.witness("reloaded");
    context.event(
        json!({"kind":"node_reloaded","node":3,"id":"failure-io-majority","fresh_cluster":false}),
    );
    // No message reaches4 until a persisted leader snapshot carries the actual learner configuration.
    group.membership(ConfChangeType::AddLearnerNode, 4, "learner-added");
    group.nodes.get_mut(&1).unwrap().snapshot();
    group.nodes.insert(
        4,
        Node::create_with_conf(
            4,
            context.root.join("node-4"),
            &context,
            ConfState::default(),
        ),
    );
    group.heartbeat();
    assert_eq!(
        group.nodes[&4].disk.state.application.operations,
        group.nodes[&1].disk.state.application.operations
    );
    assert!(!group.nodes[&4].disk.state.snapshot.is_empty());
    group.witness("learner-caught-up");
    context.event(json!({"kind":"learner_caught_up","node":4,"index":group.nodes[&4].disk.state.application.applied,"snapshot_installed":true}));
    group.membership(ConfChangeType::AddNode, 4, "voter-added");
    let conf = ConfState::parse_from_bytes(&group.nodes[&1].disk.state.conf_state).unwrap();
    assert_eq!(conf.voters.len(), 4);
    assert_eq!(
        BTreeSet::from_iter(conf.voters),
        BTreeSet::from([1, 2, 3, 4])
    );
    group.propose(1, Batch::one("failure-four-voters"));
    group.acknowledge(1, "failure-four-voters");
    group.witness("four-voters");
    group.membership(ConfChangeType::RemoveNode, 4, "voter-removed");
    let conf = ConfState::parse_from_bytes(&group.nodes[&1].disk.state.conf_state).unwrap();
    assert_eq!(conf.voters.len(), 3);
    assert_eq!(BTreeSet::from_iter(conf.voters), BTreeSet::from([1, 2, 3]));
    group.nodes.get_mut(&4).unwrap().snapshot();
    let fourth = group.nodes.remove(&4).unwrap();
    drop(fourth);
    group.discard_inbound_for_shutdown();
    for node in group.nodes.values_mut() {
        node.snapshot();
    }
    group.nodes.clear();
    for id in 1..=3 {
        group.nodes.insert(
            id,
            Node::reopen(id, context.root.join(format!("node-{id}")), &context).unwrap(),
        );
    }
    group.nodes.get_mut(&1).unwrap().raw.campaign().unwrap();
    group.pump();
    group.propose(1, Batch::one("failure-after-reopen"));
    group.acknowledge(1, "failure-after-reopen");
    group.witness("reopened");
    context.event(json!({"kind":"graceful_reopen_verified","snapshot":true,"membership_restored":true,"power_loss_qualified":false}));
    let mut corrupt: Disk<Persisted> = Disk::reopen(context.root.join("node-4"), &context).unwrap();
    let mut saved = corrupt.state.clone();
    assert!(!saved.snapshot.is_empty());
    saved.snapshot.pop();
    corrupt.commit(saved).unwrap();
    drop(corrupt);
    assert!(Node::reopen(4, context.root.join("node-4"), &context).is_err());
    context.state_witness("node-4", "truncated");
    context.event(json!({"kind":"truncated_snapshot_refused","node":4,"fresh_cluster":false,"persisted_corruption":true}));
    group.discard_inbound_for_shutdown();
    group.nodes.clear();
    context.event(json!({"kind":"failure_complete","history":"fixed-failures-membership-v1"}));
    context.finish()
}

pub(super) fn scale_component() {
    let budget = Budget::sample();
    let cases = scale_histories(&budget);
    budget.check().unwrap();
    println!(
        "NATIVE_SCALE_COMPONENT: {}",
        json!({"candidate":"raft-rs","cases":cases,"aggregate_counts":budget.counts(),"scope":"Local1/4/16active-group component; combined sample/full matrix still required"})
    );
}

fn scale_histories(sample: &Budget) -> Vec<serde_json::Value> {
    let mut cases = vec![];
    for count in [1, 4, 16] {
        let budget = Budget::scale(sample);
        let baseline = super::common::process_memory();
        let mut groups = vec![];
        let mut contexts = vec![];
        for ordinal in 0..count {
            let context = Context::for_sample("raft-rs", &budget);
            context.event(json!({"kind":"scale_group_begin","groups":count,"ordinal":ordinal,"voters":3,"physical_group":true}));
            let mut group = Group::new(&context);
            group.nodes.get_mut(&1).unwrap().raw.campaign().unwrap();
            group.pump();
            let identity = format!("scale-{count}-{ordinal}");
            group.propose(1, Batch::one(&identity));
            group.acknowledge(1, &identity);
            groups.push(group);
            contexts.push(context);
        }
        let ready = super::common::process_memory();
        let before = budget.counts();
        let started = std::time::Instant::now();
        // raft-rs has no runtime timer: the application supplies one100ms tick to every voter.
        while started.elapsed() < std::time::Duration::from_millis(100) {
            budget.check().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for group in &mut groups {
            for node in group.nodes.values_mut() {
                node.raw.tick();
            }
            group.pump();
        }
        let idle_us = started.elapsed().as_micros();
        let after = budget.counts();
        let idle_memory = super::common::process_memory();
        assert!(after.attempts > before.attempts);
        let mut raw = vec![];
        for group in &mut groups {
            group.pump();
            group.discard_inbound_for_shutdown();
            group.nodes.clear();
        }
        drop(groups);
        for context in contexts {
            context.event(json!({"kind":"scale_group_stopped","groups":count,"all_drained":true}));
            raw.push(context.finish());
        }
        budget.check().unwrap();
        cases.push(json!({"groups":count,"voters_per_group":3,"baseline_memory":baseline,"ready_memory":ready,"idle_memory":idle_memory,
            "idle_window_us":idle_us,"configured_heartbeat_interval_us":100000,"application_ticks_per_voter":1,"idle_before":before,"idle_after":after,
            "aggregate_counts":budget.counts(),"raw_groups":raw,"scope":"Whole process RSS/HWM and actual local payloads; no exact per-group allocation or fleet extrapolation"}));
    }
    cases
}
