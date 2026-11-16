//! The qualification suite passes the healthy reference store and refuses providers that are
//! disabled, missing a capability, lying about conditional writes, stalling or panicking.
use std::future::pending;
use std::sync::atomic::{AtomicBool, Ordering};

use hsearch_object::memory::{MemoryLimits, MemoryStore};
use hsearch_object::{
    ByteRange, Error, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey, Operation,
    Payload, Precondition, RequestContext, VersionToken, WriteReceipt,
};
use hsearch_testkit::provider::{Capabilities, Outcome, qualify, qualify_upload};

fn memory() -> MemoryStore {
    MemoryStore::new(MemoryLimits {
        objects: 128,
        versions: 256,
        version_bytes: 64 << 20,
        identities: 256,
        identity_bytes: 64 << 20,
    })
    .unwrap()
}

const CAPS: Capabilities = Capabilities {
    remove_version: true,
};

/// How a deliberately bad provider misbehaves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    /// Every call answers `Unavailable`: a provider that is switched off.
    Disabled,
    /// Conditional writes are not offered.
    NoConditionalWrites,
    /// A stale version is accepted: the provider does not fence.
    NoFence,
    /// Mutations answer with an error no contract names, here `Corrupt`, and never resolve.
    UnknownFailure,
    /// Every call returns a future that never completes.
    Stalled,
    /// Every call panics.
    Panics,
}

struct Bad {
    inner: MemoryStore,
    mode: Mode,
    touched: AtomicBool,
}

impl Bad {
    fn new(mode: Mode) -> Self {
        Self {
            inner: memory(),
            mode,
            touched: AtomicBool::new(false),
        }
    }

    /// The answer a misbehaving provider gives instead of the real one, if any.
    fn intercept<'a, T: Send + 'a>(&self) -> Option<Operation<'a, T>> {
        self.touched.store(true, Ordering::SeqCst);
        match self.mode {
            Mode::Disabled => Some(Box::pin(async { Err(Error::Unavailable) })),
            Mode::UnknownFailure => Some(Box::pin(async { Err(Error::Corrupt) })),
            Mode::Stalled => Some(Box::pin(pending())),
            Mode::Panics => panic!("provider panicked"),
            Mode::NoConditionalWrites | Mode::NoFence => None,
        }
    }
}

impl ObjectBackend for Bad {
    fn head<'a>(&'a self, c: &'a RequestContext, k: &'a ObjectKey) -> Operation<'a, ObjectInfo> {
        self.intercept().unwrap_or_else(|| self.inner.head(c, k))
    }
    fn create<'a>(&'a self, c: &'a RequestContext, r: &'a Mutation) -> Operation<'a, WriteReceipt> {
        self.intercept().unwrap_or_else(|| self.inner.create(c, r))
    }
    fn read_range<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
        r: ByteRange,
    ) -> Operation<'a, Payload> {
        self.intercept()
            .unwrap_or_else(|| self.inner.read_range(c, k, v, r))
    }
}

impl MetadataStore for Bad {
    fn compare_exchange<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        if let Some(answer) = self.intercept() {
            return answer;
        }
        match self.mode {
            Mode::NoConditionalWrites => Box::pin(async { Err(Error::Unsupported) }),
            Mode::NoFence => Box::pin(async move {
                // Apply the write against whatever is current, ignoring the version offered.
                if let Precondition::Matches(_) = &r.precondition
                    && let Ok(info) = self.inner.head(c, &r.key).await
                {
                    let forced = Mutation {
                        precondition: Precondition::Matches(info.version),
                        ..r.clone()
                    };
                    return self.inner.compare_exchange(c, &forced).await;
                }
                self.inner.compare_exchange(c, r).await
            }),
            _ => self.inner.compare_exchange(c, r),
        }
    }
    fn resolve<'a>(
        &'a self,
        c: &'a RequestContext,
        r: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        self.intercept().unwrap_or_else(|| self.inner.resolve(c, r))
    }
    fn remove_version<'a>(
        &'a self,
        c: &'a RequestContext,
        k: &'a ObjectKey,
        v: &'a VersionToken,
    ) -> Operation<'a, ()> {
        self.intercept()
            .unwrap_or_else(|| self.inner.remove_version(c, k, v))
    }
}

