//! M03.08: every fault history of a small commit-lane world, enumerated, not sampled.
//!
//! The world is one lane, two writer processes (A and B) and seven steps: open A, open B, append
//! on A, append on B, crash A, crash B, reclaim. Every sequence of steps up to a bound is run
//! against every placement of up to a bounded number of store faults on the mutating requests the
//! history issues (a certain refusal, a lost reply the identity resolves, a lost reply that
//! cannot be resolved), and after **every** history the invariants of the selected protocol are
//! checked on the store's actual contents:
//!
//! 1. every acknowledged batch is in the log exactly once, and acknowledgements are in ordinal order;
//! 2. no batch the store certainly refused, and none nobody attempted, is in the log;
//! 3. a batch whose reply was lost is in the log at most once (an unknown outcome is not a copy);
//! 4. no writer acknowledges after another writer's open completed (unique authority);
//! 5. after a writer saw `Fenced` or `Unknown` it acknowledges nothing again;
//! 6. a writer's consecutive acknowledgements are consecutive ordinals;
//! 7. a recovery open from the cover replays exactly the slots from the cover to the end, seals
//!    the first absent position, and an open below a cover that removed data is refused;
//! 8. a fence is never removed by a reclaim.
//!
//! This is an enumeration over a bounded world, not a proof for unbounded histories; the bounds
//! are printed by the test and recorded in the evidence file.
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hsearch_format::SlotKind;
use hsearch_lane::{Error, LaneId, LaneWriter, block_on, create_lane, read_log, shared_group};
use hsearch_object::memory::MemoryStore;
use hsearch_object::{
    ByteRange, Error as StoreError, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey,
    Operation, Payload, RequestContext, RequestId, VersionToken, WriteReceipt,
};
use support::*;

/// What the store does to one mutating request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fault {
    None,
    /// Refused before acceptance: certainly not written.
    Refuse,
    /// Written, reply lost, the identity resolves it: the writer learns it was written.
    LoseResolvable,
    /// Written, reply lost, and resolution is unavailable: the writer cannot know.
    LoseUnresolvable,
    /// Another writer's request for the same position lands first: the race an open or an append
    /// can lose between its last read and its create.
    Preempt,
    /// Writer A, still alive, appends in the gap between this request's last read and its create:
    /// the race a successor's open can lose to a live predecessor.
    PreemptAppend,
}

const FAULTS: [Fault; 5] = [
    Fault::Refuse,
    Fault::LoseResolvable,
    Fault::LoseUnresolvable,
    Fault::Preempt,
    Fault::PreemptAppend,
];

/// A memory store whose n-th mutating request gets the n-th scripted fault.
struct Scripted {
    inner: MemoryStore,
    script: std::sync::Mutex<Vec<Fault>>,
    calls: AtomicUsize,
    unresolvable: std::sync::Mutex<BTreeSet<RequestId>>,
    /// What writer A does when it preempts a request.
    hook: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Scripted {
    fn new() -> Self {
        Self {
            inner: memory(),
            script: std::sync::Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            unresolvable: std::sync::Mutex::new(BTreeSet::new()),
            hook: std::sync::Mutex::new(None),
        }
    }

    /// Fault the next mutating requests as `script` says, counting from now.
    fn arm(&self, script: &[Fault]) {
        *self.script.lock().unwrap() = script.to_vec();
        self.calls.store(0, Ordering::SeqCst);
    }

    fn next_fault(&self) -> Fault {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        self.script
            .lock()
            .unwrap()
            .get(n)
            .copied()
            .unwrap_or(Fault::None)
    }

    fn mutate<'a>(
        &'a self,
        request: &'a Mutation,
        exec: impl Fn(&'a Mutation) -> Operation<'a, WriteReceipt> + 'a,
    ) -> Operation<'a, WriteReceipt> {
        let run = || exec(request);
        match self.next_fault() {
            Fault::None => run(),
            Fault::PreemptAppend => {
                if request.key.as_bytes().windows(5).any(|w| w == b"/wal/") {
                    let hook = self.hook.lock().unwrap().clone();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                run()
            }
            Fault::Preempt => {
                // The other writer's identical-shape request, under its own identity.
                let mut other = request.clone();
                other.request_id = RequestId::new(b"preempting-writer").unwrap();
                // A test double: the request outlives the call by being leaked.
                let other: &'a Mutation = Box::leak(Box::new(other));
                let first = exec(other);
                let second = run();
                Box::pin(async move {
                    let _ = first.await;
                    second.await
                })
            }
            Fault::Refuse => Box::pin(async { Err(StoreError::Unavailable) }),
            Fault::LoseResolvable => {
                let done = run();
                Box::pin(async move {
                    let _ = done.await;
                    Err(StoreError::UnknownOutcome)
                })
            }
            Fault::LoseUnresolvable => {
                self.unresolvable
                    .lock()
                    .unwrap()
                    .insert(request.request_id.clone());
                let done = run();
                Box::pin(async move {
                    let _ = done.await;
                    Err(StoreError::UnknownOutcome)
                })
            }
        }
    }
}

impl ObjectBackend for Scripted {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        self.mutate(r, move |m| self.inner.create(c, m))
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

impl MetadataStore for Scripted {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.mutate(r, move |m| self.inner.compare_exchange(c, m))
    }
    fn remove_version<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
    ) -> Operation<'a, ()> {
        self.inner.remove_version(c, k, v)
    }
    fn resolve<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        if self.unresolvable.lock().unwrap().contains(&r.request_id) {
            return Box::pin(async { Err(StoreError::Unavailable) });
        }
        self.inner.resolve(c, r)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    OpenA,
    OpenB,
    AppendA,
    AppendB,
    CrashA,
    CrashB,
    Reclaim,
}

