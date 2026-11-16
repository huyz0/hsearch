//! The encrypted block format (M05.03): an immutable object sealed under one fresh data key, readable
//! block by block without decrypting the rest. The ratified layout is below; `tests/vectors/block_v1.py`
//! is an independent encoder written from it, and a test holds this code to its bytes.
//!
//! ```text
//! prefix (52, authenticated):  "HSEB" | version u8 = 1 | suite u8 = 1 (AES-256-GCM) | flags u16 = 0
//!                              | object [16] | domain [16] | block_size u32 | plaintext_len u64
//! wrapper lengths (6):         key_ref_len u16 | key_version_len u16 | wrapped_len u16
//! wrapper (unauthenticated here, authenticated by the provider's context check and by the header tag
//!                              verifying only under the key it unwraps to): key_ref | key_version | wrapped
//! header tag (16):             AES-GCM of nothing, nonce counter 0, AAD "hs-block-header\0\1" | prefix
//! blocks:                      ceil(plaintext_len / block_size) of ciphertext | tag (16), block i at
//!                              nonce counter i + 1, AAD "hs-block\0\1" | prefix | i u32
//! ```
//!
//! All integers are little endian except the nonce counter, which is big endian in the last eight bytes
//! of a 96-bit nonce whose first four bytes are zero. Counter zero authenticates the header and is never
//! used for data. The payload AAD binds the prefix (object, domain, suite, block size, length, so block
//! count) and the ordinal, and **not** the wrapper, so a key rewrap changes no payload byte.
//!
//! Limits: a block is at most [`MAX_BLOCK_SIZE`] and an object at most [`MAX_BLOCKS`] blocks, so one data
//! key makes at most 2^20 + 1 invocations, far below AES-GCM's 2^32 per-key bound. One fresh data key
//! encrypts one immutable object; sealing is deterministic, so a retry resends identical bytes and any
//! changed plaintext is a new object with a new key.
//!
//! What the format does not do: the wrapper bytes and their three length fields are authenticated by the
//! key provider's context check, not by the header tag, which only proves the unwrapped key is the one
//! that sealed this header; and AES-GCM is not key-committing, so the tag does not bind the object to one
//! key against an attacker who can make a provider unwrap to a key they know. Neither is reachable with
//! per-object keys the manager generates and a provider that checks context, and both are why the
//! provider's check is mandatory rather than optional.

use std::ops::Range;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};

use crate::keys::{
    KeyFormatError, KeyRef, KeyVersion, MAX_KEY_REF, MAX_VERSION, MAX_WRAPPED, SecretKey,
    WrappedKey,
};

/// A refused object or block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// Fewer bytes than the structure needs.
    Truncated,
    /// Not this format.
    BadMagic,
    /// A version this build does not read.
    UnsupportedVersion(u8),
    /// A suite this build does not read.
    UnsupportedSuite(u8),
    /// Reserved bits set.
    BadFlags,
    /// A declared size outside a bound.
    Bound(&'static str),
    /// The embedded wrapped key is malformed.
    Wrapper(KeyFormatError),
    /// The ciphertext did not authenticate: damaged, substituted or under another key.
    Authentication,
    /// A caller error: an ordinal or length that does not fit the object.
    Usage(&'static str),
}

/// The format version.
pub const FORMAT_VERSION: u8 = 1;
/// AES-256-GCM with a 128-bit tag.
pub const SUITE_AES256_GCM: u8 = 1;
/// Largest plaintext block.
pub const MAX_BLOCK_SIZE: u32 = 1 << 20;
/// Most blocks in one object.
pub const MAX_BLOCKS: u64 = 1 << 20;
/// Authentication tag length.
pub const TAG_LEN: usize = 16;
/// Length of the authenticated fixed prefix.
pub const PREFIX_LEN: usize = 52;
/// Bytes a reader must have before [`Header::total_len`] can say how long the header is.
pub const HEAD_LEN: usize = PREFIX_LEN + 6;
/// The longest possible header, so a reader can fetch it in one ranged request.
pub const MAX_HEADER_LEN: usize = HEAD_LEN + MAX_KEY_REF + MAX_VERSION + MAX_WRAPPED + TAG_LEN;

const MAGIC: [u8; 4] = *b"HSEB";
const HEADER_AAD: &[u8] = b"hs-block-header\0\x01";
const BLOCK_AAD: &[u8] = b"hs-block\0\x01";

/// The immutable object id for an object-store key: the first 16 bytes of
/// `SHA-256("hs-object-id\0\x01" | key)`. Deriving it from the key makes an object copied to any other
/// key fail to open, and gives sealing a stable id without a coordinator.
pub fn object_id(key: &[u8]) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"hs-object-id\0\x01");
    h.update(key);
    let digest = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// The 96-bit nonce for an invocation counter: zeros, then the counter big endian. Counter 0 is the
