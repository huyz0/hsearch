//! Identity, authorization and the key-provider contract (M05.01, M05.02).
//!
//! * [`identity`] and [`x509`]: who is calling. A [`identity::VerifiedPrincipal`] can only come from an
//!   [`identity::IdentityProvider`]; certificate verification has no off switch.
//! * [`policy`]: what that caller may do. Policy, quota and key scope are one index's, never several.
//! * [`keys`], [`manager`], [`audit`] and the `test-kms` feature's isolated provider: wrapping data keys under customer keys without this
//!   process ever holding a customer key, with a bounded cache that keeps the provider off the request path.
//!
//! This crate does not move bytes and has no network code. The peer listener, the object-format encryption
//! (M05.03) and rotation state machines (M05.05) build on it.

pub mod audit;
pub mod block;
pub mod clock;
#[cfg(feature = "dev-ca")]
pub mod dev_ca;
pub mod identity;
pub mod ids;
pub mod keys;
pub mod manager;
pub mod policy;
pub mod readiness;
#[cfg(feature = "test-kms")]
#[cfg(feature = "test-kms")]
pub mod test_kms;
pub mod x509;
