//! M03.05: a bounded exhaustive model of the catalog and the structural head.
//!
//! Every sequence of up to four operations (create or delete a name for an identity, and the head's
//! establish, checkpoint, schema and a deliberate regression) is run against a store that applies
//! one scripted fault to the n-th mutating request: refused, written with the reply lost and
//! resolvable, written with the reply lost and unresolvable, or **preempted** by another actor's
//! operation that lands between this operation's last read and its conditional write. After every
//! step, and again after a clean retry of whatever ended unknown, these hold:
//!
//! 1. no name ever resolves to an identity it was observed to lose, and no create of one is
//!    acknowledged;
//! 2. an acknowledged create resolves to its identity at once, an acknowledged delete to its
//!    tombstone, and `Exists` is only answered for a name live under another identity;
//! 3. a name's generation never falls; an identity is live under at most one name;
//! 4. no page is ever unreadable (`verify`);
//! 5. the head's generation, writer epoch, cover, schema, publication and membership never fall,
//!    its inventory only grows by appending, epochs run `1..=writer_epoch`, and the same
//!    generation is always the same head;
//! 6. a transition that would regress the head is refused before any request is made;
//! 7. only the documented errors occur.
//!
//! An enumeration over a bounded world (two names in one bucket, three identities, one head), not
//! a proof for unbounded histories; the bounds are printed.
mod support;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use hsearch_format::{Durability, Id, StructuralHead, bucket_of};
use hsearch_lane::{
    Advance, Catalog, Change, Error, LaneId, Resolution, advance_head, block_on, create_catalog,
    create_head, read_head,
};
use hsearch_object::memory::MemoryStore;
use hsearch_object::{
    ByteRange, Error as StoreError, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey,
    Operation, Payload, RequestContext, RequestId, VersionToken, WriteReceipt,
};
use support::*;

const CATALOG: Id = [5; 16];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Op {
    Create(usize, u32),
    Delete(usize, u32),
    Establish,
    Checkpoint,
    Schema,
    /// A checkpoint below the current cover: must be refused before any request.
    Regress,
}

const OPS: [Op; 16] = [
    Op::Create(0, 1),
    Op::Create(0, 2),
    Op::Create(0, 3),
    Op::Create(1, 1),
    Op::Create(1, 2),
    Op::Create(1, 3),
    Op::Delete(0, 1),
    Op::Delete(0, 2),
    Op::Delete(0, 3),
    Op::Delete(1, 1),
    Op::Delete(1, 2),
    Op::Delete(1, 3),
    Op::Establish,
    Op::Checkpoint,
    Op::Schema,
    Op::Regress,
];

