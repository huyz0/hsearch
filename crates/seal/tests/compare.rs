//! M06.09 measurements that are counts, not times, so they repeat exactly: the one-data-PUT slot log
//! against payload-plus-head-CAS under **one recovery oracle** (every acknowledged batch is found,
//! decrypts, and appears once and in order; nothing that was not acknowledged appears), and dirty-only
//! batching under sparse and hot tenants with the wraps and bytes sealing adds.
//!
//! Run `cargo test -p hsearch-seal --test compare -- --nocapture` for the tables recorded in
//! `docs/plan/evidence/m06-09-sealed-slots.md`. Provisional: the in-memory store and the in-memory test
//! KMS, one Linux host. Counts are exact for the seeds used; they say nothing about latency or a real KMS.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hsearch_format::{
    CommitToken, Durability, Id, KeyIdentity, Layout, PayloadRef, PolicyTuple, RecordOp,
    RecordSpec, SlotBody, SlotKind, SlotPosition, SlotView, encode_slot,
};
use hsearch_lane::batch::{BatchPolicy, Batcher, OwnedPayload, OwnedRecord};
use hsearch_lane::headcas::HeadCasLog;
use hsearch_lane::sealing::{Payloads, SlotCipher, encode_sealed_slot, open_records};
use hsearch_lane::{
    Error as LaneError, LaneId, LaneWriter, SealFailure, block_on, create_lane, read_log_sealed,
    shared_group,
};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    ByteRange, Error as StoreError, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey,
    Operation, Payload, RequestContext, VersionToken, WriteReceipt,
};
use hsearch_seal::{DekScope, Scope, SlotScopes, SlotSealer};
use hsearch_security::audit::MemoryAuditSink;
use hsearch_security::clock::ManualClock;
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{KeyRef, Profile};
use hsearch_security::manager::{CachePolicy, KeyAllowlist, KeyManager};
use hsearch_security::test_kms::TestKms;

fn id(n: u32) -> Id {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&n.to_be_bytes());
    out[15] = 0xA5;
    out
}

fn layout() -> Layout {
    Layout::new("test").unwrap()
}

fn policy() -> PolicyTuple {
    PolicyTuple {
        key: KeyIdentity::DeploymentDefault,
        residency: "region-a".into(),
        durability: Durability::Object,
        format_version: 1,
    }
}

fn memory() -> MemoryStore {
    MemoryStore::new(MemoryLimits {
        objects: 4096,
        versions: 16384,
        version_bytes: 64 << 20,
        identities: 16384,
        identity_bytes: 64 << 20,
    })
    .unwrap()
}

/// Every index `1..` is keyed under the deployment default key.
struct AllKeyed;
impl SlotScopes for AllKeyed {
    fn scope(&self, index: &Id) -> Result<Option<Scope>, SealFailure> {
        let n = u32::from_be_bytes(index[..4].try_into().unwrap());
        Ok(Some(Scope {
            index: IndexId::new(&format!("tenant-{n}")).unwrap(),
            domain: [7; 16],
            key: None,
            generation: 1,
        }))
    }
}

fn sealer() -> (Arc<TestKms>, Arc<SlotSealer>) {
    sealer_with(DekScope::default())
}

fn sealer_with(dek: DekScope) -> (Arc<TestKms>, Arc<SlotSealer>) {
    let kms = Arc::new(TestKms::new());
    let key = KeyRef::new("test/acct/default").unwrap();
    kms.create_key(&key).unwrap();
    let keys = Arc::new(
        KeyManager::new(
            Profile::Development,
            kms.clone(),
            KeyAllowlist::new(vec![("test".into(), "acct".into())]),
            key,
            CachePolicy::default(),
            Arc::new(MemoryAuditSink::new(1_000_000)),
            Arc::new(ManualClock::new()),
        )
        .unwrap(),
    );
    (
        kms,
        Arc::new(SlotSealer::new(keys, Arc::new(AllKeyed)).with_dek_scope(dek)),
    )
}

fn splitmix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A store that refuses a mutating request before writing it, or writes it and loses the reply, with
/// fixed per-request probabilities from a seed; it counts every mutating request.
struct Faulty {
    inner: MemoryStore,
    seed: u64,
    refuse_pct: u64,
    lose_pct: u64,
    next: AtomicU64,
    creates: AtomicU64,
    cas: AtomicU64,
}