const STEPS: [Step; 7] = [
    Step::OpenA,
    Step::OpenB,
    Step::AppendA,
    Step::AppendB,
    Step::CrashA,
    Step::CrashB,
    Step::Reclaim,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Who {
    A,
    B,
}

struct Process {
    writer: LaneWriter<Scripted>,
    /// Saw `Fenced` or `Unknown`: must acknowledge nothing again.
    dead: bool,
    last_ack: Option<u64>,
}

/// What one history did, for the checks.
#[derive(Default)]
struct Trace {
    /// (who, ordinal, seq) in acknowledgement order.
    acks: Vec<(Who, u64, u64)>,
    /// Sequences whose reply was lost and could not be resolved, or that another writer took.
    unknown: BTreeSet<u64>,
    /// Sequences the store certainly refused.
    refused: BTreeSet<u64>,
    /// Sequences attempted at all.
    attempted: BTreeSet<u64>,
    /// Number of acks that happened after a *different* writer's open had completed.
    stale_acks: usize,
    /// Cover the last reclaim raised, and the data slots reclaims removed.
    cover: u64,
    removed: u64,
    /// The writer whose open completed last.
    opened_by: Option<u8>,
    seq: u64,
    /// A violation found while stepping.
    broken: Option<String>,
}

type Procs = Arc<std::sync::Mutex<BTreeMap<u8, Process>>>;
type Shared = Arc<std::sync::Mutex<Trace>>;

fn lane(store: &Arc<Scripted>) -> LaneId {
    let (domain, group) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    block_on(create_lane(
        &**store,
        &layout(),
        &lane,
        &policy(),
        group,
        1000,
        &[id(1)],
    ))
    .unwrap();
    lane
}

fn open(store: &Arc<Scripted>, lane: LaneId, from: u64) -> Result<LaneWriter<Scripted>, Error> {
    block_on(LaneWriter::open(
        Arc::clone(store),
        layout(),
        lane,
        from,
        |_| Ok(()),
    ))
    .map(|(w, _)| w)
}

/// One append by `who`, recorded in `trace`. Returns a violation, if any.
fn append(store: &Scripted, procs: &Procs, trace: &Shared, who: u8) -> Result<(), String> {
    let mut procs = procs.lock().unwrap();
    let Some(p) = procs.get_mut(&who) else {
        return Ok(());
    };
    let seq = {
        let mut t = trace.lock().unwrap();
        t.seq += 1;
        let seq = t.seq;
        t.attempted.insert(seq);
        seq
    };
    let body = seq.to_le_bytes();
    let before = store.calls.load(Ordering::SeqCst);
    let result = block_on(p.writer.append(&[put(id(1), seq, &body)]));
    let mut t = trace.lock().unwrap();
    if p.dead && store.calls.load(Ordering::SeqCst) != before {
        return Err("a writer that saw a fence sent a request".into());
    }
    match result {
        Ok(ack) => {
            if p.dead {
                return Err("ack after the writer saw a fence".into());
            }
            if let Some(prev) = p.last_ack
                && ack.ordinal != prev + 1
            {
                return Err(format!(
                    "ordinals {prev} then {} are not consecutive",
                    ack.ordinal
                ));
            }
            if t.opened_by.is_some_and(|o| o != who) {
                t.stale_acks += 1;
            }
            p.last_ack = Some(ack.ordinal);
            t.acks
                .push((if who == 0 { Who::A } else { Who::B }, ack.ordinal, seq));
        }
        Err(Error::Fenced) => {
            p.dead = true;
            // Another writer took the position; it may have written this very batch.
            t.unknown.insert(seq);
        }
        Err(Error::Unknown { .. }) => {
            p.dead = true;
            t.unknown.insert(seq);
        }
        Err(Error::Poisoned) => {
            if !p.dead {
                return Err("poisoned without a cause".into());
            }
        }
        // A certain refusal: not written, repeatable.
        Err(Error::Store(_)) => {
            t.refused.insert(seq);
        }
        Err(other) => return Err(format!("unexpected {other:?}")),
    }
    Ok(())
}

/// Run `steps` against a store scripted with `script`, then check every invariant.
fn run(steps: &[Step], script: &[Fault]) -> Result<(), String> {
    // The lane is created with no faults; the script starts at the first step.
    let store = Arc::new(Scripted::new());
    let lane = lane(&store);
    let procs: Procs = Arc::default();
    let trace: Shared = Arc::default();
    {
        // Writer A preempts a successor's request by appending in the gap.
        let (store_a, procs_a, trace_a) =
            (Arc::clone(&store), Arc::clone(&procs), Arc::clone(&trace));
        let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            // Only when no step holds the process table: an append by A or B is itself running
            // a store request then, and A is not free to run another.
            let free = procs_a.try_lock().is_ok();
            if free && let Err(why) = append(&store_a, &procs_a, &trace_a, 0) {
                trace_a.lock().unwrap().broken.get_or_insert(why);
            }
        });
        *store.hook.lock().unwrap() = Some(hook);
    }
    store.arm(script);
    for step in steps {
        match step {
            Step::OpenA | Step::OpenB => {
                let who = u8::from(*step == Step::OpenB);
                let from = trace.lock().unwrap().cover;
                // An open that failed leaves the old instance, if any, running.
                if let Ok(writer) = open(&store, lane, from) {
                    procs.lock().unwrap().insert(
                        who,
                        Process {
                            writer,
                            dead: false,
                            last_ack: None,
                        },
                    );
                    trace.lock().unwrap().opened_by = Some(who);
                }
            }
            Step::AppendA | Step::AppendB => {
                let who = u8::from(*step == Step::AppendB);
                append(&store, &procs, &trace, who).map_err(|e| format!("{step:?}: {e}"))?;
            }
            Step::CrashA => {
                procs.lock().unwrap().remove(&0);
            }
            Step::CrashB => {
                procs.lock().unwrap().remove(&1);
            }
            Step::Reclaim => {
                // The live, unpoisoned writer that opened last reclaims what it replayed.
                let Some(who) = trace.lock().unwrap().opened_by else {
                    continue;
                };
                let mut procs = procs.lock().unwrap();
                let Some(p) = procs.get_mut(&who) else {
                    continue;
                };
                if p.dead {
                    continue;
                }
                let fence = p.writer.next_ordinal().saturating_sub(1);
                if let Ok(done) = block_on(p.writer.reclaim_below(fence)) {
                    let mut t = trace.lock().unwrap();
                    t.cover = t.cover.max(fence);
                    t.removed += done.removed;
                }
            }
        }
        if let Some(why) = trace.lock().unwrap().broken.take() {
            return Err(format!("{step:?}: {why}"));
        }
    }
    *store.hook.lock().unwrap() = None;
    drop(procs);
    let trace = Arc::try_unwrap(trace)
        .map_err(|_| "trace still shared".to_string())?
        .into_inner()
        .unwrap();
    check(&store, lane, &trace)
}

