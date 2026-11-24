//! Publish, recovery, compaction, pins and collection against a real S3-compatible store (RustFS in
//! CI). Needs `HSEARCH_S3_ENDPOINT`, `HSEARCH_S3_ACCESS_KEY` and `HSEARCH_S3_SECRET_KEY`; without
//! them each test prints SKIPPED and passes, unless `HSEARCH_S3_REQUIRED=1` (CI), which makes a
//! missing store a failure rather than a silent skip.
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use hsearch_commit::{AnyDependencies as AnyDeps, Committer, Outcome, Receipt, Request};
use hsearch_format::{Durability, Id, KeyIdentity, PinRecord, PolicyTuple};
use hsearch_lane::batch::BatchPolicy;
use hsearch_lane::{
    LaneId, LaneWriter, block_on, create_head, create_lane, read_head, shared_group,
};
use hsearch_object::{
    Budget, Cancellation, Error as StoreError, Inventory, Metadata, MetadataStore, Mutation as Put,
    ObjectBackend, ObjectKey, Payload, Precondition, RequestContext, RequestId,
};
use hsearch_s3::{S3Config, S3Store};
use hsearch_store::compact::Compactor;
use hsearch_store::pins::{Error as PinError, Pins};
use hsearch_store::publish::{AnyDependencies, Publisher};
use hsearch_store::recover::{Expect, Record, ReplayLimits, Target, recover};
use hsearch_store::sweep::{Config, Mode, ShardRef, Sweeper};
use support::*;

const B: Id = [1; 16];
static NEXT: AtomicU32 = AtomicU32::new(0);

fn ctx() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 8 << 20,
            response_bytes: 8 << 20,
            attempts: 8,
        },
    )
    .unwrap()
}

