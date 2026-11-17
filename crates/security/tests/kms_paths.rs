//! M05.08 request accounting: how many calls reach the key provider on each path, every attempt counted.
//!
//! Counts come from `MeteredProvider` around the in-memory test provider. A call count is a property of the
//! `KeyProvider` contract and the key manager, so it carries over to any provider; latency and throttling
//! thresholds do not (they need a real provider, see the evidence file).

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use hsearch_security::audit::MemoryAuditSink;
use hsearch_security::clock::ManualClock;
use hsearch_security::ids::IndexId;
use hsearch_security::keys::{CallOptions, KeyContext, KeyRef, Profile, ProviderError};
use hsearch_security::manager::{CachePolicy, KeyAllowlist, KeyError, KeyManager};
use hsearch_security::metered::{MeteredProvider, Op};
use hsearch_security::test_kms::{Fault, TestKms};

fn key(name: &str) -> KeyRef {
    KeyRef::new(&format!("test/acct/{name}")).unwrap()
}

fn ctx(object: u8) -> KeyContext {
    KeyContext {
        index: IndexId::new("orders").unwrap(),
        domain: [7; 16],
        object: [object; 16],
    }
}

struct Rig {
    kms: Arc<TestKms>,
    meter: Arc<MeteredProvider>,
    clock: Arc<ManualClock>,
    manager: Arc<KeyManager>,
}

fn rig(ttl: Duration) -> Rig {
    let kms = Arc::new(TestKms::new());
    kms.create_key(&key("default")).unwrap();
    let meter = Arc::new(MeteredProvider::new(kms.clone()));
    let clock = Arc::new(ManualClock::new());
    let manager = Arc::new(
        KeyManager::new(
            Profile::Development,
            meter.clone(),
            KeyAllowlist::new(vec![("test".into(), "acct".into())]),
            key("default"),
            CachePolicy {
                max_entries: 10_000,
                ttl,
                shards: 4,
            },
            Arc::new(MemoryAuditSink::new(100_000)),
            clock.clone(),
        )
        .unwrap(),
    );
    // Construction may call the provider; the accounting starts after it.
    meter.reset();
    Rig {
        kms,
        meter,
        clock,
        manager,
    }
}

fn opts() -> CallOptions {
    CallOptions::default()
}

fn calls(rig: &Rig) -> (u64, u64, u64) {
    let s = rig.meter.snapshot();
    (s.total(Op::Wrap), s.total(Op::Unwrap), s.total(Op::KeyInfo))
}

#[test]
fn a_new_object_costs_one_wrap_and_its_first_read_nothing() {
    let r = rig(Duration::from_secs(300));
    for n in 0..10u8 {
        let c = ctx(n);
        let (_dek, wrapped) = r.manager.create_object_key(None, &c, 1, &opts()).unwrap();
        r.manager.open_object_key(&wrapped, &c, 1, &opts()).unwrap();
    }
    assert_eq!(calls(&r), (10, 0, 0));
}

#[test]
fn warm_reads_cost_nothing_and_a_cold_object_costs_one_unwrap_per_lifetime() {
    let r = rig(Duration::from_secs(300));
    let c = ctx(1);
    let (_d, wrapped) = r.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    r.meter.reset();
    for _ in 0..10_000 {
        r.manager.open_object_key(&wrapped, &c, 1, &opts()).unwrap();
    }
    assert_eq!(r.meter.snapshot().all(), 0);
    // Three lifetimes later: one unwrap each, however many reads.
    for _ in 0..3 {
        r.clock.advance(Duration::from_secs(301));
        for _ in 0..1000 {
            r.manager.open_object_key(&wrapped, &c, 1, &opts()).unwrap();
        }
    }
    assert_eq!(calls(&r), (0, 3, 0));
}

#[test]
fn with_the_cache_effectively_off_every_read_is_a_provider_call() {
    // The shortest lifetime the manager accepts, with the clock moving past it before each read: this is the
    // "cache off" configuration, and it is the amplification the cache exists to remove.
    let r = rig(Duration::from_millis(1));
    let c = ctx(1);
    let (_d, wrapped) = r.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    r.meter.reset();
    for _ in 0..500 {
        r.clock.advance(Duration::from_millis(2));
        r.manager.open_object_key(&wrapped, &c, 1, &opts()).unwrap();
    }
    assert_eq!(calls(&r), (0, 500, 0));
}

