//! M05.07 adversarial corpus for the block-format reader. Deterministic (a fixed-seed generator, no external
//! corpus files), so a failure reproduces from its seed and index.
//!
//! The invariant for every input: the reader never panics, never allocates beyond the format's published
//! header bound before authentication, and never returns plaintext unless the bytes are exactly a sealed
//! object (compared to the original plaintext).

use hsearch_security::block::{HEAD_LEN, Header, MAX_HEADER_LEN, PREFIX_LEN, seal_object};
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{CallOptions, KeyContext, KeyProvider, KeyRef, SecretKey};
use hsearch_security::test_kms::TestKms;

const OBJECT: [u8; 16] = [0xA1; 16];
const DOMAIN: [u8; 16] = [0xD0; 16];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

struct Fixture {
    kms: TestKms,
    plain: Vec<u8>,
    object: Vec<u8>,
}

fn fixture() -> Fixture {
    let kms = TestKms::new();
    let key = KeyRef::new("test/acct/a").unwrap();
    kms.create_key(&key).unwrap();
    let ctx = KeyContext {
        index: IndexId::new("orders").unwrap(),
        domain: DOMAIN,
        object: OBJECT,
    };
    let dek = SecretKey::generate().unwrap();
    let wrapped = kms.wrap(&key, &ctx, &dek, &CallOptions::default()).unwrap();
    let plain: Vec<u8> = (0..5000).map(|i| (i * 13 + 1) as u8).collect();
    let object = seal_object(&dek, &wrapped, OBJECT, DOMAIN, 1024, &plain).unwrap();
    Fixture { kms, plain, object }
}

/// The whole read path. `Some(plaintext)` only when every authentication step passed.
fn read(f: &Fixture, bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < HEAD_LEN {
        return None;
    }
    let total = Header::total_len(&bytes[..HEAD_LEN]).ok()?;
    assert!(total <= MAX_HEADER_LEN, "header bound exceeded: {total}");
    if bytes.len() < total {
        return None;
    }
    let header = Header::parse(&bytes[..total]).ok()?;
    let ctx = KeyContext {
        index: IndexId::new("orders").unwrap(),
        domain: header.domain(),
        object: header.object(),
    };
    let dek = f
        .kms
        .unwrap(header.wrapped(), &ctx, &CallOptions::default())
        .ok()?;
    let (object, domain) = (header.object(), header.domain());
    let opener = header.open(&dek, object, domain).ok()?;
    opener.open_all(bytes).ok()
}

#[test]
fn the_unmodified_object_reads_back() {
    let f = fixture();
    assert_eq!(read(&f, &f.object).as_deref(), Some(&f.plain[..]));
}

#[test]
fn every_truncation_and_every_extension_is_refused() {
    let f = fixture();
    for len in 0..f.object.len() {
        assert!(
            read(&f, &f.object[..len]).is_none(),
            "truncated to {len} read"
        );
    }
    for extra in [1usize, 2, 16, 17, 1024] {
        let mut longer = f.object.clone();
        longer.extend(std::iter::repeat_n(0u8, extra));
        assert!(read(&f, &longer).is_none(), "{extra} appended bytes read");
    }
}

#[test]
fn every_byte_at_every_boundary_value_is_refused() {
    let f = fixture();
    // Each byte of the header region and the first block's edges set to 0x00, 0xFF and 0x80 and 0x7F.
    let region = (PREFIX_LEN + 64).min(f.object.len());
    for i in 0..region {
        for v in [0x00u8, 0xFF, 0x80, 0x7F, 0x01] {
            if f.object[i] == v {
                continue;
            }
            let mut bad = f.object.clone();
            bad[i] = v;
            assert!(read(&f, &bad).is_none(), "byte {i} = {v:#x} read");
        }
    }
}

#[test]
fn length_fields_at_extremes_never_allocate_past_the_published_bound() {
    let f = fixture();
    // The three wrapper length fields follow the prefix as little-endian u16: set each to extremes.
    for field in 0..3 {
        for value in [0u16, 1, 0x7FFF, 0x8000, 0xFFFF] {
            let mut bad = f.object.clone();
            let at = PREFIX_LEN + field * 2;
            bad[at..at + 2].copy_from_slice(&value.to_le_bytes());
            assert!(read(&f, &bad).is_none(), "field {field} = {value} read");
        }
    }
    // Plaintext length and block size (inside the prefix) at the extremes.
    for at in 0..PREFIX_LEN {
        for value in [0u8, 0xFF] {
            let mut bad = f.object.clone();
            bad[at] = value;
            if bad != f.object {
                assert!(read(&f, &bad).is_none());
            }
        }
    }
}

#[test]
fn seeded_garbage_and_seeded_mutations_are_all_refused() {
    let f = fixture();
    let mut rng = Rng(0x5EED_5EC0);
    // Pure garbage of assorted sizes, some starting with the real magic so they reach deeper parsing.
    for i in 0..20_000 {
        let n = rng.below(400);
        let mut bytes: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
        if i % 2 == 0 && bytes.len() >= 4 {
            bytes[..4].copy_from_slice(b"HSEB");
        }
        if i % 4 == 0 && bytes.len() >= 8 {
            bytes[4] = 1;
            bytes[5] = 1;
        }
        assert!(read(&f, &bytes).is_none(), "garbage {i} read");
    }
    // 1-4 random byte edits of a real object, anywhere in it.
    for i in 0..20_000 {
        let mut bad = f.object.clone();
        for _ in 0..1 + rng.below(4) {
            let at = rng.below(bad.len());
            bad[at] ^= 1 + rng.below(255) as u8;
        }
        assert!(read(&f, &bad).is_none(), "mutation {i} read");
    }
    // Swapping or duplicating whole blocks.
    let header = Header::total_len(&f.object[..HEAD_LEN]).unwrap();
    let body = &f.object[header..];
    let block = 1024 + 16;
    for (a, b) in [(0usize, 1usize), (1, 2), (0, 3)] {
        let mut bad = f.object.clone();
        let (a0, b0) = (header + a * block, header + b * block);
        for k in 0..block {
            bad.swap(a0 + k, b0 + k);
        }
        assert!(read(&f, &bad).is_none(), "blocks {a} and {b} swapped read");
    }
    let mut dup = f.object[..header].to_vec();
    dup.extend_from_slice(&body[..block]);
    dup.extend_from_slice(&body[..block]);
    dup.extend_from_slice(&body[2 * block..]);
    assert!(read(&f, &dup).is_none(), "a duplicated block read");
}
