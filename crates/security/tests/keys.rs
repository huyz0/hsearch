//! M05.02 key provider: wrap/unwrap binding, provider failures, the bounded data-key cache and its KMS call
//! budget, per-index keys with a deployment default, revocation of one index only, and secrecy.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use hsearch_security::audit::{AuditOp, MemoryAuditSink};
use hsearch_security::clock::ManualClock;
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{
    CallOptions, CancelToken, KeyContext, KeyFormatError, KeyProvider, KeyRef, KeyVersion, Profile,
    ProviderError, SecretKey, WrappedKey,
};
use hsearch_security::manager::{CachePolicy, KeyAllowlist, KeyError, KeyManager};
use hsearch_security::test_kms::{Fault, TestKms};
use zeroize::ZeroizeOnDrop;

fn idx(name: &str) -> IndexId {
    IndexId::new(name).unwrap()
}

fn key(name: &str) -> KeyRef {
    KeyRef::new(&format!("test/acct/{name}")).unwrap()
}

fn ctx(index: &str, object: u8) -> KeyContext {
    KeyContext {
        index: idx(index),
        domain: [7; 16],
        object: [object; 16],
    }
}

struct Rig {
    kms: Arc<TestKms>,
    clock: Arc<ManualClock>,
    audit: Arc<MemoryAuditSink>,
    manager: Arc<KeyManager>,
}

fn rig_with(policy: CachePolicy) -> Rig {
    let kms = Arc::new(TestKms::new());
    kms.create_key(&key("default")).unwrap();
    let clock = Arc::new(ManualClock::new());
    let audit = Arc::new(MemoryAuditSink::new(10_000));
    let manager = Arc::new(
        KeyManager::new(
            Profile::Development,
            kms.clone(),
            KeyAllowlist::new(vec![("test".into(), "acct".into())]),
            key("default"),
            policy,
            audit.clone(),
            clock.clone(),
        )
        .unwrap(),
    );
    Rig {
        kms,
        clock,
        audit,
        manager,
    }
}

fn rig() -> Rig {
    rig_with(CachePolicy {
        max_entries: 1000,
        ttl: Duration::from_secs(300),
        shards: 1,
    })
}

fn opts() -> CallOptions {
    CallOptions::default()
}

fn wrap_direct(kms: &TestKms, k: &KeyRef, c: &KeyContext) -> (SecretKey, WrappedKey) {
    let dek = SecretKey::generate().unwrap();
    let wrapped = kms.wrap(k, c, &dek, &opts()).unwrap();
    (dek, wrapped)
}

// ---- provider contract -------------------------------------------------------------------------------

#[test]
fn wrap_then_unwrap_returns_the_same_key() {
    let kms = TestKms::new();
    kms.create_key(&key("a")).unwrap();
    let c = ctx("orders", 1);
    let (dek, wrapped) = wrap_direct(&kms, &key("a"), &c);
    assert_eq!(wrapped.version().as_str(), "v1");
    assert_eq!(
        kms.unwrap(&wrapped, &c, &opts()).unwrap().expose(),
        dek.expose()
    );
    // Two wraps of one key differ (fresh nonce), so ciphertext equality leaks nothing.
    let again = kms.wrap(&key("a"), &c, &dek, &opts()).unwrap();
    assert_ne!(again.bytes(), wrapped.bytes());
}

#[test]
fn a_wrap_is_bound_to_index_domain_and_object() {
    let kms = TestKms::new();
    kms.create_key(&key("a")).unwrap();
    let c = ctx("orders", 1);
    let (_dek, wrapped) = wrap_direct(&kms, &key("a"), &c);
    let others = [
        ctx("payroll", 1),
        ctx("orders", 2),
        KeyContext {
            domain: [8; 16],
            ..c.clone()
        },
    ];
    for other in others {
        assert_eq!(
            kms.unwrap(&wrapped, &other, &opts()).unwrap_err(),
            ProviderError::ContextMismatch,
            "{other:?}"
        );
    }
}

