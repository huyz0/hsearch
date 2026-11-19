//! M06.09 slot-level sealing over the fenced lane, the in-memory store and the isolated test KMS:
//! a commit slot of a keyed index holds no plaintext, replay opens every acknowledged slot, a
//! tampered, moved or wrongly keyed payload is refused, no data key or nonce repeats, and sealing
//! costs the lane no request.
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use hsearch_format::{
    Durability, Id, KeyIdentity, Layout, MAX_INLINE_RECORD_BYTES, PayloadRef, PolicyTuple,
    RecordOp, RecordSpec, SlotKind, SlotPosition, SlotView,
};
use hsearch_lane::counting::CountingStore;
use hsearch_lane::sealing::{Payloads, SlotCipher, encode_sealed_slot};
use hsearch_lane::{
    Error as LaneError, LaneId, LaneWriter, SealFailure, block_on, create_head, create_lane,
    read_log, read_log_sealed, shared_group,
};
use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error as StoreError, MetadataStore, Mutation, ObjectBackend,
    ObjectInfo, ObjectKey, Operation, Payload, RequestContext, VersionToken, WriteReceipt,
};
use hsearch_seal::{DekScope, Scope, SlotScopes, SlotSealer};
use hsearch_security::audit::MemoryAuditSink;
use hsearch_security::clock::ManualClock;
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{KeyRef, Profile};
use hsearch_security::manager::{CachePolicy, KeyAllowlist, KeyManager};
use hsearch_security::test_kms::TestKms;

// ----------------------------------------------------------------------------------------- rig

fn id(n: u32) -> Id {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&n.to_be_bytes());
    out[15] = 0xA5;
    out
}

const ORDERS: u32 = 1;
const INVOICES: u32 = 2;
const PUBLIC: u32 = 3;

fn kref(name: &str) -> KeyRef {
    KeyRef::new(&format!("test/acct/{name}")).unwrap()
}

/// Index 1 `orders` (deployment default key), 2 `invoices` (its own key), 3 `public` (no managed key),
/// 4.. `tenant-n` on the default key. Anything else is refused.
struct Scopes {
    generation: AtomicU64,
    /// The index (by number) whose policy generation is one ahead of the others; 0 for none.
    rotated: AtomicU32,
    /// Indexes that choose one data key of their own whatever the sealer's setting is.
    own_key: Vec<u32>,
}

impl SlotScopes for Scopes {
    fn scope(&self, index: &Id) -> Result<Option<Scope>, SealFailure> {
        let n = u32::from_be_bytes(index[..4].try_into().unwrap());
        let generation = self.generation.load(Ordering::SeqCst)
            + u64::from(self.rotated.load(Ordering::SeqCst) == n);
        let (name, key) = match n {
            ORDERS => ("orders".to_string(), None),
            INVOICES => ("invoices".to_string(), Some(kref("invoices"))),
            PUBLIC => return Ok(None),
            4..=5000 => (format!("tenant-{n}"), None),
            _ => return Err(SealFailure::Denied),
        };
        Ok(Some(Scope {
            index: IndexId::new(&name).unwrap(),
            domain: [7; 16],
            key,
            generation,
        }))
    }

    fn dek_scope(&self, index: &Id) -> Option<DekScope> {
        let n = u32::from_be_bytes(index[..4].try_into().unwrap());
        self.own_key.contains(&n).then_some(DekScope::PerIndex)
    }
}

struct Rig {
    kms: Arc<TestKms>,
    keys: Arc<KeyManager>,
    sealer: Arc<SlotSealer>,
}

fn keys_over(kms: Arc<TestKms>) -> Arc<KeyManager> {
    Arc::new(
        KeyManager::new(
            Profile::Development,
            kms,
            KeyAllowlist::new(vec![("test".into(), "acct".into())]),
            kref("default"),
            CachePolicy::default(),
            Arc::new(MemoryAuditSink::new(100_000)),
            Arc::new(ManualClock::new()),
        )
        .unwrap(),
    )
}

fn rig_with(dek: DekScope) -> Rig {
    rig_over(dek, Vec::new())
}

fn rig_over(dek: DekScope, own_key: Vec<u32>) -> Rig {
    let kms = Arc::new(TestKms::new());
    kms.create_key(&kref("default")).unwrap();
    kms.create_key(&kref("invoices")).unwrap();
    let keys = keys_over(kms.clone());
    let scopes = Arc::new(Scopes {
        generation: AtomicU64::new(1),
        rotated: AtomicU32::new(0),
        own_key,
    });
    let sealer = Arc::new(SlotSealer::new(keys.clone(), scopes).with_dek_scope(dek));
    Rig { kms, keys, sealer }
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

fn put(index: Id, seq: u64, body: &[u8]) -> RecordSpec<'_> {
    RecordSpec {
        index,
        seq,
        op: RecordOp::Put,
        payload: PayloadRef::Inline(body),
    }
}