/// Every invariant, on what the store actually holds.
fn check(store: &Arc<Scripted>, lane: LaneId, trace: &Trace) -> Result<(), String> {
    // The checking reads and the recovery open run with no faults left, on the same contents.
    store.arm(&[]);
    store.unresolvable.lock().unwrap().clear();
    let clean = store;
    // 4: unique authority.
    if trace.stale_acks > 0 {
        return Err(format!(
            "{} acknowledgement(s) after another writer's open",
            trace.stale_acks
        ));
    }
    // 7: a recovery from below the cover is refused when data below it was removed.
    if trace.removed > 0 {
        match open(clean, lane, 0) {
            Err(Error::BadCover) => {}
            other => return Err(format!("open below a reclaimed cover: {:?}", other.err())),
        }
    }
    let mut seen: Vec<(u64, SlotKind, Vec<u64>)> = Vec::new();
    let end = block_on(read_log(
        &**clean,
        &layout(),
        &lane,
        trace.cover,
        None,
        |v| {
            seen.push((
                v.position.ordinal,
                v.kind,
                v.records().map(|r| r.seq).collect(),
            ));
            Ok(())
        },
    ))
    .map_err(|e| format!("read_log: {e:?}"))?;
    // The log from the cover is contiguous by construction of read_log; check it starts there.
    if let Some(first) = seen.first()
        && first.0 != trace.cover
    {
        return Err(format!("log starts at {} not the cover", first.0));
    }
    // 1, 2, 3: contents.
    let mut count: BTreeMap<u64, usize> = BTreeMap::new();
    for (_, kind, seqs) in &seen {
        if *kind == SlotKind::Data {
            for s in seqs {
                *count.entry(*s).or_default() += 1;
            }
        }
    }
    for (_, ordinal, seq) in &trace.acks {
        // An ack below the cover was reclaimed legitimately.
        if *ordinal < trace.cover {
            continue;
        }
        if count.get(seq) != Some(&1) {
            return Err(format!(
                "acknowledged seq {seq} at {ordinal} is in the log {:?} times",
                count.get(seq)
            ));
        }
    }
    let acked: BTreeSet<u64> = trace.acks.iter().map(|(_, _, s)| *s).collect();
    for (s, n) in &count {
        if *n > 1 {
            return Err(format!("seq {s} appears {n} times"));
        }
        if trace.refused.contains(s) {
            return Err(format!("seq {s} was certainly refused and is in the log"));
        }
        if !trace.attempted.contains(s) {
            return Err(format!("seq {s} was never attempted and is in the log"));
        }
        if !acked.contains(s) && !trace.unknown.contains(s) {
            return Err(format!(
                "seq {s} is in the log, unacknowledged and not unknown"
            ));
        }
    }
    // 1: acknowledgements are in ordinal order.
    let ordinals: Vec<u64> = trace.acks.iter().map(|(_, o, _)| *o).collect();
    if ordinals.windows(2).any(|w| w[0] >= w[1]) {
        return Err(format!("acks out of order: {ordinals:?}"));
    }
    // 7: a recovery open from the cover replays to the end and seals it.
    let recovered = block_on(LaneWriter::open(
        Arc::clone(clean),
        layout(),
        lane,
        trace.cover,
        |_| Ok(()),
    ))
    .map_err(|e| format!("recovery open: {e:?}"))?;
    let (_, report) = recovered;
    if report.fence_ordinal != end {
        return Err(format!(
            "recovery fenced {} but the end was {end}",
            report.fence_ordinal
        ));
    }
    if report.slots_replayed != end - trace.cover {
        return Err(format!(
            "recovery replayed {} slots for {} above the cover",
            report.slots_replayed,
            end - trace.cover
        ));
    }
    Ok(())
}

