#![allow(clippy::indexing_slicing)] // table indices are masked to 0..=255 and chunks are exactly 16 bytes
//! CRC-32C (Castagnoli), slicing-by-16, safe Rust only (`unsafe` is forbidden here, so the
//! hardware instruction is not used; see the format spec for the measured cost).

const POLY: u32 = 0x82F6_3B78;

const fn tables() -> [[u32; 256]; 16] {
    let mut t = [[0u32; 256]; 16];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut i = 0;
    while i < 256 {
        let mut c = t[0][i];
        let mut s = 1;
        while s < 16 {
            c = t[0][(c & 0xFF) as usize] ^ (c >> 8);
            t[s][i] = c;
            s += 1;
        }
        i += 1;
    }
    t
}

static T: [[u32; 256]; 16] = tables();

#[inline(always)]
fn lanes(top: usize, w: u32) -> u32 {
    T[top][(w & 0xFF) as usize]
        ^ T[top - 1][((w >> 8) & 0xFF) as usize]
        ^ T[top - 2][((w >> 16) & 0xFF) as usize]
        ^ T[top - 3][(w >> 24) as usize]
}

/// CRC-32C of `data`, as used for every header and body checksum.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    let mut chunks = data.chunks_exact(16);
    for c in &mut chunks {
        let a = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) ^ crc;
        let b = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
        let d = u32::from_le_bytes([c[8], c[9], c[10], c[11]]);
        let e = u32::from_le_bytes([c[12], c[13], c[14], c[15]]);
        crc = lanes(15, a) ^ lanes(11, b) ^ lanes(7, d) ^ lanes(3, e);
    }
    for &b in chunks.remainder() {
        crc = T[0][((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Byte-at-a-time reference used by the benchmark and tests to check the sliced version.
#[doc(hidden)]
#[must_use]
pub fn crc32c_bytewise(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc = T[0][((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
    }

    #[test]
    fn sliced_equals_bytewise_for_every_length() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 31 + 7) as u8).collect();
        for n in 0..data.len() {
            assert_eq!(crc32c(&data[..n]), crc32c_bytewise(&data[..n]), "len {n}");
        }
    }
}
