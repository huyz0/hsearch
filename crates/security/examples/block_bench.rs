//! Encrypted-block throughput (M05.03): `cargo run --release -p hsearch-security --all-features --example block_bench`.
//! Numbers from a shared 4-vCPU VM are provisional and relative, never absolute.

use std::time::Instant;

use hsearch_security::block::{HEAD_LEN, Header, seal_object};
use hsearch_security::keys::{KeyRef, KeyVersion, SecretKey, WrappedKey};

fn main() {
    let dek = SecretKey::generate().unwrap();
    let wrapped = WrappedKey::new(
        KeyRef::new("test/acct/bench").unwrap(),
        KeyVersion::new("v1").unwrap(),
        vec![7; 96],
    )
    .unwrap();
    let plain = vec![0xA5u8; 64 << 20];
    println!("| Block size | Seal MB/s | Open all MB/s | Open one block (us) | Overhead |");
    println!("|---|---|---|---|---|");
    for block in [4u32 << 10, 64 << 10, 1 << 20] {
        let start = Instant::now();
        let sealed = seal_object(&dek, &wrapped, [1; 16], [2; 16], block, &plain).unwrap();
        let seal = plain.len() as f64 / start.elapsed().as_secs_f64() / 1e6;
        let total = Header::total_len(&sealed[..HEAD_LEN]).unwrap();
        let opener = Header::parse(&sealed[..total])
            .unwrap()
            .open(&dek, [1; 16], [2; 16])
            .unwrap();
        let start = Instant::now();
        let back = opener.open_all(&sealed).unwrap();
        let open = plain.len() as f64 / start.elapsed().as_secs_f64() / 1e6;
        assert_eq!(back.len(), plain.len());
        let n = 2_000u64;
        let range = opener.block_range(3).unwrap();
        let ct = &sealed[range.start as usize..range.end as usize];
        let start = Instant::now();
        for _ in 0..n {
            std::hint::black_box(opener.open_block(3, ct).unwrap());
        }
        let one = start.elapsed().as_secs_f64() / n as f64 * 1e6;
        let overhead = (sealed.len() - plain.len()) as f64 / plain.len() as f64 * 100.0;
        println!(
            "| {} KiB | {seal:.0} | {open:.0} | {one:.1} | {overhead:.3}% |",
            block >> 10
        );
    }
}
