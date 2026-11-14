//! M02.10: bounded in-process openraft experiment.
//!
//! Per-group resource cost and failure/ambiguity schedules for a 3-member group, over in-memory
//! storage and an in-process network with injectable delay, loss and partitions. Everything here
//! is `provisional`: one VM, no real network, no object store. It measures the library's
//! per-group overhead and its behaviour under the stated schedules, nothing about durability.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyerror::AnyError;
use openraft::async_runtime::watch::WatchReceiver;
use openraft::error::{RPCError, Unreachable};
use openraft::network::v2::RaftNetworkV2;
use openraft::network::RPCOption;
use openraft::raft::{AppendEntriesRequest, AppendResult, VoteRequest, VoteResponse};
use openraft::{Config, Raft};
use openraft_memstore::{new_mem_store, ClientRequest, IntoMemClientRequest, TypeConfig};

type R = Raft<TypeConfig, std::sync::Arc<openraft_memstore::MemStateMachine>>;
type Key = (u32, u64);

/// Shared in-process "network": routes by (group, node), with delay, seeded loss, partitions.
#[derive(Default)]
struct Net {
    nodes: Mutex<HashMap<Key, R>>,
    sms: Mutex<HashMap<Key, Arc<openraft_memstore::MemStateMachine>>>,
    cut: Mutex<HashSet<Key>>,
    delay_ms: AtomicU64,
    loss_permille: AtomicU64,
    rng: AtomicU64,
    rpcs: AtomicU64,
    dropped: AtomicU64,
}

impl Net {
    fn lost(&self) -> bool {
        let p = self.loss_permille.load(Ordering::Relaxed);
        if p == 0 {
            return false;
        }
        // xorshift on a shared seed: deterministic given a single-threaded schedule, adequate here.
        let mut x = self.rng.load(Ordering::Relaxed) | 1;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng.store(x, Ordering::Relaxed);
        x % 1000 < p
    }
    fn blocked(&self, from: Key, to: Key) -> bool {
        let cut = self.cut.lock().unwrap();
        cut.contains(&from) || cut.contains(&to)
    }
    async fn pass(&self, from: Key, to: Key) -> Result<R, RPCError<TypeConfig>> {
        self.rpcs.fetch_add(1, Ordering::Relaxed);
        if self.blocked(from, to) || self.lost() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(unreachable("dropped"));
        }
        let d = self.delay_ms.load(Ordering::Relaxed);
        if d > 0 {
            tokio::time::sleep(Duration::from_millis(d)).await;
        }
        self.nodes
            .lock()
            .unwrap()
            .get(&to)
            .cloned()
            .ok_or_else(|| unreachable("no such node"))
    }
}

fn unreachable(why: &str) -> RPCError<TypeConfig> {
    RPCError::Unreachable(Unreachable::new(&AnyError::error(why)))
}

struct Client {
    net: Arc<Net>,
    from: Key,
    to: Key,
}

impl RaftNetworkV2<TypeConfig> for Client {
    type SnapshotData = std::io::Cursor<Vec<u8>>;

    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _o: RPCOption,
    ) -> Result<AppendResult<TypeConfig>, RPCError<TypeConfig>> {
        let r = self.net.pass(self.from, self.to).await?;
        r.append_entries(rpc)
            .await
            .map_err(|e| unreachable(&e.to_string()))
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        _o: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        let r = self.net.pass(self.from, self.to).await?;
        r.vote(rpc).await.map_err(|e| unreachable(&e.to_string()))
    }

    async fn full_snapshot(
        &mut self,
        _vote: openraft::type_config::alias::VoteOf<TypeConfig>,
        _snapshot: openraft::type_config::alias::SnapshotOf<TypeConfig, Self::SnapshotData>,
        _cancel: impl std::future::Future<Output = openraft::error::ReplicationClosed> + Send + 'static,
        _o: RPCOption,
    ) -> Result<
        openraft::raft::SnapshotResponse<TypeConfig>,
        openraft::error::StreamingError<TypeConfig>,
    > {
        // Snapshots are disabled by config in this experiment.
        Err(openraft::error::StreamingError::Unreachable(
            Unreachable::new(&AnyError::error("snapshots off")),
        ))
    }
}

struct Factory {
    net: Arc<Net>,
    from: Key,
    group: u32,
}

impl openraft::network::RaftNetworkFactory<TypeConfig> for Factory {
    type Network = Client;
    async fn new_client(&mut self, target: u64, _node: &()) -> Client {
        Client {
            net: self.net.clone(),
            from: self.from,
            to: (self.group, target),
        }
    }
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .map(|l| l.to_owned())
        })
        .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

fn cpu_ticks() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let rest = s.rsplit(')').next().unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
        + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
}