fn delete(index: Id, seq: u64) -> RecordSpec<'static> {
    RecordSpec {
        index,
        seq,
        op: RecordOp::Delete,
        payload: PayloadRef::None,
    }
}

/// A lane with members 1..=5000 admitted through the descriptor ceiling of `members`.
async fn lane_over<S: MetadataStore>(store: &S, members: &[Id]) -> LaneId {
    let (domain, group) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    create_lane(store, &layout(), &lane, &policy(), group, 4096, members)
        .await
        .unwrap();
    lane
}

async fn open_sealed<S: MetadataStore>(
    store: Arc<S>,
    lane: LaneId,
    cipher: Arc<dyn SlotCipher>,
) -> LaneWriter<S> {
    LaneWriter::open_sealed(store, layout(), lane, 0, cipher, |_, _| Ok(()))
        .await
        .unwrap()
        .0
}

/// What a replay saw, in order: index, seq, op and the plaintext of a put.
type Seen = Vec<(Id, u64, RecordOp, Vec<u8>)>;

fn flatten(records: &[RecordSpec<'_>]) -> Seen {
    records
        .iter()
        .map(|r| {
            let body = match r.payload {
                PayloadRef::Inline(b) => b.to_vec(),
                _ => Vec::new(),
            };
            (r.index, r.seq, r.op, body)
        })
        .collect()
}

async fn stored(store: &impl MetadataStore, lane: &LaneId, ordinal: u64) -> Vec<u8> {
    let ctx = ctx();
    let k = ObjectKey::new(
        layout()
            .lane_slot(
                &lane.domain,
                lane.incarnation,
                lane.activation,
                lane.lane,
                ordinal,
            )
            .as_bytes(),
    )
    .unwrap();
    let info = store.head(&ctx, &k).await.unwrap();
    store
        .read_range(
            &ctx,
            &k,
            &info.version,
            ByteRange::new(0, info.length).unwrap(),
        )
        .await
        .unwrap()
        .as_bytes()
        .to_vec()
}

fn ctx() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(30),
        Cancellation::default(),
        Budget {
            request_bytes: 64 << 20,
            response_bytes: 64 << 20,
            attempts: 100_000,
        },
    )
    .unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn position(ordinal: u64) -> SlotPosition {
    SlotPosition {
        domain: [3; 16],
        activation: 1,
        lane: 0,
        ordinal,
    }
}

/// Seal `records` at `at` into a slot, then hand back the stored (sealed) records as owned bytes.
fn sealed_payloads(
    cipher: &dyn SlotCipher,
    at: &SlotPosition,
    records: &[RecordSpec<'_>],
) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut payloads = Payloads::default();
    let token = hsearch_format::CommitToken::new([9; 16]).unwrap();
    encode_sealed_slot(cipher, &mut out, &mut payloads, at, token, records).unwrap();
    let view = SlotView::parse_at(&out, at).unwrap();
    view.records()
        .map(|r| match r.payload {
            PayloadRef::Inline(b) => b.to_vec(),
            _ => Vec::new(),
        })
        .collect()
}

/// Open stored `payloads` (index, seq, op from `like`) at `at` the way replay does.
fn try_open(
    cipher: &dyn SlotCipher,
    at: &SlotPosition,
    like: &[RecordSpec<'_>],
    payloads: &[Vec<u8>],
) -> Result<Vec<Vec<u8>>, SealFailure> {
    let stored: Vec<RecordSpec<'_>> = like
        .iter()
        .zip(payloads)
        .map(|(r, p)| RecordSpec {
            payload: PayloadRef::Inline(p),
            ..*r
        })
        .collect();
    let mut out = Payloads::default();
    out.begin(stored.len());
    cipher.open(at, &stored, &mut out)?;
    Ok((0..stored.len())
        .map(|i| out.get(i).map(<[u8]>::to_vec).unwrap_or_default())
        .collect())
}

/// Each test below runs once with one data key per key and once with one per index.
macro_rules! both_scopes {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            use super::*;
            #[test]
            fn per_key() {
                super::$name(DekScope::PerKey)
            }
            #[test]
            fn per_index() {
                super::$name(DekScope::PerIndex)
            }
        }
    )*};
}

both_scopes!(
    a_record_turned_into_another_operation_or_dropped_from_the_tail_is_refused,
    a_shared_key_still_binds_each_record_to_its_index,
    every_acknowledged_slot_is_ciphertext_and_replay_opens_it,
    a_head_driven_open_replays_sealed_slots_too,
    a_tampered_payload_is_refused_whichever_byte_changes,
    a_sealed_payload_moved_to_another_slot_index_position_or_sequence_is_refused,
    a_wrongly_keyed_slot_is_refused_and_a_revoked_key_makes_it_unreadable,
    a_keyed_index_never_reads_plaintext_but_an_unkeyed_one_does,
    no_data_key_or_nonce_repeats_across_slots,
    a_refused_commit_seals_again_under_a_new_key_and_a_lost_reply_does_not,
    an_unknown_index_refuses_the_slot_and_writes_nothing,
    memory_is_bounded_by_the_slot_and_the_sealed_form_must_fit_it
);