#[derive(PartialEq)]
enum Roll {
    Clean,
    Refuse,
    Lose,
}

impl Faulty {
    fn new(seed: u64, refuse_pct: u64, lose_pct: u64) -> Self {
        Self {
            inner: memory(),
            seed,
            refuse_pct,
            lose_pct,
            next: AtomicU64::new(0),
            creates: AtomicU64::new(0),
            cas: AtomicU64::new(0),
        }
    }
    fn roll(&self) -> Roll {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let r = splitmix(self.seed ^ n.wrapping_mul(0xA24B_AED4_963E_E407)) % 100;
        if r < self.refuse_pct {
            Roll::Refuse
        } else if r < self.refuse_pct + self.lose_pct {
            Roll::Lose
        } else {
            Roll::Clean
        }
    }
    fn mutations(&self) -> u64 {
        self.creates.load(Ordering::SeqCst) + self.cas.load(Ordering::SeqCst)
    }
}

impl ObjectBackend for Faulty {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            self.creates.fetch_add(1, Ordering::SeqCst);
            match self.roll() {
                Roll::Refuse => Err(StoreError::Unavailable),
                Roll::Lose => {
                    self.inner.create(c, r).await?;
                    Err(StoreError::UnknownOutcome)
                }
                Roll::Clean => self.inner.create(c, r).await,
            }
        })
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

impl MetadataStore for Faulty {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            self.cas.fetch_add(1, Ordering::SeqCst);
            match self.roll() {
                Roll::Refuse => Err(StoreError::Unavailable),
                Roll::Lose => {
                    self.inner.compare_exchange(c, r).await?;
                    Err(StoreError::UnknownOutcome)
                }
                Roll::Clean => self.inner.compare_exchange(c, r).await,
            }
        })
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
        self.inner.resolve(c, r)
    }
}

fn body(k: u64) -> Vec<u8> {
    let mut b = format!("batch-{k:05}-").into_bytes();
    b.resize(1024, b'x');
    b
}

fn batch_of(bytes: &[u8]) -> u64 {
    std::str::from_utf8(&bytes[6..11]).unwrap().parse().unwrap()
}

#[derive(Default, Debug)]
struct Run {
    acked: Vec<u64>,
    found: Vec<u64>,
    attempts: u64,
    reopens: u64,
    mutations: u64,
    wraps: u64,
    orphans: u64,
}

const BATCHES: u64 = 400;

/// Commit `BATCHES` one-record batches through the slot log, retrying a certain refusal and reopening
/// a poisoned writer, then replay everything with the sealed reader.
fn slot_log(seed: u64, refuse: u64, lose: u64) -> Run {
    let (kms, cipher) = sealer();
    let store = Arc::new(Faulty::new(seed, refuse, lose));
    let mut run = Run::default();
    block_on(async {
        let (domain, group) = shared_group(&policy(), 0);
        let lane = LaneId::new(domain);
        // Setup runs fault free.
        store.next.store(u64::MAX / 2, Ordering::SeqCst);
        create_lane(&*store, &layout(), &lane, &policy(), group, 4, &[id(1)])
            .await
            .unwrap();
        let mut writer = None;
        for k in 0..BATCHES {
            let plain = body(k);
            let record = [RecordSpec {
                index: id(1),
                seq: k + 1,
                op: RecordOp::Put,
                payload: PayloadRef::Inline(&plain),
            }];
            loop {
                if writer.is_none() {
                    let opened = LaneWriter::open_sealed(
                        store.clone(),
                        layout(),
                        lane,
                        0,
                        cipher.clone() as Arc<dyn SlotCipher>,
                        |_, _| Ok(()),
                    )
                    .await;
                    match opened {
                        Ok((w, _)) => {
                            writer = Some(w);
                            run.reopens += 1;
                        }
                        Err(_) => continue,
                    }
                }
                run.attempts += 1;
                match writer.as_mut().unwrap().append(&record).await {
                    Ok(_) => {
                        run.acked.push(k);
                        break;
                    }
                    Err(LaneError::Store(_)) => {}
                    Err(_) => writer = None,
                }
            }
        }
        store.next.store(u64::MAX / 2, Ordering::SeqCst);
        read_log_sealed(
            &*store,
            &layout(),
            &lane,
            0,
            None,
            &*cipher,
            |view, records| {
                if view.kind == SlotKind::Data {
                    for r in records {
                        if let PayloadRef::Inline(b) = r.payload {
                            run.found.push(batch_of(b));
                        }
                    }
                }
                Ok(())
            },
        )
        .await
        .unwrap();
    });
    run.mutations = store.mutations();
    run.wraps = kms.wrap_calls();
    run
}