fn config() -> Arc<Config> {
    Arc::new(
        Config {
            heartbeat_interval: 250,
            election_timeout_min: 600,
            election_timeout_max: 1200,
            snapshot_policy: openraft::SnapshotPolicy::Never,
            ..Default::default()
        }
        .validate()
        .expect("config"),
    )
}

async fn spawn_group(net: &Arc<Net>, group: u32) -> Vec<R> {
    let mut rafts = Vec::new();
    for id in 1..=3u64 {
        let (log, sm) = new_mem_store();
        let f = Factory {
            net: net.clone(),
            from: (group, id),
            group,
        };
        net.sms.lock().unwrap().insert((group, id), sm.clone());
        let r = Raft::new(id, config(), f, log, sm).await.expect("raft");
        net.nodes.lock().unwrap().insert((group, id), r.clone());
        rafts.push(r);
    }
    let members: std::collections::BTreeSet<u64> = [1, 2, 3].into();
    rafts[0].initialize(members).await.expect("init");
    rafts
}

/// Write, retrying refusals for up to `within`; returns the elapsed time of the successful attempt chain.
async fn write_retry(r: &R, serial: u64, within: Duration) -> Result<Duration, String> {
    let t = Instant::now();
    loop {
        match r
            .client_write(ClientRequest::make_request("c", serial))
            .await
        {
            Ok(_) => return Ok(t.elapsed()),
            Err(e) if t.elapsed() < within => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

async fn leader_of(rafts: &[R], within: Duration) -> Option<(usize, Duration)> {
    let t = Instant::now();
    while t.elapsed() < within {
        for (i, r) in rafts.iter().enumerate() {
            let m = r.metrics().borrow_watched().clone();
            if m.current_leader == Some(m.id) {
                return Some((i, t.elapsed()));
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let max_rss_kib: u64 = std::env::var("MAX_RSS_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1536)
        * 1024;
    println!(
        "cpus: {}, tokio workers: 2, RSS ceiling {} MiB\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0),
        max_rss_kib / 1024
    );

    println!("## Group scaling (3 members per group, in-memory log, no network delay)\n");
    println!("| groups | RSS MiB | KiB/group | idle CPU % of 1 core | RPC/s idle | leaders in | write p50 ms | write p99 ms |\n|---|---|---|---|---|---|---|---|");
    for groups in [1u32, 10, 100, 300, 600] {
        let net = Arc::new(Net::default());
        let base = rss_kib();
        let t = Instant::now();
        let mut all = Vec::new();
        for g in 0..groups {
            all.push(spawn_group(&net, g).await);
        }
        let mut worst = Duration::ZERO;
        let mut leaders = Vec::new();
        for g in &all {
            match leader_of(g, Duration::from_secs(20)).await {
                Some((i, d)) => {
                    worst = worst.max(d);
                    leaders.push(i);
                }
                None => leaders.push(usize::MAX),
            }
        }
        let converge = t.elapsed();
        let rss = rss_kib();
        let (c0, r0, w0) = (
            cpu_ticks(),
            net.rpcs.load(Ordering::Relaxed),
            Instant::now(),
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        let el = w0.elapsed().as_secs_f64();
        let cpu = (cpu_ticks() - c0) as f64 / 100.0 / el * 100.0;
        let rps = (net.rpcs.load(Ordering::Relaxed) - r0) as f64 / el;
        let mut lat = Vec::new();
        if let Some(&li) = leaders.first() {
            if li != usize::MAX {
                for i in 0..200u64 {
                    let s = Instant::now();
                    let _ = all[0][li]
                        .client_write(ClientRequest::make_request("c", i))
                        .await;
                    lat.push(s.elapsed().as_secs_f64() * 1000.0);
                }
            }
        }
        println!(
            "| {groups} | {} | {:.0} | {:.1} | {:.0} | {:.1}s (all groups) | {:.2} | {:.2} |",
            rss / 1024,
            (rss.saturating_sub(base)) as f64 / groups as f64,
            cpu,
            rps,
            converge.as_secs_f64(),
            pct(&mut lat.clone(), 0.5),
            pct(&mut lat, 0.99)
        );
        for g in &all {
            for r in g {
                let _ = r.shutdown().await;
            }
        }
        if rss > max_rss_kib {
            println!("\nRSS ceiling reached at {groups} groups; stopping the sweep.");
            break;
        }
    }
    schedules().await;
}

async fn schedules() {
    println!("\n## Failure and ambiguity schedules (one group of 3)\n");
    for delay in [0u64, 5] {
        let net = Arc::new(Net::default());
        net.delay_ms.store(delay, Ordering::Relaxed);
        let g = spawn_group(&net, 0).await;
        let (li, up) = leader_of(&g, Duration::from_secs(20))
            .await
            .expect("leader");
        println!(
            "### one-way RPC delay {delay} ms: leader elected in {:.0} ms\n",
            up.as_secs_f64() * 1000.0
        );

        // Steady commit latency.
        write_retry(&g[li], 999_999, Duration::from_secs(10))
            .await
            .expect("warm-up write");
        let mut lat = Vec::new();
        for i in 0..200u64 {
            let s = Instant::now();
            g[li]
                .client_write(ClientRequest::make_request("c", i))
                .await
                .expect("write");
            lat.push(s.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "steady: commit p50 {:.2} ms, p99 {:.2} ms (200 writes)\n",
            pct(&mut lat.clone(), 0.5),
            pct(&mut lat, 0.99)
        );

        // Schedule 1: cut the leader. A write is in flight against it; the client times out.
        let old = (0u32, g[li].metrics().borrow_watched().id);
        let t = Instant::now();
        net.cut.lock().unwrap().insert(old);
        let cut_leader = g[li].clone();
        let amb = tokio::spawn(async move {
            tokio::time::timeout(
                Duration::from_secs(2),
                cut_leader.client_write(ClientRequest::make_request("amb", 10_000)),
            )
            .await
        });
        let others: Vec<R> = g
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != li)
            .map(|(_, r)| r.clone())
            .collect();
        let nl = leader_of(&others, Duration::from_secs(30)).await;
        let failover = nl.map(|(_, _)| t.elapsed());
        let amb_res = amb.await.expect("join");
        let mut resumed = None;
        if let Some((ni, _)) = nl {
            let s = Instant::now();
            others[ni]
                .client_write(ClientRequest::make_request("c", 20_000))
                .await
                .expect("write on new leader");
            resumed = Some(s.elapsed());
        }
        println!(
            "partition leader: in-flight write on the cut leader -> {}; new leader after {} ; first write on new leader took {}",
            match &amb_res {
                Ok(Ok(_)) => "committed (unexpected)".to_owned(),
                Ok(Err(e)) => format!("error: {}", e.to_string().chars().take(60).collect::<String>()),
                Err(_) => "client timeout (outcome unknown to the client)".to_owned(),
            },
            failover.map_or("never".into(), |d| format!("{:.0} ms", d.as_secs_f64() * 1000.0)),
            resumed.map_or("n/a".into(), |d| format!("{:.1} ms", d.as_secs_f64() * 1000.0)),
        );
        // Heal and find out whether the ambiguous write survived.
        net.cut.lock().unwrap().clear();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let mut survived = Vec::new();
        for id in 1..=3u64 {
            let sm = net.sms.lock().unwrap().get(&(0, id)).cloned().unwrap();
            let st = sm.get_state_machine().await;
            survived.push(
                st.client_status
                    .get("amb")
                    .map(|v| v == "request-10000")
                    .unwrap_or(false),
            );
        }
        let applied: Vec<_> = g
            .iter()
            .map(|r| r.metrics().borrow_watched().last_applied.map(|l| l.index))
            .collect();
        println!("after heal: last_applied per member {:?}; the in-flight write (client amb) applied on members {:?} (true = it was applied)\n", applied, survived);

        // Schedule 2: random loss.
        for permille in [100u64, 300] {
            net.loss_permille.store(permille, Ordering::Relaxed);
            net.rng.store(0x9E3779B97F4A7C15, Ordering::Relaxed);
            let lead = leader_of(&g, Duration::from_secs(30)).await;
            let (mut ok, mut err, mut lat2) = (0, 0, Vec::new());
            if let Some((li, _)) = lead {
                for i in 0..100u64 {
                    let s = Instant::now();
                    match tokio::time::timeout(
                        Duration::from_secs(3),
                        g[li].client_write(ClientRequest::make_request("c", 30_000 + i)),
                    )
                    .await
                    {
                        Ok(Ok(_)) => ok += 1,
                        _ => err += 1,
                    }
                    lat2.push(s.elapsed().as_secs_f64() * 1000.0);
                }
            }
            println!(
                "loss {}%: {ok} ok, {err} error or timeout of 100; p50 {:.1} ms p99 {:.1} ms; RPCs {} dropped {}",
                permille / 10,
                pct(&mut lat2.clone(), 0.5),
                pct(&mut lat2, 0.99),
                net.rpcs.load(Ordering::Relaxed),
                net.dropped.load(Ordering::Relaxed)
            );
        }
        net.loss_permille.store(0, Ordering::Relaxed);
        println!();
        for r in &g {
            let _ = r.shutdown().await;
        }
    }
}