#[test]
fn a_stampede_of_cold_readers_makes_one_call() {
    let r = rig(Duration::from_secs(300));
    let c = ctx(1);
    let (_d, wrapped) = r.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    r.manager
        .invalidate_index(&IndexId::new("orders").unwrap(), 1);
    r.meter.reset();
    r.kms.set_latency(Duration::from_millis(100));
    let readers: Vec<_> = (0..64)
        .map(|_| {
            let (manager, wrapped, c) = (r.manager.clone(), wrapped.clone(), c.clone());
            thread::spawn(move || manager.open_object_key(&wrapped, &c, 1, &opts()).is_ok())
        })
        .collect();
    assert!(readers.into_iter().all(|t| t.join().unwrap()));
    assert_eq!(calls(&r), (0, 1, 0));
}

#[test]
fn rotation_costs_no_calls_and_new_wraps_use_the_new_version() {
    let r = rig(Duration::from_secs(300));
    let old = ctx(1);
    let (_d, old_wrapped) = r.manager.create_object_key(None, &old, 1, &opts()).unwrap();
    r.meter.reset();
    let v2 = r.kms.rotate(&key("default")).unwrap();
    r.manager
        .note_rotation(&IndexId::new("orders").unwrap(), &key("default"), &v2, 1);
    // Noting the rotation and reading a cached old object: no calls at all.
    r.manager
        .open_object_key(&old_wrapped, &old, 1, &opts())
        .unwrap();
    assert_eq!(r.meter.snapshot().all(), 0);
    // A new object is one wrap under the new version; the old object still opens after its key expires, with one unwrap.
    let new = ctx(2);
    let (_d, new_wrapped) = r.manager.create_object_key(None, &new, 1, &opts()).unwrap();
    assert_eq!(new_wrapped.version(), &v2);
    r.clock.advance(Duration::from_secs(301));
    r.manager
        .open_object_key(&old_wrapped, &old, 1, &opts())
        .unwrap();
    assert_eq!(calls(&r), (1, 1, 0));
}

#[test]
fn rewrapping_one_object_is_at_most_one_unwrap_and_one_wrap() {
    // The provider contract has no rewrap, so moving an object's envelope to a new key version is: open the
    // data key (one unwrap when cold) and wrap it again (one wrap). K objects cost 2K calls, once.
    let r = rig(Duration::from_secs(300));
    let mut wrapped = Vec::new();
    for n in 0..20u8 {
        wrapped.push((
            ctx(n),
            r.manager
                .create_object_key(None, &ctx(n), 1, &opts())
                .unwrap()
                .1,
        ));
    }
    r.manager
        .invalidate_index(&IndexId::new("orders").unwrap(), 1);
    r.meter.reset();
    r.kms.rotate(&key("default")).unwrap();
    for (c, w) in &wrapped {
        let dek = r.manager.open_object_key(w, c, 1, &opts()).unwrap();
        let rewrapped = r.meter_wrap(c, &dek);
        assert_eq!(rewrapped.version().as_str(), "v2");
    }
    assert_eq!(calls(&r), (20, 20, 0));
}

impl Rig {
    fn meter_wrap(
        &self,
        c: &KeyContext,
        dek: &hsearch_security::keys::SecretKey,
    ) -> hsearch_security::keys::WrappedKey {
        use hsearch_security::keys::KeyProvider;
        self.meter.wrap(&key("default"), c, dek, &opts()).unwrap()
    }
}

#[test]
fn a_revoked_key_costs_one_call_per_attempt_and_no_failure_is_cached() {
    let r = rig(Duration::from_secs(300));
    let c = ctx(1);
    let (_d, wrapped) = r.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    r.manager
        .invalidate_index(&IndexId::new("orders").unwrap(), 1);
    r.meter.reset();
    r.kms.revoke(&key("default"));
    for _ in 0..100 {
        assert_eq!(
            r.manager
                .open_object_key(&wrapped, &c, 1, &opts())
                .unwrap_err(),
            KeyError::Provider(ProviderError::Revoked)
        );
    }
    // 100 refused reads, 100 provider calls: the manager has no negative cache. Callers stop the storm one
    // layer up by consulting the policy gate first (M05.05), which refuses a revoked index with no call.
    let s = r.meter.snapshot();
    assert_eq!(s.outcome(Op::Unwrap, "revoked"), 100);
    assert_eq!(s.all(), 100);
}