/// The same, through payload-plus-head-CAS, writing the identical sealed slot bytes as the payload.
fn head_cas(seed: u64, refuse: u64, lose: u64) -> Run {
    let (kms, cipher) = sealer();
    let store = Arc::new(Faulty::new(seed, refuse, lose));
    let mut run = Run::default();
    block_on(async {
        let (domain, _) = shared_group(&policy(), 0);
        let lane = LaneId::new(domain);
        store.next.store(u64::MAX / 2, Ordering::SeqCst);
        let mut log = Some(
            HeadCasLog::open(store.clone(), layout(), lane)
                .await
                .unwrap(),
        );
        store.next.store(seed ^ 0xFEED, Ordering::SeqCst);
        let mut scratch = Vec::new();
        let mut payloads = Payloads::default();
        for k in 0..BATCHES {
            let plain = body(k);
            let record = [RecordSpec {
                index: id(1),
                seq: k + 1,
                op: RecordOp::Put,
                payload: PayloadRef::Inline(&plain),
            }];
            loop {
                if log.is_none() {
                    match HeadCasLog::open(store.clone(), layout(), lane).await {
                        Ok(l) => {
                            log = Some(l);
                            run.reopens += 1;
                        }
                        Err(_) => continue,
                    }
                }
                let l = log.as_mut().unwrap();
                let at = SlotPosition {
                    domain: lane.domain,
                    activation: lane.activation,
                    lane: lane.lane,
                    ordinal: l.committed(),
                };
                scratch.clear();
                encode_sealed_slot(
                    &*cipher,
                    &mut scratch,
                    &mut payloads,
                    &at,
                    CommitToken::new([5; 16]).unwrap(),
                    &record,
                )
                .unwrap();
                run.attempts += 1;
                let cas_before = store.cas.load(Ordering::SeqCst);
                match l.append(&scratch).await {
                    Ok(_) => {
                        run.acked.push(k);
                        break;
                    }
                    Err(LaneError::Store(_)) => {
                        // A refused head CAS leaves the payload it followed behind: an orphan.
                        if store.cas.load(Ordering::SeqCst) > cas_before {
                            run.orphans += 1;
                        }
                    }
                    Err(_) => log = None,
                }
            }
        }
        store.next.store(u64::MAX / 2, Ordering::SeqCst);
        let mut scratch = Payloads::default();
        HeadCasLog::read_all(&*store, &layout(), &lane, |ordinal, bytes| {
            let at = SlotPosition {
                domain: lane.domain,
                activation: lane.activation,
                lane: lane.lane,
                ordinal,
            };
            let view = SlotView::parse_at(bytes, &at).unwrap();
            for r in open_records(&*cipher, &view, &mut scratch).unwrap() {
                if let PayloadRef::Inline(b) = r.payload {
                    run.found.push(batch_of(b));
                }
            }
        })
        .await
        .unwrap();
    });
    run.mutations = store.mutations();
    run.wraps = kms.wrap_calls();
    run
}

/// The recovery oracle both protocols face: every acknowledged batch is found after replay, decrypts,
/// and appears exactly once and in order; nothing that was not acknowledged appears.
fn oracle(name: &str, run: &Run) {
    assert_eq!(
        run.acked.len() as u64,
        BATCHES,
        "{name}: every batch was eventually acknowledged"
    );
    assert_eq!(
        run.found, run.acked,
        "{name}: replay found exactly the acknowledged batches, in order"
    );
}

