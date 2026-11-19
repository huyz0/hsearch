//! The serving host against a real S3-compatible store (RustFS in CI): acknowledged writes
//! recover after a restart, and neither writing nor restarting issues a LIST. Needs
//! `HSEARCH_S3_ENDPOINT`, `HSEARCH_S3_ACCESS_KEY` and `HSEARCH_S3_SECRET_KEY`; without them each
//! test prints SKIPPED and passes, unless `HSEARCH_S3_REQUIRED=1` (CI).
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use hsearch_commit::{AnyDependencies as AnyDeps, Committer, Dependencies};
use hsearch_engine::{Kind, Schema, StoredValue};
use hsearch_format::{Durability, Id, KeyIdentity, PolicyTuple};
use hsearch_index::{Field, Op, Wait};
use hsearch_lane::batch::BatchPolicy;
use hsearch_lane::{LaneId, LaneWriter, block_on, create_head, create_lane, shared_group};
use hsearch_object::{Budget, Cancellation, RequestContext};
use hsearch_s3::{S3Config, S3Store};
use hsearch_store::host::{Config, Host, OpenArgs};
use hsearch_store::publish::{AnyDependencies, Publisher};
use hsearch_store::recover::ReplayLimits;
use support::{INDEX, SHARD, layout};

const B: Id = [1; 16];
static NEXT: AtomicU32 = AtomicU32::new(0);

fn ctx() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(60),
        Cancellation::default(),
        Budget {
            request_bytes: 32 << 20,
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

fn policy() -> PolicyTuple {
    PolicyTuple {
        key: KeyIdentity::DeploymentDefault,
        residency: "region-a".into(),
        durability: Durability::Object,
        format_version: 1,
    }
}

fn lane_of(store: &Arc<S3Store>) -> LaneId {
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
    lane
}

fn committer(store: &Arc<S3Store>, lane: LaneId) -> Committer<S3Store, AnyDeps> {
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
    Committer::new(w, AnyDeps, policy).unwrap()
}

fn schema() -> Schema {
    Schema::new(1, &[("tag", Kind::Keyword)]).unwrap()
}

fn open(store: &Arc<S3Store>, lane: LaneId, label: &str, seed: u8) -> Host {
    let publisher = Publisher::new(
        Arc::clone(store),
        layout(),
        5 << 20,
        Duration::from_secs(60),
    );
    Host::open(
        OpenArgs {
            store: Arc::clone(store),
            publisher,
            committer: committer(store, lane),
            lane,
            dependencies: AnyDependencies,
            schema: schema(),
        },
        Config {
            index: INDEX,
            shard: SHARD,
            incarnation: 1,
            scratch: std::env::temp_dir()
                .join(format!("hsearch-host-s3-{label}-{}", std::process::id())),
            limits: ReplayLimits {
                slots: 10_000,
                records: 10_000,
                bytes: 64 << 20,
            },
            deps: Dependencies {
                schema: [1; 32],
                crypto: [1; 32],
                resolved: [1; 32],
            },
            engine_format: "lucene-test".into(),
            mapping: [5; 32],
            crypto_envelope: [6; 16],
            window: Duration::ZERO,
            base_every_batches: 2,
            token_seed: [seed; 8],
            pin: None,
        },
    )
    .unwrap()
}

fn put(id: &str, tag: &str) -> Op {
    Op::Put {
        id: id.into(),
        fields: vec![Field::Str(Arc::new(tag.into()))],
    }
}

fn tag(host: &Host, id: &str) -> Option<String> {
    let reader = host.reader()?;
    reader.reader.get(id, &["tag"]).unwrap().map(|hit| {
        let StoredValue::String(tag) = &hit.fields[0].1 else {
            panic!("tag is a string")
        };
        tag.clone()
    })
}

#[test]
fn acknowledged_writes_survive_a_restart_and_nothing_lists() {
    let Some(store) = store() else { return };
    let lane = lane_of(&store);
    let host = open(&store, lane, "a", 1);
    let before = store.counts();
    for i in 0..5 {
        host.bulk(vec![put(&format!("d{i}"), "v")], Wait::Durable)
            .unwrap();
    }
    host.refresh().unwrap();
    let writing = store.counts();
    eprintln!(
        "provisional: 5 acknowledged writes and 2 bases on RustFS: {} puts, {} gets, {} heads, {} lists, {} deletes",
        writing.put - before.put,
        writing.get - before.get,
        writing.head - before.head,
        writing.list - before.list,
        writing.delete - before.delete
    );
    assert_eq!(writing.list, before.list, "a write path never lists");
    host.stop();
    let reopened = store.counts();
    let again = open(&store, lane, "b", 2);
    let restart = store.counts();
    eprintln!(
        "provisional: one restart on RustFS: {} puts, {} gets, {} heads, {} lists",
        restart.put - reopened.put,
        restart.get - reopened.get,
        restart.head - reopened.head,
        restart.list - reopened.list
    );
    assert_eq!(restart.list, reopened.list, "a restart never lists");
    for i in 0..5 {
        assert_eq!(tag(&again, &format!("d{i}")).as_deref(), Some("v"));
    }
    let next = again
        .bulk(vec![put("late", "1")], Wait::Searchable)
        .unwrap();
    assert!(next[0].version > 5 << 20);
}