#[test]
fn version_mismatch_and_corruption_are_distinct_errors() {
    let kms = TestKms::new();
    kms.create_key(&key("a")).unwrap();
    let c = ctx("orders", 1);
    let (_dek, wrapped) = wrap_direct(&kms, &key("a"), &c);
    let forge = |version: &str, bytes: Vec<u8>| {
        WrappedKey::new(key("a"), KeyVersion::new(version).unwrap(), bytes).unwrap()
    };
    // A version the provider never issued.
    assert_eq!(
        kms.unwrap(&forge("v9", wrapped.bytes().to_vec()), &c, &opts())
            .unwrap_err(),
        ProviderError::VersionMismatch
    );
    assert_eq!(
        kms.unwrap(&forge("latest", wrapped.bytes().to_vec()), &c, &opts())
            .unwrap_err(),
        ProviderError::VersionMismatch
    );
    // A real version, wrong one: the bytes were sealed under v1, so v2 cannot open them.
    kms.rotate(&key("a")).unwrap();
    assert_eq!(
        kms.unwrap(&forge("v2", wrapped.bytes().to_vec()), &c, &opts())
            .unwrap_err(),
        ProviderError::Corrupt
    );
    // Flip every byte in turn: nothing authenticates.
    for i in 0..wrapped.bytes().len() {
        let mut bad = wrapped.bytes().to_vec();
        bad[i] ^= 1;
        let err = kms.unwrap(&forge("v1", bad), &c, &opts()).unwrap_err();
        assert!(
            matches!(err, ProviderError::Corrupt | ProviderError::ContextMismatch),
            "byte {i}: {err:?}"
        );
    }
    // Wrong lengths.
    assert_eq!(
        kms.unwrap(&forge("v1", vec![0; 10]), &c, &opts())
            .unwrap_err(),
        ProviderError::Corrupt
    );
    // A key the provider has never heard of.
    let stranger = WrappedKey::new(
        key("nope"),
        KeyVersion::new("v1").unwrap(),
        wrapped.bytes().to_vec(),
    )
    .unwrap();
    assert_eq!(
        kms.unwrap(&stranger, &c, &opts()).unwrap_err(),
        ProviderError::NotFound
    );
    assert_eq!(
        kms.wrap(&key("nope"), &c, &SecretKey::generate().unwrap(), &opts())
            .unwrap_err(),
        ProviderError::NotFound
    );
}

#[test]
fn rotation_keeps_old_wraps_readable_and_new_wraps_use_the_new_version() {
    let kms = TestKms::new();
    kms.create_key(&key("a")).unwrap();
    let c = ctx("orders", 1);
    let (dek_old, old) = wrap_direct(&kms, &key("a"), &c);
    assert_eq!(kms.rotate(&key("a")).unwrap().as_str(), "v2");
    let (_dek_new, new) = wrap_direct(&kms, &key("a"), &c);
    assert_eq!(
        (old.version().as_str(), new.version().as_str()),
        ("v1", "v2")
    );
    assert_eq!(
        kms.unwrap(&old, &c, &opts()).unwrap().expose(),
        dek_old.expose()
    );
    // Retiring v1 makes old wraps unreadable until they are rewrapped.
    kms.disable_version(&key("a"), old.version());
    assert_eq!(
        kms.unwrap(&old, &c, &opts()).unwrap_err(),
        ProviderError::VersionMismatch
    );
    assert!(kms.unwrap(&new, &c, &opts()).is_ok());
    let info = kms.key_info(&key("a"), &opts()).unwrap();
    assert_eq!((info.current.as_str(), info.usable), ("v2", true));
}

#[test]
fn revocation_blocks_wrap_and_unwrap_and_restore_undoes_it() {
    let kms = TestKms::new();
    kms.create_key(&key("a")).unwrap();
    let c = ctx("orders", 1);
    let (dek, wrapped) = wrap_direct(&kms, &key("a"), &c);
    kms.revoke(&key("a"));
    assert_eq!(
        kms.unwrap(&wrapped, &c, &opts()).unwrap_err(),
        ProviderError::Revoked
    );
    assert_eq!(
        kms.wrap(&key("a"), &c, &dek, &opts()).unwrap_err(),
        ProviderError::Revoked
    );
    assert!(!kms.key_info(&key("a"), &opts()).unwrap().usable);
    kms.restore(&key("a"));
    assert!(kms.unwrap(&wrapped, &c, &opts()).is_ok());
}

#[test]
fn provider_failures_are_normalized_and_classified() {
    let kms = TestKms::new();
    kms.create_key(&key("a")).unwrap();
    let c = ctx("orders", 1);
    let (dek, wrapped) = wrap_direct(&kms, &key("a"), &c);
    let retry = Duration::from_millis(250);
    for (fault, expected, retryable) in [
        (
            Fault::Throttle(retry),
            ProviderError::Throttled { retry_after: retry },
            true,
        ),
        (Fault::Deny, ProviderError::Denied, false),
        (Fault::Timeout, ProviderError::Timeout, true),
        (Fault::Unavailable, ProviderError::Unavailable, true),
    ] {
        kms.inject(fault, 2);
        assert_eq!(kms.unwrap(&wrapped, &c, &opts()).unwrap_err(), expected);
        assert_eq!(
            kms.wrap(&key("a"), &c, &dek, &opts()).unwrap_err(),
            expected
        );
        assert_eq!(expected.is_retryable(), retryable);
    }
    // The faults are spent; the provider works again.
    assert!(kms.unwrap(&wrapped, &c, &opts()).is_ok());
    for e in [
        ProviderError::Revoked,
        ProviderError::NotFound,
        ProviderError::VersionMismatch,
        ProviderError::ContextMismatch,
        ProviderError::Corrupt,
        ProviderError::Cancelled,
        ProviderError::Unsupported,
    ] {
        assert!(!e.is_retryable());
        assert!(e.to_string().contains(e.code()));
    }
}

