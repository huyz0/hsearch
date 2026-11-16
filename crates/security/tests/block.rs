//! M05.03 encrypted block format: round trips, an independent decrypt written from the spec alone, a
//! known-answer object, substitution and truncation controls, every-byte damage through the whole read
//! path (parse, provider unwrap, header tag, block tag), nonce controls, and key rewrap.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use hsearch_security::block::{
    BlockError, HEAD_LEN, Header, MAX_BLOCK_SIZE, MAX_BLOCKS, PREFIX_LEN, TAG_LEN, nonce,
    seal_object,
};
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{
    CallOptions, KeyContext, KeyProvider, KeyRef, KeyVersion, ProviderError, SecretKey, WrappedKey,
};
use hsearch_security::test_kms::TestKms;

const OBJECT: [u8; 16] = [0xA1; 16];
const DOMAIN: [u8; 16] = [0xD0; 16];

fn key(name: &str) -> KeyRef {
    KeyRef::new(&format!("test/acct/{name}")).unwrap()
}

fn ctx(index: &str, object: [u8; 16]) -> KeyContext {
    KeyContext {
        index: IndexId::new(index).unwrap(),
        domain: DOMAIN,
        object,
    }
}

fn opts() -> CallOptions {
    CallOptions::default()
}

struct Sealed {
    kms: TestKms,
    dek: SecretKey,
    wrapped: WrappedKey,
    bytes: Vec<u8>,
}

fn plaintext(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + 3) as u8).collect()
}

fn sealed(index: &str, object: [u8; 16], block: u32, n: usize) -> Sealed {
    let kms = TestKms::new();
    kms.create_key(&key("a")).unwrap();
    let c = ctx(index, object);
    let dek = SecretKey::generate().unwrap();
    let wrapped = kms.wrap(&key("a"), &c, &dek, &opts()).unwrap();
    let bytes = seal_object(&dek, &wrapped, object, DOMAIN, block, &plaintext(n)).unwrap();
    Sealed {
        kms,
        dek,
        wrapped,
        bytes,
    }
}

/// The whole read path as a reader runs it. Any refusal anywhere is an `Err`.
fn read_all(kms: &TestKms, index: &str, object: [u8; 16], bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() < HEAD_LEN {
        return Err("short".into());
    }
    let total = Header::total_len(&bytes[..HEAD_LEN]).map_err(|e| format!("{e:?}"))?;
    if bytes.len() < total {
        return Err("short header".into());
    }
    let header = Header::parse(&bytes[..total]).map_err(|e| format!("{e:?}"))?;
    let c = KeyContext {
        index: IndexId::new(index).unwrap(),
        domain: header.domain(),
        object: header.object(),
    };
    let dek = kms
        .unwrap(header.wrapped(), &c, &opts())
        .map_err(|e| format!("{e:?}"))?;
    let opener = header
        .open(&dek, object, DOMAIN)
        .map_err(|e| format!("{e:?}"))?;
    opener.open_all(bytes).map_err(|e| format!("{e:?}"))
}

#[test]
fn objects_round_trip_at_every_size_around_a_block_boundary() {
    for block in [1u32, 16, 64] {
        for n in [0usize, 1, 15, 16, 17, 63, 64, 65, 128, 200] {
            let s = sealed("orders", OBJECT, block, n);
            assert_eq!(
                read_all(&s.kms, "orders", OBJECT, &s.bytes).unwrap(),
                plaintext(n),
                "block {block} len {n}"
            );
        }
    }
}

#[test]
fn a_block_is_readable_without_the_rest_of_the_object() {
    let s = sealed("orders", OBJECT, 64, 300);
    let total = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    let header = Header::parse(&s.bytes[..total]).unwrap();
    assert_eq!((header.block_count(), header.plaintext_len()), (5, 300));
    let opener = header.open(&s.dek, OBJECT, DOMAIN).unwrap();
    for ordinal in 0..5u64 {
        let range = opener.block_range(ordinal).unwrap();
        let got = opener
            .open_block(ordinal, &s.bytes[range.start as usize..range.end as usize])
            .unwrap();
        let start = ordinal as usize * 64;
        assert_eq!(got, plaintext(300)[start..(start + 64).min(300)]);
    }
    // The last block is short; one past it does not exist.
    assert_eq!(opener.block_range(4).unwrap().end as usize, s.bytes.len());
    assert!(opener.block_range(5).is_err());
}

