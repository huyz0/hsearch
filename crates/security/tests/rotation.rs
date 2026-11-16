//! M05.05 rotation and revocation: the rewrap-overlay and key-retirement state machine, enumerated over
//! every short history against an independent oracle; policy freshness that fails closed, with its measured
//! exposure; and crypto-shredding with its audit trail.

use std::sync::Arc;
use std::time::Duration;

use hsearch_security::audit::{AuditOp, MemoryAuditSink};
use hsearch_security::clock::ManualClock;
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{
    CallOptions, KeyContext, KeyProvider, KeyRef, KeyVersion, Profile, ProviderError, SecretKey,
};
use hsearch_security::manager::{CachePolicy, KeyAllowlist, KeyError, KeyManager};
use hsearch_security::rotation::{
    Envelope, GateError, Inventory, Object, PolicyGate, RotationError,
};
use hsearch_security::test_kms::TestKms;

fn key(name: &str) -> KeyRef {
    KeyRef::new(&format!("test/acct/{name}")).unwrap()
}

fn env(name: &str, version: &str) -> Envelope {
    Envelope {
        key: key(name),
        version: KeyVersion::new(version).unwrap(),
    }
}

const O1: Object = [1; 16];
const O2: Object = [2; 16];

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

// ---- policy freshness -------------------------------------------------------------------------------

#[test]
fn a_gate_with_no_policy_refuses_everything() {
    let gate = PolicyGate::new(secs(30));
    assert_eq!(gate.check(secs(0)), Err(GateError::NeverAccepted));
}

#[test]
fn a_policy_is_fresh_for_exactly_its_allowed_age_and_then_fails_closed() {
    let mut gate = PolicyGate::new(secs(30));
    gate.accept(7, false, secs(100)).unwrap();
    assert_eq!(gate.check(secs(100)), Ok(7));
    assert_eq!(gate.check(secs(130)), Ok(7));
    assert_eq!(
        gate.check(secs(130) + Duration::from_nanos(1)),
        Err(GateError::Stale)
    );
    // Time moving backwards (a clock the caller got wrong) is not trusted: it never extends freshness.
    assert_eq!(gate.check(secs(50)), Err(GateError::Stale));
    assert_eq!(gate.accept(7, false, secs(50)), Err(GateError::Regressed));
}

#[test]
fn a_revocation_is_refused_immediately_and_a_lower_generation_cannot_undo_it() {
    let mut gate = PolicyGate::new(secs(30));
    gate.accept(7, false, secs(0)).unwrap();
    gate.accept(8, true, secs(1)).unwrap();
    assert_eq!(gate.check(secs(1)), Err(GateError::Revoked));
    assert_eq!(gate.accept(7, false, secs(2)), Err(GateError::Regressed));
    assert_eq!(gate.check(secs(2)), Err(GateError::Revoked));
    // The same generation refreshes freshness without changing what it says.
    gate.accept(8, true, secs(20)).unwrap();
    assert_eq!(gate.check(secs(20)), Err(GateError::Revoked));
    // Only a higher generation grants again.
    gate.accept(9, false, secs(21)).unwrap();
    assert_eq!(gate.check(secs(21)), Ok(9));
    // A repeat of a generation that changes its meaning is refused.
    assert_eq!(gate.accept(9, true, secs(22)), Err(GateError::Regressed));
}

#[test]
fn a_node_cut_off_from_policy_serves_a_revoked_index_for_at_most_the_allowed_age() {
    // The authority revokes at t = 100 s. A node whose policy channel is cut at partition time p last
    // refreshed at p; measure how long after the revocation it keeps serving, for every p.
    let max_age = secs(30);
    let mut worst = Duration::ZERO;
    for p in 0..=100u64 {
        let mut gate = PolicyGate::new(max_age);
        for t in (0..=p).step_by(5) {
            gate.accept(1, false, secs(t)).unwrap();
        }
        let last_refresh = p / 5 * 5;
        let mut serving_until = 0;
        for t in 0..=200u64 {
            if gate.check(secs(t)).is_ok() {
                serving_until = t;
            }
        }
        assert_eq!(serving_until, last_refresh + 30);
        worst = worst.max(secs(serving_until).saturating_sub(secs(100)));
    }
    assert!(worst <= max_age, "measured exposure {worst:?}");
    // A node that does receive the revocation refuses at once.
    let mut gate = PolicyGate::new(max_age);
    gate.accept(1, false, secs(95)).unwrap();
    gate.accept(2, true, secs(100)).unwrap();
    assert_eq!(gate.check(secs(100)), Err(GateError::Revoked));
}

