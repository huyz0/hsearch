//! An isolated, in-process key provider for tests and development. It holds real AES-256-GCM key-encryption
//! keys in memory, supports rotation and revocation, injects the failures a real KMS has (throttling, denial,
//! timeouts) and counts every call, so the cache's call budget can be asserted.
//!
//! It reports [`KeyProvider::is_test`] as `true`, and [`crate::manager::KeyManager`] refuses it under
//! [`Profile::Production`](crate::keys::Profile).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::keys::{
    CallOptions, KeyContext, KeyInfo, KeyProvider, KeyRef, KeyVersion, ProviderError, SecretKey,
    WrappedKey,
};

const NONCE: usize = 12;
const TAG: usize = 16;
const CTX_DIGEST: usize = 16;

/// A failure to inject into the next calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Rate limited.
    Throttle(Duration),
    /// Access denied.
    Deny,
    /// No answer.
    Timeout,
    /// The provider is down.
    Unavailable,
}

struct Kek {
    versions: Vec<[u8; 32]>,
    disabled: BTreeSet<usize>,
    revoked: bool,
}

impl Drop for Kek {
    fn drop(&mut self) {
        for v in &mut self.versions {
            v.zeroize();
        }
    }
}

#[derive(Default)]
struct State {
    keys: HashMap<KeyRef, Kek>,
    faults: VecDeque<Fault>,
    latency: Duration,
}

/// The test provider. Its name is `test`; key references look like `test/acct/<key>`.
#[derive(Default)]
pub struct TestKms {
    state: Mutex<State>,
    wraps: AtomicU64,
    unwraps: AtomicU64,
    infos: AtomicU64,
}

impl TestKms {
    /// An empty provider.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Creates a customer key at version 1.
    pub fn create_key(&self, key: &KeyRef) -> Result<(), ProviderError> {
        if key.provider() != "test" {
            return Err(ProviderError::NotFound);
        }
        let mut kek = [0u8; 32];
        getrandom::fill(&mut kek).map_err(|_| ProviderError::Unavailable)?;
        self.lock().keys.insert(
            key.clone(),
            Kek {
                versions: vec![kek],
                disabled: BTreeSet::new(),
                revoked: false,
            },
        );
        Ok(())
    }

    /// Adds a version; new wraps use it, old wraps still unwrap.
    pub fn rotate(&self, key: &KeyRef) -> Result<KeyVersion, ProviderError> {
        let mut kek = [0u8; 32];
        getrandom::fill(&mut kek).map_err(|_| ProviderError::Unavailable)?;
        let mut state = self.lock();
        let entry = state.keys.get_mut(key).ok_or(ProviderError::NotFound)?;
        entry.versions.push(kek);
        Ok(version_name(entry.versions.len() - 1))
    }

    /// The owner revokes the key: every wrap and unwrap under it fails from now on.
    pub fn revoke(&self, key: &KeyRef) {
        if let Some(entry) = self.lock().keys.get_mut(key) {
            entry.revoked = true;
        }
    }

    /// The owner re-enables a revoked key.
    pub fn restore(&self, key: &KeyRef) {
        if let Some(entry) = self.lock().keys.get_mut(key) {
            entry.revoked = false;
        }
    }

    /// Disables one version, as a retired key version.
    pub fn disable_version(&self, key: &KeyRef, version: &KeyVersion) {
        if let (Some(entry), Some(n)) = (self.lock().keys.get_mut(key), parse_version(version)) {
            entry.disabled.insert(n);
        }
    }

    /// Makes the next `count` calls fail with `fault`.
    pub fn inject(&self, fault: Fault, count: usize) {
        let mut state = self.lock();
        for _ in 0..count {
            state.faults.push_back(fault);
        }
    }

    /// Makes every call take this long (bounded by the call's own timeout).
    pub fn set_latency(&self, latency: Duration) {
        self.lock().latency = latency;
    }

    /// Wrap calls made, including failed ones.
    pub fn wrap_calls(&self) -> u64 {
        self.wraps.load(Ordering::Relaxed)
    }

    /// Unwrap calls made, including failed ones.
    pub fn unwrap_calls(&self) -> u64 {
        self.unwraps.load(Ordering::Relaxed)
    }

    /// All provider calls made.
    pub fn total_calls(&self) -> u64 {
        self.wrap_calls() + self.unwrap_calls() + self.infos.load(Ordering::Relaxed)
    }

    /// The plaintext of a wrapped key, decrypted without checks. For tests asserting that the plaintext
    /// does not appear in logs; it exists so a test can know what to search for.
    pub fn peek_for_tests(&self, wrapped: &WrappedKey, ctx: &KeyContext) -> Option<[u8; 32]> {
        let key = self.unwrap(wrapped, ctx, &CallOptions::default()).ok()?;
        Some(*key.expose())
    }

    /// The common prelude of every call: cancellation, injected faults, latency.
    fn enter(&self, opts: &CallOptions) -> Result<(), ProviderError> {
        if opts.cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        let (fault, latency) = {
            let mut state = self.lock();
            (state.faults.pop_front(), state.latency)
        };
        match fault {
            Some(Fault::Throttle(retry_after)) => {
                return Err(ProviderError::Throttled { retry_after });
            }
            Some(Fault::Deny) => return Err(ProviderError::Denied),
            Some(Fault::Timeout) => return Err(ProviderError::Timeout),
            Some(Fault::Unavailable) => return Err(ProviderError::Unavailable),
            None => {}
        }
        if !latency.is_zero() {
            if latency > opts.timeout {
                std::thread::sleep(opts.timeout);
                return Err(ProviderError::Timeout);
            }
            std::thread::sleep(latency);
            if opts.cancel.is_cancelled() {
                return Err(ProviderError::Cancelled);
            }
        }
        Ok(())
    }
}

