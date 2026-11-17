//! M05.07 provider conformance: the suite passes for the test provider, and it fails for providers that
//! break the contract (each control below breaks exactly one rule).

use std::sync::Arc;

use hsearch_security::conformance::{OwnerActions, Verdict, run};
use hsearch_security::keys::{
    CallOptions, KeyContext, KeyInfo, KeyProvider, KeyRef, KeyVersion, ProviderError, SecretKey,
    WrappedKey,
};
use hsearch_security::test_kms::TestKms;

fn key(n: &str) -> KeyRef {
    KeyRef::new(&format!("test/acct/{n}")).unwrap()
}

struct Owner(Arc<TestKms>);

impl OwnerActions for Owner {
    fn revoke(&self, k: &KeyRef) -> bool {
        self.0.revoke(k);
        true
    }
    fn restore(&self, k: &KeyRef) -> bool {
        self.0.restore(k);
        true
    }
    fn rotate(&self, k: &KeyRef) -> Option<KeyVersion> {
        self.0.rotate(k).ok()
    }
}

fn kms() -> Arc<TestKms> {
    let k = Arc::new(TestKms::new());
    k.create_key(&key("a")).unwrap();
    k
}

#[test]
fn the_test_provider_conforms_with_and_without_owner_actions() {
    let k = kms();
    let report = run(
        k.as_ref(),
        &key("a"),
        &key("missing"),
        Some(&Owner(k.clone())),
    );
    println!("{report}");
    assert!(report.conforms() && report.fully_conforms(), "{report}");
    assert!(report.checks.len() >= 13);
    assert!(report.checks.iter().all(|(_, v)| *v == Verdict::Pass));

    let k = kms();
    let report = run(k.as_ref(), &key("a"), &key("missing"), None);
    assert!(report.conforms());
    assert!(
        !report.fully_conforms(),
        "skipped checks must not qualify a provider"
    );
    let not_run = report
        .checks
        .iter()
        .filter(|(_, v)| matches!(v, Verdict::NotRun(_)))
        .count();
    assert_eq!(not_run, 2);
    assert!(report.to_string().contains("NOT-RUN"));
}

/// A provider with no credentials: every call is denied. It must not pass anything.
struct NoCredentials;

impl KeyProvider for NoCredentials {
    fn name(&self) -> &str {
        "test"
    }
    fn is_test(&self) -> bool {
        true
    }
    fn key_info(&self, _: &KeyRef, _: &CallOptions) -> Result<KeyInfo, ProviderError> {
        Err(ProviderError::Denied)
    }
    fn wrap(
        &self,
        _: &KeyRef,
        _: &KeyContext,
        _: &SecretKey,
        _: &CallOptions,
    ) -> Result<WrappedKey, ProviderError> {
        Err(ProviderError::Denied)
    }
    fn unwrap(
        &self,
        _: &WrappedKey,
        _: &KeyContext,
        _: &CallOptions,
    ) -> Result<SecretKey, ProviderError> {
        Err(ProviderError::Denied)
    }
}

#[test]
fn a_provider_that_cannot_wrap_conforms_to_nothing() {
    let report = run(&NoCredentials, &key("a"), &key("missing"), None);
    assert!(!report.conforms());
    assert_eq!(report.failures().len(), 1);
}

/// Wraps by storing the data key in the clear next to nothing else, and ignores context on unwrap.
struct Broken {
    inner: Arc<TestKms>,
    mode: u8,
}

impl KeyProvider for Broken {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn is_test(&self) -> bool {
        true
    }
    fn key_info(&self, k: &KeyRef, o: &CallOptions) -> Result<KeyInfo, ProviderError> {
        self.inner.key_info(k, o)
    }
    fn wrap(
        &self,
        k: &KeyRef,
        c: &KeyContext,
        d: &SecretKey,
        o: &CallOptions,
    ) -> Result<WrappedKey, ProviderError> {
        if self.mode == 1 {
            // Plaintext "wrap".
            return WrappedKey::new(
                k.clone(),
                KeyVersion::new("v1").unwrap(),
                d.expose().to_vec(),
            )
            .map_err(|_| ProviderError::Corrupt);
        }
        self.inner.wrap(k, c, d, o)
    }
    fn unwrap(
        &self,
        w: &WrappedKey,
        c: &KeyContext,
        o: &CallOptions,
    ) -> Result<SecretKey, ProviderError> {
        match self.mode {
            1 => Ok(SecretKey::from_bytes(
                w.bytes().try_into().map_err(|_| ProviderError::Corrupt)?,
            )),
            // Ignores the context the wrap was bound to.
            2 => {
                let mut original = c.clone();
                original.object = [1; 16];
                original.index = hsearch_security::ids::IndexId::new("conformance").unwrap();
                original.domain = [0xC0; 16];
                self.inner.unwrap(w, &original, o)
            }
            _ => self.inner.unwrap(w, c, o),
        }
    }
}

#[test]
fn providers_that_break_the_contract_fail_the_check_that_names_the_rule() {
    let plaintext = Broken {
        inner: kms(),
        mode: 1,
    };
    let report = run(&plaintext, &key("a"), &key("missing"), None);
    assert!(!report.conforms());
    assert!(
        report
            .failures()
            .contains(&"wrapped bytes do not contain the data key")
    );

    let no_binding = Broken {
        inner: kms(),
        mode: 2,
    };
    let report = run(&no_binding, &key("a"), &key("missing"), None);
    assert!(!report.conforms());
    assert!(
        report
            .failures()
            .contains(&"unwrap for another object is refused")
    );
}