// ---- the inventory ------------------------------------------------------------------------------------

fn world() -> Inventory {
    let mut inv = Inventory::new();
    inv.add_object(O1, env("a", "v1")).unwrap();
    inv.add_object(O2, env("a", "v1")).unwrap();
    inv
}

#[test]
fn an_overlay_is_staged_then_verified_then_published_and_only_then_selected() {
    let mut inv = world();
    assert_eq!(inv.selected(O1).unwrap(), &env("a", "v1"));
    inv.stage(O1, env("a", "v2")).unwrap();
    assert_eq!(
        inv.selected(O1).unwrap(),
        &env("a", "v1"),
        "staged is not selected"
    );
    assert_eq!(inv.publish(O1), Err(RotationError::NotVerified));
    inv.verified(O1).unwrap();
    assert_eq!(
        inv.selected(O1).unwrap(),
        &env("a", "v1"),
        "verified is not selected"
    );
    inv.publish(O1).unwrap();
    assert_eq!(inv.selected(O1).unwrap(), &env("a", "v2"));
    // The neighbour is untouched, and unknown objects are refused.
    assert_eq!(inv.selected(O2).unwrap(), &env("a", "v1"));
    assert_eq!(
        inv.stage([9; 16], env("a", "v2")),
        Err(RotationError::UnknownObject)
    );
    assert_eq!(inv.verified(O2), Err(RotationError::NotStaged));
}

#[test]
fn a_key_version_is_not_retired_because_new_writes_use_another() {
    let mut inv = world();
    // New objects use v2; the old ones still depend on v1.
    inv.add_object([3; 16], env("a", "v2")).unwrap();
    assert_eq!(
        inv.retire(&key("a"), &KeyVersion::new("v1").unwrap()),
        Err(RotationError::InUse(2))
    );
    // Rewrap both; v1 is now unused by any selection.
    for o in [O1, O2] {
        inv.stage(o, env("a", "v2")).unwrap();
        inv.verified(o).unwrap();
        inv.publish(o).unwrap();
    }
    assert_eq!(inv.users(&env("a", "v1")), 0);
    inv.retire(&key("a"), &KeyVersion::new("v1").unwrap())
        .unwrap();
    assert!(inv.is_retired(&key("a"), &KeyVersion::new("v1").unwrap()));
    // The versions still in use cannot be retired.
    assert_eq!(
        inv.retire(&key("a"), &KeyVersion::new("v2").unwrap()),
        Err(RotationError::InUse(3))
    );
}

#[test]
fn a_reader_pinned_before_the_rewrap_keeps_its_envelope_until_it_is_released() {
    let mut inv = world();
    let root = inv.pin(&[O1]).unwrap();
    inv.stage(O1, env("a", "v2")).unwrap();
    inv.verified(O1).unwrap();
    inv.publish(O1).unwrap();
    assert_eq!(inv.root_envelope(root, O1).unwrap(), &env("a", "v1"));
    // O2 still uses v1 as well, so make the point on a world with only O1.
    inv.remove_object(O2).unwrap();
    assert_eq!(inv.users(&env("a", "v1")), 1, "the pinned root");
    assert_eq!(
        inv.retire(&key("a"), &KeyVersion::new("v1").unwrap()),
        Err(RotationError::InUse(1))
    );
    inv.release(root).unwrap();
    inv.retire(&key("a"), &KeyVersion::new("v1").unwrap())
        .unwrap();
    assert_eq!(inv.release(root), Err(RotationError::UnknownRoot));
}

