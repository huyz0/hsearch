//! The key-provider contract: how a data key is wrapped and unwrapped without this crate, or anything
//! above it, ever holding a key-encryption key.
//!
//! A customer key (KEK) lives at a provider and never leaves it. This crate asks the provider to wrap a
//! per-object data key (DEK) and to unwrap it again, binding the call to the index and object it is for.
//! A wrapped key from one index cannot be unwrapped for another: the binding is checked by the provider.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::ids::IndexId;

/// The size of a data key: AES-256.
pub const DEK_LEN: usize = 32;
/// The longest key reference.
pub const MAX_KEY_REF: usize = 256;
/// The longest provider key version.
pub const MAX_VERSION: usize = 128;
/// The largest wrapped key accepted from storage. Envelopes are bounded before anything is allocated.
pub const MAX_WRAPPED: usize = 1024;

/// A provider key reference such as `test/acct-1/key-a`: `<provider>/<account>/<key>`, each part drawn from
/// `[A-Za-z0-9_.:-]`. It names a customer key; it is never a URL, so a tenant cannot make a node call out.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyRef(Arc<str>);

/// A malformed key reference, version or wrapped key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyFormatError(pub &'static str);

impl fmt::Display for KeyFormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid key material descriptor: {}", self.0)
    }
}

impl std::error::Error for KeyFormatError {}

fn part_ok(part: &str) -> bool {
    !part.is_empty()
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

impl KeyRef {
    /// Parses a key reference.
    pub fn new(text: &str) -> Result<Self, KeyFormatError> {
        if text.len() > MAX_KEY_REF {
            return Err(KeyFormatError("key reference too long"));
        }
        let parts: Vec<&str> = text.split('/').collect();
        if parts.len() != 3 || !parts.iter().all(|p| part_ok(p)) {
            return Err(KeyFormatError(
                "key reference must be <provider>/<account>/<key>",
            ));
        }
        Ok(Self(text.into()))
    }

    /// The reference.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The provider part.
    pub fn provider(&self) -> &str {
        self.0.split('/').next().unwrap_or("")
    }

    /// The account part.
    pub fn account(&self) -> &str {
        self.0.split('/').nth(1).unwrap_or("")
    }
}

impl fmt::Debug for KeyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyRef({})", self.0)
    }
}

impl fmt::Display for KeyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A provider's version of a key, as an opaque bounded string.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyVersion(Arc<str>);

impl KeyVersion {
    /// Parses a version.
    pub fn new(text: &str) -> Result<Self, KeyFormatError> {
        if text.len() > MAX_VERSION || !part_ok(text) {
            return Err(KeyFormatError("key version"));
        }
        Ok(Self(text.into()))
    }

    /// The version.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for KeyVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyVersion({})", self.0)
    }
}

/// What a wrap is bound to: the index, the crypto domain and the immutable object. The provider receives
/// it as authenticated context, so a wrapped key presented for any other index, domain or object fails.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyContext {
    /// The index (tenant).
    pub index: IndexId,
    /// The crypto domain (see the block-format spec).
    pub domain: [u8; 16],
    /// The immutable object the key encrypts.
    pub object: [u8; 16],
}

impl KeyContext {
    /// The canonical bytes handed to the provider: a fixed label, then length-prefixed fields.
    pub fn encode(&self) -> Vec<u8> {
        let name = self.index.as_str().as_bytes();
        let mut out = Vec::with_capacity(16 + 1 + name.len() + 32);
        out.extend_from_slice(b"hsearch.kc.v1\0");
        out.push(u8::try_from(name.len()).unwrap_or(u8::MAX));
        out.extend_from_slice(name);
        out.extend_from_slice(&self.domain);
        out.extend_from_slice(&self.object);
        out
    }
}

/// A plaintext data key. It zeroizes on drop, prints as `<redacted>`, cannot be cloned or serialized, and
/// is reachable only through [`SecretKey::expose`], which keeps the places that touch it greppable.
pub struct SecretKey([u8; DEK_LEN]);

impl ZeroizeOnDrop for SecretKey {}

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl SecretKey {
    /// Wraps raw bytes.
    pub fn from_bytes(bytes: [u8; DEK_LEN]) -> Self {
        Self(bytes)
    }

    /// A fresh key from the operating system's CSPRNG.
    pub fn generate() -> Result<Self, ProviderError> {
        let mut bytes = [0u8; DEK_LEN];
        getrandom::fill(&mut bytes).map_err(|_| ProviderError::Unavailable)?;
        Ok(Self(bytes))
    }

    /// The key bytes, for the cipher only.
    pub fn expose(&self) -> &[u8; DEK_LEN] {
        &self.0
    }

    /// Zeroes the key now. The cache calls this on revocation for keys nobody else holds.
    pub fn wipe(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

/// A data key as the provider wrapped it. This is what is stored beside the object.
#[derive(Clone, PartialEq, Eq)]
pub struct WrappedKey {
    key: KeyRef,
    version: KeyVersion,
    bytes: Arc<[u8]>,
    digest: [u8; 32],
}

impl WrappedKey {
    /// Builds a wrapped key, computing its identity digest once.
    pub fn new(key: KeyRef, version: KeyVersion, bytes: Vec<u8>) -> Result<Self, KeyFormatError> {
        if bytes.is_empty() || bytes.len() > MAX_WRAPPED {
            return Err(KeyFormatError("wrapped key size"));
        }
        let mut h = Sha256::new();
        h.update(key.as_str().as_bytes());
        h.update([0]);
        h.update(version.as_str().as_bytes());
        h.update([0]);
        h.update(&bytes);
        Ok(Self {
            key,
            version,
            bytes: bytes.into(),
            digest: h.finalize().into(),
        })
    }

    /// The customer key that wrapped it.
    pub fn key(&self) -> &KeyRef {
        &self.key
    }

    /// The provider's key version that wrapped it.
    pub fn version(&self) -> &KeyVersion {
        &self.version
    }

    /// The opaque wrapped bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// SHA-256 over reference, version and bytes: the envelope's identity for cache keys and audit.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl fmt::Debug for WrappedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WrappedKey({} {:?} {} bytes)",
            self.key,
            self.version,
            self.bytes.len()
        )
    }
}

