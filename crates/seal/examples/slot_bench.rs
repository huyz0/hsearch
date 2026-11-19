//! CPU cost of sealing commit slots (M06.09): `cargo run --release -p hsearch-seal --example slot_bench`.
//!
//! Seals and opens whole slots through [`SlotSealer`] against the in-memory test KMS, so wrap and unwrap
//! cost is an in-process call, **not** a network KMS round trip: it isolates CPU and allocation, not
//! latency. Each row is the median of seven runs of a fixed amount of plaintext; "plain" is the same
//! slot encoded without sealing. Provisional and relative: a shared VM, one architecture, and whatever
//! AES backend the build selected (printed first).
use std::sync::Arc;
use std::time::Instant;

use hsearch_format::{
    CommitToken, Id, PayloadRef, RecordOp, RecordSpec, SlotBody, SlotPosition, SlotView,
    encode_slot,
};
use hsearch_lane::SealFailure;
use hsearch_lane::sealing::{Payloads, SlotCipher, encode_sealed_slot, open_records};
use hsearch_seal::{DekScope, Scope, SlotScopes, SlotSealer};
use hsearch_security::audit::MemoryAuditSink;
use hsearch_security::clock::SystemClock;
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

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    // Plaintext sealed per timed run, MiB; `SLOT_BENCH_MB=16` for a build on the slow software AES path.
    let budget_mib: f64 = std::env::var("SLOT_BENCH_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256.0);
    let report = hsearch_security::backend::report();
    println!("AES backend: {report:?}\n");
    let kms = Arc::new(TestKms::new());
    let key = KeyRef::new("test/acct/default").unwrap();
    kms.create_key(&key).unwrap();
    let keys = Arc::new(
        KeyManager::new(
            Profile::Development,
            kms,
            KeyAllowlist::new(vec![("test".into(), "acct".into())]),
            key,
            CachePolicy::default(),
            Arc::new(MemoryAuditSink::new(1_000)),
            Arc::new(SystemClock::default()),
        )
        .unwrap(),
    );
    let sealers = [DekScope::PerKey, DekScope::PerIndex].map(|dek| {
        (
            dek,
            Arc::new(SlotSealer::new(keys.clone(), Arc::new(AllKeyed)).with_dek_scope(dek)),
        )
    });
    println!(
        "| shape | data key | plaintext per slot | plain encode MiB/s | seal MiB/s | open MiB/s | seal CPU ms per MiB | seal over plain | wraps per slot |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
    // (name, records, distinct indexes, bytes per record)
    for (name, records, indexes, bytes) in [
        ("hot, 1 KiB x 250", 250usize, 1u32, 1024usize),
        ("hot, 64 KiB x 62", 62, 1, 64 * 1024),
        ("hot, 1 MiB - 1 KiB x 4", 4, 1, 1024 * 1023),
        ("warm, 20 indexes, 1 KiB x 250", 250, 20, 1024),
        ("sparse, 250 indexes, 1 KiB x 250", 250, 250, 1024),
        ("tiny, 1 index, 100 B x 4096", 4096, 1, 100),
    ] {
        for (dek, sealer) in &sealers {
            let docs: Vec<Vec<u8>> = (0..records).map(|i| vec![(i % 251) as u8; bytes]).collect();
            let specs: Vec<RecordSpec<'_>> = docs
                .iter()
                .enumerate()
                .map(|(i, d)| RecordSpec {
                    index: id(i as u32 % indexes + 1),
                    seq: i as u64 + 1,
                    op: RecordOp::Put,
                    payload: PayloadRef::Inline(d),
                })
                .collect();
            let plain_bytes = (records * bytes) as f64;
            let rounds = ((budget_mib * 1024.0 * 1024.0) / plain_bytes)
                .ceil()
                .max(2.0) as usize;
            let token = CommitToken::new([5; 16]).unwrap();
            let mut out = Vec::new();
            let mut payloads = Payloads::default();
            let (mut plain_t, mut seal_t, mut open_t) = (Vec::new(), Vec::new(), Vec::new());
            let wraps0 = sealer.stats().wraps;
            for _ in 0..7 {
                let at = |n: u64| SlotPosition {
                    domain: [3; 16],
                    activation: 1,
                    lane: 0,
                    ordinal: n,
                };
                let t = Instant::now();
                for n in 0..rounds {
                    out.clear();
                    encode_slot(&mut out, &at(n as u64), token, &SlotBody::Data(&specs)).unwrap();
                }
                plain_t.push(t.elapsed().as_secs_f64() / rounds as f64);
                let t = Instant::now();
                for n in 0..rounds {
                    out.clear();
                    encode_sealed_slot(
                        &**sealer,
                        &mut out,
                        &mut payloads,
                        &at(n as u64),
                        token,
                        &specs,
                    )
                    .unwrap();
                }
                seal_t.push(t.elapsed().as_secs_f64() / rounds as f64);
                // Open the last sealed slot over and over (the data key is warm in the key manager).
                let view = SlotView::parse_at(&out, &at(rounds as u64 - 1)).unwrap();
                let mut scratch = Payloads::default();
                let t = Instant::now();
                for _ in 0..rounds {
                    std::hint::black_box(
                        open_records(&**sealer as &dyn SlotCipher, &view, &mut scratch).unwrap(),
                    );
                }
                open_t.push(t.elapsed().as_secs_f64() / rounds as f64);
            }
            let (plain, seal, open) = (median(plain_t), median(seal_t), median(open_t));
            let mib = plain_bytes / (1024.0 * 1024.0);
            let wraps = (sealer.stats().wraps - wraps0) as f64 / (7 * rounds) as f64;
            println!(
                "| {name} | {dek:?} | {:.0} KiB | {:.0} | {:.0} | {:.0} | {:.2} | {:.2}x | {:.0} |",
                plain_bytes / 1024.0,
                mib / plain,
                mib / seal,
                mib / open,
                seal * 1000.0 / mib,
                seal / plain,
                wraps
            );
        }
    }
}