#[test]
fn a_pinned_object_cannot_be_collected_and_a_retired_version_cannot_be_staged() {
    let mut inv = world();
    let root = inv.pin(&[O1]).unwrap();
    assert_eq!(inv.remove_object(O1), Err(RotationError::Pinned));
    inv.release(root).unwrap();
    inv.remove_object(O1).unwrap();
    assert_eq!(inv.selected(O1), Err(RotationError::UnknownObject));
    // Retire v1 after rewrapping the other object to v2; v1 can then not be staged again.
    inv.stage(O2, env("a", "v2")).unwrap();
    inv.verified(O2).unwrap();
    inv.publish(O2).unwrap();
    inv.retire(&key("a"), &KeyVersion::new("v1").unwrap())
        .unwrap();
    assert_eq!(inv.stage(O2, env("a", "v1")), Err(RotationError::Retired));
}

#[test]
fn a_restart_loses_unpublished_overlays_but_keeps_published_ones_retirements_and_pinned_roots() {
    let mut inv = world();
    inv.stage(O1, env("a", "v2")).unwrap();
    inv.verified(O1).unwrap();
    inv.publish(O1).unwrap();
    inv.stage(O2, env("a", "v2")).unwrap();
    inv.verified(O2).unwrap();
    let root = inv.pin(&[O1, O2]).unwrap();
    inv.crash();
    assert_eq!(inv.selected(O1).unwrap(), &env("a", "v2"));
    assert_eq!(
        inv.selected(O2).unwrap(),
        &env("a", "v1"),
        "crash before publication: the inline wrapper reads"
    );
    // Pinned roots are durable records: they survive, and keep their envelopes.
    assert_eq!(inv.root_envelope(root, O1).unwrap(), &env("a", "v2"));
    assert_eq!(inv.root_envelope(root, O2).unwrap(), &env("a", "v1"));
    // Publishing after a restart needs the overlay staged and verified again.
    assert_eq!(inv.publish(O2), Err(RotationError::NotStaged));
}

#[test]
fn retirements_survive_a_restart() {
    let mut inv = Inventory::new();
    inv.add_object(O1, env("a", "v1")).unwrap();
    inv.stage(O1, env("a", "v2")).unwrap();
    inv.verified(O1).unwrap();
    inv.publish(O1).unwrap();
    inv.retire(&key("a"), &KeyVersion::new("v1").unwrap())
        .unwrap();
    inv.crash();
    assert!(inv.is_retired(&key("a"), &KeyVersion::new("v1").unwrap()));
}

// ---- every short history ----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Step {
    Stage(usize),
    Verify(usize),
    Publish(usize),
    Pin,
    Unpin,
    Retire,
    Remove(usize),
    Crash,
}

const STEPS: [Step; 11] = [
    Step::Stage(0),
    Step::Stage(1),
    Step::Verify(0),
    Step::Verify(1),
    Step::Publish(0),
    Step::Publish(1),
    Step::Pin,
    Step::Unpin,
    Step::Retire,
    Step::Remove(0),
    Step::Crash,
];

/// An independent model: what each object publishes, what each root captured, what is retired.
#[derive(Clone, Default)]
struct Model {
    present: [bool; 2],
    staged: [u8; 2], // 0 none, 1 staged, 2 verified
    published: [bool; 2],
    roots: Vec<[Option<u8>; 2]>, // version captured per object: 1 or 2
    retired_v1: bool,
}

impl Model {
    fn new() -> Self {
        Model {
            present: [true; 2],
            ..Model::default()
        }
    }
    fn selected(&self, o: usize) -> u8 {
        if self.published[o] { 2 } else { 1 }
    }
    fn v1_users(&self) -> usize {
        let current = (0..2)
            .filter(|&o| self.present[o] && self.selected(o) == 1)
            .count();
        let pinned: usize = self
            .roots
            .iter()
            .map(|r| r.iter().filter(|c| **c == Some(1)).count())
            .sum();
        current + pinned
    }
}