#[test]
fn throttles_timeouts_and_outages_are_each_counted_as_an_attempt() {
    let r = rig(Duration::from_secs(300));
    let c = ctx(1);
    let (_d, wrapped) = r.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    r.manager
        .invalidate_index(&IndexId::new("orders").unwrap(), 1);
    r.meter.reset();
    r.kms.inject(Fault::Throttle(Duration::from_millis(50)), 2);
    r.kms.inject(Fault::Timeout, 1);
    r.kms.inject(Fault::Unavailable, 1);
    let mut failures = 0;
    while r.manager.open_object_key(&wrapped, &c, 1, &opts()).is_err() {
        failures += 1;
    }
    assert_eq!(failures, 4);
    let s = r.meter.snapshot();
    assert_eq!(s.outcome(Op::Unwrap, "throttled"), 2);
    assert_eq!(s.outcome(Op::Unwrap, "timeout"), 1);
    assert_eq!(s.outcome(Op::Unwrap, "unavailable"), 1);
    assert_eq!(s.outcome(Op::Unwrap, "ok"), 1);
    assert_eq!(s.all(), 5);
    assert_eq!(r.meter.attempts(Op::Unwrap), 5);
}

#[test]
fn a_stalled_provider_shows_up_in_the_latency_histogram_not_as_a_hang() {
    let r = rig(Duration::from_secs(300));
    let c = ctx(1);
    let (_d, wrapped) = r.manager.create_object_key(None, &c, 1, &opts()).unwrap();
    r.manager
        .invalidate_index(&IndexId::new("orders").unwrap(), 1);
    r.meter.reset();
    r.kms.set_latency(Duration::from_secs(30));
    let short = CallOptions {
        timeout: Duration::from_millis(100),
        ..opts()
    };
    assert!(r.manager.open_object_key(&wrapped, &c, 1, &short).is_err());
    let s = r.meter.snapshot();
    assert_eq!(s.outcome(Op::Unwrap, "timeout"), 1);
    let h = &s.latency[&Op::Unwrap];
    assert_eq!(h.count, 1);
    assert!(h.max_us >= 100_000 && h.max_us < 1_000_000, "{}", h.max_us);
    assert!(h.quantile_us(0.5).unwrap() <= h.max_us);
}

#[test]
fn calls_scale_with_objects_not_with_reads_or_indexes_touched() {
    let r = rig(Duration::from_secs(300));
    let objects: Vec<_> = (0..50u8)
        .map(|n| {
            (
                ctx(n),
                r.manager
                    .create_object_key(None, &ctx(n), 1, &opts())
                    .unwrap()
                    .1,
            )
        })
        .collect();
    for _round in 0..200 {
        for (c, w) in &objects {
            r.manager.open_object_key(w, c, 1, &opts()).unwrap();
        }
    }
    // 50 wraps for 10,000 reads; nothing on the read path.
    assert_eq!(calls(&r), (50, 0, 0));
}

#[test]
fn the_meter_passes_everything_through_unchanged() {
    use hsearch_security::keys::KeyProvider;
    let kms = Arc::new(TestKms::new());
    kms.create_key(&key("a")).unwrap();
    let meter = MeteredProvider::new(kms.clone());
    assert_eq!(meter.name(), kms.name());
    assert!(meter.is_test());
    let info = meter.key_info(&key("a"), &opts()).unwrap();
    assert!(info.usable);
    assert_eq!(
        meter.key_info(&key("missing"), &opts()).unwrap_err(),
        ProviderError::NotFound
    );
    assert_eq!(meter.snapshot().outcome(Op::KeyInfo, "not_found"), 1);
    assert_eq!(meter.destroy_key(&key("a"), &opts()), Ok(()));
    assert_eq!(meter.attempts(Op::Destroy), 1);
    assert_eq!(
        meter.key_info(&key("a"), &opts()).map(|i| i.usable),
        Ok(false),
        "the wrapped provider's own state is what the meter reports on"
    );
}