/// header's; block `i` uses `i + 1`.
pub fn nonce(counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

fn nonce_of(counter: u64) -> aes_gcm::aead::Nonce<Aes256Gcm> {
    nonce(counter).into()
}

fn block_count(block_size: u32, plaintext_len: u64) -> Result<u64, BlockError> {
    if block_size == 0 || block_size > MAX_BLOCK_SIZE {
        return Err(BlockError::Bound("block size"));
    }
    let count = plaintext_len.div_ceil(u64::from(block_size));
    if count > MAX_BLOCKS {
        return Err(BlockError::Bound("block count"));
    }
    Ok(count)
}

/// The wrapper lengths and the parts of the head that the sizes depend on.
struct Head {
    block_size: u32,
    plaintext_len: u64,
    key_len: usize,
    version_len: usize,
    wrapped_len: usize,
}

impl Head {
    fn parse(head: &[u8]) -> Result<Self, BlockError> {
        if head.len() < HEAD_LEN {
            return Err(BlockError::Truncated);
        }
        if head[..4] != MAGIC {
            return Err(BlockError::BadMagic);
        }
        if head[4] != FORMAT_VERSION {
            return Err(BlockError::UnsupportedVersion(head[4]));
        }
        if head[5] != SUITE_AES256_GCM {
            return Err(BlockError::UnsupportedSuite(head[5]));
        }
        if head[6..8] != [0, 0] {
            return Err(BlockError::BadFlags);
        }
        let block_size = u32::from_le_bytes([head[40], head[41], head[42], head[43]]);
        let mut len = [0u8; 8];
        len.copy_from_slice(&head[44..52]);
        let plaintext_len = u64::from_le_bytes(len);
        block_count(block_size, plaintext_len)?;
        let key_len = usize::from(u16::from_le_bytes([head[52], head[53]]));
        let version_len = usize::from(u16::from_le_bytes([head[54], head[55]]));
        let wrapped_len = usize::from(u16::from_le_bytes([head[56], head[57]]));
        if key_len == 0 || key_len > MAX_KEY_REF {
            return Err(BlockError::Bound("key reference length"));
        }
        if version_len == 0 || version_len > MAX_VERSION {
            return Err(BlockError::Bound("key version length"));
        }
        if wrapped_len == 0 || wrapped_len > MAX_WRAPPED {
            return Err(BlockError::Bound("wrapped key length"));
        }
        Ok(Self {
            block_size,
            plaintext_len,
            key_len,
            version_len,
            wrapped_len,
        })
    }

    fn header_len(&self) -> usize {
        HEAD_LEN + self.key_len + self.version_len + self.wrapped_len + TAG_LEN
    }
}

/// A parsed, not yet verified, object header. Nothing in it is trusted until [`Header::open`] has
/// verified it under the data key the provider unwrapped from [`Header::wrapped`].
#[derive(Debug)]
pub struct Header {
    raw: Vec<u8>,
    object: [u8; 16],
    domain: [u8; 16],
    block_size: u32,
    plaintext_len: u64,
    wrapped: WrappedKey,
}

impl Header {
    /// The whole header's length, from its first [`HEAD_LEN`] bytes, so a reader can fetch the rest
    /// with one more bounded range (or fetch [`MAX_HEADER_LEN`] up front). Applies every bound.
    pub fn total_len(head: &[u8]) -> Result<usize, BlockError> {
        Ok(Head::parse(head)?.header_len())
    }

    /// Parses exactly one header. Every length is bounded before anything is allocated.
    pub fn parse(bytes: &[u8]) -> Result<Header, BlockError> {
        let head = Head::parse(bytes)?;
        let total = head.header_len();
        if bytes.len() < total {
            return Err(BlockError::Truncated);
        }
        if bytes.len() > total {
            return Err(BlockError::Usage("bytes after the header"));
        }
        let mut at = HEAD_LEN;
        let mut take = |n: usize| {
            let part = &bytes[at..at + n];
            at += n;
            part
        };
        let key = std::str::from_utf8(take(head.key_len))
            .map_err(|_| BlockError::Wrapper(KeyFormatError("key reference is not text")))?;
        let version = std::str::from_utf8(take(head.version_len))
            .map_err(|_| BlockError::Wrapper(KeyFormatError("key version is not text")))?;
        let wrapped = WrappedKey::new(
            KeyRef::new(key).map_err(BlockError::Wrapper)?,
            KeyVersion::new(version).map_err(BlockError::Wrapper)?,
            take(head.wrapped_len).to_vec(),
        )
        .map_err(BlockError::Wrapper)?;
        let mut object = [0u8; 16];
        object.copy_from_slice(&bytes[8..24]);
        let mut domain = [0u8; 16];
        domain.copy_from_slice(&bytes[24..40]);
        Ok(Header {
            raw: bytes.to_vec(),
            object,
            domain,
            block_size: head.block_size,
            plaintext_len: head.plaintext_len,
            wrapped,
        })
    }

    /// The immutable object id the header names.
    pub fn object(&self) -> [u8; 16] {
        self.object
    }

    /// The crypto domain the header names.
    pub fn domain(&self) -> [u8; 16] {
        self.domain
    }

    /// Plaintext bytes per block (the last may be shorter).
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Plaintext length of the whole object.
    pub fn plaintext_len(&self) -> u64 {
        self.plaintext_len
    }

    /// Number of blocks.
    pub fn block_count(&self) -> u64 {
        self.plaintext_len.div_ceil(u64::from(self.block_size))
    }

    /// The inline wrapped data key, to hand to the key provider with a context built from
    /// [`Header::domain`] and [`Header::object`] and the index the key path named.
    pub fn wrapped(&self) -> &WrappedKey {
        &self.wrapped
    }

    /// The header's length in bytes; block 0 starts here.
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    /// Never empty: a header has a fixed prefix.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Verifies the header under the data key **and** against the object and domain the caller's key
    /// path and policy named: a whole, valid object copied from another object or crypto domain is
    /// refused here, not left to a comparison the caller might forget. A wrapper that unwraps to any
    /// other key and a header whose prefix was altered fail the tag.
    pub fn open(
        self,
        dek: &SecretKey,
        expected_object: [u8; 16],
        expected_domain: [u8; 16],
    ) -> Result<Opener, BlockError> {
        if self.object != expected_object || self.domain != expected_domain {
            return Err(BlockError::Authentication);
        }
        let cipher =
            Aes256Gcm::new_from_slice(dek.expose()).map_err(|_| BlockError::Authentication)?;
        let mut aad = Vec::with_capacity(HEADER_AAD.len() + PREFIX_LEN);
        aad.extend_from_slice(HEADER_AAD);
        aad.extend_from_slice(&self.raw[..PREFIX_LEN]);
        let tag = &self.raw[self.raw.len() - TAG_LEN..];
        cipher
            .decrypt(
                &nonce_of(0),
                Payload {
                    msg: tag,
                    aad: &aad,
                },
            )
            .map_err(|_| BlockError::Authentication)?;
        Ok(Opener {
            cipher,
            header: self,
        })
    }
}

/// A header verified under its key: opens blocks.
pub struct Opener {
    cipher: Aes256Gcm,
    header: Header,
}

impl std::fmt::Debug for Opener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Opener({:?}, <key redacted>)", self.header.object)
    }
}

