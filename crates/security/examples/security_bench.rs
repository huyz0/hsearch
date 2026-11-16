//! Hot-path numbers for M05.01 and M05.02. Run in release:
//! `cargo run --release -p hsearch-security --all-features --example security_bench`.
//!
//! Reports the cost of what sits on a request path (an authorization decision, a data-key cache hit, a
//! cache hit under contention) and of what does not (a provider unwrap, a certificate chain verification,
//! which happens once per connection or per rotation). Single-process, in-memory test provider: relative
//! numbers, not a KMS measurement.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use hsearch_security::audit::MemoryAuditSink;
use hsearch_security::clock::SystemClock;
use hsearch_security::dev_ca::{DevCa, NodeCert};
use hsearch_security::identity::{CertUse, Channel, IdentityProvider, Presented, Role};
use hsearch_security::ids::{ClusterId, IndexId, TrustDomain};
use hsearch_security::keys::{CallOptions, KeyContext, KeyRef, Profile};
use hsearch_security::manager::{CachePolicy, KeyAllowlist, KeyManager};
use hsearch_security::policy::{Authorizer, IndexPolicy, Permission, PolicyStore};
use hsearch_security::test_kms::TestKms;
use hsearch_security::x509::{AcceptedIdentity, ChainVerifier, SpiffeIdentityProvider};

fn per_op(label: &str, iterations: u64, mut f: impl FnMut()) {
    for _ in 0..iterations / 10 {
        f();
    }
    let start = Instant::now();
    for _ in 0..iterations {
        f();
    }
    let ns = start.elapsed().as_nanos() as f64 / iterations as f64;
    println!("{label:<58} {ns:>10.1} ns/op");
}