/// A cooperative cancellation flag shared between a caller and the provider call it started.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels every call holding this token.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Whether it was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Per-call limits.
#[derive(Clone, Debug)]
pub struct CallOptions {
    /// Cancellation.
    pub cancel: CancelToken,
    /// The most a provider may take. A provider that cannot answer in time returns [`ProviderError::Timeout`].
    pub timeout: Duration,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self {
            cancel: CancelToken::new(),
            timeout: Duration::from_secs(2),
        }
    }
}

/// What a provider can say about a customer key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    /// The version new wraps use.
    pub current: KeyVersion,
    /// Whether the key may be used to wrap and unwrap.
    pub usable: bool,
    /// The account the key lives in.
    pub account: String,
}

/// A provider failure, normalized so callers do not parse provider messages. None carry key material.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderError {
    /// The provider is rate limiting. Retry after the hint; do not retry in a tight loop.
    Throttled {
        /// How long the provider asked callers to wait.
        retry_after: Duration,
    },
    /// The caller is not permitted to use the key.
    Denied,
    /// The key was revoked, disabled or scheduled for deletion by its owner.
    Revoked,
    /// The key was destroyed (crypto-shredding): its material is gone and nothing can restore it.
    Destroyed,
    /// No such key or version.
    NotFound,
    /// The wrapped key names a version the provider does not hold or no longer permits.
    VersionMismatch,
    /// The wrapped key was bound to a different index, domain or object.
    ContextMismatch,
    /// The wrapped bytes are not a valid wrap (corrupt or forged).
    Corrupt,
    /// The provider did not answer within the call's timeout.
    Timeout,
    /// The caller cancelled.
    Cancelled,
    /// The provider could not be reached or failed. Retryable.
    Unavailable,
    /// The provider does not support the operation.
    Unsupported,
}

impl ProviderError {
    /// A short stable code for audit records and metrics.
    pub fn code(&self) -> &'static str {
        match self {
            ProviderError::Throttled { .. } => "throttled",
            ProviderError::Denied => "denied",
            ProviderError::Revoked => "revoked",
            ProviderError::Destroyed => "destroyed",
            ProviderError::NotFound => "not_found",
            ProviderError::VersionMismatch => "version_mismatch",
            ProviderError::ContextMismatch => "context_mismatch",
            ProviderError::Corrupt => "corrupt",
            ProviderError::Timeout => "timeout",
            ProviderError::Cancelled => "cancelled",
            ProviderError::Unavailable => "unavailable",
            ProviderError::Unsupported => "unsupported",
        }
    }

    /// Whether trying again later can succeed without anyone changing anything.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            ProviderError::Throttled { .. } | ProviderError::Timeout | ProviderError::Unavailable
        )
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key provider error: {}", self.code())
    }
}

impl std::error::Error for ProviderError {}

/// A key provider: AWS KMS, GCP KMS, Azure Key Vault, Vault transit, or the isolated test provider.
pub trait KeyProvider: Send + Sync {
    /// A short name matching [`KeyRef::provider`].
    fn name(&self) -> &str;

    /// Whether this is a test provider. A production profile refuses one.
    fn is_test(&self) -> bool;

    /// Describes a customer key.
    fn key_info(&self, key: &KeyRef, opts: &CallOptions) -> Result<KeyInfo, ProviderError>;

    /// Wraps `dek` under `key`'s current version, bound to `ctx`.
    fn wrap(
        &self,
        key: &KeyRef,
        ctx: &KeyContext,
        dek: &SecretKey,
        opts: &CallOptions,
    ) -> Result<WrappedKey, ProviderError>;

    /// Unwraps `wrapped` for `ctx`. Fails with [`ProviderError::ContextMismatch`] for any other context.
    fn unwrap(
        &self,
        wrapped: &WrappedKey,
        ctx: &KeyContext,
        opts: &CallOptions,
    ) -> Result<SecretKey, ProviderError>;

    /// Destroys the customer key for good (crypto-shredding): every later wrap and unwrap fails with
    /// [`ProviderError::Destroyed`] and nothing restores it. A provider that cannot destroy a key says
    /// [`ProviderError::Unsupported`]; a provider that only schedules deletion must not report success
    /// until the key is unusable.
    fn destroy_key(&self, key: &KeyRef, opts: &CallOptions) -> Result<(), ProviderError> {
        let _ = (key, opts);
        Err(ProviderError::Unsupported)
    }
}

/// Whether the node runs for real tenants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Local development and tests.
    Development,
    /// Real tenants: test providers are refused.
    Production,
}