#[test]
fn both_protocols_pass_one_recovery_oracle_and_the_table_is_recorded() {
    println!(
        "\n## One-data-PUT slot log vs payload + head CAS, sealed, {BATCHES} one-record batches of 1 KiB\n"
    );
    println!(
        "Per-request fault probabilities, same seed. Counts exact for these seeds; provisional (in-memory store and test KMS).\n"
    );
    println!(
        "| protocol | faults (refuse / lose reply) | mutating requests | per acknowledged batch | payload attempts per batch | orphan payloads | reopens | KMS wraps |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for (refuse, lose) in [(0u64, 0u64), (5, 5), (15, 5)] {
        for (name, run) in [
            ("slot log (1 PUT)", slot_log(0xC0FFEE, refuse, lose)),
            ("payload + head CAS", head_cas(0xC0FFEE, refuse, lose)),
        ] {
            oracle(name, &run);
            println!(
                "| {name} | {refuse}% / {lose}% | {} | {:.2} | {:.2} | {} | {} | {} |",
                run.mutations,
                run.mutations as f64 / BATCHES as f64,
                run.attempts as f64 / BATCHES as f64,
                run.orphans,
                run.reopens,
                run.wraps
            );
        }
    }
    // Clean-run invariants: the slot log is one PUT a batch, head CAS is two.
    let clean_slot = slot_log(1, 0, 0);
    let clean_cas = head_cas(1, 0, 0);
    assert_eq!(
        clean_slot.mutations,
        BATCHES + 1 + 1,
        "one create per batch, one fence, one lane descriptor"
    );
    assert_eq!(
        clean_cas.mutations,
        2 * BATCHES + 1,
        "two PUTs per batch and the head create"
    );
    assert_eq!(
        (clean_slot.attempts, clean_cas.attempts),
        (BATCHES, BATCHES)
    );
    assert_eq!(
        (clean_slot.wraps, clean_cas.wraps),
        (BATCHES, BATCHES),
        "one wrap per slot either way"
    );
}

// -------------------------------------------------------------------------- dirty-only batching

struct Workload {
    name: &'static str,
    indexes: u32,
    docs: u64,
    gap_ms: u64,
    bytes: usize,
}

struct Counts {
    slots: u64,
    records: u64,
    wraps: u64,
    plain_slot_bytes: u64,
    sealed_slot_bytes: u64,
}

/// Feed `w` through the real batcher on a virtual clock and encode each closed batch plain and sealed.
fn batching(w: &Workload, attempts_per_slot: u64, dek: DekScope) -> Counts {
    let (kms, cipher) = sealer_with(dek);
    let mut batcher = Batcher::new(BatchPolicy::default()).unwrap();
    let mut counts = Counts {
        slots: 0,
        records: 0,
        wraps: 0,
        plain_slot_bytes: 0,
        sealed_slot_bytes: 0,
    };
    let mut ordinal = 0u64;
    let mut seqs = vec![0u64; w.indexes as usize + 1];
    let mut emit = |batch: hsearch_lane::batch::Batch, counts: &mut Counts| {
        let specs = batch.specs();
        let at = SlotPosition {
            domain: [3; 16],
            activation: 1,
            lane: 0,
            ordinal,
        };
        ordinal += 1;
        let token = CommitToken::new([5; 16]).unwrap();
        let mut plain = Vec::new();
        encode_slot(&mut plain, &at, token, &SlotBody::Data(&specs)).unwrap();
        let before = kms.wrap_calls();
        let mut sealed = Vec::new();
        let mut payloads = Payloads::default();
        // A refused commit attempt seals again: attempts_per_slot > 1 charges those wraps and CPU too.
        for _ in 0..attempts_per_slot {
            sealed.clear();
            encode_sealed_slot(&*cipher, &mut sealed, &mut payloads, &at, token, &specs).unwrap();
        }
        counts.slots += 1;
        counts.records += specs.len() as u64;
        counts.wraps += kms.wrap_calls() - before;
        counts.plain_slot_bytes += plain.len() as u64;
        counts.sealed_slot_bytes += sealed.len() as u64;
    };
    for n in 0..w.docs {
        let index = (n % u64::from(w.indexes)) as u32 + 1;
        seqs[index as usize] += 1;
        let record = OwnedRecord {
            index: id(index),
            seq: seqs[index as usize],
            op: RecordOp::Put,
            payload: OwnedPayload::Inline(vec![0x42; w.bytes]),
        };
        let now = Duration::from_millis(n * w.gap_ms);
        if let Some(batch) = batcher.poll(now) {
            emit(batch, &mut counts);
        }
        for batch in batcher.push(record, now).unwrap() {
            emit(batch, &mut counts);
        }
    }
    if let Some(batch) = batcher.flush() {
        emit(batch, &mut counts);
    }
    counts
}