#[test]
fn cancellation_and_timeouts_stop_a_call() {
    let kms = TestKms::new();
    kms.create_key(&key("a")).unwrap();
    let c = ctx("orders", 1);
    let (_dek, wrapped) = wrap_direct(&kms, &key("a"), &c);

    let cancelled = CallOptions {
        cancel: CancelToken::new(),
        ..opts()
    };
    cancelled.cancel.cancel();
    assert_eq!(
        kms.unwrap(&wrapped, &c, &cancelled).unwrap_err(),
        ProviderError::Cancelled
    );
    assert_eq!(
        kms.key_info(&key("a"), &cancelled).unwrap_err(),
        ProviderError::Cancelled
    );

    kms.set_latency(Duration::from_millis(400));
    let short = CallOptions {
        timeout: Duration::from_millis(30),
        ..opts()
    };
    let started = std::time::Instant::now();
    assert_eq!(
        kms.unwrap(&wrapped, &c, &short).unwrap_err(),
        ProviderError::Timeout
    );
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "a timed-out call must not wait out the provider"
    );

    // Cancelled while the provider is working.
    kms.set_latency(Duration::from_millis(150));
    let token = CancelToken::new();
    let during = CallOptions {
        cancel: token.clone(),
        timeout: Duration::from_secs(5),
    };
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        token.cancel();
    });
    assert_eq!(
        kms.unwrap(&wrapped, &c, &during).unwrap_err(),
        ProviderError::Cancelled
    );
    canceller.join().unwrap();
}

#[test]
fn identifiers_are_validated() {
    assert!(KeyRef::new("aws/123456789012/alias.my-key_1:v2").is_ok());
    for bad in [
        "",
        "a/b",
        "a/b/c/d",
        "a//c",
        "a/b/c d",
        "http://evil/x",
        "a/b/\u{e9}",
        &format!("a/b/{}", "c".repeat(300)),
    ] {
        assert!(KeyRef::new(bad).is_err(), "{bad:?}");
    }
    assert!(
        KeyVersion::new("").is_err()
            && KeyVersion::new("a b").is_err()
            && KeyVersion::new(&"v".repeat(200)).is_err()
    );
    let k = KeyRef::new("test/acct/k").unwrap();
    let v = KeyVersion::new("v1").unwrap();
    assert_eq!((k.provider(), k.account()), ("test", "acct"));
    assert_eq!(
        WrappedKey::new(k.clone(), v.clone(), vec![]).unwrap_err(),
        KeyFormatError("wrapped key size")
    );
    assert!(WrappedKey::new(k.clone(), v.clone(), vec![0; 1025]).is_err());
    // The digest identifies key reference, version and bytes together.
    let base = WrappedKey::new(k.clone(), v.clone(), vec![1, 2, 3]).unwrap();
    assert_ne!(
        base.digest(),
        WrappedKey::new(k.clone(), KeyVersion::new("v2").unwrap(), vec![1, 2, 3])
            .unwrap()
            .digest()
    );
    assert_ne!(
        base.digest(),
        WrappedKey::new(KeyRef::new("test/acct/other").unwrap(), v, vec![1, 2, 3])
            .unwrap()
            .digest()
    );
}

// ---- manager -----------------------------------------------------------------------------------------

#[test]
fn a_production_profile_refuses_the_test_provider() {
    let r = KeyManager::new(
        Profile::Production,
        Arc::new(TestKms::new()),
        KeyAllowlist::new(vec![("test".into(), "acct".into())]),
        key("default"),
        CachePolicy::default(),
        Arc::new(MemoryAuditSink::new(1)),
        Arc::new(ManualClock::new()),
    );
    assert_eq!(r.err(), Some(KeyError::TestProviderInProduction));
}

