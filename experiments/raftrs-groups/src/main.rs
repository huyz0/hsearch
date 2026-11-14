//! M02.10: bounded raft-rs experiment, the counterpart of `experiments/openraft-groups`.
//!
//! raft-rs does no I/O: this harness is the application loop (tick, step, ready, persist, send,
//! advance) over `MemStorage` and an in-process message queue. `provisional`: one VM, no network,
//! no disk, a single driver thread. It measures raft-rs's own per-group cost and its behaviour
//! under cut/heal schedules, nothing about durability, and the harness itself is part of the cost.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use raft::eraftpb::{ConfState, Message};
use raft::prelude::*;
use raft::storage::MemStorage;
use raft::StateRole;
use slog::{o, Discard, Logger};

const TICK: Duration = Duration::from_millis(50);

struct Group {
    nodes: Vec<RawNode<MemStorage>>,
    queue: VecDeque<Message>,
    /// Node ids whose messages are dropped in both directions.
    cut: Vec<u64>,
    applied: Vec<Vec<Vec<u8>>>,
    sent: u64,
    /// Requests only (responses excluded), the count comparable to openraft's RPCs.
    requests: u64,
    dropped: u64,
}

impl Group {
    fn new(logger: &Logger) -> Self {
        let nodes = (1..=3u64)
            .map(|id| {
                let storage =
                    MemStorage::new_with_conf_state(ConfState::from((vec![1, 2, 3], vec![])));
                let cfg = Config {
                    id,
                    // 50 ms ticks: heartbeat every 5 ticks (250 ms), election 12-24 ticks
                    // (600-1200 ms), matching the openraft run.
                    election_tick: 12,
                    heartbeat_tick: 5,
                    max_size_per_msg: 1 << 20,
                    max_inflight_msgs: 256,
                    ..Default::default()
                };
                RawNode::new(&cfg, storage, logger).expect("node")
            })
            .collect();
        Group {
            nodes,
            queue: VecDeque::new(),
            cut: Vec::new(),
            applied: vec![Vec::new(); 3],
            sent: 0,
            requests: 0,
            dropped: 0,
        }
    }

    fn tick(&mut self) {
        for n in &mut self.nodes {
            n.tick();
        }
        self.pump();
    }