#[test]
fn an_independent_decrypt_written_from_the_spec_agrees() {
    let s = sealed("orders", OBJECT, 32, 100);
    let b = &s.bytes;
    assert_eq!(&b[0..4], b"HSEB");
    assert_eq!((b[4], b[5]), (1, 1));
    assert_eq!(u16::from_le_bytes([b[6], b[7]]), 0);
    assert_eq!(&b[8..24], &OBJECT);
    assert_eq!(&b[24..40], &DOMAIN);
    assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 32);
    assert_eq!(u64::from_le_bytes(b[44..52].try_into().unwrap()), 100);
    let kl = u16::from_le_bytes([b[52], b[53]]) as usize;
    let vl = u16::from_le_bytes([b[54], b[55]]) as usize;
    let wl = u16::from_le_bytes([b[56], b[57]]) as usize;
    assert_eq!(&b[58..58 + kl], s.wrapped.key().as_str().as_bytes());
    assert_eq!(
        &b[58 + kl..58 + kl + vl],
        s.wrapped.version().as_str().as_bytes()
    );
    assert_eq!(&b[58 + kl + vl..58 + kl + vl + wl], s.wrapped.bytes());
    let tag_at = 58 + kl + vl + wl;
    let prefix = &b[..52];
    let cipher = Aes256Gcm::new_from_slice(s.dek.expose()).unwrap();
    let n = |counter: u64| {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&counter.to_be_bytes());
        n
    };
    // Counter 0 authenticates the header alone.
    let mut header_aad = b"hs-block-header\0\x01".to_vec();
    header_aad.extend_from_slice(prefix);
    let tag = cipher
        .encrypt(
            &Nonce::from(n(0)),
            Payload {
                msg: b"",
                aad: &header_aad,
            },
        )
        .unwrap();
    assert_eq!(&b[tag_at..tag_at + 16], tag.as_slice());
    // Blocks use counters 1.. and bind the prefix and their ordinal.
    let mut at = tag_at + 16;
    let mut out = Vec::new();
    for ordinal in 0..4u32 {
        let len = if ordinal == 3 { 4 } else { 32 } + TAG_LEN;
        let mut aad = b"hs-block\0\x01".to_vec();
        aad.extend_from_slice(prefix);
        aad.extend_from_slice(&ordinal.to_le_bytes());
        out.extend(
            cipher
                .decrypt(
                    &Nonce::from(n(u64::from(ordinal) + 1)),
                    Payload {
                        msg: &b[at..at + len],
                        aad: &aad,
                    },
                )
                .unwrap(),
        );
        at += len;
    }
    assert_eq!(at, b.len());
    assert_eq!(out, plaintext(100));
}