#[test]
fn construction_rejects_unusable_settings() {
    let build = |default: KeyRef, allow: Vec<(String, String)>, policy: CachePolicy| {
        KeyManager::new(
            Profile::Development,
            Arc::new(TestKms::new()),
            KeyAllowlist::new(allow),
            default,
            policy,
            Arc::new(MemoryAuditSink::new(1)),
            Arc::new(ManualClock::new()),
        )
        .err()
    };
    let ok_allow = vec![("test".to_string(), "acct".to_string())];
    assert_eq!(
        build(
            key("d"),
            ok_allow.clone(),
            CachePolicy {
                max_entries: 0,
                ttl: Duration::from_secs(1),
                shards: 1
            }
        ),
        Some(KeyError::BadCachePolicy)
    );
    assert_eq!(
        build(
            key("d"),
            ok_allow.clone(),
            CachePolicy {
                max_entries: 1,
                ttl: Duration::ZERO,
                shards: 1
            }
        ),
        Some(KeyError::BadCachePolicy)
    );
    assert_eq!(
        build(key("d"), vec![], CachePolicy::default()),
        Some(KeyError::NotAllowed)
    );
    assert_eq!(
        build(
            KeyRef::new("aws/acct/d").unwrap(),
            vec![("aws".into(), "acct".into())],
            CachePolicy::default()
        ),
        Some(KeyError::WrongProvider)
    );
    for e in [
        KeyError::NotAllowed,
        KeyError::KeyUnusable,
        KeyError::WrongProvider,
        KeyError::BadCachePolicy,
        KeyError::TestProviderInProduction,
        KeyError::Provider(ProviderError::Denied),
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn an_index_uses_its_own_key_or_the_deployment_default() {
    let rig = rig();
    rig.kms.create_key(&key("orders-key")).unwrap();
    let own = key("orders-key");
    assert_eq!(rig.manager.resolve(Some(&own)), &own);
    assert_eq!(rig.manager.resolve(None), &key("default"));
    assert_eq!(rig.manager.default_key(), &key("default"));
    let (_, w_own) = rig
        .manager
        .create_object_key(Some(&own), &ctx("orders", 1), 1, &opts())
        .unwrap();
    let (_, w_default) = rig
        .manager
        .create_object_key(None, &ctx("logs", 1), 1, &opts())
        .unwrap();
    assert_eq!((w_own.key(), w_default.key()), (&own, &key("default")));
}

#[test]
fn activation_checks_allowlist_provider_and_usability() {
    let rig = rig();
    rig.kms.create_key(&key("k")).unwrap();
    let c = opts();
    assert_eq!(
        rig.manager
            .validate_key(&idx("orders"), &key("k"), 1, &c)
            .unwrap()
            .as_str(),
        "v1"
    );
    assert_eq!(
        rig.manager
            .validate_key(
                &idx("orders"),
                &KeyRef::new("test/other-acct/k").unwrap(),
                1,
                &c
            )
            .unwrap_err(),
        KeyError::NotAllowed
    );
    assert_eq!(
        rig.manager
            .validate_key(&idx("orders"), &KeyRef::new("aws/acct/k").unwrap(), 1, &c)
            .unwrap_err(),
        KeyError::WrongProvider
    );
    assert_eq!(
        rig.manager
            .validate_key(&idx("orders"), &key("missing"), 1, &c)
            .unwrap_err(),
        KeyError::Provider(ProviderError::NotFound)
    );
    rig.kms.revoke(&key("k"));
    assert_eq!(
        rig.manager
            .validate_key(&idx("orders"), &key("k"), 1, &c)
            .unwrap_err(),
        KeyError::KeyUnusable
    );
    let outcomes: Vec<_> = rig
        .audit
        .events()
        .iter()
        .map(|e| (e.op, e.outcome))
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (AuditOp::Activate, "ok"),
            (AuditOp::Refused, "not_allowed"),
            (AuditOp::Refused, "not_allowed"),
            (AuditOp::Activate, "not_found"),
            (AuditOp::Activate, "unusable"),
        ]
    );
}

#[test]
fn a_wrapped_key_read_from_storage_cannot_name_a_key_off_the_allowlist() {
    let rig = rig();
    let c = ctx("orders", 1);
    let forged = WrappedKey::new(
        KeyRef::new("test/attacker/k").unwrap(),
        KeyVersion::new("v1").unwrap(),
        vec![0; 60],
    )
    .unwrap();
    assert_eq!(
        rig.manager
            .open_object_key(&forged, &c, 1, &opts())
            .unwrap_err(),
        KeyError::NotAllowed
    );
    let foreign = WrappedKey::new(
        KeyRef::new("aws/acct/k").unwrap(),
        KeyVersion::new("v1").unwrap(),
        vec![0; 60],
    )
    .unwrap();
    assert_eq!(
        rig.manager
            .open_object_key(&foreign, &c, 1, &opts())
            .unwrap_err(),
        KeyError::WrongProvider
    );
    assert_eq!(
        rig.kms.total_calls(),
        0,
        "the provider must never see a refused reference"
    );
    assert!(rig.audit.events().iter().all(|e| e.op == AuditOp::Refused));
    let create = rig.manager.create_object_key(
        Some(&KeyRef::new("test/attacker/k").unwrap()),
        &c,
        1,
        &opts(),
    );
    assert_eq!(create.err(), Some(KeyError::NotAllowed));
}

#[test]
fn the_cache_keeps_the_provider_off_the_request_path() {
    let rig = rig();
    let c = ctx("orders", 1);
    let (dek, wrapped) = rig.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    assert_eq!((rig.kms.wrap_calls(), rig.kms.unwrap_calls()), (1, 0));
    // A thousand reads of that object: no provider call at all, because the writer's key was cached.
    for _ in 0..1000 {
        assert_eq!(
            rig.manager
                .open_object_key(&wrapped, &c, 1, &opts())
                .unwrap()
                .expose(),
            dek.expose()
        );
    }
    assert_eq!(rig.kms.total_calls(), 1);
    // After the lifetime, exactly one unwrap, then a thousand more hits.
    rig.clock.advance(Duration::from_secs(301));
    for _ in 0..1000 {
        rig.manager
            .open_object_key(&wrapped, &c, 1, &opts())
            .unwrap();
    }
    assert_eq!((rig.kms.wrap_calls(), rig.kms.unwrap_calls()), (1, 1));
    let stats = rig.manager.stats();
    assert_eq!((stats.hits, stats.misses), (1999, 1));
}