/// What another actor does between an operation's last read and its conditional write.
const COMPETITORS: [Op; 5] = [
    Op::Create(0, 2),
    Op::Delete(0, 1),
    Op::Create(1, 3),
    Op::Delete(0, 2),
    Op::Schema,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Fault {
    None,
    Refuse,
    LoseResolvable,
    LoseUnresolvable,
    Preempt(usize),
}

fn faults() -> Vec<Fault> {
    let mut out = vec![
        Fault::Refuse,
        Fault::LoseResolvable,
        Fault::LoseUnresolvable,
    ];
    out.extend((0..COMPETITORS.len()).map(Fault::Preempt));
    out
}

type Hook = Arc<dyn Fn(usize) + Send + Sync>;

struct Scripted {
    inner: Arc<MemoryStore>,
    script: Mutex<Vec<Fault>>,
    calls: AtomicUsize,
    unresolvable: Mutex<BTreeSet<RequestId>>,
    hook: Mutex<Option<Hook>>,
}

impl Scripted {
    fn new() -> Self {
        Self {
            inner: Arc::new(memory()),
            script: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            unresolvable: Mutex::new(BTreeSet::new()),
            hook: Mutex::new(None),
        }
    }
    fn arm(&self, script: &[Fault]) {
        *self.script.lock().unwrap() = script.to_vec();
        self.calls.store(0, Ordering::SeqCst);
    }
    fn mutations(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn mutate<'a>(
        &'a self,
        request: &'a Mutation,
        exec: impl Fn(&'a Mutation) -> Operation<'a, WriteReceipt> + 'a,
    ) -> Operation<'a, WriteReceipt> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let fault = self
            .script
            .lock()
            .unwrap()
            .get(n)
            .copied()
            .unwrap_or(Fault::None);
        match fault {
            Fault::None => exec(request),
            Fault::Preempt(who) => {
                let hook = self.hook.lock().unwrap().clone();
                if let Some(hook) = hook {
                    hook(who);
                }
                exec(request)
            }
            Fault::Refuse => Box::pin(async { Err(StoreError::Unavailable) }),
            Fault::LoseResolvable | Fault::LoseUnresolvable => {
                if fault == Fault::LoseUnresolvable {
                    self.unresolvable
                        .lock()
                        .unwrap()
                        .insert(request.request_id.clone());
                }
                let done = exec(request);
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

/// What the checks remember between steps.
#[derive(Default)]
struct Trace {
    /// (name, identity) pairs observed or acknowledged to have been lost.
    lost: BTreeSet<(usize, u32)>,
    generation: [u64; 2],
    head: Option<StructuralHead>,
    /// Operations that ended unknown and are retried clean at the end.
    unknown: Vec<Op>,
    violation: Option<String>,
}

impl Trace {
    fn fail(&mut self, why: String) {
        self.violation.get_or_insert(why);
    }
}

struct World {
    store: Arc<Scripted>,
    catalog: Catalog,
    names: [String; 2],
    lane: LaneId,
    trace: Arc<Mutex<Trace>>,
}

fn two_names_in_one_bucket() -> [String; 2] {
    let mut found = Vec::new();
    for i in 0u32.. {
        let n = format!("n{i}");
        if bucket_of(&CATALOG, 1, &n) == 0 {
            found.push(n);
            if found.len() == 2 {
                break;
            }
        }
    }
    [found[0].clone(), found[1].clone()]
}

fn world() -> World {
    let store = Arc::new(Scripted::new());
    let catalog = block_on(create_catalog(&*store.inner, &layout(), CATALOG, 1)).unwrap();
    let lane = LaneId::new([7; 16]);
    block_on(create_head(
        &*store.inner,
        &layout(),
        &lane,
        Durability::Object,
    ))
    .unwrap();
    let w = World {
        store: Arc::clone(&store),
        catalog,
        names: two_names_in_one_bucket(),
        lane,
        trace: Arc::new(Mutex::new(Trace::default())),
    };
    let inner = Arc::clone(&store.inner);
    let catalog = w.catalog.clone();
    let names = w.names.clone();
    let trace = Arc::clone(&w.trace);
    *store.hook.lock().unwrap() = Some(Arc::new(move |who| {
        // Another actor's operation, straight on the store (no faults), recorded like any other.
        let op = COMPETITORS[who];
        let result = apply(&*inner, &catalog, &names, &lane, op);
        record(&*inner, &catalog, &names, &trace, op, &result, None);
    }));
    w
}

/// Run one operation against `store`.
fn apply<S: MetadataStore>(
    store: &S,
    catalog: &Catalog,
    names: &[String; 2],
    lane: &LaneId,
    op: Op,
) -> Result<(), Error> {
    match op {
        Op::Create(n, i) => block_on(catalog.create(store, &layout(), &names[n], id(i))),
        Op::Delete(n, i) => block_on(catalog.delete(store, &layout(), &names[n], &id(i))),
        Op::Establish | Op::Checkpoint | Op::Schema | Op::Regress => {
            let (head, version) = block_on(read_head(store, &layout(), lane))?;
            let change = match op {
                Op::Establish => Change::Establish {
                    activation: 1,
                    lane: 0,
                    fence: head.tail.ordinal,
                },
                Op::Checkpoint => Change::Checkpoint {
                    cover: head.cover + 1,
                    root: [(head.cover + 1) as u8; 16],
                },
                Op::Schema => Change::Schema,
                _ => {
                    if head.cover == 0 {
                        return Ok(());
                    }
                    Change::Checkpoint {
                        cover: head.cover - 1,
                        root: [9; 16],
                    }
                }
            };
            match block_on(advance_head(
                store,
                &layout(),
                lane,
                &head,
                &version,
                change,
            ))? {
                Advance::Done(..) | Advance::Lost => Ok(()),
            }
        }
    }
}

fn resolve_all<S: MetadataStore>(
    store: &S,
    catalog: &Catalog,
    names: &[String; 2],
) -> Result<[Resolution; 2], Error> {
    Ok([
        block_on(catalog.resolve(store, &layout(), &names[0]))?,
        block_on(catalog.resolve(store, &layout(), &names[1]))?,
    ])
}

fn ident(n: Id) -> u32 {
    u32::from_be_bytes([n[0], n[1], n[2], n[3]])
}

/// Check one operation's result against the state read straight afterwards, then the whole state.
fn record<S: MetadataStore>(
    clean: &S,
    catalog: &Catalog,
    names: &[String; 2],
    trace: &Mutex<Trace>,
    op: Op,
    result: &Result<(), Error>,
    mutations_made: Option<usize>,
) {
    let mut t = trace.lock().unwrap();
    let state = match resolve_all(clean, catalog, names) {
        Ok(s) => s,
        Err(e) => {
            t.fail(format!("{op:?}: state unreadable: {e:?}"));
            return;
        }
    };
    match (op, result) {
        (Op::Create(n, i), Ok(())) => {
            if t.lost.contains(&(n, i)) {
                t.fail(format!(
                    "create {n} for {i} acknowledged after the name lost it"
                ));
            }
            if !matches!(state[n], Resolution::Live { index, .. } if ident(index) == i) {
                t.fail(format!(
                    "acknowledged create {n} for {i} reads {:?}",
                    state[n]
                ));
            }
        }
        (Op::Delete(n, i), Ok(())) => {
            t.lost.insert((n, i));
            if !matches!(state[n], Resolution::Deleted { last, .. } if ident(last) == i) {
                t.fail(format!(
                    "acknowledged delete {n} of {i} reads {:?}",
                    state[n]
                ));
            }
        }
        (Op::Create(n, i), Err(Error::Exists)) => {
            if !matches!(state[n], Resolution::Live { index, .. } if ident(index) != i) {
                t.fail(format!(
                    "Exists for {n} with {i}, but it reads {:?}",
                    state[n]
                ));
            }
        }
        (Op::Create(..) | Op::Delete(..), Err(Error::Invalid(_) | Error::NoName)) => {}
        (
            Op::Create(..) | Op::Delete(..) | Op::Establish | Op::Checkpoint | Op::Schema,
            Err(Error::Contended | Error::Store(StoreError::Unavailable)),
        ) => t.unknown.push(op),
        (Op::Regress, Err(Error::Format(_))) => {
            if mutations_made != Some(0) {
                t.fail(format!("a regression made {mutations_made:?} requests"));
            }
        }
        (Op::Regress | Op::Establish | Op::Checkpoint | Op::Schema, Ok(())) => {}
        (op, other) => t.fail(format!("{op:?} ended {other:?}")),
    }
    // The whole state, every time.
    for (n, res) in state.iter().enumerate() {
        let (generation, live, last) = match res {
            Resolution::Absent => (0, None, None),
            Resolution::Live {
                index,
                name_generation,
            } => (*name_generation, Some(ident(*index)), None),
            Resolution::Deleted {
                last,
                name_generation,
            } => (*name_generation, None, Some(ident(*last))),
        };
        if generation < t.generation[n] {
            let was = t.generation[n];
            t.fail(format!("name {n} generation fell {was} -> {generation}"));
        }
        t.generation[n] = generation.max(t.generation[n]);
        if let Some(l) = last {
            t.lost.insert((n, l));
        }
        if let Some(i) = live
            && t.lost.contains(&(n, i))
        {
            t.fail(format!("name {n} resolves to {i}, an identity it lost"));
        }
    }
    let live: Vec<u32> = state
        .iter()
        .filter_map(|r| match r {
            Resolution::Live { index, .. } => Some(ident(*index)),
            _ => None,
        })
        .collect();
    if live.len() == 2 && live[0] == live[1] {
        t.fail(format!("identity {} is live under both names", live[0]));
    }
    match block_on(catalog.verify(clean, &layout())) {
        Ok(r) if r.pages_bad == 0 => {}
        other => t.fail(format!("pages after {op:?}: {other:?}")),
    }
    check_head(clean, &mut t);
}

fn check_head<S: MetadataStore>(clean: &S, t: &mut Trace) {
    let (head, _) = match block_on(read_head(clean, &layout(), &LaneId::new([7; 16]))) {
        Ok(h) => h,
        Err(e) => return t.fail(format!("head unreadable: {e:?}")),
    };
    if head.writer_epoch as usize != head.inventory.len()
        || head
            .inventory
            .iter()
            .enumerate()
            .any(|(k, e)| e.epoch != k as u64 + 1)
    {
        t.fail(format!(
            "inventory is not epochs 1..=n: {:?}",
            head.inventory
        ));
    }
    if let Some(old) = t.head.clone() {
        let falls = head.generation < old.generation
            || head.writer_epoch < old.writer_epoch
            || head.cover < old.cover
            || head.schema_generation < old.schema_generation
            || head.publication_generation < old.publication_generation
            || head.membership_epoch < old.membership_epoch
            || head.tail.ordinal < old.tail.ordinal;
        if falls {
            t.fail(format!("head regressed {old:?} -> {head:?}"));
        }
        if head.generation == old.generation && head != old {
            t.fail("one generation, two heads".into());
        }
        if head.inventory.get(..old.inventory.len()) != Some(old.inventory.as_slice()) {
            t.fail("the epoch inventory was rewritten".into());
        }
    }
    t.head = Some(head);
}

fn run(ops: &[Op], script: &[Fault]) -> Result<(), String> {
    let w = world();
    check_head(&*w.store.inner, &mut w.trace.lock().unwrap());
    w.store.arm(script);
    for op in ops {
        let before = w.store.mutations();
        let result = apply(&*w.store, &w.catalog, &w.names, &w.lane, *op);
        let made = w.store.mutations() - before;
        record(
            &*w.store.inner,
            &w.catalog,
            &w.names,
            &w.trace,
            *op,
            &result,
            Some(made),
        );
        if let Some(v) = w.trace.lock().unwrap().violation.clone() {
            return Err(v);
        }
    }
    // Whatever ended unknown is repeated with the store healthy: it must settle, never corrupt.
    w.store.arm(&[]);
    let unknown = std::mem::take(&mut w.trace.lock().unwrap().unknown);
    for op in unknown {
        let result = apply(&*w.store, &w.catalog, &w.names, &w.lane, op);
        record(
            &*w.store.inner,
            &w.catalog,
            &w.names,
            &w.trace,
            op,
            &result,
            None,
        );
        w.trace.lock().unwrap().unknown.clear();
        if let Some(v) = w.trace.lock().unwrap().violation.clone() {
            return Err(format!("on clean retry of {op:?}: {v}"));
        }
        if matches!(result, Err(Error::Contended | Error::Store(_))) {
            return Err(format!(
                "{op:?} did not settle on a healthy store: {result:?}"
            ));
        }
    }
    Ok(())
}

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
            for f in faults() {
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

fn sequences(len: usize) -> Vec<Vec<Op>> {
    let mut out: Vec<Vec<Op>> = vec![Vec::new()];
    for _ in 0..len {
        out = out
            .into_iter()
            .flat_map(|s| {
                OPS.iter().map(move |op| {
                    let mut n = s.clone();
                    n.push(*op);
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
    for ops in sequences(len) {
        for script in &scripts {
            if let Err(why) = run(&ops, script) {
                panic!("history {ops:?} with faults {script:?}: {why}");
            }
            runs += 1;
        }
    }
    runs
}

#[test]
fn every_history_of_three_operations_with_up_to_two_faults_keeps_every_invariant() {
    let runs = sweep(3, 2, 2);
    eprintln!("model: length 3, 2 faults among the first 2 requests, {runs} runs");
}

#[test]
fn every_history_of_three_operations_with_one_fault_in_four_requests_keeps_every_invariant() {
    let runs = sweep(3, 4, 1);
    eprintln!("model: length 3, 1 fault among the first 4 requests, {runs} runs");
}

#[test]
#[ignore = "minutes: run for the evidence, not on every push"]
fn the_larger_bounds_keep_every_invariant() {
    let a = sweep(3, 4, 2);
    let b = sweep(4, 4, 1);
    eprintln!(
        "model: length 3 with 2 faults in 4 requests, {a} runs; length 4 with 1 fault, {b} runs"
    );
}

/// The checks must be able to fail: a history that really does resurrect a name is caught.
#[test]
fn the_checks_catch_a_resurrection() {
    let w = world();
    let mut t = Trace::default();
    t.lost.insert((0, 1));
    *w.trace.lock().unwrap() = t;
    // The store honestly refuses the create, so the trace alone is not enough: fake the
    // acknowledgement the way a broken catalog would report it.
    block_on(
        w.catalog
            .create(&*w.store.inner, &layout(), &w.names[0], id(1)),
    )
    .unwrap();
    record(
        &*w.store.inner,
        &w.catalog,
        &w.names,
        &w.trace,
        Op::Create(0, 1),
        &Ok(()),
        None,
    );
    assert!(w.trace.lock().unwrap().violation.is_some());
}