#[test]
fn nonces_are_counters_and_header_and_blocks_never_share_one() {
    assert_eq!(nonce(0), [0u8; 12]);
    assert_eq!(nonce(1), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(nonce(0x0102)[10..], [1, 2]);
    let mut seen = std::collections::HashSet::new();
    for c in 0..1000u64 {
        assert!(seen.insert(nonce(c)));
    }
}

#[test]
fn sealing_is_deterministic_so_a_retry_resends_identical_bytes() {
    let s = sealed("orders", OBJECT, 32, 100);
    let again = seal_object(&s.dek, &s.wrapped, OBJECT, DOMAIN, 32, &plaintext(100)).unwrap();
    assert_eq!(again, s.bytes);
}

#[test]
fn declared_sizes_are_bounded_before_anything_is_allocated() {
    let s = sealed("orders", OBJECT, 32, 100);
    let seal = |block: u32, n: usize| {
        seal_object(&s.dek, &s.wrapped, OBJECT, DOMAIN, block, &plaintext(n))
    };
    assert!(matches!(seal(0, 10), Err(BlockError::Bound(_))));
    assert!(matches!(
        seal(MAX_BLOCK_SIZE + 1, 10),
        Err(BlockError::Bound(_))
    ));
    assert!(seal(MAX_BLOCK_SIZE, 10).is_ok());
    // MAX_BLOCKS + 1 one-byte blocks.
    const { assert!(MAX_BLOCKS < 1 << 24) };
    assert!(matches!(
        seal(1, MAX_BLOCKS as usize + 1),
        Err(BlockError::Bound(_))
    ));
    // Exactly MAX_BLOCKS is allowed: checked on a header, since sealing a million blocks is slow.
    let mut edge = s.bytes.clone();
    edge[40..44].copy_from_slice(&1u32.to_le_bytes());
    edge[44..52].copy_from_slice(&MAX_BLOCKS.to_le_bytes());
    assert!(Header::total_len(&edge[..HEAD_LEN]).is_ok());
    edge[44..52].copy_from_slice(&(MAX_BLOCKS + 1).to_le_bytes());
    assert!(matches!(
        Header::total_len(&edge[..HEAD_LEN]),
        Err(BlockError::Bound(_))
    ));
    // A forged header that claims an absurd plaintext length is refused at parse, not at allocation.
    let mut forged = s.bytes.clone();
    forged[44..52].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(matches!(
        Header::total_len(&forged[..HEAD_LEN]),
        Err(BlockError::Bound(_))
    ));
    assert!(matches!(Header::parse(&forged), Err(BlockError::Bound(_))));
}

#[test]
fn unknown_versions_suites_flags_and_magic_are_refused() {
    let s = sealed("orders", OBJECT, 32, 100);
    for (at, value, want) in [
        (0usize, b'X', "magic"),
        (4, 2, "version"),
        (5, 2, "suite"),
        (6, 1, "flags"),
        (7, 1, "flags"),
    ] {
        let mut b = s.bytes.clone();
        b[at] = value;
        let r = Header::total_len(&b[..HEAD_LEN]).and_then(|t| Header::parse(&b[..t]).map(|_| ()));
        assert!(r.is_err(), "{want}");
    }
}

#[test]
fn every_byte_of_an_object_is_covered_by_some_check() {
    let s = sealed("orders", OBJECT, 16, 40);
    assert!(read_all(&s.kms, "orders", OBJECT, &s.bytes).is_ok());
    for at in 0..s.bytes.len() {
        for mask in [0x01u8, 0x80, 0xFF] {
            let mut b = s.bytes.clone();
            b[at] ^= mask;
            assert!(
                read_all(&s.kms, "orders", OBJECT, &b).is_err(),
                "byte {at} mask {mask:#x} was accepted"
            );
        }
    }
}

#[test]
fn every_truncation_and_extension_is_refused() {
    let s = sealed("orders", OBJECT, 16, 40);
    for cut in 0..s.bytes.len() {
        assert!(
            read_all(&s.kms, "orders", OBJECT, &s.bytes[..cut]).is_err(),
            "cut at {cut}"
        );
    }
    let mut longer = s.bytes.clone();
    longer.push(0);
    assert!(read_all(&s.kms, "orders", OBJECT, &longer).is_err());
    // Dropping the last whole block and fixing nothing else.
    let total = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    let header = Header::parse(&s.bytes[..total]).unwrap();
    let opener = header.open(&s.dek, OBJECT, DOMAIN).unwrap();
    let last = opener.block_range(header_blocks(&s) - 1).unwrap();
    assert!(opener.open_all(&s.bytes[..last.start as usize]).is_err());
}

fn header_blocks(s: &Sealed) -> u64 {
    let total = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    Header::parse(&s.bytes[..total]).unwrap().block_count()
}

#[test]
fn a_block_moved_to_another_position_or_object_is_refused() {
    let s = sealed("orders", OBJECT, 16, 64);
    let total = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    let opener = Header::parse(&s.bytes[..total])
        .unwrap()
        .open(&s.dek, OBJECT, DOMAIN)
        .unwrap();
    let slice = |o: u64| {
        let r = opener.block_range(o).unwrap();
        s.bytes[r.start as usize..r.end as usize].to_vec()
    };
    // Position: block 1's ciphertext offered as block 2 and the reverse.
    assert_eq!(
        opener.open_block(2, &slice(1)),
        Err(BlockError::Authentication)
    );
    assert_eq!(
        opener.open_block(1, &slice(2)),
        Err(BlockError::Authentication)
    );
    // Another object under the very same data key (a misuse the AAD still catches).
    let other = seal_object(&s.dek, &s.wrapped, [0xB2; 16], DOMAIN, 16, &plaintext(64)).unwrap();
    let ot = Header::parse(&other[..total])
        .unwrap()
        .open(&s.dek, [0xB2; 16], DOMAIN)
        .unwrap();
    let r = ot.block_range(1).unwrap();
    assert_eq!(
        opener.open_block(1, &other[r.start as usize..r.end as usize]),
        Err(BlockError::Authentication)
    );
    // Another domain under the same key.
    let cross = seal_object(&s.dek, &s.wrapped, OBJECT, [0xD1; 16], 16, &plaintext(64)).unwrap();
    let ct = Header::parse(&cross[..total])
        .unwrap()
        .open(&s.dek, OBJECT, [0xD1; 16])
        .unwrap();
    let r = ct.block_range(1).unwrap();
    assert_eq!(
        opener.open_block(1, &cross[r.start as usize..r.end as usize]),
        Err(BlockError::Authentication)
    );
    // A block of a different block count (same object, same key): the prefix is bound.
    let longer = seal_object(&s.dek, &s.wrapped, OBJECT, DOMAIN, 16, &plaintext(80)).unwrap();
    let lt = Header::parse(&longer[..total])
        .unwrap()
        .open(&s.dek, OBJECT, DOMAIN)
        .unwrap();
    let r = lt.block_range(1).unwrap();
    assert_eq!(
        opener.open_block(1, &longer[r.start as usize..r.end as usize]),
        Err(BlockError::Authentication)
    );
}

#[test]
fn a_header_from_one_object_over_the_blocks_of_another_is_refused() {
    let a = sealed("orders", OBJECT, 16, 64);
    let kms = &a.kms;
    let b_bytes = seal_object(&a.dek, &a.wrapped, [0xB2; 16], DOMAIN, 16, &plaintext(64)).unwrap();
    let total = Header::total_len(&a.bytes[..HEAD_LEN]).unwrap();
    let mut spliced = a.bytes[..total].to_vec();
    spliced.extend_from_slice(&b_bytes[total..]);
    assert!(read_all(kms, "orders", OBJECT, &spliced).is_err());
}

#[test]
fn the_wrapper_is_authenticated_by_the_provider_and_must_unwrap_to_the_key_that_sealed_the_header()
{
    let s = sealed("orders", OBJECT, 16, 40);
    // Right object, wrong index: the provider refuses the unwrap.
    assert!(read_all(&s.kms, "invoices", OBJECT, &s.bytes).is_err());
    // A wrapper lifted from another object of the same index.
    let other_object = [0xB2; 16];
    let c2 = ctx("orders", other_object);
    let dek2 = SecretKey::generate().unwrap();
    let w2 = s.kms.wrap(&key("a"), &c2, &dek2, &opts()).unwrap();
    let total = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    let header = Header::parse(&s.bytes[..total]).unwrap();
    // Provider: object A's context cannot unwrap B's wrapper.
    assert_eq!(
        s.kms
            .unwrap(&w2, &ctx("orders", OBJECT), &opts())
            .unwrap_err(),
        ProviderError::ContextMismatch
    );
    // Header tag: even a wrapper the provider accepts must yield the key that sealed this header.
    let w3 = s
        .kms
        .wrap(&key("a"), &ctx("orders", OBJECT), &dek2, &opts())
        .unwrap();
    let dek3 = s.kms.unwrap(&w3, &ctx("orders", OBJECT), &opts()).unwrap();
    assert_eq!(
        header.open(&dek3, OBJECT, DOMAIN).unwrap_err(),
        BlockError::Authentication
    );
}

#[test]
fn a_rewrapped_key_opens_the_same_unchanged_ciphertext() {
    let s = sealed("orders", OBJECT, 16, 40);
    let c = ctx("orders", OBJECT);
    s.kms.rotate(&key("a")).unwrap();
    // The overlay: the same data key wrapped under the new key version. The object is not rewritten.
    let overlay = s.kms.wrap(&key("a"), &c, &s.dek, &opts()).unwrap();
    assert_eq!(overlay.version().as_str(), "v2");
    let before = s.bytes.clone();
    let dek = s.kms.unwrap(&overlay, &c, &opts()).unwrap();
    let total = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    let opener = Header::parse(&s.bytes[..total])
        .unwrap()
        .open(&dek, OBJECT, DOMAIN)
        .unwrap();
    assert_eq!(opener.open_all(&s.bytes).unwrap(), plaintext(40));
    assert_eq!(s.bytes, before);
    // Crash before the overlay was published: the inline wrapper alone still reads the object.
    assert!(read_all(&s.kms, "orders", OBJECT, &s.bytes).is_ok());
    // Revocation applies to the overlay and to the inline wrapper alike.
    s.kms.revoke(&key("a"));
    assert_eq!(
        s.kms.unwrap(&overlay, &c, &opts()).unwrap_err(),
        ProviderError::Revoked
    );
    assert!(read_all(&s.kms, "orders", OBJECT, &s.bytes).is_err());
}

#[test]
fn the_wrapper_may_change_without_changing_any_block_byte() {
    let s = sealed("orders", OBJECT, 16, 40);
    let c = ctx("orders", OBJECT);
    s.kms.rotate(&key("a")).unwrap();
    let overlay = s.kms.wrap(&key("a"), &c, &s.dek, &opts()).unwrap();
    // The same data key, a different wrapper: the payload ciphertext is the same bytes, because the
    // payload AAD excludes the wrapper. Seal again with the overlay wrapper and compare past the header.
    let resealed = seal_object(&s.dek, &overlay, OBJECT, DOMAIN, 16, &plaintext(40)).unwrap();
    let t1 = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    let t2 = Header::total_len(&resealed[..HEAD_LEN]).unwrap();
    assert_eq!(s.bytes[t1..], resealed[t2..]);
    assert_eq!(s.bytes[..PREFIX_LEN], resealed[..PREFIX_LEN]);
}

#[test]
fn an_object_or_domain_other_than_the_one_named_is_refused_even_when_valid() {
    // A whole valid object, wrapper minted for its own domain, read by a path that expects another.
    let kms = TestKms::new();
    kms.create_key(&key("a")).unwrap();
    let foreign_domain = [0xE5; 16];
    let c = KeyContext {
        index: IndexId::new("other").unwrap(),
        domain: foreign_domain,
        object: OBJECT,
    };
    let dek = SecretKey::generate().unwrap();
    let wrapped = kms.wrap(&key("a"), &c, &dek, &opts()).unwrap();
    let bytes = seal_object(&dek, &wrapped, OBJECT, foreign_domain, 16, &plaintext(40)).unwrap();
    let total = Header::total_len(&bytes[..HEAD_LEN]).unwrap();
    let header = Header::parse(&bytes[..total]).unwrap();
    assert!(header.open(&dek, OBJECT, DOMAIN).is_err());
    let header = Header::parse(&bytes[..total]).unwrap();
    assert!(header.open(&dek, [0xB2; 16], foreign_domain).is_err());
    let header = Header::parse(&bytes[..total]).unwrap();
    assert!(header.open(&dek, OBJECT, foreign_domain).is_ok());
}

#[test]
fn a_wrong_data_key_never_opens_anything() {
    let s = sealed("orders", OBJECT, 16, 40);
    let wrong = SecretKey::generate().unwrap();
    let total = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    assert_eq!(
        Header::parse(&s.bytes[..total])
            .unwrap()
            .open(&wrong, OBJECT, DOMAIN)
            .unwrap_err(),
        BlockError::Authentication
    );
}

#[test]
fn a_malformed_embedded_wrapper_is_refused_at_parse() {
    let s = sealed("orders", OBJECT, 16, 40);
    let mut b = s.bytes.clone();
    // A zero-length key reference.
    b[52] = 0;
    b[53] = 0;
    let t = Header::total_len(&b[..HEAD_LEN]);
    assert!(t.is_err() || Header::parse(&b).is_err());
    // A wrapped-key length above the bound.
    let mut b = s.bytes.clone();
    b[56..58].copy_from_slice(&u16::MAX.to_le_bytes());
    assert!(matches!(
        Header::total_len(&b[..HEAD_LEN]),
        Err(BlockError::Bound(_))
    ));
}

#[test]
fn key_material_never_appears_in_the_object_or_in_debug_output() {
    let s = sealed("orders", OBJECT, 16, 40);
    let dek = s.dek.expose();
    assert!(!s.bytes.windows(32).any(|w| w == dek));
    let total = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    let header = Header::parse(&s.bytes[..total]).unwrap();
    let opener = header.open(&s.dek, OBJECT, DOMAIN).unwrap();
    let shown = format!("{opener:?}");
    assert!(shown.contains("redacted"));
    assert!(!shown.contains(&format!("{dek:?}")));
}

#[test]
fn the_known_answer_object_is_byte_identical_to_an_independent_encoder() {
    // Written by tests/vectors/block_v1.py, which shares no code with this crate.
    let expected = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/vectors/block_v1.hex"
    ))
    .unwrap();
    let dek = SecretKey::from_bytes([0x42; 32]);
    let wrapped = WrappedKey::new(
        KeyRef::new("test/acct/kat").unwrap(),
        KeyVersion::new("v1").unwrap(),
        (0..48u8).collect(),
    )
    .unwrap();
    let pt: Vec<u8> = (0..100u8).collect();
    let got = seal_object(&dek, &wrapped, [0x11; 16], [0x22; 16], 32, &pt).unwrap();
    let hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(hex, expected.trim());
}