// ---------------------------------------------------------------------------- the commit path

fn every_acknowledged_slot_is_ciphertext_and_replay_opens_it(scope: DekScope) {
    let r = rig_with(scope);
    let store = Arc::new(memory());
    let members: Vec<Id> = (1..=4).map(id).collect();
    block_on(async {
        let lane = lane_over(&*store, &members).await;
        let mut w = open_sealed(store.clone(), lane, r.sealer.clone()).await;
        let large = vec![0x5A; 100_000];
        let batches: Vec<Vec<RecordSpec<'_>>> = vec![
            vec![
                put(id(ORDERS), 1, b"order-secret-alpha"),
                put(id(INVOICES), 1, b"invoice-secret-beta"),
                put(id(PUBLIC), 1, b"public-readable-gamma"),
            ],
            vec![
                put(id(ORDERS), 2, b"order-secret-delta"),
                put(id(ORDERS), 3, b"order-secret-epsilon"),
                delete(id(INVOICES), 2),
            ],
            vec![put(id(4), 1, &large)],
        ];
        let mut want = Seen::new();
        for batch in &batches {
            let ack = w.append(batch).await.unwrap();
            assert_eq!(ack.records, batch.len());
            want.extend(flatten(batch));
        }
        // What the store holds: the keyed payloads are not there, the unkeyed one is as given.
        for (ordinal, marker, unkeyed) in [
            (1u64, &b"order-secret-alpha"[..], false),
            (1, b"invoice-secret-beta", false),
            (1, b"public-readable-gamma", true),
            (2, b"order-secret-delta", false),
            (3, &[0x5A; 64][..], false),
        ] {
            let raw = stored(&*store, &lane, ordinal).await;
            assert_eq!(contains(&raw, marker), unkeyed, "slot {ordinal}");
        }
        // The plain log shows sealed bytes; the sealed log shows exactly what was committed.
        let mut plain = Vec::new();
        read_log(&*store, &layout(), &lane, 0, None, |view| {
            if view.kind == SlotKind::Data {
                plain.extend(flatten(&view.records().collect::<Vec<_>>()));
            }
            Ok(())
        })
        .await
        .unwrap();
        assert_ne!(plain, want);
        assert_eq!(plain.len(), want.len());
        let mut opened = Seen::new();
        read_log_sealed(
            &*store,
            &layout(),
            &lane,
            0,
            None,
            &*r.sealer,
            |view, records| {
                if view.kind == SlotKind::Data {
                    opened.extend(flatten(records));
                }
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(opened, want);
        // A reopened writer replays the same plaintext and keeps committing sealed slots.
        drop(w);
        let mut replayed = Seen::new();
        let (mut w2, report) = LaneWriter::open_sealed(
            store.clone(),
            layout(),
            lane,
            0,
            r.sealer.clone(),
            |view, records| {
                if view.kind == SlotKind::Data {
                    replayed.extend(flatten(records));
                }
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(replayed, want);
        assert_eq!(report.records_replayed, want.len() as u64);
        let next = [put(id(ORDERS), 4, b"order-secret-zeta")];
        w2.append(&next).await.unwrap();
        let raw = stored(&*store, &lane, 5).await;
        assert!(!contains(&raw, b"order-secret-zeta"));
    });
    let s = r.sealer.stats();
    assert_eq!((s.slots, s.records), (4, 6));
}

fn a_head_driven_open_replays_sealed_slots_too(scope: DekScope) {
    let r = rig_with(scope);
    let store = Arc::new(memory());
    block_on(async {
        let lane = lane_over(&*store, &[id(ORDERS)]).await;
        create_head(&*store, &layout(), &lane, Durability::Object)
            .await
            .unwrap();
        let (mut w, _) = LaneWriter::open_with_head_sealed(
            store.clone(),
            layout(),
            lane,
            r.sealer.clone(),
            |_, _| Ok(()),
        )
        .await
        .unwrap();
        w.append(&[put(id(ORDERS), 1, b"head driven secret")])
            .await
            .unwrap();
        assert!(!contains(
            &stored(&*store, &lane, 1).await,
            b"head driven secret"
        ));
        drop(w);
        let mut seen = Seen::new();
        LaneWriter::open_with_head_sealed(
            store.clone(),
            layout(),
            lane,
            r.sealer.clone(),
            |view, recs| {
                if view.kind == SlotKind::Data {
                    seen.extend(flatten(recs));
                }
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].3, b"head driven secret");
    });
}

// ------------------------------------------------------------------------------ refusals

fn a_tampered_payload_is_refused_whichever_byte_changes(scope: DekScope) {
    let r = rig_with(scope);
    let at = position(7);
    let like = [
        put(id(ORDERS), 1, b"first-order-payload"),
        put(id(ORDERS), 2, b"second-order-payload"),
    ];
    let sealed = sealed_payloads(&*r.sealer, &at, &like);
    assert_eq!(
        try_open(&*r.sealer, &at, &like, &sealed).unwrap(),
        vec![
            b"first-order-payload".to_vec(),
            b"second-order-payload".to_vec()
        ]
    );
    let mut flips = 0;
    for record in 0..sealed.len() {
        for byte in 0..sealed[record].len() {
            for bit in [0x01u8, 0x80] {
                let mut bad = sealed.clone();
                bad[record][byte] ^= bit;
                let got = try_open(&*r.sealer, &at, &like, &bad);
                assert!(
                    got.is_err(),
                    "record {record} byte {byte} bit {bit:#x} opened"
                );
                flips += 1;
            }
        }
    }
    assert!(flips > 300, "{flips}");
    // Truncated and extended forms are refused as well.
    for record in 0..sealed.len() {
        let mut short = sealed.clone();
        short[record].pop();
        assert!(try_open(&*r.sealer, &at, &like, &short).is_err());
        let mut long = sealed.clone();
        long[record].push(0);
        assert!(try_open(&*r.sealer, &at, &like, &long).is_err());
        let mut empty = sealed.clone();
        empty[record].clear();
        assert!(try_open(&*r.sealer, &at, &like, &empty).is_err());
    }
}

fn a_sealed_payload_moved_to_another_slot_index_position_or_sequence_is_refused(scope: DekScope) {
    let r = rig_with(scope);
    let at = position(7);
    let like = [
        put(id(ORDERS), 1, b"payload-one"),
        put(id(ORDERS), 2, b"payload-two"),
        put(id(INVOICES), 1, b"payload-three"),
    ];
    let sealed = sealed_payloads(&*r.sealer, &at, &like);
    assert!(try_open(&*r.sealer, &at, &like, &sealed).is_ok());
    // Another ordinal, lane, activation or domain.
    for other in [
        SlotPosition { ordinal: 8, ..at },
        SlotPosition { lane: 1, ..at },
        SlotPosition {
            activation: 2,
            ..at
        },
        SlotPosition {
            domain: [4; 16],
            ..at
        },
    ] {
        assert!(
            try_open(&*r.sealer, &other, &like, &sealed).is_err(),
            "{other:?}"
        );
    }
    // Another sequence number on the record.
    let mut resequenced = like;
    resequenced[1].seq = 9;
    assert!(try_open(&*r.sealer, &at, &resequenced, &sealed).is_err());
    // Records swapped within the slot (same index, different position).
    let swapped = vec![sealed[1].clone(), sealed[0].clone(), sealed[2].clone()];
    assert!(try_open(&*r.sealer, &at, &like, &swapped).is_err());
    // A payload sealed for one index presented under another.
    let mut other_index = like;
    other_index[2].index = id(ORDERS);
    assert!(try_open(&*r.sealer, &at, &other_index, &sealed).is_err());
    let mut third = like;
    third[0].index = id(INVOICES);
    assert!(try_open(&*r.sealer, &at, &third, &sealed).is_err());
    // A record that lost its wrapper (the first of its index) and one that gained a second.
    let mut headless = sealed.clone();
    headless[0] = sealed[1].clone();
    assert!(try_open(&*r.sealer, &at, &like, &headless).is_err());
    let mut twice = sealed.clone();
    twice[1] = sealed[0].clone();
    assert!(try_open(&*r.sealer, &at, &like, &twice).is_err());
}

fn a_record_turned_into_another_operation_or_dropped_from_the_tail_is_refused(scope: DekScope) {
    let r = rig_with(scope);
    let at = position(2);
    let like = [
        put(id(ORDERS), 1, b"first"),
        put(id(ORDERS), 2, b"second"),
        put(id(ORDERS), 3, b"third"),
    ];
    let sealed = sealed_payloads(&*r.sealer, &at, &like);
    assert!(try_open(&*r.sealer, &at, &like, &sealed).is_ok());
    let open_with = |records: Vec<RecordSpec<'_>>| {
        let mut out = Payloads::default();
        out.begin(records.len());
        r.sealer.open(&at, &records, &mut out)
    };
    let stored = |i: usize| RecordSpec {
        payload: PayloadRef::Inline(&sealed[i]),
        ..like[i]
    };
    assert!(open_with(vec![stored(0), stored(1), stored(2)]).is_ok());
    // The tail dropped: every remaining record is individually valid and still refused.
    assert!(open_with(vec![stored(0), stored(1)]).is_err());
    // A sealed put rewritten as a delete: the other sealed records stop opening.
    let deleted = RecordSpec {
        op: RecordOp::Delete,
        payload: PayloadRef::None,
        ..like[1]
    };
    assert!(open_with(vec![stored(0), deleted, stored(2)]).is_err());
    // A sealed put swapped for an external payload record.
    let external = RecordSpec {
        payload: PayloadRef::External {
            object: [1; 16],
            length: 5,
            digest: [2; 32],
        },
        ..like[1]
    };
    assert!(open_with(vec![stored(0), external, stored(2)]).is_err());
}

fn a_shared_key_still_binds_each_record_to_its_index(scope: DekScope) {
    let r = rig_with(scope);
    let at = position(2);
    // Tenants 4, 5 and 6 are on one key; under `PerKey` they share one data key in this slot.
    let like = [
        put(id(4), 1, b"tenant four"),
        put(id(5), 1, b"tenant five"),
        put(id(6), 1, b"tenant six"),
    ];
    let sealed = sealed_payloads(&*r.sealer, &at, &like);
    assert!(try_open(&*r.sealer, &at, &like, &sealed).is_ok());
    for (record, wrong) in [(1usize, 4u32), (1, 6), (2, 4), (2, 5)] {
        let mut as_other = like;
        as_other[record].index = id(wrong);
        assert!(
            try_open(&*r.sealer, &at, &as_other, &sealed).is_err(),
            "record {record} read as tenant {wrong}"
        );
    }
}

fn a_wrongly_keyed_slot_is_refused_and_a_revoked_key_makes_it_unreadable(scope: DekScope) {
    let r = rig_with(scope);
    let at = position(11);
    let like = [
        put(id(INVOICES), 1, b"invoice body"),
        put(id(ORDERS), 1, b"order body"),
    ];
    let sealed = sealed_payloads(&*r.sealer, &at, &like);

    // Another deployment: same key names, different key material. The wrapped key does not unwrap.
    let other_kms = Arc::new(TestKms::new());
    other_kms.create_key(&kref("default")).unwrap();
    other_kms.create_key(&kref("invoices")).unwrap();
    let other = SlotSealer::new(
        keys_over(other_kms),
        Arc::new(Scopes {
            generation: AtomicU64::new(1),
            rotated: AtomicU32::new(0),
            own_key: Vec::new(),
        }),
    );
    assert_eq!(
        try_open(&other, &at, &like, &sealed),
        Err(SealFailure::Corrupt)
    );

    // The owner revokes the invoice key and the node is told: the invoice payload cannot be read,
    // and a slot whose first record is refused is refused whole.
    assert!(try_open(&*r.sealer, &at, &like, &sealed).is_ok());
    r.kms.revoke(&kref("invoices"));
    r.keys
        .invalidate_index(&IndexId::new("invoices").unwrap(), 1);
    assert_eq!(
        try_open(&*r.sealer, &at, &like, &sealed),
        Err(SealFailure::Denied)
    );
}

fn a_keyed_index_never_reads_plaintext_but_an_unkeyed_one_does(scope: DekScope) {
    let r = rig_with(scope);
    let store = Arc::new(memory());
    block_on(async {
        let members = [id(ORDERS), id(PUBLIC)];
        let lane = lane_over(&*store, &members).await;
        // A writer with no cipher stores both as given.
        let (mut plain, _) = LaneWriter::open(store.clone(), layout(), lane, 0, |_| Ok(()))
            .await
            .unwrap();
        plain
            .append(&[put(id(PUBLIC), 1, b"public body")])
            .await
            .unwrap();
        plain
            .append(&[put(id(ORDERS), 1, b"not sealed at all")])
            .await
            .unwrap();
        drop(plain);
        // Sealed replay: the unkeyed slot reads, the keyed index's unsealed payload is refused.
        let first = read_log_sealed(
            &*store,
            &layout(),
            &lane,
            0,
            Some(2),
            &*r.sealer,
            |view, recs| {
                if view.kind == SlotKind::Data {
                    assert_eq!(flatten(recs)[0].3, b"public body");
                }
                Ok(())
            },
        )
        .await;
        assert!(first.is_ok());
        let second = read_log_sealed(
            &*store,
            &layout(),
            &lane,
            0,
            None,
            &*r.sealer,
            |_, _| Ok(()),
        )
        .await;
        assert_eq!(second, Err(LaneError::Seal(SealFailure::Corrupt)));
        let reopen = LaneWriter::open_sealed(
            store.clone(),
            layout(),
            lane,
            0,
            r.sealer.clone(),
            |_, _| Ok(()),
        )
        .await;
        assert!(matches!(reopen, Err(LaneError::Seal(SealFailure::Corrupt))));
    });
}

fn an_unknown_index_refuses_the_slot_and_writes_nothing(scope: DekScope) {
    let r = rig_with(scope);
    let store = Arc::new(CountingStore::new(memory()));
    block_on(async {
        let lane = lane_over(&*store, &[id(ORDERS), id(9_000_000)]).await;
        let mut w = open_sealed(store.clone(), lane, r.sealer.clone()).await;
        let before = store.requests();
        let err = w
            .append(&[
                put(id(ORDERS), 1, b"fine"),
                put(id(9_000_000), 1, b"nobody"),
            ])
            .await;
        assert_eq!(err, Err(LaneError::Seal(SealFailure::Denied)));
        assert_eq!(store.requests().since(&before).create, 0);
        assert!(!w.is_poisoned());
        w.append(&[put(id(ORDERS), 1, b"fine")]).await.unwrap();
    });
}

// ---------------------------------------------------------------- keys and nonces never repeat

/// The wrapper bytes carried by a first sealed payload.
fn wrapper_of(payload: &[u8]) -> &[u8] {
    assert_eq!(payload[0] & 0x02, 0x02, "first payload carries the wrapper");
    let len = |at: usize| usize::from(u16::from_le_bytes([payload[at], payload[at + 1]]));
    &payload[7..7 + len(1) + len(3) + len(5)]
}

fn no_data_key_or_nonce_repeats_across_slots(scope: DekScope) {
    let r = rig_with(scope);
    let like = [
        put(id(ORDERS), 1, b"same plaintext"),
        put(id(ORDERS), 2, b"same plaintext"),
    ];
    let mut wrappers = BTreeSet::new();
    let mut ciphertexts = BTreeSet::new();
    let mut keyed_nonce = BTreeSet::new();
    let mut sealed_total = 0;
    // Distinct ordinals, and the same ordinal sealed repeatedly (a refused commit seals again).
    for ordinal in (0..120u64).chain(std::iter::repeat_n(5, 120)) {
        let at = position(ordinal);
        let sealed = sealed_payloads(&*r.sealer, &at, &like);
        let wrapper = wrapper_of(&sealed[0]).to_vec();
        // A (key, nonce) pair: the wrapped key stands for the data key, the record position for the nonce.
        for (position, payload) in sealed.iter().enumerate() {
            let body = if position == 0 {
                &payload[7 + wrapper.len()..]
            } else {
                &payload[1..]
            };
            // The ciphertext without its tag: equal plaintext under one (key, nonce) repeats it.
            assert!(
                ciphertexts.insert(body[..body.len() - 16].to_vec()),
                "a ciphertext repeated"
            );
            assert!(
                keyed_nonce.insert((wrapper.clone(), position)),
                "a (key, nonce) pair repeated"
            );
        }
        assert!(wrappers.insert(wrapper), "a wrapped data key repeated");
        sealed_total += 1;
    }
    assert_eq!(wrappers.len(), sealed_total);
    assert_eq!(r.kms.wrap_calls(), 240, "one wrap per slot sealed");
}

fn a_refused_commit_seals_again_under_a_new_key_and_a_lost_reply_does_not(scope: DekScope) {
    let r = rig_with(scope);
    let inner = Arc::new(memory());
    let store = Arc::new(Lossy::new(inner));
    block_on(async {
        let lane = lane_over(&*store, &[id(ORDERS)]).await;
        let mut w = open_sealed(store.clone(), lane, r.sealer.clone()).await;
        let batch = [put(id(ORDERS), 1, b"retried secret")];
        // Certainly refused: nothing written, the writer is usable, the repeat is a new sealing.
        store.refuse_next.store(true, Ordering::SeqCst);
        let wraps = r.kms.wrap_calls();
        assert_eq!(
            w.append(&batch).await,
            Err(LaneError::Store(StoreError::Unavailable))
        );
        assert!(!w.is_poisoned());
        w.append(&batch).await.unwrap();
        assert_eq!(
            r.kms.wrap_calls() - wraps,
            2,
            "the repeat sealed under a fresh key"
        );
        // A lost reply that landed is resolved to our slot by its identity: one sealing, one slot.
        store.lose_next_reply.store(true, Ordering::SeqCst);
        let wraps = r.kms.wrap_calls();
        let ack = w
            .append(&[put(id(ORDERS), 2, b"lost reply secret")])
            .await
            .unwrap();
        assert_eq!(ack.ordinal, 2);
        assert_eq!(r.kms.wrap_calls() - wraps, 1);
        assert_eq!(
            store
                .created
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, b)| contains(b, b"lost"))
                .count(),
            0
        );
        let mut seen = Seen::new();
        read_log_sealed(
            &*store,
            &layout(),
            &lane,
            0,
            None,
            &*r.sealer,
            |view, recs| {
                if view.kind == SlotKind::Data {
                    seen.extend(flatten(recs));
                }
                Ok(())
            },
        )
        .await
        .unwrap();
        let bodies: Vec<_> = seen.iter().map(|s| s.3.clone()).collect();
        assert_eq!(
            bodies,
            vec![b"retried secret".to_vec(), b"lost reply secret".to_vec()]
        );
    });
}

