//! Provider qualification: the shared object-contract suite as a reusable artifact (M03.06).
//!
//! Any implementation of [`hsearch_object::MetadataStore`] (and optionally
//! [`hsearch_object::LargeObjectWriter`]) is held to the same predicates and gets a [`Report`]: a
//! named result per check and a JSON rendering a deployment policy can read. A provider is usable
//! only when its report passes; a check that panics, stalls or fails closes the report. The
//! predicates live in `crates/object/tests/support/mod.rs` and are included, not copied, so the
//! fake, the S3 adapter and an external provider are judged by the same code.
use std::panic::{AssertUnwindSafe, catch_unwind};

use hsearch_object::{
    ByteRange, Error, LargeObjectWriter, MetadataStore, Mutation, ObjectKey, Payload, Precondition,
};

#[path = "../../object/tests/support/mod.rs"]
mod histories;
use histories::{context, limited, mutation, run};

/// The suite's version. Bump it when a check is added, removed or made stricter.
pub const SUITE: &str = "hsearch-object-provider/2";

/// What a provider declares it can do. An undeclared capability is refused, not skipped: a
/// provider that cannot delete must say `Unsupported`, and the suite checks that it does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    /// `remove_version` deletes a version (otherwise it must answer [`Error::Unsupported`]).
    pub remove_version: bool,
}

/// How one check ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The predicate held.
    Passed,
    /// The predicate failed, with the reason.
    Failed(String),
    /// Not run, with the reason (the provider stalled earlier).
    Skipped(String),
}

/// One named check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Check {
    /// A stable name.
    pub name: &'static str,
    /// The result.
    pub outcome: Outcome,
}

/// The result of qualifying one provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Report {
    /// The provider's name, as it will appear in the artifact.
    pub provider: String,
    /// The checks, in the order they ran.
    pub checks: Vec<Check>,
    stalled: bool,
}

impl Report {
    fn new(provider: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            checks: Vec::new(),
            stalled: false,
        }
    }

    /// Every check ran and passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|c| c.outcome == Outcome::Passed)
    }

    /// The checks that did not pass.
    #[must_use]
    pub fn failures(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|c| c.outcome != Outcome::Passed)
            .collect()
    }

    /// Whether the named check passed.
    #[must_use]
    pub fn check_passed(&self, name: &str) -> bool {
        self.checks
            .iter()
            .any(|c| c.name == name && c.outcome == Outcome::Passed)
    }

    /// Append another report's checks (the upload suite after the metadata suite).
    pub fn merge(&mut self, other: Report) {
        self.stalled |= other.stalled;
        self.checks.extend(other.checks);
    }

    /// The artifact: a JSON object with the suite version, provider, verdict and every check.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = format!(
            "{{\"suite\":\"{SUITE}\",\"provider\":{},\"passed\":{},\"checks\":[",
            quote(&self.provider),
            self.passed()
        );
        for (i, c) in self.checks.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let (status, why) = match &c.outcome {
                Outcome::Passed => ("passed", None),
                Outcome::Failed(why) => ("failed", Some(why)),
                Outcome::Skipped(why) => ("skipped", Some(why)),
            };
            out.push_str(&format!(
                "{{\"name\":\"{}\",\"outcome\":\"{status}\"",
                c.name
            ));
            if let Some(why) = why {
                out.push_str(&format!(",\"reason\":{}", quote(why)));
            }
            out.push('}');
        }
        out.push_str("]}");
        out
    }

    fn check(&mut self, name: &'static str, body: impl FnOnce() -> Result<(), String>) {
        if self.stalled {
            self.checks.push(Check {
                name,
                outcome: Outcome::Skipped("the provider stalled on an earlier check".into()),
            });
            return;
        }
        let outcome = match catch_unwind(AssertUnwindSafe(body)) {
            Ok(Ok(())) => Outcome::Passed,
            Ok(Err(why)) => Outcome::Failed(why),
            Err(panic) => {
                let text = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_else(|| "the provider panicked".into());
                // A future that never completes trips the suite's bounded wait.
                self.stalled = text.contains("timed out");
                Outcome::Failed(format!("panicked: {text}"))
            }
        };
        self.checks.push(Check { name, outcome });
    }
}

fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn expect<T: PartialEq + std::fmt::Debug>(got: T, want: T, what: &str) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}: got {got:?}, wanted {want:?}"))
    }
}

/// Qualify a conditional metadata store. `name` identifies the provider in the artifact.
pub fn qualify<B: MetadataStore>(name: &str, backend: &B, caps: Capabilities) -> Report {
    let mut r = Report::new(name);
    r.check("conditional_history", || {
        histories::conditional_history(backend).map_err(str::to_owned)
    });
    r.check("stale_update_refused", || {
        histories::stale_update_history(backend).map_err(str::to_owned)
    });
    r.check("ranges_exact_and_key_bound", || ranges(backend));
    r.check("changed_identity_refused", || identity(backend));
    r.check("budget_refused_before_effect", || budget(backend));
    r.check("cancelled_context_creates_nothing", || cancelled(backend));
    r.check("one_winner_among_concurrent_updates", || {
        concurrent(backend)
    });
    r.check("remove_version_declared", || {
        removal(backend, caps.remove_version)
    });
    r
}

/// Qualify a create-only multipart writer.
pub fn qualify_upload<B: LargeObjectWriter>(name: &str, backend: &B) -> Report {
    let mut r = Report::new(name);
    r.check("create_only_upload", || {
        histories::upload_history(backend).map_err(str::to_owned)
    });
    r
}