fn version_name(index: usize) -> KeyVersion {
    KeyVersion::new(&format!("v{}", index + 1))
        .unwrap_or_else(|_| unreachable!("v<n> is a valid version"))
}

fn parse_version(version: &KeyVersion) -> Option<usize> {
    version
        .as_str()
        .strip_prefix('v')?
        .parse::<usize>()
        .ok()?
        .checked_sub(1)
}

fn aad(key: &KeyRef, version: &KeyVersion, ctx_bytes: &[u8]) -> Vec<u8> {
    let mut aad = Vec::new();
    aad.extend_from_slice(key.as_str().as_bytes());
    aad.push(0);
    aad.extend_from_slice(version.as_str().as_bytes());
    aad.push(0);
    aad.extend_from_slice(ctx_bytes);
    aad
}

fn ctx_digest(ctx_bytes: &[u8]) -> [u8; CTX_DIGEST] {
    let full = Sha256::digest(ctx_bytes);
    let mut out = [0u8; CTX_DIGEST];
    out.copy_from_slice(&full[..CTX_DIGEST]);
    out
}

impl KeyProvider for TestKms {
    fn name(&self) -> &str {
        "test"
    }

    fn is_test(&self) -> bool {
        true
    }

    fn key_info(&self, key: &KeyRef, opts: &CallOptions) -> Result<KeyInfo, ProviderError> {
        self.infos.fetch_add(1, Ordering::Relaxed);
        self.enter(opts)?;
        let state = self.lock();
        let entry = state.keys.get(key).ok_or(ProviderError::NotFound)?;
        let latest = entry.versions.len() - 1;
        Ok(KeyInfo {
            current: version_name(latest),
            usable: !entry.revoked && !entry.disabled.contains(&latest),
            account: key.account().to_string(),
        })
    }

    fn wrap(
        &self,
        key: &KeyRef,
        ctx: &KeyContext,
        dek: &SecretKey,
        opts: &CallOptions,
    ) -> Result<WrappedKey, ProviderError> {
        self.wraps.fetch_add(1, Ordering::Relaxed);
        self.enter(opts)?;
        let (version, mut kek) = {
            let state = self.lock();
            let entry = state.keys.get(key).ok_or(ProviderError::NotFound)?;
            if entry.revoked {
                return Err(ProviderError::Revoked);
            }
            let latest = entry.versions.len() - 1;
            if entry.disabled.contains(&latest) {
                return Err(ProviderError::Revoked);
            }
            (version_name(latest), entry.versions[latest])
        };
        let ctx_bytes = ctx.encode();
        let mut nonce = [0u8; NONCE];
        getrandom::fill(&mut nonce).map_err(|_| ProviderError::Unavailable)?;
        let cipher = Aes256Gcm::new_from_slice(&kek).map_err(|_| ProviderError::Unavailable);
        kek.zeroize();
        let sealed = cipher?
            .encrypt(
                &Nonce::try_from(&nonce[..]).map_err(|_| ProviderError::Corrupt)?,
                Payload {
                    msg: dek.expose(),
                    aad: &aad(key, &version, &ctx_bytes),
                },
            )
            .map_err(|_| ProviderError::Unavailable)?;
        let mut bytes = Vec::with_capacity(CTX_DIGEST + NONCE + sealed.len());
        bytes.extend_from_slice(&ctx_digest(&ctx_bytes));
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&sealed);
        WrappedKey::new(key.clone(), version, bytes).map_err(|_| ProviderError::Corrupt)
    }

    fn unwrap(
        &self,
        wrapped: &WrappedKey,
        ctx: &KeyContext,
        opts: &CallOptions,
    ) -> Result<SecretKey, ProviderError> {
        self.unwraps.fetch_add(1, Ordering::Relaxed);
        self.enter(opts)?;
        let version_index =
            parse_version(wrapped.version()).ok_or(ProviderError::VersionMismatch)?;
        let mut kek = {
            let state = self.lock();
            let entry = state
                .keys
                .get(wrapped.key())
                .ok_or(ProviderError::NotFound)?;
            if entry.revoked {
                return Err(ProviderError::Revoked);
            }
            let kek = entry
                .versions
                .get(version_index)
                .ok_or(ProviderError::VersionMismatch)?;
            if entry.disabled.contains(&version_index) {
                return Err(ProviderError::VersionMismatch);
            }
            *kek
        };
        let bytes = wrapped.bytes();
        if bytes.len() != CTX_DIGEST + NONCE + 32 + TAG {
            kek.zeroize();
            return Err(ProviderError::Corrupt);
        }
        let ctx_bytes = ctx.encode();
        if bytes[..CTX_DIGEST] != ctx_digest(&ctx_bytes) {
            kek.zeroize();
            return Err(ProviderError::ContextMismatch);
        }
        let cipher = Aes256Gcm::new_from_slice(&kek).map_err(|_| ProviderError::Unavailable);
        kek.zeroize();
        let plain = cipher?
            .decrypt(
                &Nonce::try_from(&bytes[CTX_DIGEST..CTX_DIGEST + NONCE])
                    .map_err(|_| ProviderError::Corrupt)?,
                Payload {
                    msg: &bytes[CTX_DIGEST + NONCE..],
                    aad: &aad(wrapped.key(), wrapped.version(), &ctx_bytes),
                },
            )
            .map_err(|_| ProviderError::Corrupt);
        let mut plain = plain?;
        let mut dek = [0u8; 32];
        let ok = plain.len() == 32;
        if ok {
            dek.copy_from_slice(&plain);
        }
        plain.zeroize();
        if !ok {
            return Err(ProviderError::Corrupt);
        }
        let key = SecretKey::from_bytes(dek);
        dek.zeroize();
        Ok(key)
    }
}
