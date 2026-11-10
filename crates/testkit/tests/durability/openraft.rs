//! OpenRaft v0.9.25 storage-v2 is unstable; this is a research adapter only.
use super::common::{Batch, Context, Disk, Operation};
use super::{budget::Budget, workload};
use openraft::error::{InstallSnapshotError, RPCError, RaftError, RemoteError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::storage::{LogFlushed, LogState, RaftLogStorage, RaftStateMachine};
use openraft::{
    Config, Entry, EntryPayload, LogId, OptionalSend, Raft, RaftLogReader, RaftSnapshotBuilder,
    Snapshot, SnapshotMeta, StorageError, StorageIOError, StoredMembership, Vote,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::io::{self, Cursor};
use std::ops::RangeBounds;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, Notify};

openraft::declare_raft_types!(Type: D=Batch, R=bool, Node=());
type Result<T> = std::result::Result<T, StorageError<u64>>;

#[derive(Default)]
struct Gate {
    hold: AtomicBool,
    reached: AtomicBool,
    release: Notify,
}
impl Gate {
    async fn wait(&self) {
        loop {
            let released = self.release.notified();
            if !self.hold.load(Ordering::Acquire) {
                break;
            }
            self.reached.store(true, Ordering::Release);
            released.await;
        }
    }
    fn resume(&self) {
        self.hold.store(false, Ordering::Release);
        self.release.notify_waiters();
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogRecord {
    vote: Option<Vote<u64>>,
    purged: Option<LogId<u64>>,
    committed: Option<LogId<u64>>,
    entries: BTreeMap<u64, Entry<Type>>,
}
#[derive(Clone)]
struct LogStore {
    disk: Arc<AsyncMutex<Disk<LogRecord>>>,
    gate: Arc<Gate>,
    context: Context,
    id: u64,
}

impl RaftLogReader<Type> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<Type>>> {
        Ok(self
            .disk
            .lock()
            .await
            .state
            .entries
            .range(range)
            .map(|(_, entry)| entry.clone())
            .collect())
    }
}
impl RaftLogStorage<Type> for LogStore {
    type LogReader = Self;
    async fn get_log_state(&mut self) -> Result<LogState<Type>> {
        let disk = self.disk.lock().await;
        Ok(LogState {
            last_purged_log_id: disk.state.purged,
            last_log_id: disk
                .state
                .entries
                .last_key_value()
                .map(|(_, entry)| entry.log_id)
                .or(disk.state.purged),
        })
    }
    async fn get_log_reader(&mut self) -> Self {
        self.clone()
    }
    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<()> {
        let mut disk = self.disk.lock().await;
        let mut next = disk.state.clone();
        next.vote = Some(*vote);
        disk.commit(next)
            .map_err(|error| StorageIOError::write_vote(&error))?;
        self.context
            .event(json!({"kind":"vote_persisted","node":self.id,"vote":vote}));
        Ok(())
    }
    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>> {
        Ok(self.disk.lock().await.state.vote)
    }
    async fn save_committed(&mut self, committed: Option<LogId<u64>>) -> Result<()> {
        let mut disk = self.disk.lock().await;
        let mut next = disk.state.clone();
        next.committed = committed;
        disk.commit(next)
            .map_err(|error| StorageIOError::write_logs(&error))?;
        Ok(())
    }
    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>> {
        Ok(self.disk.lock().await.state.committed)
    }
    async fn append<I>(&mut self, entries: I, callback: LogFlushed<Type>) -> Result<()>
    where
        I: IntoIterator<Item = Entry<Type>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let entries = entries.into_iter().collect::<Vec<_>>();
        assert!(entries.len() <= 128);
        // The same async lock serializes vote/log IO while an acknowledged barrier waits.
        let mut disk = self.disk.lock().await;
        if self.gate.hold.load(Ordering::Acquire) {
            self.context
                .event(json!({"kind":"persistence_barrier_reached","node":self.id,
                "indices":entries.iter().map(|entry|entry.log_id.index).collect::<Vec<_>>()}));
            self.gate.wait().await;
        }
        let mut next = disk.state.clone();
        for entry in &entries {
            let previous = next
                .entries
                .last_key_value()
                .map(|(_, entry)| entry.log_id.index)
                .or(next.purged.map(|id| id.index));
            assert_eq!(entry.log_id.index, previous.map_or(0, |index| index + 1));
            next.entries.insert(entry.log_id.index, entry.clone());
        }
        let result = disk.commit(next);
        if let Err(error) = result {
            callback.log_io_completed(Err(io::Error::other("Research log persistence refused")));
            return Err(StorageIOError::write_logs(&error).into());
        }
        self.context.event(
            json!({"kind":"append_readable_and_persisted","node":self.id,
            "indices":entries.iter().map(|entry|entry.log_id.index).collect::<Vec<_>>(),
            "log_ids":entries.iter().map(|entry|entry.log_id).collect::<Vec<_>>(),"purged":disk.state.purged,"transaction":disk.transaction()}),
        );
        callback.log_io_completed(Ok(()));
        self.context.event(
            json!({"kind":"flush_callback_success","node":self.id,"after_actual_sync":true,"transaction":disk.transaction()}),
        );
        Ok(())
    }
    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<()> {
        let mut disk = self.disk.lock().await;
        let mut next = disk.state.clone();
        next.entries.retain(|index, _| *index < log_id.index);
        disk.commit(next)
            .map_err(|error| StorageIOError::write_logs(&error))?;
        Ok(())
    }
    async fn purge(&mut self, log_id: LogId<u64>) -> Result<()> {
        let mut disk = self.disk.lock().await;
        let mut next = disk.state.clone();
        next.entries.retain(|index, _| *index > log_id.index);
        next.purged = Some(log_id);
        disk.commit(next)
            .map_err(|error| StorageIOError::write_logs(&error))?;
        Ok(())
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Applied {
    last: Option<LogId<u64>>,
    membership: StoredMembership<u64, ()>,
    operations: BTreeMap<String, Operation>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedSnapshot {
    meta: SnapshotMeta<u64, ()>,
    data: Vec<u8>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineRecord {
    applied: Applied,
    snapshot: Option<SavedSnapshot>,
}
impl MachineRecord {
    fn reopen(path: std::path::PathBuf, context: &Context) -> io::Result<Disk<Self>> {
        let disk: Disk<Self> = Disk::reopen(path, context)?;
        if let Some(saved) = &disk.state.snapshot {
            let applied: Applied = serde_json::from_slice(&saved.data).map_err(io::Error::other)?;
            if applied.last != saved.meta.last_log_id
                || applied.membership != saved.meta.last_membership
                || applied.last > disk.state.applied.last
            {
                return Err(io::Error::other(
                    "Research persisted snapshot coordinates refused",
                ));
            }
        }
        Ok(disk)
    }
}
#[derive(Clone)]
struct Machine {
    disk: Arc<AsyncMutex<Disk<MachineRecord>>>,
    context: Context,
    id: u64,
}
fn snapshot(record: &SavedSnapshot) -> Snapshot<Type> {
    Snapshot {
        meta: record.meta.clone(),
        snapshot: Box::new(Cursor::new(record.data.clone())),
    }
}

impl RaftSnapshotBuilder<Type> for Machine {
    async fn build_snapshot(&mut self) -> Result<Snapshot<Type>> {
        let mut disk = self.disk.lock().await;
        let mut next = disk.state.clone();
        let applied = &next.applied;
        let data = serde_json::to_vec(applied).unwrap();
        assert!(data.len() <= 1024 * 1024);
        let meta = SnapshotMeta {
            last_log_id: applied.last,
            last_membership: applied.membership.clone(),
            snapshot_id: format!(
                "research-{}-{}",
                self.id,
                applied.last.map_or(0, |id| id.index)
            ),
        };
        let saved = SavedSnapshot { meta, data };
        next.snapshot = Some(saved.clone());
        disk.commit(next)
            .map_err(|error| StorageIOError::write_snapshot(None, &error))?;
        self.context.event(json!({"kind":"snapshot_persisted","node":self.id,"index":saved.meta.last_log_id.map(|id|id.index),"transaction":disk.transaction()}));
        Ok(snapshot(&saved))
    }
}
impl RaftStateMachine<Type> for Machine {
    type SnapshotBuilder = Self;
    async fn applied_state(&mut self) -> Result<(Option<LogId<u64>>, StoredMembership<u64, ()>)> {
        let disk = self.disk.lock().await;
        Ok((
            disk.state.applied.last,
            disk.state.applied.membership.clone(),
        ))
    }
    async fn apply<I>(&mut self, entries: I) -> Result<Vec<bool>>
    where
        I: IntoIterator<Item = Entry<Type>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut disk = self.disk.lock().await;
        let mut next = disk.state.clone();
        let mut output = vec![];
        let mut receipt = vec![];
        for entry in entries {
            assert!(receipt.len() < 128);
            assert_eq!(
                entry.log_id.index,
                next.applied.last.map_or(0, |id| id.index + 1)
            );
            let mut ids = vec![];
            match entry.payload {
                EntryPayload::Normal(batch) => {
                    batch.validate();
                    for operation in batch.operations {
                        ids.push(operation.id.clone());
                        if let Some(previous) = next.applied.operations.get(&operation.id) {
                            assert_eq!(previous, &operation);
                        } else {
                            next.applied
                                .operations
                                .insert(operation.id.clone(), operation);
                        }
                    }
                    assert!(next.applied.operations.len() <= 128);
                }
                EntryPayload::Membership(membership) => {
                    next.applied.membership = StoredMembership::new(Some(entry.log_id), membership);
                }
                EntryPayload::Blank => {}
            }
            next.applied.last = Some(entry.log_id);
            output.push(true);
            receipt.push((entry.log_id, ids));
        }
        assert!(output.len() <= 128);
        self.context
            .event(json!({"kind":"committed_apply_received","node":self.id,
            "log_ids":receipt.iter().map(|(log_id,_)|log_id).collect::<Vec<_>>()}));
        disk.commit(next)
            .map_err(|error| StorageIOError::write_state_machine(&error))?;
        for (log_id, ids) in receipt {
            self.context.event(json!({"kind":"applied","node":self.id,"index":log_id.index,"log_id":log_id,"ids":ids,"application_synced":true,"candidate_committed":true,"transaction":disk.transaction()}));
        }
        Ok(output)
    }
    async fn get_snapshot_builder(&mut self) -> Self {
        self.clone()
    }
    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>> {
        Ok(Box::new(Cursor::new(vec![])))
    }
    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, ()>,
        incoming: Box<Cursor<Vec<u8>>>,
    ) -> Result<()> {
        let data = incoming.into_inner();
        assert!(data.len() <= 1024 * 1024);
        let applied: Applied = serde_json::from_slice(&data)
            .map_err(|error| StorageIOError::read_snapshot(None, &error))?;
        if applied.last != meta.last_log_id || applied.membership != meta.last_membership {
            return Err(StorageIOError::read_snapshot(
                None,
                &io::Error::other("Snapshot coordinates differ"),
            )
            .into());
        }
        let mut disk = self.disk.lock().await;
        let next = MachineRecord {
            applied,
            snapshot: Some(SavedSnapshot {
                meta: meta.clone(),
                data,
            }),
        };
        disk.commit(next)
            .map_err(|error| StorageIOError::write_snapshot(None, &error))?;
        Ok(())
    }
    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<Type>>> {
        Ok(self.disk.lock().await.state.snapshot.as_ref().map(snapshot))
    }
}

#[derive(Clone)]
struct Router {
    nodes: Arc<Mutex<BTreeMap<u64, Raft<Type>>>>,
    blocked: Arc<Mutex<BTreeSet<(u64, u64)>>>,
    observed_drops: Arc<Mutex<BTreeSet<(u64, u64)>>>,
    context: Context,
}
struct Factory {
    from: u64,
    router: Router,
}
struct Client {
    from: u64,
    target: u64,
    router: Router,
}
impl RaftNetworkFactory<Type> for Factory {
    type Network = Client;
    async fn new_client(&mut self, target: u64, _node: &()) -> Client {
        Client {
            from: self.from,
            target,
            router: self.router.clone(),
        }
    }
}
impl Client {
    fn request<T: Serialize + for<'de> Deserialize<'de>>(
        &self,
        kind: &str,
        input: &T,
    ) -> std::result::Result<(Raft<Type>, T), Unreachable> {
        self.router.context.deadline();
        let raw = serde_json::to_vec(input).unwrap();
        assert!(raw.len() <= 1024 * 1024);
        let remote = self.router.nodes.lock().unwrap().get(&self.target).cloned();
        let dropped = self
            .router
            .blocked
            .lock()
            .unwrap()
            .contains(&(self.from, self.target))
            || remote.is_none();
        self.router
            .context
            .wire(self.from, self.target, kind, &raw, dropped);
        if dropped {
            self.router
                .observed_drops
                .lock()
                .unwrap()
                .insert((self.from, self.target));
            return Err(Unreachable::new(&io::Error::other(
                "Injected research partition/unavailable peer",
            )));
        }
        Ok((remote.unwrap(), serde_json::from_slice(&raw).unwrap()))
    }
    fn response<T: Serialize + for<'de> Deserialize<'de>>(
        &self,
        kind: &str,
        response: &T,
    ) -> std::result::Result<T, Unreachable> {
        let raw = serde_json::to_vec(response).unwrap();
        assert!(raw.len() <= 1024 * 1024);
        let dropped = self
            .router
            .blocked
            .lock()
            .unwrap()
            .contains(&(self.target, self.from));
        self.router
            .context
            .wire(self.target, self.from, kind, &raw, dropped);
        if dropped {
            self.router
                .observed_drops
                .lock()
                .unwrap()
                .insert((self.target, self.from));
            return Err(Unreachable::new(&io::Error::other(
                "Injected research response partition",
            )));
        }
        Ok(serde_json::from_slice(&raw).unwrap())
    }
}
impl RaftNetwork<Type> for Client {
    async fn append_entries(
        &mut self,
        input: AppendEntriesRequest<Type>,
        _option: RPCOption,
    ) -> std::result::Result<AppendEntriesResponse<u64>, RPCError<u64, (), RaftError<u64>>> {
        let (remote, input) = self
            .request("append_request", &input)
            .map_err(RPCError::Unreachable)?;
        let response = remote.append_entries(input).await;
        self.response("append_response", &response)
            .map_err(RPCError::Unreachable)?
            .map_err(|error| RPCError::RemoteError(RemoteError::new(self.target, error)))
    }
    async fn vote(
        &mut self,
        input: VoteRequest<u64>,
        _option: RPCOption,
    ) -> std::result::Result<VoteResponse<u64>, RPCError<u64, (), RaftError<u64>>> {
        let (remote, input) = self
            .request("vote_request", &input)
            .map_err(RPCError::Unreachable)?;
        let response = remote.vote(input).await;
        self.response("vote_response", &response)
            .map_err(RPCError::Unreachable)?
            .map_err(|error| RPCError::RemoteError(RemoteError::new(self.target, error)))
    }
    async fn install_snapshot(
        &mut self,
        input: InstallSnapshotRequest<Type>,
        _option: RPCOption,
    ) -> std::result::Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, (), RaftError<u64, InstallSnapshotError>>,
    > {
        let (remote, input) = self
            .request("snapshot_request", &input)
            .map_err(RPCError::Unreachable)?;
        let response = remote.install_snapshot(input).await;
        self.response("snapshot_response", &response)
            .map_err(RPCError::Unreachable)?
            .map_err(|error| RPCError::RemoteError(RemoteError::new(self.target, error)))
    }
}

struct Group {
    router: Router,
    logs: BTreeMap<u64, LogStore>,
    machines: BTreeMap<u64, Machine>,
    config: Arc<Config>,
    context: Context,
}
impl Group {
    async fn new(context: &Context) -> Self {
        Self::with_background(context, false).await
    }
    async fn with_background(context: &Context, background: bool) -> Self {
        let config = Config {
            cluster_name: "hsearch-public-research".into(),
            enable_tick: background,
            enable_heartbeat: background,
            enable_elect: false,
            election_timeout_min: 1000,
            election_timeout_max: 2000,
            heartbeat_interval: 100,
            max_payload_entries: 16,
            snapshot_max_chunk_size: 64 * 1024,
            ..Config::default()
        }
        .validate()
        .unwrap();
        let router = Router {
            nodes: Arc::new(Mutex::new(BTreeMap::new())),
            blocked: Arc::new(Mutex::new(BTreeSet::new())),
            observed_drops: Arc::new(Mutex::new(BTreeSet::new())),
            context: context.clone(),
        };
        let mut group = Self {
            router,
            logs: BTreeMap::new(),
            machines: BTreeMap::new(),
            config: Arc::new(config),
            context: context.clone(),
        };
        for id in 1..=3 {
            group.start(id, false).await;
        }
        group
            .node(1)
            .initialize(BTreeSet::from([1, 2, 3]))
            .await
            .unwrap();
        group.node(1).trigger().elect().await.unwrap();
        group
            .node(1)
            .wait(Some(Duration::from_secs(2)))
            .current_leader(1, "research election")
            .await
            .unwrap();
        group
    }
    fn node(&self, id: u64) -> Raft<Type> {
        self.router.nodes.lock().unwrap()[&id].clone()
    }
    async fn start(&mut self, id: u64, reopen: bool) {
        let log_path = self.context.root.join(format!("node-{id}-log"));
        let machine_path = self.context.root.join(format!("node-{id}-machine"));
        let log = if reopen {
            Disk::reopen(log_path, &self.context).unwrap()
        } else {
            Disk::create(log_path, LogRecord::default(), &self.context).unwrap()
        };
        let machine = if reopen {
            MachineRecord::reopen(machine_path, &self.context).unwrap()
        } else {
            Disk::create(machine_path, MachineRecord::default(), &self.context).unwrap()
        };
        let log = LogStore {
            disk: Arc::new(AsyncMutex::new(log)),
            gate: Arc::new(Gate::default()),
            context: self.context.clone(),
            id,
        };
        let machine = Machine {
            disk: Arc::new(AsyncMutex::new(machine)),
            context: self.context.clone(),
            id,
        };
        let node = Raft::new(
            id,
            self.config.clone(),
            Factory {
                from: id,
                router: self.router.clone(),
            },
            log.clone(),
            machine.clone(),
        )
        .await
        .unwrap();
        self.router.nodes.lock().unwrap().insert(id, node);
        self.logs.insert(id, log);
        self.machines.insert(id, machine);
    }
    async fn write(&self, id: &str) {
        self.context.deadline();
        self.node(1).client_write(Batch::one(id)).await.unwrap();
        self.context
            .event(json!({"kind":"client_response_observed","leader":1,"id":id}));
        self.acknowledge(id).await;
    }
    async fn acknowledge(&self, id: &str) {
        assert!(
            self.machines[&1]
                .disk
                .lock()
                .await
                .state
                .applied
                .operations
                .contains_key(id)
        );
        let mut persisted = vec![];
        for node in self.logs.keys().copied() {
            let log: LogRecord =
                Disk::read(&self.context.root.join(format!("node-{node}-log"))).unwrap();
            let machine: MachineRecord =
                Disk::read(&self.context.root.join(format!("node-{node}-machine"))).unwrap();
            let in_log=log.entries.values().any(|entry|matches!(&entry.payload,EntryPayload::Normal(batch) if batch.operations.iter().any(|operation|operation.id==id)));
            if in_log || machine.applied.operations.contains_key(id) {
                persisted.push(node);
            }
        }
        let applied = self.machines[&1].disk.lock().await;
        let voter_sets = applied
            .state
            .applied
            .membership
            .membership()
            .get_joint_config()
            .iter()
            .map(|set| set.iter().copied().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert!(!voter_sets.is_empty());
        for voters in &voter_sets {
            assert!(
                voters
                    .iter()
                    .filter(|node| persisted.contains(node))
                    .count()
                    > voters.len() / 2
            );
        }
        self.context
            .event(json!({"kind":"client_ack","leader":1,"id":id,"persisted_nodes":persisted,"voter_sets":voter_sets}));
    }
    fn isolate(&self, id: u64, blocked: bool) {
        let ids = self
            .router
            .nodes
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut edges = self.router.blocked.lock().unwrap();
        for peer in ids.into_iter().filter(|peer| *peer != id) {
            for edge in [(id, peer), (peer, id)] {
                if blocked {
                    edges.insert(edge);
                } else {
                    edges.remove(&edge);
                }
            }
        }
        drop(edges);
        self.router.observed_drops.lock().unwrap().clear();
        self.context
            .event(json!({"kind":"partition_changed","node":id,"isolated":blocked}));
    }
    async fn wait_applied(&self, node: u64, id: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                self.context.deadline();
                if self.machines[&node]
                    .disk
                    .lock()
                    .await
                    .state
                    .applied
                    .operations
                    .contains_key(id)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    async fn membership(&self, label: &str) {
        let disk = self.machines[&1].disk.lock().await;
        let state = &disk.state.applied;
        self.witness(label);
        self.context.event(json!({"kind":"membership_transition","label":label,"voter_sets":state.membership.membership().get_joint_config(),
            "learners":state.membership.membership().learner_ids().collect::<Vec<_>>(),"index":state.last.unwrap().index,"persisted":true}));
    }
    fn witness(&self, label: &str) {
        for id in self.logs.keys() {
            for suffix in ["log", "machine"] {
                self.context
                    .state_witness(&format!("node-{id}-{suffix}"), label);
            }
        }
    }
    async fn stop_node(&mut self, id: u64) {
        self.node(id).shutdown().await.unwrap();
        self.router.nodes.lock().unwrap().remove(&id);
        self.logs.remove(&id);
        self.machines.remove(&id);
        self.context.event(json!({"kind":"node_stopped","node":id}));
    }
    async fn shutdown(&mut self) {
        let nodes = self
            .router
            .nodes
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let required = self
            .node(1)
            .metrics()
            .borrow()
            .last_applied
            .map(|id| id.index);
        for node in &nodes {
            node.wait(Some(Duration::from_secs(2)))
                .applied_index_at_least(required, "explicit application drain before shutdown")
                .await
                .unwrap();
        }
        self.context.event(
            json!({"kind":"application_drain_barrier","required_index":required,"nodes":[1,2,3]}),
        );
        for node in nodes {
            node.shutdown().await.unwrap();
        }
        self.router.nodes.lock().unwrap().clear();
        self.logs.clear();
        self.machines.clear();
    }
}

pub(super) fn persistence_probe() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let context = Context::new("openraft");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(15),async {
            let mut group=Group::new(&context).await;group.write("healthy").await;
            for id in [2,3] {group.logs[&id].gate.hold.store(true,Ordering::Release);}
            let leader=group.node(1);
            let mut waiting=tokio::spawn(async move {leader.client_write(Batch::one("held")).await});
            tokio::time::timeout(Duration::from_secs(2),async {
                while !group.logs[&2].gate.reached.load(Ordering::Acquire) || !group.logs[&3].gate.reached.load(Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            assert!(!waiting.is_finished());
            assert!(!group.machines[&1].disk.lock().await.state.applied.operations.contains_key("held"));
            group.logs[&2].gate.resume();
            tokio::time::timeout(Duration::from_secs(2),&mut waiting).await.unwrap().unwrap().unwrap();
            group.logs[&3].gate.resume();group.write("after-hold").await;
            group.node(1).trigger().snapshot().await.unwrap();
            tokio::time::timeout(Duration::from_secs(2),async {
                while group.machines[&1].disk.lock().await.state.snapshot.is_none() {tokio::task::yield_now().await;}
            }).await.unwrap();
            group.shutdown().await;
            for id in 1..=3 {group.start(id,true).await;}
            group.node(1).trigger().elect().await.unwrap();
            group.node(1).wait(Some(Duration::from_secs(2))).current_leader(1,"research reopen").await.unwrap();
            group.write("after-reopen").await;group.shutdown().await;
            context.event(json!({"kind":"graceful_reopen_verified","snapshot":true,"power_loss_qualified":false}));
        }).await.unwrap();
    });
    drop(runtime);
    context.event(json!({"kind":"test_runtime_stopped","application_drain_checked":true}));
    println!("DURABILITY_ADAPTER_PROBE: {}", context.finish());
}

struct Service {
    runtime: Option<tokio::runtime::Runtime>,
    group: Option<Group>,
    pending: BTreeMap<usize, tokio::task::JoinHandle<u64>>,
    context: Context,
}
impl Service {
    fn new(context: &Context) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let group = runtime.block_on(Group::new(context));
        Self {
            runtime: Some(runtime),
            group: Some(group),
            pending: BTreeMap::new(),
            context: context.clone(),
        }
    }
}
impl workload::Driver for Service {
    fn submit(&mut self, ticket: usize, batch: &Batch) {
        batch.validate();
        assert!(self.pending.len() < 4);
        let node = self.group.as_ref().unwrap().node(1);
        let batch = batch.clone();
        let reply = self
            .runtime
            .as_ref()
            .unwrap()
            .spawn(async move { node.client_write(batch).await.unwrap().log_id.index });
        assert!(self.pending.insert(ticket, reply).is_none());
    }
    fn advance(&mut self) {
        std::thread::yield_now();
    }
    fn finished(&mut self, ticket: usize, batch: &Batch) -> bool {
        let reply = self.pending.get(&ticket).unwrap();
        if !reply.is_finished() {
            return false;
        }
        let reply = self.pending.remove(&ticket).unwrap();
        let index = self.runtime.as_ref().unwrap().block_on(reply).unwrap();
        self.context.event(
            json!({"kind":"client_response_observed","ticket":ticket,"index":index,
            "ids":batch.operations.iter().map(|op|&op.id).collect::<Vec<_>>()}),
        );
        let group = self.group.as_ref().unwrap();
        for operation in &batch.operations {
            self.runtime
                .as_ref()
                .unwrap()
                .block_on(group.acknowledge(&operation.id));
        }
        true
    }
    fn heartbeat(&mut self) {
        self.runtime
            .as_ref()
            .unwrap()
            .block_on(self.group.as_ref().unwrap().node(1).trigger().heartbeat())
            .unwrap();
    }
    fn stop(mut self) {
        assert!(self.pending.is_empty());
        self.runtime
            .as_ref()
            .unwrap()
            .block_on(self.group.as_mut().unwrap().shutdown());
        drop(self.group.take());
        drop(self.runtime.take());
        self.context
            .event(json!({"kind":"test_runtime_stopped","application_drain_checked":true}));
    }
}

pub(super) fn matched_workload_experiment() {
    let runtime = super::common::sample_runtime("openraft");
    let budget = Budget::sample();
    let workloads = matched_histories(&budget);
    let failure = failure_history(&budget);
    let scales = scale_histories(&budget);
    budget.check().unwrap();
    println!(
        "NATIVE_DURABILITY_SAMPLE: {}",
        json!({"sample_schema":2,"runtime":runtime,"candidate":"openraft","profile":if cfg!(debug_assertions) {"dev"} else {"release"},
        "workloads":workloads,"failure":failure,"scales":scales,"aggregate_counts":budget.counts(),"elapsed_us":budget.elapsed_us(),
        "scope":"One complete candidate sample; native/profile/repetition/source/coverage/review/CI acceptance remains separate"})
    );
}

fn matched_histories(budget: &Budget) -> Vec<serde_json::Value> {
    let mut cases = vec![];
    for trace in workload::fixtures() {
        for wait in [50000, 250000] {
            budget.check().unwrap();
            let context = Context::for_sample("openraft", budget);
            let service = Service::new(&context);
            let measurement = workload::run_case(service, &context, &trace, wait);
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
        json!({"candidate":"openraft","case":result,"aggregate_counts":budget.counts(),"scope":"Failure/membership/snapshot component; full sample/ARM/release/coverage/review remains"})
    );
}

fn failure_history(budget: &Budget) -> serde_json::Value {
    let context = Context::for_sample("openraft", budget);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(15),async {
            let mut group=Group::new(&context).await;context.event(json!({"kind":"failure_begin","history":"fixed-failures-membership-v1"}));
            group.write("failure-healthy").await;
            group.isolate(3,true);group.write("failure-minority-offline").await;
            assert!(!group.machines[&3].disk.lock().await.state.applied.operations.contains_key("failure-minority-offline"));
            group.witness("minority-offline");
            context.event(json!({"kind":"minority_offline_verified","id":"failure-minority-offline","offline":3,"applied":false}));
            group.isolate(3,false);group.node(1).trigger().heartbeat().await.unwrap();group.wait_applied(3,"failure-minority-offline").await;
            group.witness("minority-recovered");
            context.event(json!({"kind":"minority_recovered","node":3,"id":"failure-minority-offline"}));
            group.isolate(1,true);let leader=group.node(1);
            let mut waiting=tokio::spawn(async move {leader.client_write(Batch::one("failure-leader-unknown")).await});
            tokio::time::timeout(Duration::from_secs(2),async {
                loop {let drops=group.router.observed_drops.lock().unwrap().clone();if drops.contains(&(1,2)) && drops.contains(&(1,3)) {break;}tokio::task::yield_now().await;}
            }).await.unwrap();
            assert!(!waiting.is_finished());assert!(!group.machines[&1].disk.lock().await.state.applied.operations.contains_key("failure-leader-unknown"));
            group.witness("unreplicated");
            context.event(json!({"kind":"unreplicated_barrier","id":"failure-leader-unknown","persisted_nodes":[1],"applied":false,"successful_reply":false}));
            group.isolate(1,false);group.node(1).trigger().heartbeat().await.unwrap();tokio::time::timeout(Duration::from_secs(2),&mut waiting).await.unwrap().unwrap().unwrap();
            group.acknowledge("failure-leader-unknown").await;group.witness("leader-healed");context.event(json!({"kind":"identity_resolved","id":"failure-leader-unknown","resubmitted":false,"outcome":"published"}));
            for id in [2,3] {group.logs[&id].gate.hold.store(true,Ordering::Release);}
            let leader=group.node(1);let mut waiting=tokio::spawn(async move {leader.client_write(Batch::one("failure-held")).await});
            tokio::time::timeout(Duration::from_secs(2),async {
                while !group.logs[&2].gate.reached.load(Ordering::Acquire) || !group.logs[&3].gate.reached.load(Ordering::Acquire) {tokio::task::yield_now().await;}
            }).await.unwrap();
            assert!(!waiting.is_finished());assert!(!group.machines[&1].disk.lock().await.state.applied.operations.contains_key("failure-held"));
            group.witness("held");
            context.event(json!({"kind":"held_majority_barrier","id":"failure-held","followers":[2,3],"successful_reply":false,"applied":false}));
            group.logs[&2].gate.resume();tokio::time::timeout(Duration::from_secs(2),&mut waiting).await.unwrap().unwrap().unwrap();group.logs[&3].gate.resume();group.acknowledge("failure-held").await;
            group.node(1).client_write(Batch::one("failure-suppressed")).await.unwrap();
            group.witness("suppressed");
            context.event(json!({"kind":"client_reply_suppressed","id":"failure-suppressed","observed_adapter_commit":true,"outcome":"unknown","injection":"wrapper_suppression_not_transport_loss"}));
            group.wait_applied(1,"failure-suppressed").await;context.event(json!({"kind":"identity_resolved","id":"failure-suppressed","resubmitted":false,"outcome":"published"}));group.acknowledge("failure-suppressed").await;
            group.logs[&3].disk.lock().await.fail_next=true;group.write("failure-io-majority").await;
            group.node(3).wait(Some(Duration::from_secs(2))).metrics(|metrics|metrics.running_state.is_err(),"explicit failed storage barrier").await.unwrap();
            let mut poisoned=group.logs[&3].disk.lock().await;let next=poisoned.state.clone();assert!(poisoned.commit(next).is_err());drop(poisoned);
            context.event(json!({"kind":"poisoned_node_refused","node":3}));group.witness("poisoned");group.stop_node(3).await;group.start(3,true).await;
            group.node(1).trigger().heartbeat().await.unwrap();group.wait_applied(3,"failure-io-majority").await;group.witness("reloaded");context.event(json!({"kind":"node_reloaded","node":3,"id":"failure-io-majority","fresh_cluster":false}));
            group.start(4,false).await;context.event(json!({"kind":"membership_api_proposed","api":"add_learner","target":4,"label":"learner-added"}));
            group.node(1).add_learner(4,(),true).await.unwrap();group.membership("learner-added").await;group.wait_applied(4,"failure-io-majority").await;
            group.witness("learner-caught-up");
            context.event(json!({"kind":"learner_caught_up","node":4,"index":group.machines[&4].disk.lock().await.state.applied.last.unwrap().index,"snapshot_installed":false}));
            context.event(json!({"kind":"membership_api_proposed","api":"change_membership","voters":[1,2,3,4],"label":"voter-added"}));
            group.node(1).change_membership(BTreeSet::from([1,2,3,4]),false).await.unwrap();group.membership("voter-added").await;
            assert_eq!(group.machines[&1].disk.lock().await.state.applied.membership.membership().voter_ids().collect::<BTreeSet<_>>(),BTreeSet::from([1,2,3,4]));
            group.write("failure-four-voters").await;
            group.witness("four-voters");
            context.event(json!({"kind":"membership_api_proposed","api":"change_membership","voters":[1,2,3],"label":"voter-removed"}));
            group.node(1).change_membership(BTreeSet::from([1,2,3]),false).await.unwrap();group.membership("voter-removed").await;
            assert_eq!(group.machines[&1].disk.lock().await.state.applied.membership.membership().voter_ids().collect::<BTreeSet<_>>(),BTreeSet::from([1,2,3]));
            group.node(4).trigger().snapshot().await.unwrap();tokio::time::timeout(Duration::from_secs(2),async {
                while group.machines[&4].disk.lock().await.state.snapshot.is_none() {tokio::task::yield_now().await;}
            }).await.unwrap();group.stop_node(4).await;
            group.node(1).trigger().snapshot().await.unwrap();tokio::time::timeout(Duration::from_secs(2),async {
                while group.machines[&1].disk.lock().await.state.snapshot.is_none() {tokio::task::yield_now().await;}
            }).await.unwrap();group.shutdown().await;
            for id in 1..=3 {group.start(id,true).await;}group.node(1).trigger().elect().await.unwrap();group.node(1).wait(Some(Duration::from_secs(2))).current_leader(1,"research full membership reopen").await.unwrap();
            group.write("failure-after-reopen").await;group.witness("reopened");context.event(json!({"kind":"graceful_reopen_verified","snapshot":true,"membership_restored":true,"power_loss_qualified":false}));
            let mut corrupt:Disk<MachineRecord>=Disk::reopen(context.root.join("node-4-machine"),&context).unwrap();let mut saved=corrupt.state.clone();assert!(!saved.snapshot.as_ref().unwrap().data.is_empty());saved.snapshot.as_mut().unwrap().data.pop();corrupt.commit(saved).unwrap();drop(corrupt);
            assert!(MachineRecord::reopen(context.root.join("node-4-machine"),&context).is_err());context.state_witness("node-4-machine","truncated");context.event(json!({"kind":"truncated_snapshot_refused","node":4,"fresh_cluster":false,"persisted_corruption":true}));
            group.shutdown().await;context.event(json!({"kind":"failure_complete","history":"fixed-failures-membership-v1"}));
        }).await.unwrap();
    });
    drop(runtime);
    context.event(json!({"kind":"test_runtime_stopped","application_drain_checked":true}));
    context.finish()
}

pub(super) fn scale_component() {
    let budget = Budget::sample();
    let cases = scale_histories(&budget);
    budget.check().unwrap();
    println!(
        "NATIVE_SCALE_COMPONENT: {}",
        json!({"candidate":"openraft","cases":cases,"aggregate_counts":budget.counts(),"scope":"Local1/4/16active-group component; combined sample/full matrix still required"})
    );
}

fn scale_histories(sample: &Budget) -> Vec<serde_json::Value> {
    let mut cases = vec![];
    for count in [1, 4, 16] {
        let budget = Budget::scale(sample);
        let baseline = super::common::process_memory();
        // All groups share exactly two Tokio worker threads; each real Raft ticker/heartbeat is enabled.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (contexts,ready,idle_memory,before,after,idle_us)=runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(15),async {
                let mut groups=vec![];let mut contexts=vec![];
                for ordinal in 0..count {
                    let context=Context::for_sample("openraft",&budget);context.event(json!({"kind":"scale_group_begin","groups":count,"ordinal":ordinal,"voters":3,"physical_group":true}));
                    let group=Group::with_background(&context,true).await;group.write(&format!("scale-{count}-{ordinal}")).await;
                    groups.push(group);contexts.push(context);
                }
                let ready=super::common::process_memory();let before=budget.counts();let started=std::time::Instant::now();
                tokio::time::sleep(Duration::from_millis(100)).await;
                let idle_us=started.elapsed().as_micros();let after=budget.counts();let idle_memory=super::common::process_memory();
                for group in &mut groups {group.shutdown().await;}
                drop(groups);(contexts,ready,idle_memory,before,after,idle_us)
            }).await.unwrap()
        });
        drop(runtime);
        let mut raw = vec![];
        for context in contexts {
            context.event(json!({"kind":"test_runtime_stopped","application_drain_checked":true}));
            context.event(json!({"kind":"scale_group_stopped","groups":count,"all_drained":true}));
            raw.push(context.finish());
        }
        budget.check().unwrap();
        cases.push(json!({"groups":count,"voters_per_group":3,"baseline_memory":baseline,"ready_memory":ready,"idle_memory":idle_memory,
            "idle_window_us":idle_us,"configured_heartbeat_interval_us":100000,"native_tick_emitter_interval_us":150000,"native_tickers_enabled":true,"native_heartbeats_enabled":true,
            "idle_before":before,"idle_after":after,"aggregate_counts":budget.counts(),"raw_groups":raw,
            "scope":"Whole process RSS/HWM;100mswindow may miss150msnative tick phases. No zero idle cost/exact per-group allocation/fleet extrapolation claim"}));
    }
    cases
}