    /// Drive ready/persist/send/advance and deliver queued messages until quiet.
    fn pump(&mut self) {
        loop {
            let mut progressed = false;
            for i in 0..self.nodes.len() {
                if !self.nodes[i].has_ready() {
                    continue;
                }
                progressed = true;
                let store = self.nodes[i].raft.raft_log.store.clone();
                let mut ready = self.nodes[i].ready();
                for m in ready.take_messages() {
                    self.queue.push_back(m);
                }
                if !ready.snapshot().is_empty() {
                    store
                        .wl()
                        .apply_snapshot(ready.snapshot().clone())
                        .expect("snapshot");
                }
                for e in ready.take_committed_entries() {
                    if !e.data.is_empty() {
                        self.applied[i].push(e.data.to_vec());
                    }
                }
                if !ready.entries().is_empty() {
                    store.wl().append(ready.entries()).expect("append");
                }
                if let Some(hs) = ready.hs() {
                    store.wl().set_hardstate(hs.clone());
                }
                for m in ready.take_persisted_messages() {
                    self.queue.push_back(m);
                }
                let mut light = self.nodes[i].advance(ready);
                for m in light.take_messages() {
                    self.queue.push_back(m);
                }
                for e in light.take_committed_entries() {
                    if !e.data.is_empty() {
                        self.applied[i].push(e.data.to_vec());
                    }
                }
                self.nodes[i].advance_apply();
            }
            while let Some(m) = self.queue.pop_front() {
                self.sent += 1;
                if !format!("{:?}", m.get_msg_type()).ends_with("Response") {
                    self.requests += 1;
                }
                if self.cut.contains(&m.from) || self.cut.contains(&m.to) {
                    self.dropped += 1;
                    continue;
                }
                let to = (m.to - 1) as usize;
                let _ = self.nodes[to].step(m);
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
    }

    fn leader(&self) -> Option<usize> {
        (0..3).find(|&i| {
            self.nodes[i].raft.state == StateRole::Leader && !self.cut.contains(&(i as u64 + 1))
        })
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

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

/// Tick every group until each has a leader or `within` passes.
fn elect(groups: &mut [Group], within: Duration) -> Duration {
    let t = Instant::now();
    while t.elapsed() < within && groups.iter().any(|g| g.leader().is_none()) {
        for g in groups.iter_mut() {
            g.tick();
        }
        std::thread::sleep(TICK);
    }
    t.elapsed()
}

fn propose_and_commit(g: &mut Group, li: usize, tag: &[u8]) -> Option<Duration> {
    let t = Instant::now();
    g.nodes[li].propose(vec![], tag.to_vec()).ok()?;
    g.pump();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if g.applied
            .iter()
            .filter(|a| a.iter().any(|d| d == tag))
            .count()
            >= 2
        {
            return Some(t.elapsed());
        }
        g.tick();
        std::thread::sleep(Duration::from_millis(1));
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let logger = Logger::root(Discard, o!());
    match args.first().map(String::as_str) {
        Some("scale") => scale(
            args.get(1).and_then(|v| v.parse().ok()).unwrap_or(1),
            &logger,
        ),
        Some("schedules") => schedules(&logger),
        _ => println!("usage: raftrs-groups scale <groups> | schedules"),
    }
}

fn scale(groups: usize, logger: &Logger) {
    let empty = rss_kib();
    let mut gs: Vec<Group> = (0..groups).map(|_| Group::new(logger)).collect();
    // A contested election: nobody is initialised as leader, so every group runs a real election.
    let elected = elect(&mut gs, Duration::from_secs(30));
    let all = gs.iter().all(|g| g.leader().is_some());
    let rss = rss_kib();
    let (c0, w0) = (cpu_ticks(), Instant::now());
    let sent0: u64 = gs.iter().map(|g| g.requests).sum();
    while w0.elapsed() < Duration::from_secs(5) {
        for g in gs.iter_mut() {
            g.tick();
        }
        std::thread::sleep(TICK);
    }
    let el = w0.elapsed().as_secs_f64();
    let cpu = (cpu_ticks() - c0) as f64 / 100.0 / el * 100.0;
    let rps = (gs.iter().map(|g| g.requests).sum::<u64>() - sent0) as f64 / el;
    let mut lat = Vec::new();
    if let Some(li) = gs[0].leader() {
        for i in 0..200u32 {
            if let Some(d) = propose_and_commit(&mut gs[0], li, format!("w{i}").as_bytes()) {
                lat.push(d.as_secs_f64() * 1000.0);
            }
        }
    }
    println!(
        "| {groups} | {} | {:.0} | {:.1} | {:.0} | {:.1}{} | {:.3} | {:.3} |",
        rss / 1024,
        rss.saturating_sub(empty) as f64 / groups as f64,
        cpu,
        rps,
        elected.as_secs_f64(),
        if all { "" } else { " (not all elected)" },
        pct(&mut lat.clone(), 0.5),
        pct(&mut lat, 0.99)
    );
}

fn schedules(logger: &Logger) {
    println!("## raft-rs schedules (one group of 3, in-process, 50 ms ticks)\n");
    let mut gs = vec![Group::new(logger)];
    let elected = elect(&mut gs, Duration::from_secs(30));
    let g = &mut gs[0];
    let li = g.leader().expect("leader");
    println!(
        "contested election: leader after {:.0} ms",
        elected.as_secs_f64() * 1000.0
    );
    let mut lat = Vec::new();
    for i in 0..200u32 {
        lat.push(
            propose_and_commit(g, li, format!("s{i}").as_bytes())
                .expect("commit")
                .as_secs_f64()
                * 1000.0,
        );
    }
    println!(
        "steady: commit p50 {:.3} ms, p99 {:.3} ms (200 proposals, no network delay)\n",
        pct(&mut lat.clone(), 0.5),
        pct(&mut lat, 0.99)
    );

    // Cut the leader; propose on it (cannot commit); time the new leader; heal; check the proposal.
    let old = li as u64 + 1;
    g.cut.push(old);
    let t = Instant::now();
    g.nodes[li]
        .propose(vec![], b"amb".to_vec())
        .expect("the cut leader accepts the proposal into its log");
    g.pump();
    let mut new_leader = None;
    while t.elapsed() < Duration::from_secs(30) {
        g.tick();
        if let Some(n) = g.leader() {
            new_leader = Some((n, t.elapsed()));
            break;
        }
        std::thread::sleep(TICK);
    }
    match new_leader {
        Some((n, d)) => {
            let ok = propose_and_commit(g, n, b"after").is_some();
            println!(
                "cut leader: new leader after {:.0} ms; first write on it {}",
                d.as_secs_f64() * 1000.0,
                if ok {
                    "committed"
                } else {
                    "did not commit in 2 s"
                }
            );
        }
        None => println!("cut leader: no new leader within 30 s"),
    }
    g.cut.clear();
    let heal = Instant::now();
    while heal.elapsed() < Duration::from_secs(3) {
        g.tick();
        std::thread::sleep(TICK);
    }
    let applied: Vec<bool> = g
        .applied
        .iter()
        .map(|a| a.iter().any(|d| d == b"amb"))
        .collect();
    println!("after heal: the write proposed on the cut leader was applied on members {applied:?}; commit index per member {:?}", g.nodes.iter().map(|n| n.raft.raft_log.committed).collect::<Vec<_>>());
    println!(
        "messages sent {} (requests {}) dropped {} (cumulative)",
        g.sent, g.requests, g.dropped
    );
}