#[test]
fn kms_calls_scale_with_objects_not_with_reads() {
    let rig = rig();
    let mut wrapped = Vec::new();
    for n in 0..20u8 {
        wrapped.push((
            ctx("orders", n),
            rig.manager
                .create_object_key(None, &ctx("orders", n), 1, &opts())
                .unwrap()
                .1,
        ));
    }
    rig.manager.invalidate_index(&idx("orders"), 1);
    for _round in 0..50 {
        for (c, w) in &wrapped {
            rig.manager.open_object_key(w, c, 1, &opts()).unwrap();
        }
    }
    // 20 wraps to create, 20 unwraps on the first read of each, and nothing for the other 980 reads.
    assert_eq!((rig.kms.wrap_calls(), rig.kms.unwrap_calls()), (20, 20));
}

#[test]
fn an_expired_key_is_never_served_when_the_provider_is_down() {
    let rig = rig();
    let c = ctx("orders", 1);
    let (_dek, wrapped) = rig.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    rig.clock.advance(Duration::from_secs(299));
    assert!(
        rig.manager
            .open_object_key(&wrapped, &c, 1, &opts())
            .is_ok()
    );
    rig.clock.advance(Duration::from_secs(2));
    rig.kms.inject(Fault::Unavailable, 1);
    // No refresh-on-error, no stale fallback.
    assert_eq!(
        rig.manager
            .open_object_key(&wrapped, &c, 1, &opts())
            .unwrap_err(),
        KeyError::Provider(ProviderError::Unavailable)
    );
    // The failure is not cached: the provider is asked again and succeeds.
    assert!(
        rig.manager
            .open_object_key(&wrapped, &c, 1, &opts())
            .is_ok()
    );
}

#[test]
fn a_new_policy_generation_does_not_see_keys_cached_under_the_old_one() {
    let rig = rig();
    let c = ctx("orders", 1);
    let (_dek, wrapped) = rig.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    assert!(
        rig.manager
            .open_object_key(&wrapped, &c, 1, &opts())
            .is_ok()
    );
    assert_eq!(rig.kms.unwrap_calls(), 0);
    assert!(
        rig.manager
            .open_object_key(&wrapped, &c, 2, &opts())
            .is_ok()
    );
    assert_eq!(rig.kms.unwrap_calls(), 1);
}

#[test]
fn revoking_one_indexes_key_blocks_that_index_only() {
    let rig = rig();
    rig.kms.create_key(&key("orders-key")).unwrap();
    rig.kms.create_key(&key("payroll-key")).unwrap();
    let (orders_key, payroll_key) = (key("orders-key"), key("payroll-key"));
    let (o, p, d) = (ctx("orders", 1), ctx("payroll", 1), ctx("logs", 1));
    // Mixed keys: two indexes with their own, one on the deployment default.
    let (_, wo) = rig
        .manager
        .create_object_key(Some(&orders_key), &o, 1, &opts())
        .unwrap();
    let (_, wp) = rig
        .manager
        .create_object_key(Some(&payroll_key), &p, 1, &opts())
        .unwrap();
    let (_, wd) = rig.manager.create_object_key(None, &d, 1, &opts()).unwrap();

    // The owner revokes orders' key. Cached keys keep working until the node learns of it...
    rig.kms.revoke(&orders_key);
    assert!(
        rig.manager.open_object_key(&wo, &o, 1, &opts()).is_ok(),
        "bounded by the cache lifetime"
    );
    // ...and the node learns of it (policy generation bump, or a provider denial): drop that key's entries.
    assert_eq!(
        rig.manager.invalidate_key(&orders_key, &idx("orders"), 2),
        1
    );
    assert_eq!(
        rig.manager
            .open_object_key(&wo, &o, 2, &opts())
            .unwrap_err(),
        KeyError::Provider(ProviderError::Revoked)
    );
    // Every other index is untouched, from cache and from the provider.
    assert!(rig.manager.open_object_key(&wp, &p, 1, &opts()).is_ok());
    assert!(rig.manager.open_object_key(&wd, &d, 1, &opts()).is_ok());
    rig.manager.invalidate_index(&idx("payroll"), 2);
    rig.manager.invalidate_index(&idx("logs"), 2);
    assert!(rig.manager.open_object_key(&wp, &p, 2, &opts()).is_ok());
    assert!(rig.manager.open_object_key(&wd, &d, 2, &opts()).is_ok());
    // Without any explicit invalidation the cache lifetime bounds how long a revoked key keeps working.
    rig.kms.revoke(&payroll_key);
    rig.clock.advance(Duration::from_secs(301));
    assert_eq!(
        rig.manager
            .open_object_key(&wp, &p, 2, &opts())
            .unwrap_err(),
        KeyError::Provider(ProviderError::Revoked)
    );
    assert!(rig.manager.open_object_key(&wd, &d, 2, &opts()).is_ok());
}