impl Opener {
    /// The verified header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    fn plain_len(&self, ordinal: u64) -> Result<u64, BlockError> {
        let size = u64::from(self.header.block_size);
        if ordinal >= self.header.block_count() {
            return Err(BlockError::Usage("block ordinal out of range"));
        }
        Ok(size.min(self.header.plaintext_len - ordinal * size))
    }

    /// The absolute byte range of one block's ciphertext, for a ranged read.
    pub fn block_range(&self, ordinal: u64) -> Result<Range<u64>, BlockError> {
        let plain = self.plain_len(ordinal)?;
        let stride = u64::from(self.header.block_size) + TAG_LEN as u64;
        let start = self.header.raw.len() as u64 + ordinal * stride;
        Ok(start..start + plain + TAG_LEN as u64)
    }

    /// The object's total length in bytes.
    pub fn object_len(&self) -> u64 {
        self.header.raw.len() as u64
            + self.header.plaintext_len
            + self.header.block_count() * TAG_LEN as u64
    }

    /// Opens one block from exactly its ciphertext bytes.
    pub fn open_block(&self, ordinal: u64, ciphertext: &[u8]) -> Result<Vec<u8>, BlockError> {
        let range = self.block_range(ordinal)?;
        if ciphertext.len() as u64 != range.end - range.start {
            return Err(BlockError::Usage("block length"));
        }
        let ordinal32 = u32::try_from(ordinal).map_err(|_| BlockError::Usage("ordinal"))?;
        let mut aad = Vec::with_capacity(BLOCK_AAD.len() + PREFIX_LEN + 4);
        aad.extend_from_slice(BLOCK_AAD);
        aad.extend_from_slice(&self.header.raw[..PREFIX_LEN]);
        aad.extend_from_slice(&ordinal32.to_le_bytes());
        self.cipher
            .decrypt(
                &nonce_of(ordinal + 1),
                Payload {
                    msg: ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| BlockError::Authentication)
    }

    /// Opens a whole object. The bytes must be exactly the object, and their header the verified one.
    pub fn open_all(&self, object: &[u8]) -> Result<Vec<u8>, BlockError> {
        if object.len() as u64 != self.object_len() {
            return Err(BlockError::Usage("object length"));
        }
        if object[..self.header.raw.len()] != self.header.raw[..] {
            return Err(BlockError::Authentication);
        }
        let mut out = Vec::with_capacity(usize::try_from(self.header.plaintext_len).unwrap_or(0));
        for ordinal in 0..self.header.block_count() {
            let r = self.block_range(ordinal)?;
            out.extend(self.open_block(ordinal, &object[r.start as usize..r.end as usize])?);
        }
        Ok(out)
    }
}