fn run(history: &[Step]) {
    let mut inv = Inventory::new();
    inv.add_object(O1, env("a", "v1")).unwrap();
    inv.add_object(O2, env("a", "v1")).unwrap();
    let mut m = Model::new();
    let objects = [O1, O2];
    let mut roots: Vec<u64> = Vec::new();
    let v1 = KeyVersion::new("v1").unwrap();
    for step in history {
        match *step {
            Step::Stage(o) => {
                let r = inv.stage(objects[o], env("a", "v2"));
                assert_eq!(r.is_ok(), m.present[o], "{history:?}");
                if r.is_ok() {
                    m.staged[o] = 1;
                }
            }
            Step::Verify(o) => {
                let r = inv.verified(objects[o]);
                assert_eq!(r.is_ok(), m.present[o] && m.staged[o] == 1, "{history:?}");
                if r.is_ok() {
                    m.staged[o] = 2;
                }
            }
            Step::Publish(o) => {
                let r = inv.publish(objects[o]);
                assert_eq!(
                    r.is_ok(),
                    m.present[o] && m.staged[o] == 2,
                    "publish needs a verified overlay: {history:?}"
                );
                if r.is_ok() {
                    m.published[o] = true;
                    m.staged[o] = 0;
                }
            }
            Step::Pin => {
                let live: Vec<Object> = (0..2)
                    .filter(|&o| m.present[o])
                    .map(|o| objects[o])
                    .collect();
                let r = inv.pin(&live);
                assert!(r.is_ok());
                roots.push(r.unwrap());
                let mut captured = [None; 2];
                for (o, slot) in captured.iter_mut().enumerate() {
                    if m.present[o] {
                        *slot = Some(m.selected(o));
                    }
                }
                m.roots.push(captured);
            }
            Step::Unpin => {
                if let Some(root) = roots.pop() {
                    inv.release(root).unwrap();
                    m.roots.pop();
                }
            }
            Step::Retire => {
                let r = inv.retire(&key("a"), &v1);
                let busy = m.v1_users();
                if m.retired_v1 {
                    // Retiring twice is the same fact, not a failure.
                    assert!(r.is_ok() || busy > 0, "{history:?}");
                } else {
                    assert_eq!(
                        r.is_ok(),
                        busy == 0,
                        "retire v1 with {busy} users: {history:?}"
                    );
                }
                if r.is_ok() {
                    m.retired_v1 = true;
                }
            }
            Step::Remove(o) => {
                let pinned = m.roots.iter().any(|r| r[o].is_some());
                let r = inv.remove_object(objects[o]);
                assert_eq!(r.is_ok(), m.present[o] && !pinned, "{history:?}");
                if r.is_ok() {
                    m.present[o] = false;
                    m.staged[o] = 0;
                }
            }
            Step::Crash => {
                inv.crash();
                m.staged = [0; 2];
            }
        }
        // Invariants after every step.
        for (o, object) in objects.iter().enumerate() {
            if m.present[o] {
                let selected = inv.selected(*object).unwrap();
                let want = if m.published[o] { "v2" } else { "v1" };
                assert_eq!(selected.version.as_str(), want, "{history:?}");
                // A retired version is never what readers select.
                assert!(
                    !inv.is_retired(&selected.key, &selected.version),
                    "{history:?}"
                );
            }
        }
        for (root, captured) in roots.iter().zip(&m.roots) {
            for (o, c) in captured.iter().enumerate() {
                if let Some(version) = c {
                    let e = inv.root_envelope(*root, objects[o]).unwrap();
                    assert_eq!(e.version.as_str(), if *version == 1 { "v1" } else { "v2" });
                    // A live root never depends on a retired version.
                    assert!(!inv.is_retired(&e.key, &e.version), "{history:?}");
                }
            }
        }
    }
}