#[test]
fn a_revocation_seen_at_the_provider_flushes_that_index_but_a_denial_does_not() {
    let rig = rig();
    let (a, other) = (ctx("orders", 1), ctx("payroll", 1));
    let (_, wa) = rig.manager.create_object_key(None, &a, 1, &opts()).unwrap();
    rig.manager
        .create_object_key(None, &ctx("orders", 2), 1, &opts())
        .unwrap();
    let (_, wo) = rig
        .manager
        .create_object_key(None, &other, 1, &opts())
        .unwrap();
    // A denial (which anyone planting wrapped keys could provoke) must not flush the index's cache.
    // A different object of the same index forces a provider call that is denied.
    let (_, planted) = wrap_direct(&rig.kms, &key("default"), &ctx("orders", 7));
    rig.kms.inject(Fault::Deny, 1);
    assert_eq!(
        rig.manager
            .open_object_key(&planted, &ctx("orders", 7), 1, &opts())
            .unwrap_err(),
        KeyError::Provider(ProviderError::Denied)
    );
    assert_eq!(rig.manager.cached(), 3);
    // A revocation seen by the provider does flush that index, and only that index.
    rig.kms.revoke(&key("default"));
    assert_eq!(
        rig.manager
            .open_object_key(&planted, &ctx("orders", 7), 1, &opts())
            .unwrap_err(),
        KeyError::Provider(ProviderError::Revoked)
    );
    assert_eq!(rig.manager.cached(), 1);
    let _ = (wa, wo);
}

#[test]
fn a_cached_key_is_bound_to_its_object_like_a_cold_one() {
    let rig = rig();
    let (a, b) = (ctx("orders", 1), ctx("orders", 2));
    let (_, wa) = rig.manager.create_object_key(None, &a, 1, &opts()).unwrap();
    // Object A's wrapped key pasted into object B's header, same index and domain: warm cache must refuse
    // exactly as a cold one does.
    let warm = rig.manager.open_object_key(&wa, &b, 1, &opts());
    assert_eq!(
        warm.unwrap_err(),
        KeyError::Provider(ProviderError::ContextMismatch)
    );
    rig.manager.invalidate_index(&idx("orders"), 1);
    let cold = rig.manager.open_object_key(&wa, &b, 1, &opts());
    assert_eq!(
        cold.unwrap_err(),
        KeyError::Provider(ProviderError::ContextMismatch)
    );
}

#[test]
fn an_unwrap_that_raced_a_revocation_is_not_cached() {
    let rig = rig();
    let c = ctx("orders", 1);
    let (_dek, wrapped) = wrap_direct(&rig.kms, &key("default"), &c);
    rig.kms.set_latency(Duration::from_millis(150));
    let reader = {
        let (m, c, w) = (rig.manager.clone(), c.clone(), wrapped.clone());
        thread::spawn(move || {
            m.open_object_key(
                &w,
                &c,
                1,
                &CallOptions {
                    timeout: Duration::from_secs(5),
                    ..CallOptions::default()
                },
            )
        })
    };
    thread::sleep(Duration::from_millis(40));
    rig.manager.invalidate_index(&idx("orders"), 2);
    assert!(
        reader.join().unwrap().is_ok(),
        "the in-flight call still answers its own caller"
    );
    assert_eq!(
        rig.manager.cached(),
        0,
        "but its key must not outlive the invalidation"
    );
}

#[test]
fn expired_keys_leave_memory_on_the_next_insert() {
    let rig = rig();
    for n in 0..5u8 {
        rig.manager
            .create_object_key(None, &ctx("orders", n), 1, &opts())
            .unwrap();
    }
    assert_eq!(rig.manager.cached(), 5);
    rig.clock.advance(Duration::from_secs(301));
    rig.manager
        .create_object_key(None, &ctx("orders", 9), 1, &opts())
        .unwrap();
    assert_eq!(rig.manager.cached(), 1);
}

struct PanickingKms(TestKms);

impl KeyProvider for PanickingKms {
    fn name(&self) -> &str {
        "test"
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
    ) -> Result<WrappedKey, ProviderError> {
        self.0.wrap(k, c, d, o)
    }
    fn unwrap(
        &self,
        _: &WrappedKey,
        _: &KeyContext,
        _: &CallOptions,
    ) -> Result<SecretKey, ProviderError> {
        panic!("provider bug")
    }
}

#[test]
fn a_panicking_provider_does_not_wedge_the_key() {
    let kms = TestKms::new();
    kms.create_key(&key("default")).unwrap();
    let c = ctx("orders", 1);
    let (_dek, wrapped) = wrap_direct(&kms, &key("default"), &c);
    let manager = Arc::new(
        KeyManager::new(
            Profile::Development,
            Arc::new(PanickingKms(kms)),
            KeyAllowlist::new(vec![("test".into(), "acct".into())]),
            key("default"),
            CachePolicy::default(),
            Arc::new(MemoryAuditSink::new(10)),
            Arc::new(ManualClock::new()),
        )
        .unwrap(),
    );
    let m = manager.clone();
    let (w, c2) = (wrapped.clone(), c.clone());
    assert!(
        thread::spawn(move || m.open_object_key(&w, &c2, 1, &CallOptions::default()))
            .join()
            .is_err()
    );
    // The next reader gets an answer at once, not a wait on a flight nobody will finish.
    let started = std::time::Instant::now();
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        manager.open_object_key(&wrapped, &c, 1, &CallOptions::default())
    }));
    assert!(r.is_err(), "it became the leader and hit the same panic");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn concurrent_misses_make_one_provider_call() {
    let rig = rig();
    let c = ctx("orders", 1);
    let (dek, wrapped) = wrap_direct(&rig.kms, &key("default"), &c);
    let before = rig.kms.unwrap_calls();
    rig.kms.set_latency(Duration::from_millis(80));
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let (m, c, w) = (rig.manager.clone(), c.clone(), wrapped.clone());
            thread::spawn(move || {
                m.open_object_key(
                    &w,
                    &c,
                    1,
                    &CallOptions {
                        timeout: Duration::from_secs(5),
                        ..CallOptions::default()
                    },
                )
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap().unwrap().expose(), dek.expose());
    }
    assert_eq!(rig.kms.unwrap_calls() - before, 1, "single flight");
    assert!(rig.manager.stats().coalesced >= 1);
}