fn ranges<B: MetadataStore>(b: &B) -> Result<(), String> {
    let request = mutation(b"q-range", b"q/range", b"123456", Precondition::Absent);
    let version = run(b.create(&context(), &request))
        .map_err(|e| format!("create: {e:?}"))?
        .object
        .version;
    let range = |o, l| ByteRange::new(o, l).map_err(|e| format!("{e:?}"));
    expect(
        run(b.read_range(&context(), &request.key, &version, range(5, 1)?)),
        Ok(Payload::new(b"6").map_err(|e| format!("{e:?}"))?),
        "last byte",
    )?;
    for (o, l) in [(6, 1), (1, 6), (u64::MAX - 1, 1)] {
        expect(
            run(b.read_range(&context(), &request.key, &version, range(o, l)?)),
            Err(Error::InvalidRange),
            "a range past the end",
        )?;
    }
    let wrong = ObjectKey::new(b"q/other").map_err(|e| format!("{e:?}"))?;
    expect(
        run(b.read_range(&context(), &wrong, &version, range(0, 1)?)),
        Err(Error::NotFound),
        "a version of another key",
    )
}

fn identity<B: MetadataStore>(b: &B) -> Result<(), String> {
    let request = mutation(
        b"q-identity",
        b"q/identity",
        b"accepted",
        Precondition::Absent,
    );
    let receipt = run(b.create(&context(), &request)).map_err(|e| format!("create: {e:?}"))?;
    expect(
        run(b.resolve(&context(), &request)),
        Ok(receipt),
        "resolving the accepted request",
    )?;
    let changed = Mutation {
        payload: Payload::new(b"changed").map_err(|e| format!("{e:?}"))?,
        ..request.clone()
    };
    expect(
        run(b.resolve(&context(), &changed)),
        Err(Error::IdentityConflict),
        "resolving a changed body under an accepted identity",
    )?;
    expect(
        run(b.compare_exchange(&context(), &changed)),
        Err(Error::IdentityConflict),
        "writing a changed body under an accepted identity",
    )
}

fn budget<B: MetadataStore>(b: &B) -> Result<(), String> {
    let request = mutation(b"q-budget", b"q/budget", b"payload", Precondition::Absent);
    for ctx in [limited(0, 100, 1), limited(100, 0, 1), limited(100, 100, 0)] {
        expect(
            run(b.create(&ctx, &request)),
            Err(Error::BudgetExceeded),
            "a create over its budget",
        )?;
    }
    expect(
        run(b.head(&context(), &request.key)),
        Err(Error::NotFound),
        "an object created over its budget",
    )
}

fn cancelled<B: MetadataStore>(b: &B) -> Result<(), String> {
    let request = mutation(b"q-cancel", b"q/cancel", b"payload", Precondition::Absent);
    let ctx = context();
    ctx.cancel();
    expect(
        run(b.create(&ctx, &request)),
        Err(Error::Cancelled),
        "a create on a cancelled context",
    )?;
    expect(
        run(b.head(&context(), &request.key)),
        Err(Error::NotFound),
        "an object created on a cancelled context",
    )
}

fn concurrent<B: MetadataStore>(b: &B) -> Result<(), String> {
    let first = mutation(b"q-cas-create", b"q/cas", b"old", Precondition::Absent);
    let version = run(b.create(&context(), &first))
        .map_err(|e| format!("create: {e:?}"))?
        .object
        .version;
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let version = version.clone();
                scope.spawn(move || {
                    let id = format!("q-cas-{i}");
                    let m = mutation(
                        id.as_bytes(),
                        b"q/cas",
                        id.as_bytes(),
                        Precondition::Matches(version),
                    );
                    run(b.compare_exchange(&context(), &m)).map(|_| ())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join()).collect()
    });
    let mut won = 0;
    for r in results {
        match r {
            Ok(Ok(())) => won += 1,
            Ok(Err(Error::Conflict)) => {}
            Ok(Err(e)) => return Err(format!("a loser answered {e:?}")),
            Err(_) => return Err("a contender panicked".into()),
        }
    }
    expect(won, 1, "winners among 8 updates from one version")
}

fn removal<B: MetadataStore>(b: &B, supported: bool) -> Result<(), String> {
    let create = mutation(b"q-rm-1", b"q/rm", b"one", Precondition::Absent);
    let v1 = run(b.create(&context(), &create))
        .map_err(|e| format!("create: {e:?}"))?
        .object
        .version;
    let update = mutation(
        b"q-rm-2",
        b"q/rm",
        b"two",
        Precondition::Matches(v1.clone()),
    );
    let v2 = run(b.compare_exchange(&context(), &update))
        .map_err(|e| format!("update: {e:?}"))?
        .object;
    let removed = run(b.remove_version(&context(), &create.key, &v1));
    if !supported {
        return expect(
            removed,
            Err(Error::Unsupported),
            "removal on a store without it",
        );
    }
    if !matches!(removed, Ok(()) | Err(Error::NotFound)) {
        return Err(format!("removal answered {removed:?}"));
    }
    expect(
        run(b.head(&context(), &create.key)),
        Ok(v2),
        "the current version after removing an old one",
    )?;
    let old = run(b.read_range(
        &context(),
        &create.key,
        &v1,
        ByteRange::new(0, 3).map_err(|e| format!("{e:?}"))?,
    ));
    if old.is_ok() {
        return Err("a removed version is still readable".into());
    }
    Ok(())
}
