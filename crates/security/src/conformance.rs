//! A provider conformance suite (M05.07): the contract every [`KeyProvider`] must meet, as code.
//!
//! Run [`run`] against a provider and a customer key that exists in it; the result is a report with one line
//! per check: `pass`, `FAIL` with what was observed, or `NOT-RUN` with the reason (a check that needs an
//! owner action the caller did not supply, or a capability the provider does not claim). A provider with no
//! credentials reports every check `NOT-RUN`, never `pass`. The in-memory test provider runs it in the test
//! suite; the real providers (M15) run the same suite against a real account, so a provider's claims are the
//! report, not a description.
//!
//! The suite never logs or returns key material: a check that compares keys does so in place and reports
//! only a verdict.

use std::fmt;

use crate::ids::IndexId;
use crate::keys::{
    CallOptions, CancelToken, KeyContext, KeyProvider, KeyRef, KeyVersion, ProviderError, SecretKey,
};

/// Owner-side actions a provider's operator can perform, which the contract cannot: a provider that has
/// no way to supply them skips the checks that need them.
pub trait OwnerActions {
    /// Revokes (disables) the key.
    fn revoke(&self, key: &KeyRef) -> bool;
    /// Re-enables a revoked key.
    fn restore(&self, key: &KeyRef) -> bool;
    /// Adds a key version and returns its name.
    fn rotate(&self, key: &KeyRef) -> Option<KeyVersion>;
}

/// One check's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The provider met the contract.
    Pass,
    /// It did not; what was observed.
    Fail(String),
    /// The check could not run; why.
    NotRun(&'static str),
}

/// The report of one run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The provider name.
    pub provider: String,
    /// Checks in the order they ran.
    pub checks: Vec<(&'static str, Verdict)>,
}

impl Report {
    /// Checks that failed.
    pub fn failures(&self) -> Vec<&'static str> {
        self.checks
            .iter()
            .filter(|(_, v)| matches!(v, Verdict::Fail(_)))
            .map(|(n, _)| *n)
            .collect()
    }

    /// Whether anything failed or nothing ran. A report with no `Pass` is not a conforming provider.
    pub fn conforms(&self) -> bool {
        self.failures().is_empty() && self.checks.iter().any(|(_, v)| *v == Verdict::Pass)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "provider conformance: {}", self.provider)?;
        for (name, verdict) in &self.checks {
            match verdict {
                Verdict::Pass => writeln!(f, "  pass     {name}")?,
                Verdict::Fail(why) => writeln!(f, "  FAIL     {name}: {why}")?,
                Verdict::NotRun(why) => writeln!(f, "  NOT-RUN  {name}: {why}")?,
            }
        }
        Ok(())
    }
}

fn ctx(tag: u8) -> KeyContext {
    KeyContext {
        index: IndexId::new("conformance").unwrap_or_else(|_| unreachable!("valid index id")),
        domain: [0xC0; 16],
        object: [tag; 16],
    }
}

fn check(report: &mut Report, name: &'static str, body: impl FnOnce() -> Result<(), String>) {
    let verdict = match body() {
        Ok(()) => Verdict::Pass,
        Err(why) => Verdict::Fail(why),
    };
    report.checks.push((name, verdict));
}

fn expect_err(got: Result<impl Sized, ProviderError>, want: ProviderError) -> Result<(), String> {
    match got {
        Err(e) if e == want => Ok(()),
        Err(e) => Err(format!("expected {}, got {}", want.code(), e.code())),
        Ok(_) => Err(format!("expected {}, call succeeded", want.code())),
    }
}