#[test]
fn a_waiter_gives_up_on_cancel_and_a_cancelled_leader_does_not_poison_the_rest() {
    let rig = rig();
    let c = ctx("orders", 1);
    let (_dek, wrapped) = wrap_direct(&rig.kms, &key("default"), &c);
    rig.kms.set_latency(Duration::from_millis(200));
    // The leader is cancelled mid-call by its own caller.
    let leader_token = CancelToken::new();
    let leader = {
        let (m, c, w, t) = (
            rig.manager.clone(),
            c.clone(),
            wrapped.clone(),
            leader_token.clone(),
        );
        thread::spawn(move || {
            m.open_object_key(
                &w,
                &c,
                1,
                &CallOptions {
                    cancel: t,
                    timeout: Duration::from_secs(5),
                },
            )
        })
    };
    thread::sleep(Duration::from_millis(40));
    // A follower with a short fuse times out waiting.
    let impatient = rig.manager.open_object_key(
        &wrapped,
        &c,
        1,
        &CallOptions {
            timeout: Duration::from_millis(30),
            ..CallOptions::default()
        },
    );
    assert_eq!(
        impatient.unwrap_err(),
        KeyError::Provider(ProviderError::Timeout)
    );
    // A follower that is cancelled stops waiting.
    let token = CancelToken::new();
    token.cancel();
    let cancelled = rig.manager.open_object_key(
        &wrapped,
        &c,
        1,
        &CallOptions {
            cancel: token,
            timeout: Duration::from_secs(5),
        },
    );
    assert_eq!(
        cancelled.unwrap_err(),
        KeyError::Provider(ProviderError::Cancelled)
    );
    // A patient follower outlives a cancelled leader and still gets a key.
    let patient = {
        let (m, c, w) = (rig.manager.clone(), c.clone(), wrapped.clone());
        thread::spawn(move || {
            m.open_object_key(
                &w,
                &c,
                1,
                &CallOptions {
                    timeout: Duration::from_secs(5),
                    ..CallOptions::default()
                },
            )
        })
    };
    thread::sleep(Duration::from_millis(30));
    leader_token.cancel();
    assert_eq!(
        leader.join().unwrap().unwrap_err(),
        KeyError::Provider(ProviderError::Cancelled)
    );
    assert!(patient.join().unwrap().is_ok());
}

#[test]
fn the_cache_is_bounded_and_evicts_oldest_first() {
    let rig = rig_with(CachePolicy {
        max_entries: 4,
        ttl: Duration::from_secs(300),
        shards: 1,
    });
    let mut made = Vec::new();
    for n in 0..10u8 {
        let c = ctx("orders", n);
        made.push((
            c.clone(),
            rig.manager
                .create_object_key(None, &c, 1, &opts())
                .unwrap()
                .1,
        ));
        assert!(rig.manager.cached() <= 4);
    }
    assert_eq!(
        (rig.manager.cached(), rig.manager.stats().evictions),
        (4, 6)
    );
    // The newest four are cached; the oldest costs a provider call.
    let calls = rig.kms.unwrap_calls();
    for (c, w) in &made[6..] {
        rig.manager.open_object_key(w, c, 1, &opts()).unwrap();
    }
    assert_eq!(rig.kms.unwrap_calls(), calls);
    rig.manager
        .open_object_key(&made[0].1, &made[0].0, 1, &opts())
        .unwrap();
    assert_eq!(rig.kms.unwrap_calls(), calls + 1);
    assert!(rig.manager.cached() <= 4);
}

#[test]
fn a_sharded_cache_stays_within_its_total_bound_and_invalidates_across_shards() {
    for (max, shards) in [(10usize, 4usize), (16, 16), (7, 3)] {
        let rig = rig_with(CachePolicy {
            max_entries: max,
            ttl: Duration::from_secs(300),
            shards,
        });
        for n in 0..200u8 {
            rig.manager
                .create_object_key(None, &ctx("orders", n), 1, &opts())
                .unwrap();
            assert!(rig.manager.cached() <= max, "{max}/{shards}");
        }
        // Digests spread over every shard, so the cache fills to its bound rather than to one shard's.
        assert_eq!(rig.manager.cached(), max, "{max}/{shards}");
        assert_eq!(rig.manager.invalidate_index(&idx("orders"), 2), max);
        assert_eq!(rig.manager.cached(), 0);
    }
}