fn enumerate(prefix: &mut Vec<Step>, depth: usize, count: &mut u64) {
    if depth == 0 {
        run(prefix);
        *count += 1;
        return;
    }
    for step in STEPS {
        prefix.push(step);
        enumerate(prefix, depth - 1, count);
        prefix.pop();
    }
}

#[test]
fn every_history_of_five_steps_keeps_every_live_reader_on_a_version_that_can_still_open_its_objects()
 {
    let mut count = 0;
    enumerate(&mut Vec::new(), 5, &mut count);
    assert_eq!(count, 11u64.pow(5));
    println!("histories checked: {count}");
}

// ---- audit, revocation and destruction -----------------------------------------------------------------------

struct Rig {
    kms: Arc<TestKms>,
    audit: Arc<MemoryAuditSink>,
    keys: KeyManager,
}

fn rig() -> Rig {
    let kms = Arc::new(TestKms::new());
    for name in ["default", "a", "b"] {
        kms.create_key(&key(name)).unwrap();
    }
    let clock = Arc::new(ManualClock::new());
    let audit = Arc::new(MemoryAuditSink::new(1000));
    let keys = KeyManager::new(
        Profile::Development,
        kms.clone(),
        KeyAllowlist::new(vec![("test".into(), "acct".into())]),
        key("default"),
        CachePolicy::default(),
        audit.clone(),
        clock.clone(),
    )
    .unwrap();
    Rig { kms, audit, keys }
}

fn ctx(index: &str, object: u8) -> KeyContext {
    KeyContext {
        index: IndexId::new(index).unwrap(),
        domain: [7; 16],
        object: [object; 16],
    }
}

fn opts() -> CallOptions {
    CallOptions::default()
}

#[test]
fn destroying_an_index_key_makes_its_objects_unreadable_everywhere_and_for_good() {
    let r = rig();
    let a = IndexId::new("alpha").unwrap();
    let (_dek_a, wrapped_a) = r
        .keys
        .create_object_key(Some(&key("a")), &ctx("alpha", 1), 1, &opts())
        .unwrap();
    let (_dek_b, wrapped_b) = r
        .keys
        .create_object_key(Some(&key("b")), &ctx("beta", 2), 1, &opts())
        .unwrap();
    assert!(
        r.keys
            .open_object_key(&wrapped_a, &ctx("alpha", 1), 1, &opts())
            .is_ok()
    );
    let cached_before = r.keys.cached();
    let dropped = r.keys.shred_key(&key("a"), &a, 2, &opts()).unwrap();
    assert_eq!(
        dropped, 1,
        "the warm data key of the destroyed key is dropped"
    );
    assert_eq!(r.keys.cached(), cached_before - 1);
    // Warm and cold paths both fail; nothing the manager or the provider holds can recover the data key.
    for _ in 0..2 {
        let err = r
            .keys
            .open_object_key(&wrapped_a, &ctx("alpha", 1), 2, &opts())
            .unwrap_err();
        assert_eq!(err, KeyError::Provider(ProviderError::Destroyed));
    }
    assert_eq!(
        r.kms
            .unwrap(&wrapped_a, &ctx("alpha", 1), &opts())
            .unwrap_err(),
        ProviderError::Destroyed
    );
    // The owner cannot restore a destroyed key; a revoked one can be restored.
    r.kms.restore(&key("a"));
    assert_eq!(
        r.kms
            .wrap(
                &key("a"),
                &ctx("alpha", 3),
                &SecretKey::from_bytes([1; 32]),
                &opts()
            )
            .unwrap_err(),
        ProviderError::Destroyed
    );
    // Another index is untouched.
    assert!(
        r.keys
            .open_object_key(&wrapped_b, &ctx("beta", 2), 1, &opts())
            .is_ok()
    );
    // The destruction is in the audit log: requested, then ok, with the key and generation.
    let destroys: Vec<_> = r
        .audit
        .events()
        .into_iter()
        .filter(|e| e.op == AuditOp::Destroy)
        .collect();
    assert_eq!(destroys.len(), 2);
    assert_eq!(destroys[0].outcome, "requested");
    let last = destroys.last().unwrap();
    assert_eq!(last.outcome, "ok");
    assert_eq!(last.key.as_ref(), Some(&key("a")));
    assert_eq!(last.generation, 2);
    assert!(last.to_json().contains("\"op\":\"destroy\""));
}

