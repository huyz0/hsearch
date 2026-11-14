//! Shared hardening helpers.
#![allow(dead_code)]
use hsearch_format::{FormatError, Header, Kind, crc32c, seal};

/// Every proper prefix must be refused, and every single-byte change must be refused.
/// The checksum covers every byte of header and body, so a flip can never decode.
pub fn assert_hardened<T: std::fmt::Debug>(
    bytes: &[u8],
    decode: impl Fn(&[u8]) -> Result<T, FormatError>,
) {
    decode(bytes).expect("pristine bytes decode");
    for n in 0..bytes.len() {
        assert!(decode(&bytes[..n]).is_err(), "prefix of {n} bytes decoded");
    }
    let mut m = bytes.to_vec();
    for i in 0..m.len() {
        for flip in [0x01u8, 0x80, 0xFF] {
            m[i] ^= flip;
            assert!(decode(&m).is_err(), "byte {i} ^ {flip:#x} decoded");
            m[i] ^= flip;
        }
    }
    let mut t = bytes.to_vec();
    t.push(0);
    assert_eq!(decode(&t).unwrap_err(), FormatError::Trailing);
}

/// Rewrite the header's length/kind-version fields and re-seal both checksums, so only the
/// bounds checks (not the CRC) can refuse the result.
pub fn forge(bytes: &[u8], f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let header = Header::parse(bytes).unwrap();
    let mut body = bytes[Header::LEN..].to_vec();
    f(&mut body);
    let mut out = Vec::new();
    seal(
        &mut out,
        header.kind,
        header.kind_version,
        header.mandatory_caps,
        &body,
    )
    .unwrap_or_else(|_| out.clear());
    out
}

/// Patch a raw header word and recompute the header CRC (and nothing else).
pub fn patch_header(bytes: &[u8], at: usize, value: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    b[at..at + value.len()].copy_from_slice(value);
    let crc = crc32c(&b[..36]);
    b[36..40].copy_from_slice(&crc.to_le_bytes());
    b
}

pub fn kind_of(bytes: &[u8]) -> Kind {
    Header::parse(bytes).unwrap().kind
}