// ---------------------------------------------------------------------------------- cost

#[test]
fn sealing_adds_no_request_and_the_wraps_follow_the_dek_scope() {
    for (dek, own_key, wraps) in [
        // hot (one index), sparse (50 indexes, one key), unkeyed.
        (DekScope::PerKey, vec![], [1u64, 1, 0]),
        (DekScope::PerIndex, vec![], [1, 50, 0]),
        // Ten of the fifty choose a key of their own: ten wraps plus one for the rest.
        (DekScope::PerKey, (10..20).collect(), [1, 11, 0]),
    ] {
        let r = rig_over(dek, own_key);
        let plain_store = Arc::new(CountingStore::new(memory()));
        let sealed_store = Arc::new(CountingStore::new(memory()));
        let members: Vec<Id> = (1..=60).map(id).collect();
        block_on(async {
            let pl = lane_over(&*plain_store, &members).await;
            let sl = lane_over(&*sealed_store, &members).await;
            let (mut plain, _) = LaneWriter::open(plain_store.clone(), layout(), pl, 0, |_| Ok(()))
                .await
                .unwrap();
            let mut sealed = open_sealed(sealed_store.clone(), sl, r.sealer.clone()).await;
            let body = vec![0x42u8; 1024];
            let hot: Vec<RecordSpec<'_>> = (1..=100).map(|s| put(id(ORDERS), s, &body)).collect();
            let sparse: Vec<RecordSpec<'_>> = (10..60).map(|n| put(id(n), 1, &body)).collect();
            let unkeyed = [put(id(PUBLIC), 1, &body)];
            for (batch, wraps) in [
                (&hot[..], wraps[0]),
                (&sparse[..], wraps[1]),
                (&unkeyed[..], wraps[2]),
            ] {
                let (p0, s0) = (plain_store.requests(), sealed_store.requests());
                let w0 = r.kms.wrap_calls();
                plain.append(batch).await.unwrap();
                sealed.append(batch).await.unwrap();
                let (dp, ds) = (
                    plain_store.requests().since(&p0),
                    sealed_store.requests().since(&s0),
                );
                assert_eq!(
                    (dp.create, dp.reads(), dp.cas, dp.resolve),
                    (ds.create, ds.reads(), ds.cas, ds.resolve),
                    "sealing changed the request count"
                );
                assert_eq!((ds.create, ds.reads()), (1, 0));
                assert_eq!(r.kms.wrap_calls() - w0, wraps, "{dek:?}");
            }
        });
        assert_eq!(
            r.kms.unwrap_calls(),
            0,
            "the writer never asks the provider to unwrap"
        );
    }
}