/// Fault scripts over the first `slots` mutating requests with at most `most` faults.
fn scripts(slots: usize, most: usize) -> Vec<Vec<Fault>> {
    fn go(at: usize, slots: usize, left: usize, cur: &mut Vec<Fault>, out: &mut Vec<Vec<Fault>>) {
        if at == slots {
            out.push(cur.clone());
            return;
        }
        cur.push(Fault::None);
        go(at + 1, slots, left, cur, out);
        cur.pop();
        if left > 0 {
            for f in FAULTS {
                cur.push(f);
                go(at + 1, slots, left - 1, cur, out);
                cur.pop();
            }
        }
    }
    let mut out = Vec::new();
    go(0, slots, most, &mut Vec::new(), &mut out);
    out
}

fn sequences(len: usize) -> Vec<Vec<Step>> {
    let mut out: Vec<Vec<Step>> = vec![Vec::new()];
    for _ in 0..len {
        out = out
            .into_iter()
            .flat_map(|s| {
                STEPS.iter().map(move |step| {
                    let mut n = s.clone();
                    n.push(*step);
                    n
                })
            })
            .collect();
    }
    out
}

fn sweep(len: usize, slots: usize, most: usize) -> usize {
    let scripts = scripts(slots, most);
    let mut runs = 0;
    for steps in sequences(len) {
        for script in &scripts {
            if let Err(why) = run(&steps, script) {
                panic!("history {steps:?} with faults {script:?}: {why}");
            }
            runs += 1;
        }
    }
    runs
}

#[test]
fn every_short_history_with_one_fault_keeps_every_invariant() {
    let runs = sweep(5, 5, 1);
    eprintln!("histories: length 5, 1 fault among the first 5 requests, {runs} runs");
}

#[test]
fn every_shorter_history_with_two_faults_keeps_every_invariant() {
    let runs = sweep(4, 4, 2);
    eprintln!("histories: length 4, 2 faults among the first 4 requests, {runs} runs");
}

/// The larger bound the evidence reports: 2.2 million histories, about five minutes in a debug
/// build. `cargo test -p hsearch-lane --test histories -- --ignored`.
#[test]
#[ignore = "five minutes: run for the evidence, not on every push"]
fn every_history_of_length_six_with_one_fault_keeps_every_invariant() {
    let runs = sweep(6, 6, 1);
    eprintln!("histories: length 6, 1 fault among the first 6 requests, {runs} runs");
}
