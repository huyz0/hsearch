//! Hot-path cost of the formats: checksum throughput, slot encode and validate, header parse,
//! manifest page decode, mapping hash. Single thread, release build; numbers are `provisional`
//! (relative, on whatever machine runs it). Each cell is the median of 7 runs with its
//! min-max range; a range above 25% is printed as unstable.
//!
//!     cargo run --release -p hsearch-format --example bench
use hsearch_format::*;
use std::hint::black_box;
use std::time::Instant;

fn id(n: u8) -> Id {
    [n; 16]
}

fn cell(name: &str, unit_bytes: usize, iters: usize, mut f: impl FnMut()) {
    let mut ns = Vec::new();
    f();
    for _ in 0..7 {
        let t = Instant::now();
        for _ in 0..iters {
            f();
        }
        ns.push(t.elapsed().as_nanos() as f64 / iters as f64);
    }
    ns.sort_by(f64::total_cmp);
    let (min, med, max) = (ns[0], ns[3], ns[6]);
    let spread = (max - min) / med * 100.0;
    let rate = if unit_bytes > 0 {
        format!("{:8.1} MiB/s", unit_bytes as f64 / med * 1e9 / 1048576.0)
    } else {
        String::new()
    };
    let flag = if spread > 25.0 { " UNSTABLE" } else { "" };
    println!(
        "{name:<44} {med:>12.0} ns/op {rate:>14}  range {min:.0}..{max:.0} ({spread:.0}%){flag}"
    );
}

fn main() {
    println!("hsearch-format bench (provisional; single thread)");
    for &n in &[64usize, 4096, 1 << 20, 8 << 20] {
        let data: Vec<u8> = (0..n).map(|i| (i * 31 + 7) as u8).collect();
        let iters = (64 << 20) / n.max(4096);
        cell(&format!("crc32c slicing-by-16, {n} B"), n, iters, || {
            black_box(crc32c(black_box(&data)));
        });
        cell(
            &format!("crc32c bytewise (reference), {n} B"),
            n,
            iters,
            || {
                black_box(crc32c_bytewise(black_box(&data)));
            },
        );
    }

    let token = CommitToken::new([1; 16]).unwrap();
    let at = SlotPosition {
        domain: id(1),
        activation: 1,
        lane: 0,
        ordinal: 7,
    };
    let doc = vec![b'x'; 512];
    let recs: Vec<_> = (0..1000u64)
        .map(|i| RecordSpec {
            index: id((i % 200) as u8 + 1),
            seq: i,
            op: RecordOp::Put,
            payload: PayloadRef::Inline(&doc),
        })
        .collect();
    let mut buf = Vec::with_capacity(1 << 20);
    encode_slot(&mut buf, &at, token, &SlotBody::Data(&recs)).unwrap();
    let slot_len = buf.len();
    println!("slot of 1000 x 512 B records: {slot_len} bytes");
    cell(
        "slot encode 1000 records (reused buffer)",
        slot_len,
        200,
        || {
            buf.clear();
            encode_slot(&mut buf, &at, token, &SlotBody::Data(&recs)).unwrap();
        },
    );
    let bytes = buf.clone();
    cell("header parse (40 B)", 0, 2_000_000, || {
        black_box(Header::parse(black_box(&bytes)).unwrap());
    });
    cell(
        "slot validate + iterate 1000 records",
        slot_len,
        200,
        || {
            let v = SlotView::parse(black_box(&bytes)).unwrap();
            let mut n = 0u64;
            for r in v.records() {
                n += r.seq;
            }
            black_box(n);
        },
    );
    cell("aad (72 B, no allocation)", 0, 5_000_000, || {
        black_box(aad(black_box(&id(1)), 1, 5, 2, 3, black_box(&id(4))));
    });

    let page = ManifestPage {
        entries: (0..MAX_PAGE_ENTRIES)
            .map(|i| FileEntry {
                name: format!("_{i:05}.cfs"),
                object: id(1),
                length: 1 << 20,
                digest: [3; 32],
            })
            .collect(),
    };
    let mut pb = Vec::new();
    page.encode(&mut pb).unwrap();
    println!(
        "manifest page of {MAX_PAGE_ENTRIES} entries: {} bytes",
        pb.len()
    );
    cell(
        "manifest page decode 2048 entries (allocates names)",
        pb.len(),
        100,
        || {
            black_box(ManifestPage::decode(black_box(&pb)).unwrap());
        },
    );

    let canon = vec![b'm'; 64 * 1024];
    let obj = MappingObject::new(&canon).unwrap();
    let mut mb = Vec::new();
    obj.encode(&mut mb).unwrap();
    cell(
        "mapping decode + SHA-256 verify, 64 KiB",
        canon.len(),
        200,
        || {
            black_box(MappingObject::decode(black_box(&mb)).unwrap());
        },
    );
}