fn main() {
    println!(
        "hsearch-security hot path (release, provisional: one shared VM, in-memory test provider)\n"
    );

    // ---- authorization: per request ----
    let store = Arc::new(PolicyStore::new());
    let cluster = ClusterId::new("prod").unwrap();
    let now = SystemTime::now();
    // One policy in a store of 100,000, to show the lookup does not depend on how many indexes a node holds.
    for n in 0..100_000 {
        let index = IndexId::new(&format!("idx-{n}")).unwrap();
        store
            .install(
                IndexPolicy::new(index, 1)
                    .grant("token:alice", Permission::Search)
                    .unwrap(),
                now,
            )
            .unwrap();
    }
    let authz = Authorizer::new(cluster.clone(), store, Duration::from_secs(60));
    let mut tokens = hsearch_security::identity::TokenProvider::new(cluster.clone());
    tokens
        .add("alice", "a-long-enough-secret-token-0001", Role::Client)
        .unwrap();
    let alice = tokens
        .verify(&Presented::Bearer("a-long-enough-secret-token-0001"), now)
        .unwrap();
    assert_eq!(alice.channel(), Channel::Token);
    let index = IndexId::new("idx-54321").unwrap();
    per_op(
        "token verify (SHA-256 of the token, table lookup)",
        1_000_000,
        || {
            std::hint::black_box(
                tokens
                    .verify(&Presented::Bearer("a-long-enough-secret-token-0001"), now)
                    .unwrap(),
            );
        },
    );
    per_op(
        "authorize_api (1 policy of 100,000, grant lookup)",
        2_000_000,
        || {
            std::hint::black_box(
                authz
                    .authorize_api(&alice, Permission::Search, &index, now)
                    .unwrap(),
            );
        },
    );

    // ---- data-key cache: per read of an encrypted object ----
    let shards: usize = std::env::var("SEC_BENCH_SHARDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    println!("data-key cache shards: {shards}");
    let kms = Arc::new(TestKms::new());
    let default_key = KeyRef::new("test/acct/default").unwrap();
    kms.create_key(&default_key).unwrap();
    let manager = Arc::new(
        KeyManager::new(
            Profile::Development,
            kms.clone(),
            KeyAllowlist::new(vec![("test".into(), "acct".into())]),
            default_key,
            CachePolicy {
                max_entries: 100_000,
                ttl: Duration::from_secs(300),
                shards,
            },
            Arc::new(MemoryAuditSink::new(1024)),
            Arc::new(SystemClock::default()),
        )
        .unwrap(),
    );
    let ctx = |n: u32| KeyContext {
        index: IndexId::new("orders").unwrap(),
        domain: [1; 16],
        object: {
            let mut o = [0u8; 16];
            o[..4].copy_from_slice(&n.to_le_bytes());
            o
        },
    };
    let opts = CallOptions::default();
    let (_dek, wrapped) = manager.create_object_key(None, &ctx(0), 1, &opts).unwrap();
    let c0 = ctx(0);
    per_op("open_object_key, cache hit (1 thread)", 2_000_000, || {
        std::hint::black_box(manager.open_object_key(&wrapped, &c0, 1, &opts).unwrap());
    });
    for threads in [4usize, 16] {
        let per_thread = 500_000u64;
        let start = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (m, w, c) = (manager.clone(), wrapped.clone(), c0.clone());
                std::thread::spawn(move || {
                    let opts = CallOptions::default();
                    for _ in 0..per_thread {
                        std::hint::black_box(m.open_object_key(&w, &c, 1, &opts).unwrap());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let total = per_thread * threads as u64;
        let elapsed = start.elapsed();
        println!(
            "{:<58} {:>10.1} ns/op   ({:.1} M hits/s total)",
            format!("open_object_key, cache hit ({threads} threads, one key)"),
            elapsed.as_nanos() as f64 * threads as f64 / total as f64,
            total as f64 / elapsed.as_secs_f64() / 1e6
        );
    }
    // Many different objects read at once (the usual shape: a query touches many segments), 64 objects spread
    // over the threads, so only the cache's own structure can contend.
    let objects: Vec<_> = (1000..1064)
        .map(|n| {
            (
                ctx(n),
                manager
                    .create_object_key(None, &ctx(n), 1, &opts)
                    .unwrap()
                    .1,
            )
        })
        .collect();
    let objects = Arc::new(objects);
    for threads in [4usize, 16] {
        let per_thread = 500_000u64;
        let start = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (m, objects) = (manager.clone(), objects.clone());
                std::thread::spawn(move || {
                    let opts = CallOptions::default();
                    for i in 0..per_thread as usize {
                        let (c, w) = &objects[(t * 7 + i) % objects.len()];
                        std::hint::black_box(m.open_object_key(w, c, 1, &opts).unwrap());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let total = per_thread * threads as u64;
        let elapsed = start.elapsed();
        println!(
            "{:<58} {:>10.1} ns/op   ({:.1} M hits/s total)",
            format!("open_object_key, cache hit ({threads} threads, 64 objects)"),
            elapsed.as_nanos() as f64 * threads as f64 / total as f64,
            total as f64 / elapsed.as_secs_f64() / 1e6
        );
    }
    // Misses: a distinct object each time, so each costs the provider's unwrap (AES-256-GCM here, a network
    // round trip with a real KMS) plus a cache insert, with eviction once the bound is reached.
    let made: Vec<_> = (1..=20_000)
        .map(|n| {
            (
                ctx(n),
                manager
                    .create_object_key(None, &ctx(n), 1, &opts)
                    .unwrap()
                    .1,
            )
        })
        .collect();
    manager.invalidate_index(&IndexId::new("orders").unwrap(), 1);
    let mut next = 0usize;
    per_op(
        "open_object_key, cache miss (in-memory provider)",
        20_000,
        || {
            let (c, w) = &made[next % made.len()];
            next += 1;
            std::hint::black_box(manager.open_object_key(w, c, 1, &opts).unwrap());
        },
    );
    let wraps = kms.wrap_calls();
    per_op(
        "create_object_key (generate + wrap + cache insert)",
        20_000,
        || {
            std::hint::black_box(manager.create_object_key(None, &ctx(99), 1, &opts).unwrap());
        },
    );
    println!(
        "  provider wraps for those: {} (one per object, as designed)",
        kms.wrap_calls() - wraps
    );

    // ---- certificate chain: once per connection, not per request ----
    let ca = DevCa::new("bench root").unwrap();
    let verifier = Arc::new(ChainVerifier::new(vec![ca.der()], Duration::from_secs(30)).unwrap());
    let provider = SpiffeIdentityProvider::new(
        verifier,
        vec![AcceptedIdentity {
            trust_domain: TrustDomain::new("example.test").unwrap(),
            cluster,
            roles: [Role::SearchNode].into_iter().collect(),
        }],
    )
    .unwrap();
    let leaf = ca
        .issue(&NodeCert::spiffe(
            "spiffe://example.test/hsearch/prod/search",
        ))
        .unwrap()
        .der;
    let chain = [leaf];
    per_op(
        "SPIFFE certificate verify (per connection, ECDSA P-256)",
        5_000,
        || {
            std::hint::black_box(
                provider
                    .verify(
                        &Presented::Certificate {
                            chain: &chain,
                            usage: CertUse::Client,
                        },
                        now,
                    )
                    .unwrap(),
            );
        },
    );
}