#[test]
fn the_reference_store_passes_every_check() {
    let report = qualify("memory", &memory(), CAPS);
    assert!(report.passed(), "{:?}", report.failures());
    assert!(report.checks.len() >= 8);
    let json = report.to_json();
    assert!(json.starts_with(
        "{\"suite\":\"hsearch-object-provider/2\",\"provider\":\"memory\",\"passed\":true"
    ));
    if let Ok(out) = std::env::var("HSEARCH_CONFORMANCE_DIR") {
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(std::path::Path::new(&out).join("memory.json"), json).unwrap();
    }
    let up = qualify_upload("memory", &memory());
    assert!(up.passed(), "{:?}", up.failures());
}

#[test]
fn a_store_that_cannot_delete_passes_only_by_saying_unsupported() {
    // The memory store deletes, so declaring that it cannot is a wrong declaration and fails.
    let wrong = qualify(
        "memory",
        &memory(),
        Capabilities {
            remove_version: false,
        },
    );
    assert!(!wrong.passed());
    assert!(!wrong.check_passed("remove_version_declared"));
}

#[test]
fn a_disabled_provider_fails_closed() {
    let report = qualify("disabled", &Bad::new(Mode::Disabled), CAPS);
    assert!(!report.passed());
    assert!(report.checks.iter().all(|c| c.outcome != Outcome::Passed
        || c.name == "cancelled_context_creates_nothing"
        || c.name == "budget_refused_before_effect"));
}

#[test]
fn a_provider_without_conditional_writes_fails_closed() {
    let report = qualify("no-cas", &Bad::new(Mode::NoConditionalWrites), CAPS);
    assert!(!report.passed());
    assert!(!report.check_passed("conditional_history"));
    assert!(!report.check_passed("one_winner_among_concurrent_updates"));
}

#[test]
fn a_provider_that_does_not_fence_is_refused() {
    let report = qualify("no-fence", &Bad::new(Mode::NoFence), CAPS);
    assert!(!report.passed());
    for name in [
        "conditional_history",
        "stale_update_refused",
        "one_winner_among_concurrent_updates",
    ] {
        assert!(!report.check_passed(name), "{name}: {:?}", report.checks);
    }
}

#[test]
fn an_unknown_failure_is_never_reported_as_a_pass() {
    let report = qualify("corrupt", &Bad::new(Mode::UnknownFailure), CAPS);
    assert!(!report.passed());
    assert!(!report.check_passed("conditional_history"));
}

#[test]
fn a_stalled_provider_is_reported_in_bounded_time_and_the_rest_is_skipped() {
    let started = std::time::Instant::now();
    let report = qualify("stalled", &Bad::new(Mode::Stalled), CAPS);
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    assert!(!report.passed());
    assert!(
        report
            .checks
            .iter()
            .any(|c| matches!(&c.outcome, Outcome::Failed(why) if why.contains("timed out")))
    );
    assert!(
        report
            .checks
            .iter()
            .any(|c| matches!(c.outcome, Outcome::Skipped(_))),
        "later checks are skipped rather than each waiting out the stall"
    );
}

#[test]
fn a_provider_that_panics_is_reported_not_propagated() {
    let report = qualify("panics", &Bad::new(Mode::Panics), CAPS);
    assert!(!report.passed());
    assert!(
        report.checks.iter().any(
            |c| matches!(&c.outcome, Outcome::Failed(why) if why.contains("provider panicked"))
        )
    );
    // The artifact is still well formed, with the failure quoted.
    let json = report.to_json();
    assert!(json.contains("\"passed\":false") && json.contains("\"outcome\":\"failed\""));
}