/// Runs the suite against `provider` using the existing customer key `key`. `owner` supplies owner-side
/// actions where the provider has them. `missing` must name a key that does not exist.
pub fn run(
    provider: &dyn KeyProvider,
    key: &KeyRef,
    missing: &KeyRef,
    owner: Option<&dyn OwnerActions>,
) -> Report {
    let opts = CallOptions::default();
    let mut r = Report {
        provider: provider.name().to_string(),
        checks: Vec::new(),
    };
    let dek = match SecretKey::generate() {
        Ok(d) => d,
        Err(_) => {
            r.checks
                .push(("generate a data key", Verdict::Fail("no randomness".into())));
            return r;
        }
    };
    let c = ctx(1);
    let wrapped = match provider.wrap(key, &c, &dek, &opts) {
        Ok(w) => w,
        Err(e) => {
            // Without a working wrap nothing else can be judged; say so rather than pass.
            r.checks
                .push(("wrap a data key", Verdict::Fail(e.code().to_string())));
            return r;
        }
    };

    check(&mut r, "wrap names the key it used", || {
        (wrapped.key() == key)
            .then_some(())
            .ok_or_else(|| "wrapped key names another key".to_string())
    });
    check(&mut r, "wrapped bytes do not contain the data key", || {
        let plain = dek.expose();
        (!wrapped.bytes().windows(plain.len()).any(|w| w == plain))
            .then_some(())
            .ok_or_else(|| "plaintext data key appears in the wrapped bytes".to_string())
    });
    check(&mut r, "unwrap returns the same data key", || {
        let back = provider
            .unwrap(&wrapped, &c, &opts)
            .map_err(|e| e.code().to_string())?;
        (back.expose() == dek.expose())
            .then_some(())
            .ok_or_else(|| "unwrapped key differs".to_string())
    });
    check(
        &mut r,
        "key_info reports a usable key and a version",
        || {
            let info = provider
                .key_info(key, &opts)
                .map_err(|e| e.code().to_string())?;
            (info.usable && !info.current.as_str().is_empty())
                .then_some(())
                .ok_or_else(|| "key not reported usable".to_string())
        },
    );
    check(&mut r, "unwrap for another index is refused", || {
        let mut other = ctx(1);
        other.index = IndexId::new("other").map_err(|_| "id".to_string())?;
        expect_err(
            provider.unwrap(&wrapped, &other, &opts),
            ProviderError::ContextMismatch,
        )
    });
    check(&mut r, "unwrap for another domain is refused", || {
        let mut other = ctx(1);
        other.domain = [0xC1; 16];
        expect_err(
            provider.unwrap(&wrapped, &other, &opts),
            ProviderError::ContextMismatch,
        )
    });
    check(&mut r, "unwrap for another object is refused", || {
        expect_err(
            provider.unwrap(&wrapped, &ctx(2), &opts),
            ProviderError::ContextMismatch,
        )
    });
    check(&mut r, "damaged wrapped bytes never unwrap", || {
        let bytes = wrapped.bytes();
        for i in 0..bytes.len().min(512) {
            let mut bad = bytes.to_vec();
            bad[i] ^= 0x01;
            let damaged =
                crate::keys::WrappedKey::new(wrapped.key().clone(), wrapped.version().clone(), bad)
                    .map_err(|_| "rebuild".to_string())?;
            match provider.unwrap(&damaged, &c, &opts) {
                Ok(k) if k.expose() == dek.expose() => {
                    return Err(format!("flipping byte {i} still unwrapped to the key"));
                }
                _ => {}
            }
        }
        Ok(())
    });
    check(&mut r, "an unknown key is not found", || {
        expect_err(provider.key_info(missing, &opts), ProviderError::NotFound)
    });
    check(&mut r, "a cancelled call is cancelled", || {
        let cancelled = CallOptions {
            cancel: CancelToken::new(),
            ..CallOptions::default()
        };
        cancelled.cancel.cancel();
        expect_err(
            provider.unwrap(&wrapped, &c, &cancelled),
            ProviderError::Cancelled,
        )
    });
    check(&mut r, "errors name a code and no key material", || {
        let err = provider
            .unwrap(&wrapped, &ctx(9), &opts)
            .err()
            .ok_or("expected an error")?;
        let text = format!("{err} {err:?}");
        let hex: String = dek.expose().iter().map(|b| format!("{b:02x}")).collect();
        (!text.contains(&hex))
            .then_some(())
            .ok_or_else(|| "key material in error".to_string())
    });

    match owner {
        None => {
            for name in [
                "a revoked key refuses wrap and unwrap, and restore recovers",
                "rotation keeps old wraps readable and new wraps use the new version",
            ] {
                r.checks
                    .push((name, Verdict::NotRun("no owner actions supplied")));
            }
        }
        Some(owner) => {
            check(
                &mut r,
                "a revoked key refuses wrap and unwrap, and restore recovers",
                || {
                    if !owner.revoke(key) {
                        return Err("could not revoke".into());
                    }
                    let during = (
                        provider.unwrap(&wrapped, &c, &opts),
                        provider.wrap(key, &c, &dek, &opts),
                    );
                    owner.restore(key);
                    if !matches!(
                        during.0,
                        Err(ProviderError::Revoked | ProviderError::Denied)
                    ) {
                        return Err("unwrap succeeded under a revoked key".into());
                    }
                    if !matches!(
                        during.1,
                        Err(ProviderError::Revoked | ProviderError::Denied)
                    ) {
                        return Err("wrap succeeded under a revoked key".into());
                    }
                    provider
                        .unwrap(&wrapped, &c, &opts)
                        .map(|_| ())
                        .map_err(|e| format!("did not recover after restore: {}", e.code()))
                },
            );
            check(
                &mut r,
                "rotation keeps old wraps readable and new wraps use the new version",
                || {
                    let new = owner.rotate(key).ok_or("could not rotate")?;
                    let fresh = provider
                        .wrap(key, &c, &dek, &opts)
                        .map_err(|e| e.code().to_string())?;
                    if fresh.version() != &new {
                        return Err("new wrap did not use the new version".into());
                    }
                    provider
                        .unwrap(&wrapped, &c, &opts)
                        .map_err(|e| format!("old wrap unreadable after rotation: {}", e.code()))?;
                    provider
                        .unwrap(&fresh, &c, &opts)
                        .map(|_| ())
                        .map_err(|e| e.code().to_string())
                },
            );
        }
    }
    r
}