#[test]
fn a_cold_replay_costs_one_provider_unwrap_per_data_key_per_slot() {
    let r = rig_with(DekScope::PerKey);
    let store = Arc::new(memory());
    block_on(async {
        let members: Vec<Id> = (4..9).map(id).collect();
        let lane = lane_over(&*store, &members).await;
        let mut w = open_sealed(store.clone(), lane, r.sealer.clone()).await;
        let bodies = [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()];
        for slot in 0..5u64 {
            // Three tenants on one key in each slot: one data key, so one unwrap on replay.
            let batch: Vec<RecordSpec<'_>> = (0..3)
                .map(|i| put(id(4 + i), slot + 1, &bodies[i as usize]))
                .collect();
            w.append(&batch).await.unwrap();
        }
        assert_eq!(r.kms.unwrap_calls(), 0, "the writer never unwraps");
        // A node that has not seen these keys: a new key manager over the same provider.
        let cold = SlotSealer::new(
            keys_over(r.kms.clone()),
            Arc::new(Scopes {
                generation: AtomicU64::new(1),
                rotated: AtomicU32::new(0),
                own_key: Vec::new(),
            }),
        );
        let mut records = 0;
        read_log_sealed(&*store, &layout(), &lane, 0, None, &cold, |_, recs| {
            records += recs.len();
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(records, 15);
        assert_eq!(
            r.kms.unwrap_calls(),
            5,
            "one unwrap per slot, however many tenants it holds"
        );
    });
}

#[test]
fn a_lane_may_hold_slots_of_both_scopes_and_replay_honours_what_each_recorded() {
    let per_key = rig_with(DekScope::PerKey);
    // The same provider and key manager, a sealer that would now choose the other scope.
    let per_index = Arc::new(
        SlotSealer::new(
            per_key.keys.clone(),
            Arc::new(Scopes {
                generation: AtomicU64::new(1),
                rotated: AtomicU32::new(0),
                own_key: Vec::new(),
            }),
        )
        .with_dek_scope(DekScope::PerIndex),
    );
    let store = Arc::new(memory());
    block_on(async {
        let members: Vec<Id> = (4..8).map(id).collect();
        let lane = lane_over(&*store, &members).await;
        let batch = [
            put(id(4), 1, b"tenant four"),
            put(id(5), 1, b"tenant five"),
            put(id(4), 2, b"tenant four again"),
        ];
        let mut w = open_sealed(store.clone(), lane, per_key.sealer.clone()).await;
        w.append(&batch).await.unwrap();
        drop(w);
        let mut w = open_sealed(store.clone(), lane, per_index.clone()).await;
        w.append(&batch).await.unwrap();
        drop(w);
        for cipher in [&*per_key.sealer, &*per_index] {
            let mut seen = Seen::new();
            read_log_sealed(&*store, &layout(), &lane, 0, None, cipher, |view, recs| {
                if view.kind == SlotKind::Data {
                    seen.extend(flatten(recs));
                }
                Ok(())
            })
            .await
            .unwrap();
            assert_eq!(seen.len(), 6);
            assert_eq!(seen[1].3, b"tenant five");
            assert_eq!(seen[5].3, b"tenant four again");
        }
    });
}

#[test]
fn an_index_that_rotates_its_policy_generation_still_opens_an_older_slot() {
    let kms = Arc::new(TestKms::new());
    kms.create_key(&kref("default")).unwrap();
    let keys = keys_over(kms);
    let scopes = Arc::new(Scopes {
        generation: AtomicU64::new(1),
        rotated: AtomicU32::new(0),
        own_key: Vec::new(),
    });
    let sealer = SlotSealer::new(keys, scopes.clone());
    let at = position(3);
    let like = [put(id(4), 1, b"first"), put(id(5), 1, b"second")];
    let sealed = sealed_payloads(&sealer, &at, &like);
    // Both indexes were at generation one; now tenant four's policy generation moves and five's does not.
    scopes.rotated.store(4, Ordering::SeqCst);
    assert_eq!(
        try_open(&sealer, &at, &like, &sealed).unwrap(),
        vec![b"first".to_vec(), b"second".to_vec()]
    );
}

fn memory_is_bounded_by_the_slot_and_the_sealed_form_must_fit_it(scope: DekScope) {
    let r = rig_with(scope);
    let store = Arc::new(CountingStore::new(memory()));
    block_on(async {
        let lane = lane_over(&*store, &[id(ORDERS), id(PUBLIC)]).await;
        let mut w = open_sealed(store.clone(), lane, r.sealer.clone()).await;
        // A small slot keeps a small buffer; a large one is not kept for life.
        w.append(&[put(id(ORDERS), 1, &[1u8; 4096])]).await.unwrap();
        assert!(
            w.retained_bytes() <= 2 * hsearch_lane::sealing::RETAIN,
            "{}",
            w.retained_bytes()
        );
        let big = vec![2u8; MAX_INLINE_RECORD_BYTES - 512];
        let batch: Vec<RecordSpec<'_>> = (2..6).map(|s| put(id(ORDERS), s, &big)).collect();
        w.append(&batch).await.unwrap();
        assert_eq!(w.retained_bytes(), 0, "a 4 MiB slot's buffers are dropped");

        // A record that fits the inline bound only in the clear is refused, nothing written.
        let before = store.requests();
        let exact = vec![3u8; MAX_INLINE_RECORD_BYTES];
        let refused = w.append(&[put(id(ORDERS), 6, &exact)]).await;
        assert!(matches!(refused, Err(LaneError::Format(_))), "{refused:?}");
        assert_eq!(store.requests().since(&before).create, 0);
        assert!(!w.is_poisoned());
        // The same record on an unkeyed index is accepted: the bound is the sealed form's.
        w.append(&[put(id(PUBLIC), 1, &exact)]).await.unwrap();

        // A slot that fits the 8 MiB body bound in the clear and not once each payload carries its
        // flag byte and tag: refused whole and writing nothing; the writer then commits half of it.
        let small = vec![4u8; 2010];
        let many: Vec<RecordSpec<'_>> = (0..4096)
            .map(|s| put(id(PUBLIC), 100 + s, &small))
            .collect();
        w.append(&many).await.unwrap();
        let many: Vec<RecordSpec<'_>> = (0..4096)
            .map(|s| put(id(ORDERS), 100 + s, &small))
            .collect();
        let before = store.requests();
        let refused = w.append(&many).await;
        assert!(matches!(refused, Err(LaneError::Format(_))), "{refused:?}");
        assert_eq!(store.requests().since(&before).create, 0);
        w.append(&many[..2048]).await.unwrap();
    });
}

// ------------------------------------------------------------------------------ test store

/// A store that can refuse the next create before writing it, or write it and lose the reply.
struct Lossy {
    inner: Arc<MemoryStore>,
    refuse_next: AtomicBool,
    lose_next_reply: AtomicBool,
    created: std::sync::Mutex<Vec<(Vec<u8>, Vec<u8>)>>,
}

impl Lossy {
    fn new(inner: Arc<MemoryStore>) -> Self {
        Self {
            inner,
            refuse_next: AtomicBool::new(false),
            lose_next_reply: AtomicBool::new(false),
            created: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl ObjectBackend for Lossy {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.inner.head(c, k)
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            if self.refuse_next.swap(false, Ordering::SeqCst) {
                return Err(StoreError::Unavailable);
            }
            let receipt = self.inner.create(c, r).await?;
            // The plaintext of a lost-reply slot is never in what the store was sent.
            self.created
                .lock()
                .unwrap()
                .push((r.key.as_bytes().to_vec(), r.payload.as_bytes().to_vec()));
            if self.lose_next_reply.swap(false, Ordering::SeqCst) {
                return Err(StoreError::UnknownOutcome);
            }
            Ok(receipt)
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

impl MetadataStore for Lossy {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.inner.compare_exchange(c, r)
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