/// Seals `plaintext` as one immutable object under `dek`, which must be fresh for this object (from
/// `KeyManager::create_object_key`) and is used for no other. Deterministic: the same inputs give the
/// same bytes, which is what makes an upload retry safe. `wrapped` is the provider's wrap of `dek`.
pub fn seal_object(
    dek: &SecretKey,
    wrapped: &WrappedKey,
    object: [u8; 16],
    domain: [u8; 16],
    block_size: u32,
    plaintext: &[u8],
) -> Result<Vec<u8>, BlockError> {
    let plaintext_len = plaintext.len() as u64;
    let count = block_count(block_size, plaintext_len)?;
    let key = wrapped.key().as_str().as_bytes();
    let version = wrapped.version().as_str().as_bytes();
    let wrap = wrapped.bytes();
    let header_len = HEAD_LEN + key.len() + version.len() + wrap.len() + TAG_LEN;
    let mut out = Vec::with_capacity(header_len + plaintext.len() + (count as usize) * TAG_LEN);
    out.extend_from_slice(&MAGIC);
    out.push(FORMAT_VERSION);
    out.push(SUITE_AES256_GCM);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&object);
    out.extend_from_slice(&domain);
    out.extend_from_slice(&block_size.to_le_bytes());
    out.extend_from_slice(&plaintext_len.to_le_bytes());
    debug_assert_eq!(out.len(), PREFIX_LEN);
    // The wrapper's parts were bounded when they were built, so these casts cannot truncate.
    out.extend_from_slice(&(key.len() as u16).to_le_bytes());
    out.extend_from_slice(&(version.len() as u16).to_le_bytes());
    out.extend_from_slice(&(wrap.len() as u16).to_le_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(version);
    out.extend_from_slice(wrap);
    let cipher = Aes256Gcm::new_from_slice(dek.expose()).map_err(|_| BlockError::Authentication)?;
    let prefix: [u8; PREFIX_LEN] = out[..PREFIX_LEN]
        .try_into()
        .map_err(|_| BlockError::Usage("prefix"))?;
    let mut aad = Vec::with_capacity(HEADER_AAD.len() + PREFIX_LEN);
    aad.extend_from_slice(HEADER_AAD);
    aad.extend_from_slice(&prefix);
    let tag = cipher
        .encrypt(
            &nonce_of(0),
            Payload {
                msg: b"",
                aad: &aad,
            },
        )
        .map_err(|_| BlockError::Authentication)?;
    out.extend_from_slice(&tag);
    for (ordinal, chunk) in plaintext.chunks(block_size as usize).enumerate() {
        let mut aad = Vec::with_capacity(BLOCK_AAD.len() + PREFIX_LEN + 4);
        aad.extend_from_slice(BLOCK_AAD);
        aad.extend_from_slice(&prefix);
        let ordinal32 = u32::try_from(ordinal).map_err(|_| BlockError::Usage("ordinal"))?;
        aad.extend_from_slice(&ordinal32.to_le_bytes());
        let ct = cipher
            .encrypt(
                &nonce_of(ordinal as u64 + 1),
                Payload {
                    msg: chunk,
                    aad: &aad,
                },
            )
            .map_err(|_| BlockError::Authentication)?;
        out.extend_from_slice(&ct);
    }
    Ok(out)
}