#[test]
fn invalidation_leaves_the_eviction_queue_consistent() {
    let rig = rig_with(CachePolicy {
        max_entries: 3,
        ttl: Duration::from_secs(300),
        shards: 1,
    });
    for n in 0..3u8 {
        rig.manager
            .create_object_key(None, &ctx("orders", n), 1, &opts())
            .unwrap();
    }
    assert_eq!(rig.manager.invalidate_index(&idx("orders"), 2), 3);
    assert_eq!(rig.manager.stats().invalidated, 3);
    for n in 0..6u8 {
        rig.manager
            .create_object_key(None, &ctx("logs", n), 1, &opts())
            .unwrap();
        assert!(rig.manager.cached() <= 3);
    }
    assert_eq!(rig.manager.cached(), 3);
}

// ---- secrecy -----------------------------------------------------------------------------------------

fn assert_zeroizes<T: ZeroizeOnDrop>() {}

#[test]
fn key_material_never_reaches_debug_audit_or_error_text() {
    assert_zeroizes::<SecretKey>();
    let rig = rig();
    let c = ctx("orders", 1);
    let (dek, wrapped) = rig.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    let plaintext = *dek.expose();
    let hex: String = plaintext.iter().map(|b| format!("{b:02x}")).collect();
    let mut haystacks = vec![
        format!("{dek:?}"),
        format!("{wrapped:?}"),
        format!("{:?}", rig.manager.stats()),
    ];
    let _ = rig
        .manager
        .open_object_key(&wrapped, &ctx("payroll", 1), 1, &opts())
        .map_err(|e| haystacks.push(format!("{e} {e:?}")));
    rig.kms.revoke(&key("default"));
    rig.manager.invalidate_index(&idx("orders"), 2);
    let denied = rig
        .manager
        .open_object_key(&wrapped, &c, 2, &opts())
        .unwrap_err();
    haystacks.push(format!("{denied} {denied:?}"));
    for e in rig.audit.events() {
        haystacks.push(e.to_json());
        haystacks.push(format!("{e:?}"));
    }
    for text in &haystacks {
        assert!(!text.contains(&hex), "plaintext key in: {text}");
        assert!(
            !text.contains(&format!("{plaintext:?}")),
            "plaintext key bytes in: {text}"
        );
    }
    assert!(format!("{dek:?}").contains("redacted"));
    // The wrapped bytes are not printed either, so a log line cannot be replayed as a key.
    assert!(!format!("{wrapped:?}").contains(&format!("{:?}", wrapped.bytes())));
}

#[test]
fn wiping_a_key_zeroes_it() {
    let mut k = SecretKey::from_bytes([0xAB; 32]);
    assert_eq!(k.expose(), &[0xAB; 32]);
    k.wipe();
    assert_eq!(k.expose(), &[0; 32]);
    let a = SecretKey::generate().unwrap();
    let b = SecretKey::generate().unwrap();
    assert_ne!(a.expose(), b.expose());
}

#[test]
fn audit_records_every_provider_call_in_a_fixed_schema() {
    let rig = rig();
    let c = ctx("orders", 1);
    let (_dek, wrapped) = rig.manager.create_object_key(None, &c, 3, &opts()).unwrap();
    rig.manager.invalidate_index(&idx("orders"), 4);
    rig.kms.inject(Fault::Throttle(Duration::from_secs(1)), 1);
    assert!(
        rig.manager
            .open_object_key(&wrapped, &c, 4, &opts())
            .is_err()
    );
    rig.manager
        .open_object_key(&wrapped, &c, 4, &opts())
        .unwrap();
    let events = rig.audit.events();
    let summary: Vec<_> = events
        .iter()
        .map(|e| (e.op, e.generation, e.outcome))
        .collect();
    assert_eq!(
        summary,
        vec![
            (AuditOp::Wrap, 3, "ok"),
            (AuditOp::Invalidate, 4, "ok"),
            (AuditOp::Unwrap, 4, "throttled"),
            (AuditOp::Unwrap, 4, "ok"),
        ]
    );
    let json = events[0].to_json();
    assert!(
        json.starts_with("{\"at_ms\":")
            && json.contains("\"op\":\"wrap\"")
            && json.contains("\"index\":\"orders\"")
    );
    assert!(
        json.contains("\"key\":\"test/acct/default\"")
            && json.contains("\"version\":\"v1\"")
            && json.ends_with("\"outcome\":\"ok\"}")
    );
    // An event with no key omits the field instead of printing null.
    assert!(!events[1].to_json().contains("\"key\""));
}

#[test]
fn the_audit_sink_is_bounded() {
    let sink = MemoryAuditSink::new(3);
    let rig_audit = |n: u64| hsearch_security::audit::AuditEvent {
        at: std::time::SystemTime::UNIX_EPOCH,
        op: AuditOp::Wrap,
        index: idx("orders"),
        key: None,
        version: None,
        generation: n,
        outcome: "ok",
    };
    for n in 0..10 {
        hsearch_security::audit::AuditSink::record(&sink, rig_audit(n));
    }
    let kept: Vec<_> = sink.events().iter().map(|e| e.generation).collect();
    assert_eq!((kept, sink.dropped()), (vec![7, 8, 9], 7));
}