/// A store on a fresh versioned bucket, or `None` when no endpoint is configured.
fn store() -> Option<Arc<S3Store>> {
    let Ok(endpoint) = std::env::var("HSEARCH_S3_ENDPOINT") else {
        assert!(
            std::env::var("HSEARCH_S3_REQUIRED").as_deref() != Ok("1"),
            "HSEARCH_S3_REQUIRED=1 but HSEARCH_S3_ENDPOINT is unset"
        );
        eprintln!(
            "SKIPPED: HSEARCH_S3_ENDPOINT is not set; nothing was checked against a real store"
        );
        return None;
    };
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let store = S3Store::new(S3Config {
        endpoint,
        bucket: format!("hs-store-{nanos}-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        access_key: std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        secret_key: std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
        region: "us-east-1".into(),
    })
    .unwrap();
    store.prepare_bucket(&ctx()).expect("prepare bucket");
    Some(Arc::new(store))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn policy() -> PolicyTuple {
    PolicyTuple {
        key: KeyIdentity::DeploymentDefault,
        residency: "region-a".into(),
        durability: Durability::Object,
        format_version: 1,
    }
}

fn publisher(store: &Arc<S3Store>) -> Publisher<S3Store> {
    Publisher::new(
        Arc::clone(store),
        layout(),
        5 << 20,
        Duration::from_secs(60),
    )
}

fn sweeper(store: &Arc<S3Store>) -> Sweeper<S3Store> {
    Sweeper::new(Arc::clone(store), layout(), Duration::from_secs(60))
}

fn config(mode: Mode, now: u64) -> Config {
    Config {
        mode,
        now_unix_ms: now,
        page: 1000,
        max_tracked: 1 << 20,
        max_roots: 1 << 20,
        max_deletes: usize::MAX,
        orphan_grace_ms: None,
    }
}

/// A shared lane of two indexes with its head, and the committer that writes it.
fn open_lane(store: &Arc<S3Store>) -> (LaneId, Committer<S3Store, AnyDeps>) {
    let (domain, group) = shared_group(&policy(), 0);
    let lane = LaneId::new(domain);
    block_on(create_lane(
        &**store,
        &layout(),
        &lane,
        &policy(),
        group,
        1000,
        &[B, INDEX],
    ))
    .unwrap();
    block_on(create_head(&**store, &layout(), &lane, Durability::Object)).unwrap();
    let (w, _) = block_on(LaneWriter::open_with_head(
        Arc::clone(store),
        layout(),
        lane,
        |_| Ok(()),
    ))
    .unwrap();
    let policy = BatchPolicy {
        max_records: 1,
        max_bytes: 4 << 20,
        max_wait: Duration::from_millis(1),
    };
    (lane, Committer::new(w, AnyDeps, policy).unwrap())
}

fn commit(c: &mut Committer<S3Store, AnyDeps>, seq: u64) -> Receipt {
    let r = Request {
        index: INDEX,
        seq,
        body: format!("a{seq}").into_bytes(),
        delete: false,
        deps: hsearch_commit::Dependencies {
            schema: [1; 32],
            crypto: [1; 32],
            resolved: [1; 32],
        },
    };
    match block_on(c.commit(vec![r])).remove(0) {
        Outcome::Committed(receipt) => receipt,
        other => panic!("not committed: {other:?}"),
    }
}

fn recovered(store: &Arc<S3Store>, lane: &LaneId, expect: Expect) -> Vec<(u64, String)> {
    let p = publisher(store);
    let mut got = Vec::new();
    let target = Target {
        lane,
        index: INDEX,
        shard: SHARD,
        incarnation: 1,
        limits: ReplayLimits {
            slots: 10_000,
            records: 10_000,
            bytes: 64 << 20,
        },
        expect,
        base: hsearch_store::pointer::BaseSource::Head,
    };
    recover(&p, &target, &AnyDependencies, |r: &Record<'_>| {
        got.push((r.seq, String::from_utf8(r.body.to_vec()).unwrap()));
        Ok(())
    })
    .unwrap();
    got
}

fn exists(store: &Arc<S3Store>, key: &str) -> bool {
    match block_on(store.head(&ctx(), &ObjectKey::new(key.as_bytes()).unwrap())) {
        Ok(_) => true,
        Err(StoreError::NotFound) => false,
        Err(e) => panic!("head {key}: {e:?}"),
    }
}

#[test]
fn publish_recover_compact_and_sweep_on_a_real_store() {
    let Some(store) = store() else { return };
    let (lane, mut c) = open_lane(&store);
    let ledger: Vec<Receipt> = (0..10).map(|seq| commit(&mut c, seq)).collect();
    let last = ledger.iter().map(|r| r.ordinal).max().unwrap();
    drop(c);
    let expect = Expect {
        ordinal: last + 1,
        seq: 10,
    };
    let want: Vec<(u64, String)> = (5..10).map(|s| (s, format!("a{s}"))).collect();
    with_commit("rustfs-gc", |local, _, names| {
        let p = publisher(&store);
        let s1 = spec(cut(5), 10, 1);
        let b1 = p.publish(local, names, &s1, &AnyDependencies).unwrap();
        let (head, version) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
        let cover = ledger.iter().find(|r| r.seq == 5).unwrap().ordinal;
        p.activate(
            &at(&lane, &head, &version),
            &b1,
            &s1,
            cover,
            &AnyDependencies,
        )
        .unwrap();
        assert_eq!(
            recovered(&store, &lane, expect),
            want,
            "recovery on the first base"
        );
        // Compact: the same history, rewritten.
        let comp = Compactor::new(
            Arc::clone(&store),
            layout(),
            5 << 20,
            Duration::from_secs(60),
        );
        let mut s2 = spec(cut(5), 10, 2);
        s2.previous = Some(&b1.root);
        let (head, version) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
        let (b2, next, _) = comp
            .compact(
                local,
                names,
                &s2,
                &at(&lane, &head, &version),
                &AnyDependencies,
            )
            .unwrap();
        assert_eq!(next.checkpoint, Some(b2.manifest));
        assert_eq!((next.cover, next.publication_generation), (cover, 2));
        assert_eq!(
            recovered(&store, &lane, expect),
            want,
            "recovery on the compacted base"
        );
        // Sweep against the real listing.
        let root1 = layout().manifest_root(&INDEX, &SHARD, 1, &b1.manifest);
        let root2 = layout().manifest_root(&INDEX, &SHARD, 1, &b2.manifest);
        assert!(exists(&store, &root1) && exists(&store, &root2));
        let shard = ShardRef {
            shard: SHARD,
            incarnation: 1,
            lane,
            base: hsearch_store::pointer::BaseSource::Head,
        };
        let dry = sweeper(&store)
            .run(&INDEX, &[shard], &config(Mode::DryRun, now_ms()))
            .unwrap();
        assert!(exists(&store, &root1), "a dry run deletes nothing");
        let before = store.counts();
        let done = sweeper(&store)
            .run(&INDEX, &[shard], &config(Mode::Sweep, now_ms()))
            .unwrap();
        let after = store.counts();
        assert!(done.complete);
        assert_eq!(
            (done.roots_swept, done.files_swept),
            (dry.roots_swept, dry.files_swept)
        );
        assert_eq!(
            (done.roots_swept, done.pages_swept, done.files_swept),
            (1, 1, names.len())
        );
        assert!(!exists(&store, &root1), "the replaced root is gone");
        assert!(exists(&store, &root2), "the live root stays");
        p.verify_root(&INDEX, &SHARD, 1, &b2.manifest, &AnyDependencies)
            .unwrap();
        assert!(
            p.verify_root(&INDEX, &SHARD, 1, &b1.manifest, &AnyDependencies)
                .is_err()
        );
        assert_eq!(
            recovered(&store, &lane, expect),
            want,
            "recovery after the sweep"
        );
        // The cost of the sweep on the wire: one LIST per prefix (pins, roots, payloads, pages),
        // one DELETE per object reclaimed, reads that do not grow with the files.
        eprintln!(
            "sweep of one replaced base ({} files) on S3: list {} delete {} get {} head {} put {}; report {:?}",
            names.len(),
            after.list - before.list,
            after.delete - before.delete,
            after.get - before.get,
            after.head - before.head,
            after.put - before.put,
            done.requests
        );
        assert_eq!(after.list - before.list, 4);
        assert_eq!(after.delete - before.delete, (1 + 1 + names.len()) as u64);
        // A second sweep has nothing left to delete.
        let again = sweeper(&store)
            .run(&INDEX, &[shard], &config(Mode::Sweep, now_ms()))
            .unwrap();
        assert_eq!(
            (again.roots_swept, again.pages_swept, again.files_swept),
            (0, 0, 0)
        );
    });
}

#[test]
fn a_pin_holds_a_base_on_a_real_store_until_it_expires() {
    let Some(store) = store() else { return };
    let (lane, c) = open_lane(&store);
    drop(c);
    with_commit("rustfs-pin", |local, _, names| {
        let p = publisher(&store);
        let s1 = spec(cut(3), 3, 1);
        let b1 = p.publish(local, names, &s1, &AnyDependencies).unwrap();
        let (head, version) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
        p.activate(&at(&lane, &head, &version), &b1, &s1, 3, &AnyDependencies)
            .unwrap();
        let pins = Pins::new(Arc::clone(&store), layout(), Duration::from_secs(60));
        let now = now_ms();
        let record = PinRecord {
            pin: [4; 16],
            index: INDEX,
            manifest: b1.manifest,
            manifest_generation: 1,
            expires_unix_ms: now + 3_600_000,
            token: token(40),
        };
        pins.register(&record, &SHARD, 1, now).unwrap();
        let comp = Compactor::new(
            Arc::clone(&store),
            layout(),
            5 << 20,
            Duration::from_secs(60),
        );
        let mut s2 = spec(cut(3), 3, 2);
        s2.previous = Some(&b1.root);
        let (head, version) = block_on(read_head(&*store, &layout(), &lane)).unwrap();
        let (b2, _, _) = comp
            .compact(
                local,
                names,
                &s2,
                &at(&lane, &head, &version),
                &AnyDependencies,
            )
            .unwrap();
        let shard = ShardRef {
            shard: SHARD,
            incarnation: 1,
            lane,
            base: hsearch_store::pointer::BaseSource::Head,
        };
        let held = sweeper(&store)
            .run(&INDEX, &[shard], &config(Mode::Sweep, now))
            .unwrap();
        assert_eq!((held.roots_swept, held.pins_active), (0, 1));
        p.verify_root(&INDEX, &SHARD, 1, &b1.manifest, &AnyDependencies)
            .unwrap();
        // A pin on the swept generation is refused once the boundary is above it.
        let late = PinRecord {
            pin: [5; 16],
            token: token(41),
            ..record
        };
        let refused = pins.register(&late, &SHARD, 1, now);
        assert!(
            matches!(refused, Err(PinError::Swept { boundary: 2 })),
            "{refused:?}"
        );
        // Two hours on, the pin has expired: the base goes, the pin object with it.
        let freed = sweeper(&store)
            .run(&INDEX, &[shard], &config(Mode::Sweep, now + 7_200_000))
            .unwrap();
        assert_eq!((freed.roots_swept, freed.pins_expired), (1, 1));
        assert!(
            p.verify_root(&INDEX, &SHARD, 1, &b1.manifest, &AnyDependencies)
                .is_err()
        );
        p.verify_root(&INDEX, &SHARD, 1, &b2.manifest, &AnyDependencies)
            .unwrap();
        assert!(!exists(&store, &layout().pin(&INDEX, &record.pin)));
    });
}

/// The listing the sweep stands on: pages, order, the versions and modification times a removal and
/// an age check need, against what the same store says on HEAD.
#[test]
fn listing_returns_the_exact_versions_in_order_and_follows_pages() {
    let Some(store) = store() else { return };
    let prefix = format!("listing/{}/", now_ms());
    let mut made = Vec::new();
    for i in 0..7u8 {
        let key = ObjectKey::new(format!("{prefix}k{i}").as_bytes()).unwrap();
        let put = Put {
            request_id: RequestId::new(format!("list-{prefix}{i}").as_bytes()).unwrap(),
            key: key.clone(),
            payload: Payload::new(&[i; 16]).unwrap(),
            metadata: Metadata::new(b"").unwrap(),
            precondition: Precondition::Absent,
        };
        let receipt = block_on(store.create(&ctx(), &put)).unwrap();
        made.push((key, receipt.object.version));
    }
    // Something that merely shares the start of the prefix is not listed.
    let other = ObjectKey::new(format!("listing/other-{}", now_ms()).as_bytes()).unwrap();
    let put = Put {
        request_id: RequestId::new(b"list-other").unwrap(),
        key: other,
        payload: Payload::new(b"x").unwrap(),
        metadata: Metadata::new(b"").unwrap(),
        precondition: Precondition::Absent,
    };
    block_on(store.create(&ctx(), &put)).unwrap();
    let before = store.counts();
    let mut listed = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0;
    loop {
        let page = block_on(store.list(&ctx(), &prefix, after.as_deref(), 3)).unwrap();
        pages += 1;
        assert!(page.objects.len() <= 3);
        if let Some(last) = page.objects.last() {
            after = Some(String::from_utf8(last.key.as_bytes().to_vec()).unwrap());
        }
        listed.extend(page.objects);
        if !page.more {
            break;
        }
        assert!(pages < 10, "no progress");
    }
    assert_eq!(pages, 3, "7 objects, 3 a page");
    assert_eq!(
        store.counts().list - before.list,
        3,
        "one LIST request a page"
    );
    assert_eq!(listed.len(), 7);
    for (got, (key, version)) in listed.iter().zip(&made) {
        assert_eq!(&got.key, key);
        // The same version a write returned (the id half of the token; a removal names it).
        let id = |t: &hsearch_object::VersionToken| {
            t.as_bytes().split(|b| *b == b'\n').next().unwrap().to_vec()
        };
        assert_eq!(id(&got.version), id(version), "{:?}", got.key);
        let skew = now_ms().abs_diff(got.modified_unix_ms);
        assert!(skew < 3_600_000, "last-modified is {skew} ms from now");
    }
    for got in &listed {
        block_on(store.remove_version(&ctx(), &got.key, &got.version)).unwrap();
    }
    let empty = block_on(store.list(&ctx(), &prefix, None, 1000)).unwrap();
    assert!(empty.objects.is_empty() && !empty.more);
}