#[test]
fn a_header_must_be_exactly_the_verified_one_and_lengths_are_exact_bounds() {
    let s = sealed("orders", OBJECT, 16, 40);
    let total = Header::total_len(&s.bytes[..HEAD_LEN]).unwrap();
    // Bytes after the header are not part of it.
    assert!(Header::parse(&s.bytes).is_err());
    // open_all refuses an object whose header differs from the verified header in a byte the tag does not
    // cover (the wrapper): the verified header is the one that was opened.
    let opener = Header::parse(&s.bytes[..total])
        .unwrap()
        .open(&s.dek, OBJECT, DOMAIN)
        .unwrap();
    let mut other = s.bytes.clone();
    other[HEAD_LEN] ^= 1;
    assert_eq!(opener.open_all(&other), Err(BlockError::Authentication));
    // The key reference, version and wrapped-key bounds are inclusive.
    for (at, max) in [(52usize, 256u16), (54, 128), (56, 1024)] {
        let mut b = s.bytes.clone();
        b[at..at + 2].copy_from_slice(&max.to_le_bytes());
        assert!(Header::total_len(&b[..HEAD_LEN]).is_ok(), "{max} allowed");
        b[at..at + 2].copy_from_slice(&(max + 1).to_le_bytes());
        assert!(
            matches!(Header::total_len(&b[..HEAD_LEN]), Err(BlockError::Bound(_))),
            "{} refused",
            max + 1
        );
    }
}