#[test]
fn dirty_only_batching_under_sparse_and_hot_tenants_is_measured() {
    let workloads = [
        Workload {
            name: "hot: 1 index, 1 KiB docs, 1 per ms",
            indexes: 1,
            docs: 5_000,
            gap_ms: 1,
            bytes: 1024,
        },
        Workload {
            name: "hot: 1 index, 64 KiB docs, 1 per ms",
            indexes: 1,
            docs: 500,
            gap_ms: 1,
            bytes: 64 * 1024,
        },
        Workload {
            name: "warm: 20 indexes, 1 KiB docs, 1 per ms",
            indexes: 20,
            docs: 5_000,
            gap_ms: 1,
            bytes: 1024,
        },
        Workload {
            name: "sparse: 1000 indexes, 1 KiB docs, 1 per ms",
            indexes: 1000,
            docs: 5_000,
            gap_ms: 1,
            bytes: 1024,
        },
        Workload {
            name: "very sparse: 1000 indexes, 1 KiB docs, 1 per 10 ms",
            indexes: 1000,
            docs: 2_000,
            gap_ms: 10,
            bytes: 1024,
        },
    ];
    println!(
        "\n## Dirty-only batching, sealed (default policy: 4096 records, 4 MiB, 250 ms), virtual clock\n"
    );
    println!(
        "| workload | data key | slots | records per slot | KMS wraps per slot | wraps per record | plain slot KiB | sealed slot KiB | sealing overhead |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    for w in &workloads {
        for dek in [DekScope::PerKey, DekScope::PerIndex] {
            let c = batching(w, 1, dek);
            // Every record is sealed once, whatever the scope.
            assert_eq!(c.records, w.docs, "{}", w.name);
            assert!(c.wraps >= c.slots, "{}", w.name);
            assert!(c.wraps <= c.records, "{}", w.name);
            assert!(c.sealed_slot_bytes > c.plain_slot_bytes, "{}", w.name);
            println!(
                "| {} | {dek:?} | {} | {:.0} | {:.1} | {:.3} | {:.1} | {:.1} | {:+.1}% |",
                w.name,
                c.slots,
                c.records as f64 / c.slots as f64,
                c.wraps as f64 / c.slots as f64,
                c.wraps as f64 / c.records as f64,
                c.plain_slot_bytes as f64 / c.slots as f64 / 1024.0,
                c.sealed_slot_bytes as f64 / c.slots as f64 / 1024.0,
                (c.sealed_slot_bytes as f64 / c.plain_slot_bytes as f64 - 1.0) * 100.0
            );
        }
    }
    // One key per slot costs one wrap however many tenants; one per index costs one per tenant in the slot.
    let hot = batching(&workloads[0], 1, DekScope::PerKey);
    assert_eq!(hot.wraps, hot.slots);
    let sparse_key = batching(&workloads[3], 1, DekScope::PerKey);
    let sparse_index = batching(&workloads[3], 1, DekScope::PerIndex);
    assert_eq!(sparse_key.wraps, sparse_key.slots);
    assert!(sparse_index.wraps as f64 / sparse_index.records as f64 > 0.99);
    assert!(sparse_key.sealed_slot_bytes < sparse_index.sealed_slot_bytes);
    // A refused attempt seals the slot again, wraps included: three attempts triple the wraps.
    assert_eq!(
        batching(&workloads[3], 3, DekScope::PerIndex).wraps,
        3 * sparse_index.wraps
    );
    assert_eq!(
        batching(&workloads[3], 3, DekScope::PerKey).wraps,
        3 * sparse_key.wraps
    );
}
