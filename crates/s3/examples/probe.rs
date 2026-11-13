//! Capability probe for an S3-compatible store (M03.02).
//!
//! `probe run` checks every capability the adapter relies on and exits non-zero
//! on the first required failure, printing one `PASS`/`FAIL` line per capability
//! and a final JSON evidence line. `probe write` then `probe verify` (with the
//! store restarted in between) check that acknowledged objects survive a restart.
//!
//! Environment: `HSEARCH_S3_ENDPOINT` (host:port), `HSEARCH_S3_ACCESS_KEY`,
//! `HSEARCH_S3_SECRET_KEY`, `HSEARCH_S3_BUCKET` (default `hsearch-probe`) and
//! `HSEARCH_S3_IMAGE` (the deployed image digest, recorded verbatim in evidence).
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error, Metadata, MetadataStore, Mutation, ObjectBackend,
    ObjectKey, Payload, Precondition, RequestContext, RequestId, VersionToken, WriteReceipt,
};
use hsearch_s3::{S3Config, S3Store};
use std::future::Future;
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

struct Thread(std::thread::Thread);
impl Wake for Thread {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block<T>(future: impl Future<Output = T>) -> T {
    let waker = Waker::from(Arc::new(Thread(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}

fn ctx() -> RequestContext {
    RequestContext::new(
        Duration::from_secs(30),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 24,
            response_bytes: 1 << 24,
            attempts: 1000,
        },
    )
    .unwrap()
}

fn put(id: &str, key: &str, body: &str, precondition: Precondition) -> Mutation {
    Mutation {
        request_id: RequestId::new(id.as_bytes()).unwrap(),
        key: ObjectKey::new(key.as_bytes()).unwrap(),
        payload: Payload::new(body.as_bytes()).unwrap(),
        metadata: Metadata::new(b"probe").unwrap(),
        precondition,
    }
}

fn store() -> Arc<S3Store> {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"));
    Arc::new(
        S3Store::new(S3Config {
            endpoint: var("HSEARCH_S3_ENDPOINT"),
            bucket: std::env::var("HSEARCH_S3_BUCKET").unwrap_or_else(|_| "hsearch-probe".into()),
            access_key: var("HSEARCH_S3_ACCESS_KEY"),
            secret_key: var("HSEARCH_S3_SECRET_KEY"),
            region: "us-east-1".into(),
        })
        .unwrap(),
    )
}

fn check(name: &str, ok: bool, detail: &str) -> bool {
    println!(
        "{} {name}{}",
        if ok { "PASS" } else { "FAIL" },
        if ok {
            String::new()
        } else {
            format!(": {detail}")
        }
    );
    ok
}

fn run(store: Arc<S3Store>, suffix: &str) -> bool {
    let mut all = true;
    let mut cap = |name: &str, ok: bool, detail: String| all &= check(name, ok, &detail);
    cap(
        "bucket versioning is enabled",
        store.verify_versioning(&ctx()).is_ok(),
        "enable versioning".into(),
    );

    let key = format!("probe/{suffix}/obj");
    let first = put("c1", &key, "abcdef", Precondition::Absent);
    let created = block(store.create(&ctx(), &first));
    cap(
        "conditional create (If-None-Match) accepts a new key",
        created.is_ok(),
        format!("{created:?}"),
    );
    let Ok(created) = created else { return false };
    let again = block(store.create(&ctx(), &put("c2", &key, "zzz", Precondition::Absent)));
    cap(
        "conditional create refuses an existing key",
        again == Err(Error::Conflict),
        format!("{again:?}"),
    );

    let v1 = created.object.version.clone();
    let update = put("u1", &key, "new-data", Precondition::Matches(v1.clone()));
    let updated = block(store.compare_exchange(&ctx(), &update));
    cap(
        "conditional update (If-Match) accepts the current version",
        updated.is_ok(),
        format!("{updated:?}"),
    );
    let stale = block(store.compare_exchange(
        &ctx(),
        &put("u2", &key, "x", Precondition::Matches(v1.clone())),
    ));
    cap(
        "conditional update refuses a stale version",
        stale == Err(Error::Conflict),
        format!("{stale:?}"),
    );

    let old = block(store.read_range(&ctx(), &first.key, &v1, ByteRange::new(1, 3).unwrap()));
    cap(
        "range read of a replaced version returns that version's bytes",
        old.as_ref().map(|p| p.as_bytes()) == Ok(b"bcd".as_slice()),
        format!("{old:?}"),
    );
    let over = block(store.read_range(&ctx(), &first.key, &v1, ByteRange::new(4, 4).unwrap()));
    cap(
        "a range past the object end fails instead of truncating",
        over == Err(Error::InvalidRange),
        format!("{over:?}"),
    );
    let ghost = VersionToken::new(b"no-such-version\n\"e\"").unwrap();
    let missing =
        block(store.read_range(&ctx(), &first.key, &ghost, ByteRange::new(0, 1).unwrap()));
    cap(
        "an unknown version id is NotFound",
        missing == Err(Error::NotFound),
        format!("{missing:?}"),
    );
    let replay = block(store.compare_exchange(&ctx(), &update));
    cap(
        "an accepted update replays to its receipt",
        replay == updated,
        format!("{replay:?}"),
    );

    // Many-client race: per round exactly one writer wins and the store agrees.
    let race_key = format!("probe/{suffix}/race");
    let mut token = block(store.create(&ctx(), &put("r0", &race_key, "0", Precondition::Absent)))
        .unwrap()
        .object
        .version;
    let (mut winners_ok, mut final_ok) = (true, true);
    for round in 1..=20 {
        let barrier = Arc::new(Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|client| {
                let (store, token, barrier, key) = (
                    store.clone(),
                    token.clone(),
                    barrier.clone(),
                    race_key.clone(),
                );
                std::thread::spawn(move || {
                    let request = put(
                        &format!("race-{round}-{client}"),
                        &key,
                        &format!("{round}-{client}"),
                        Precondition::Matches(token),
                    );
                    barrier.wait();
                    (
                        request.clone(),
                        block(store.compare_exchange(&ctx(), &request)),
                    )
                })
            })
            .collect();
        let results: Vec<(Mutation, Result<WriteReceipt, Error>)> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let wins: Vec<_> = results.iter().filter(|(_, r)| r.is_ok()).collect();
        let conflicts = results
            .iter()
            .filter(|(_, r)| *r == Err(Error::Conflict))
            .count();
        winners_ok &= wins.len() == 1 && conflicts == 15;
        if let Some((request, Ok(receipt))) = wins.first() {
            let head = block(store.head(&ctx(), &request.key));
            final_ok &= head.as_ref() == Ok(&receipt.object);
            token = receipt.object.version.clone();
        } else {
            winners_ok = false;
            break;
        }
    }
    cap(
        "20 rounds of 16 concurrent clients: exactly one winner per round",
        winners_ok,
        "see rounds".into(),
    );
    cap(
        "the store's current version is always the winner's",
        final_ok,
        "head differs".into(),
    );
    all
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let store = store();
    let image = std::env::var("HSEARCH_S3_IMAGE").unwrap_or_else(|_| "unrecorded".into());
    match mode.as_str() {
        "run" => {
            block(async { store.prepare_bucket(&ctx()).expect("prepare bucket") });
            let suffix = std::process::id().to_string();
            let ok = run(store.clone(), &suffix);
            println!(
                "{{\"probe\":\"run\",\"image\":\"{image}\",\"required_capabilities_ok\":{ok}}}"
            );
            std::process::exit(i32::from(!ok));
        }
        "write" => {
            block(async { store.prepare_bucket(&ctx()).expect("prepare bucket") });
            let request = put(
                "restart-1",
                "probe/restart",
                "survives-restart",
                Precondition::Absent,
            );
            let receipt = block(store.create(&ctx(), &request)).expect("write");
            println!(
                "{}",
                String::from_utf8_lossy(receipt.object.version.as_bytes()).replace('\n', "|")
            );
        }
        "verify" => {
            let key = ObjectKey::new(b"probe/restart").unwrap();
            let info = block(store.head(&ctx(), &key)).expect("object survived restart");
            let bytes = block(store.read_range(
                &ctx(),
                &key,
                &info.version,
                ByteRange::new(0, 16).unwrap(),
            ))
            .expect("read");
            let ok = bytes.as_bytes() == b"survives-restart";
            let request = put(
                "restart-1",
                "probe/restart",
                "survives-restart",
                Precondition::Absent,
            );
            let replay = block(store.resolve(&ctx(), &request));
            let ok = check(
                "acknowledged object and its identity survive a restart",
                ok && replay.is_ok(),
                &format!("{replay:?}"),
            );
            println!("{{\"probe\":\"restart\",\"image\":\"{image}\",\"ok\":{ok}}}");
            std::process::exit(i32::from(!ok));
        }
        _ => {
            eprintln!("usage: probe run | write | verify");
            std::process::exit(2);
        }
    }
}