#[test]
fn a_destruction_the_provider_cannot_perform_leaves_the_caches_alone_and_is_audited() {
    struct NoDestroy(Arc<TestKms>);
    impl KeyProvider for NoDestroy {
        fn name(&self) -> &str {
            self.0.name()
        }
        fn is_test(&self) -> bool {
            true
        }
        fn key_info(
            &self,
            k: &KeyRef,
            o: &CallOptions,
        ) -> Result<hsearch_security::keys::KeyInfo, ProviderError> {
            self.0.key_info(k, o)
        }
        fn wrap(
            &self,
            k: &KeyRef,
            c: &KeyContext,
            d: &SecretKey,
            o: &CallOptions,
        ) -> Result<hsearch_security::keys::WrappedKey, ProviderError> {
            self.0.wrap(k, c, d, o)
        }
        fn unwrap(
            &self,
            w: &hsearch_security::keys::WrappedKey,
            c: &KeyContext,
            o: &CallOptions,
        ) -> Result<SecretKey, ProviderError> {
            self.0.unwrap(w, c, o)
        }
    }
    let kms = Arc::new(TestKms::new());
    kms.create_key(&key("default")).unwrap();
    kms.create_key(&key("a")).unwrap();
    let audit = Arc::new(MemoryAuditSink::new(100));
    let keys = KeyManager::new(
        Profile::Development,
        Arc::new(NoDestroy(kms)),
        KeyAllowlist::new(vec![("test".into(), "acct".into())]),
        key("default"),
        CachePolicy::default(),
        audit.clone(),
        Arc::new(ManualClock::new()),
    )
    .unwrap();
    keys.create_object_key(Some(&key("a")), &ctx("alpha", 1), 1, &opts())
        .unwrap();
    let err = keys
        .shred_key(&key("a"), &IndexId::new("alpha").unwrap(), 2, &opts())
        .unwrap_err();
    assert_eq!(err, KeyError::Provider(ProviderError::Unsupported));
    assert_eq!(
        keys.cached(),
        1,
        "nothing was destroyed, so nothing is dropped"
    );
    let last = audit
        .events()
        .into_iter()
        .rfind(|e| e.op == AuditOp::Destroy)
        .unwrap();
    assert_eq!(last.outcome, "unsupported");
}

#[test]
fn rotation_and_revocation_are_audited_and_a_revocation_drops_that_index_only() {
    let r = rig();
    let alpha = IndexId::new("alpha").unwrap();
    let (_d1, w1) = r
        .keys
        .create_object_key(Some(&key("a")), &ctx("alpha", 1), 1, &opts())
        .unwrap();
    let (_d2, _w2) = r
        .keys
        .create_object_key(Some(&key("b")), &ctx("beta", 2), 1, &opts())
        .unwrap();
    let version = r.kms.rotate(&key("a")).unwrap();
    r.keys.note_rotation(&alpha, &key("a"), &version, 2);
    r.kms.revoke(&key("a"));
    let dropped = r.keys.apply_revocation(&alpha, &key("a"), 3);
    assert_eq!(dropped, 1);
    assert_eq!(r.keys.cached(), 1, "beta's key is still warm");
    assert_eq!(
        r.keys
            .open_object_key(&w1, &ctx("alpha", 1), 3, &opts())
            .unwrap_err(),
        KeyError::Provider(ProviderError::Revoked)
    );
    let ops: Vec<_> = r
        .audit
        .events()
        .into_iter()
        .filter(|e| matches!(e.op, AuditOp::Rotate | AuditOp::Revoke))
        .map(|e| {
            (
                e.op,
                e.version.map(|v| v.as_str().to_string()),
                e.generation,
            )
        })
        .collect();
    assert_eq!(
        ops,
        vec![
            (AuditOp::Rotate, Some("v2".to_string()), 2),
            (AuditOp::Revoke, None, 3)
        ]
    );
}

#[test]
fn an_overlay_whose_version_was_retired_after_staging_is_not_published() {
    let mut inv = world();
    inv.stage(O1, env("a", "v2")).unwrap();
    inv.verified(O1).unwrap();
    // Nothing selects v2 yet, so the owner may retire it; the staged overlay must then not go live.
    inv.retire(&key("a"), &KeyVersion::new("v2").unwrap())
        .unwrap();
    assert_eq!(inv.publish(O1), Err(RotationError::Retired));
    assert_eq!(inv.selected(O1).unwrap(), &env("a", "v1"));
}

#[test]
fn an_object_cannot_be_added_under_a_retired_version_or_replace_an_existing_one() {
    let mut inv = world();
    inv.stage(O1, env("a", "v2")).unwrap();
    inv.verified(O1).unwrap();
    inv.publish(O1).unwrap();
    // Re-adding an existing object would drop its published overlay.
    assert_eq!(
        inv.add_object(O1, env("a", "v1")),
        Err(RotationError::Exists)
    );
    assert_eq!(inv.selected(O1).unwrap(), &env("a", "v2"));
    inv.stage(O2, env("a", "v2")).unwrap();
    inv.verified(O2).unwrap();
    inv.publish(O2).unwrap();
    inv.retire(&key("a"), &KeyVersion::new("v1").unwrap())
        .unwrap();
    assert_eq!(
        inv.add_object([3; 16], env("a", "v1")),
        Err(RotationError::Retired)
    );
    // Retiring again is the same fact.
    inv.retire(&key("a"), &KeyVersion::new("v1").unwrap())
        .unwrap();
}

#[test]
fn a_publication_refused_because_the_version_was_retired_leaves_the_overlay_staged() {
    let mut inv = world();
    inv.stage(O1, env("a", "v2")).unwrap();
    inv.verified(O1).unwrap();
    inv.retire(&key("a"), &KeyVersion::new("v2").unwrap())
        .unwrap();
    assert_eq!(inv.publish(O1), Err(RotationError::Retired));
    // Still staged and verified: publishing again is refused the same way, not as "nothing staged".
    assert_eq!(inv.publish(O1), Err(RotationError::Retired));
}

#[test]
fn the_revocation_bound_is_the_gate_age_when_readers_consult_it_and_the_cache_lifetime_when_not() {
    use hsearch_security::rotation::{meets_target, revocation_bound};
    let (age, ttl, target) = (secs(30), secs(300), secs(60));
    assert_eq!(revocation_bound(age, ttl, true), age);
    assert_eq!(revocation_bound(age, ttl, false), ttl);
    assert!(meets_target(revocation_bound(age, ttl, true), target));
    assert!(!meets_target(revocation_bound(age, ttl, false), target));
    assert!(meets_target(target, target));
    assert!(!meets_target(target + Duration::from_nanos(1), target));
}

#[test]
fn shredding_the_default_key_or_an_unlisted_key_is_refused_before_the_provider_is_touched() {
    let r = rig();
    let a = IndexId::new("alpha").unwrap();
    r.kms.create_key(&KeyRef::new("test/other/k").unwrap()).ok();
    let err = r
        .keys
        .shred_key(&key("default"), &a, 1, &opts())
        .unwrap_err();
    assert_eq!(err, KeyError::DefaultKeyProtected);
    let off = KeyRef::new("test/other/k").unwrap();
    let err = r.keys.shred_key(&off, &a, 1, &opts()).unwrap_err();
    assert_eq!(err, KeyError::NotAllowed);
    // Neither key was destroyed, and no destroy was even requested.
    assert!(r.kms.key_info(&key("default"), &opts()).is_ok());
    assert!(r.audit.events().iter().all(|e| e.op != AuditOp::Destroy));
    assert_eq!(
        r.audit
            .events()
            .iter()
            .filter(|e| e.op == AuditOp::Refused)
            .count(),
        2
    );
}
