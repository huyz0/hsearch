//! M03.13: the adapter's attribution and an SDK's hidden retries, checked against an independent
//! record of the wire. Needs RustFS like `rustfs.rs` (`HSEARCH_S3_ENDPOINT`, or SKIPPED).
//!
//! Two clients run through a recording proxy that classifies requests by its own rules (the verb from the wire's method, path,
//! query and headers; the outcome from a status table written separately in `wire/mod.rs`)
//! (`wire/mod.rs`): the adapter's own client (`S3Store`, ureq underneath) and a real SDK
//! (`aws-sdk-s3`, whose retry loop the adapter does not control), the SDK's every attempt reported
//! through `S3Store::record_attempt` by an interceptor. Counts must agree with the wire exactly;
//! byte totals are compared with a stated bound because both sides here estimate a request's size.
#[path = "../../object/tests/support/mod.rs"]
mod support;
mod wire;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::interceptors::{
    BeforeSerializationInterceptorContextRef, BeforeTransmitInterceptorContextRef,
    FinalizerInterceptorContextRef,
};
use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::{
    BehaviorVersion, ConfigBag, Credentials, Intercept, Region, RequestChecksumCalculation,
    ResponseChecksumValidation, RuntimeComponents,
};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::orchestrator::Metadata;
use hsearch_object::{Error, MIN_PART, ObjectBackend, ObjectKey, Precondition, RequestContext};
use hsearch_s3::{
    Outcome, Role, S3Config, S3Store, Verb, encoded_request_bytes, encoded_response_bytes,
    reconcile,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};
use support::{limited, mutation, run};
use wire::{Fault, Kind, Wire};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct Rig {
    bucket: String,
    access: String,
    secret: String,
    /// Writes and resolves directly, outside the record.
    direct: S3Store,
    wire: Wire,
}

fn rig() -> Option<Rig> {
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
    let access = std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key");
    let secret = std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key");
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let bucket = format!("hi-{nanos}-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let direct = store_at(&endpoint, &bucket, &access, &secret);
    direct.prepare_bucket(&ctx()).unwrap();
    let wire = Wire::start(endpoint);
    Some(Rig {
        bucket,
        access,
        secret,
        direct,
        wire,
    })
}

fn store_at(endpoint: &str, bucket: &str, access: &str, secret: &str) -> S3Store {
    S3Store::new(S3Config {
        endpoint: endpoint.into(),
        bucket: bucket.into(),
        access_key: access.into(),
        secret_key: secret.into(),
        region: "us-east-1".into(),
    })
    .unwrap()
}

fn ctx() -> RequestContext {
    limited(64 << 20, 4 << 20, 100)
}

impl Rig {
    /// The adapter's own client through the proxy, retrying reads up to `attempts` times.
    fn adapter(&self, attempts: u32) -> S3Store {
        store_at(&self.wire.addr, &self.bucket, &self.access, &self.secret).with_retry(
            hsearch_s3::RetryPolicy {
                max_attempts: attempts,
                backoff: Duration::from_millis(2),
            },
        )
    }

    /// An SDK client through the proxy whose every attempt is attributed to `ledger`.
    fn sdk(&self, ledger: Arc<S3Store>, attempts: u32) -> Client {
        let http = aws_smithy_http_client::Builder::new().build_http();
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(format!("http://{}", self.wire.addr))
            .force_path_style(true)
            .credentials_provider(Credentials::new(
                self.access.clone(),
                self.secret.clone(),
                None,
                None,
                "test",
            ))
            .http_client(http)
            .retry_config(
                RetryConfig::standard()
                    .with_max_attempts(attempts)
                    .with_initial_backoff(Duration::from_millis(5)),
            )
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .interceptor(Tap {
                ledger,
                state: Mutex::default(),
            })
            .build();
        Client::from_conf(config)
    }

    fn seed(&self, name: &[u8], bytes: &[u8]) {
        let m = mutation(b"seed", name, bytes, Precondition::Absent);
        run(self.direct.create(&ctx(), &m)).expect("seed");
        self.wire.clear();
    }
}

/// Reports every attempt the SDK makes, hidden retries included, to the adapter's ledger.
#[derive(Debug)]
struct Tap {
    ledger: Arc<S3Store>,
    state: Mutex<TapState>,
}

#[derive(Debug, Default)]
struct TapState {
    attempt: u32,
    verb: Option<Verb>,
    request_bytes: u64,
}

/// The canonical reason phrase the status line carries, for the statuses these tests produce.
fn reason(status: u16) -> u64 {
    match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        412 => "Precondition Failed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
    .len() as u64
}

fn verb_of(operation: &str) -> Verb {
    match operation {
        "GetObject" => Verb::Get,
        "HeadObject" => Verb::Head,
        "ListObjectVersions" | "ListObjectsV2" => Verb::List,
        "CreateMultipartUpload" | "CompleteMultipartUpload" => Verb::Post,
        "AbortMultipartUpload" | "DeleteObject" => Verb::Delete,
        _ => Verb::Put,
    }
}

impl Intercept for Tap {
    fn name(&self) -> &'static str {
        "tap"
    }

    fn read_before_execution(
        &self,
        _context: &BeforeSerializationInterceptorContextRef<'_>,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .attempt = 0;
        self.ledger.record_logical(Role::Payload);
        Ok(())
    }

    fn read_before_transmit(
        &self,
        context: &BeforeTransmitInterceptorContextRef<'_>,
        _components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let request = context.request();
        let uri = request.uri();
        let target = uri
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
            .unwrap_or(uri);
        let mut headers: Vec<(String, String)> = request
            .headers()
            .iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            headers.push((
                "host".into(),
                request.uri().split('/').nth(2).unwrap_or("").into(),
            ));
        }
        let body = request.body().content_length().unwrap_or(0);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.verb = cfg.load::<Metadata>().map(|m| verb_of(m.name()));
        state.request_bytes = encoded_request_bytes(request.method(), target, &headers, body);
        Ok(())
    }

    fn read_after_attempt(
        &self,
        context: &FinalizerInterceptorContextRef<'_>,
        _components: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let retry = state.attempt > 0;
        state.attempt += 1;
        let Some(verb) = state.verb.take() else {
            return Ok(());
        };
        let (status, response_bytes) = context.response().map_or((None, 0), |r| {
            let headers: Vec<(String, String)> = r
                .headers()
                .iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect();
            let body = r
                .headers()
                .get("content-length")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            (
                Some(r.status().as_u16()),
                encoded_response_bytes(&headers, body) + reason(r.status().as_u16()),
            )
        });
        self.ledger.record_attempt(
            Role::Payload,
            verb,
            status,
            state.request_bytes,
            response_bytes,
            retry,
        );
        Ok(())
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn key(name: &[u8]) -> ObjectKey {
    ObjectKey::new(name).unwrap()
}

fn shape(wire: &Wire) -> Vec<(Kind, Option<u16>)> {
    wire.seen().iter().map(|s| (s.kind, s.status)).collect()
}

/// Counts and bytes must agree exactly: each client's size estimate is built from the headers it
/// sets itself, and the status line's reason phrase is canonical, so a difference is a defect.
fn assert_agree(store: &S3Store, wire: &Wire) {
    let seen = wire.seen();
    let ledger = store.ledger();
    let mismatch = wire::attempt_mismatches(&ledger, &seen);
    assert!(
        mismatch.is_empty(),
        "attempts differ: {mismatch:?}\n{seen:#?}"
    );
    let verbs: Vec<Verb> = seen.iter().map(|s| s.kind.verb()).collect();
    assert!(reconcile(&ledger, &verbs).is_empty(), "verb totals differ");
    for row in wire::rows(&ledger, &seen) {
        eprintln!(
            "ROW {:?}/{:?} n={} req ledger={} wire={} resp ledger={} wire={}",
            row.verb,
            row.outcome,
            row.wire_attempts,
            row.ledger.request_bytes,
            row.wire_request_bytes,
            row.ledger.response_bytes,
            row.wire_response_bytes
        );
        assert_eq!(
            row.ledger.request_bytes, row.wire_request_bytes,
            "{:?}/{:?}: request bytes",
            row.verb, row.outcome
        );
        assert_eq!(
            row.ledger.response_bytes, row.wire_response_bytes,
            "{:?}/{:?}: response bytes",
            row.verb, row.outcome
        );
    }
}

#[test]
fn sdk_hidden_read_retries_are_counted_and_flagged() {
    let Some(rig) = rig() else { return };
    rig.seed(b"obj", b"hello");
    let ledger = Arc::new(rig.adapter(1));
    let sdk = rig.sdk(Arc::clone(&ledger), 3);
    rig.wire.arm(Kind::Get, Fault::Throttle);
    rig.wire.arm(Kind::Get, Fault::Throttle);
    let rt = runtime();
    let out = rt
        .block_on(sdk.get_object().bucket(&rig.bucket).key("obj").send())
        .expect("third attempt succeeds");
    let body = rt.block_on(out.body.collect()).unwrap().into_bytes();
    assert_eq!(&body[..], b"hello");
    assert_eq!(
        shape(&rig.wire),
        [
            (Kind::Get, Some(503)),
            (Kind::Get, Some(503)),
            (Kind::Get, Some(200))
        ]
    );
    let l = ledger.ledger();
    assert_eq!(l.logical_calls(Role::Payload), 1, "one SDK call");
    assert_eq!(l.attempts(Role::Payload), 3, "three attempts on the wire");
    assert_eq!(l.retries(), 2, "two of them hidden retries, flagged");
    assert_eq!(l.attempts_with(Outcome::Throttled), 2);
    assert_agree(&ledger, &rig.wire);
}

#[test]
fn sdk_connection_loss_is_a_counted_transport_attempt() {
    let Some(rig) = rig() else { return };
    rig.seed(b"obj", b"hello");
    let ledger = Arc::new(rig.adapter(1));
    let sdk = rig.sdk(Arc::clone(&ledger), 3);
    rig.wire.arm(Kind::Get, Fault::DropRequest);
    let rt = runtime();
    rt.block_on(sdk.get_object().bucket(&rig.bucket).key("obj").send())
        .expect("retried after the dropped connection");
    assert_eq!(
        shape(&rig.wire),
        [(Kind::Get, None), (Kind::Get, Some(200))]
    );
    let l = ledger.ledger();
    assert_eq!(l.attempts_with(Outcome::Transport), 1);
    assert_eq!(l.retries(), 1);
    assert_agree(&ledger, &rig.wire);
}

#[test]
fn sdk_multipart_every_part_and_control_request_is_attributed() {
    let Some(rig) = rig() else { return };
    let ledger = Arc::new(rig.adapter(1));
    let sdk = rig.sdk(Arc::clone(&ledger), 3);
    let rt = runtime();
    // The first part is throttled once: a hidden retry of an idempotent part.
    rig.wire.arm(Kind::Part, Fault::Throttle);
    let id = rt
        .block_on(
            sdk.create_multipart_upload()
                .bucket(&rig.bucket)
                .key("big")
                .send(),
        )
        .unwrap()
        .upload_id
        .unwrap();
    let mut done = Vec::new();
    for n in 1..=3 {
        let size = if n == 3 { 10 } else { MIN_PART };
        let part = rt
            .block_on(
                sdk.upload_part()
                    .bucket(&rig.bucket)
                    .key("big")
                    .upload_id(&id)
                    .part_number(n)
                    .body(ByteStream::from(vec![n as u8; size]))
                    .send(),
            )
            .unwrap();
        done.push(
            CompletedPart::builder()
                .part_number(n)
                .e_tag(part.e_tag.unwrap())
                .build(),
        );
    }
    rt.block_on(
        sdk.complete_multipart_upload()
            .bucket(&rig.bucket)
            .key("big")
            .upload_id(&id)
            .if_none_match("*")
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(done))
                    .build(),
            )
            .send(),
    )
    .unwrap();
    assert_eq!(
        shape(&rig.wire),
        [
            (Kind::MultipartCreate, Some(200)),
            (Kind::Part, Some(503)),
            (Kind::Part, Some(200)),
            (Kind::Part, Some(200)),
            (Kind::Part, Some(200)),
            (Kind::MultipartComplete, Some(200)),
        ]
    );
    let l = ledger.ledger();
    assert_eq!(
        l.logical_calls(Role::Payload),
        5,
        "create, three parts, complete"
    );
    assert_eq!(l.attempts_by_verb(Verb::Put), 4);
    assert_eq!(l.attempts_by_verb(Verb::Post), 2);
    assert_eq!(l.retries(), 1);
    assert_agree(&ledger, &rig.wire);
}

#[test]
fn sdk_retrying_a_conditional_write_amplifies_and_the_ledger_shows_it() {
    let Some(rig) = rig() else { return };
    let ledger = Arc::new(rig.adapter(1));
    let sdk = rig.sdk(Arc::clone(&ledger), 3);
    let rt = runtime();
    // The first attempt commits and its answer is lost; the SDK's hidden retry then loses to its
    // own earlier write.
    rig.wire.arm(Kind::PutConditional, Fault::LoseResponse);
    let result = rt.block_on(
        sdk.put_object()
            .bucket(&rig.bucket)
            .key("c")
            .if_none_match("*")
            .body(ByteStream::from_static(b"v1"))
            .send(),
    );
    assert!(result.is_err(), "the retry is refused as a conflict");
    assert_eq!(
        shape(&rig.wire),
        [
            (Kind::PutConditional, Some(200)),
            (Kind::PutConditional, Some(412))
        ]
    );
    let l = ledger.ledger();
    assert_eq!(l.amplification(Role::Payload), Some((2, 1)));
    assert_eq!(l.retries(), 1);
    assert_eq!(l.attempts_with(Outcome::Conflict), 1);
    assert_agree(&ledger, &rig.wire);
}

#[test]
fn sdk_with_retries_off_sends_a_conditional_write_exactly_once() {
    let Some(rig) = rig() else { return };
    let ledger = Arc::new(rig.adapter(1));
    let sdk = rig.sdk(Arc::clone(&ledger), 1);
    let rt = runtime();
    rig.wire.arm(Kind::PutConditional, Fault::LoseResponse);
    let result = rt.block_on(
        sdk.put_object()
            .bucket(&rig.bucket)
            .key("c")
            .if_none_match("*")
            .body(ByteStream::from_static(b"v1"))
            .send(),
    );
    assert!(result.is_err(), "the caller cannot tell");
    assert_eq!(shape(&rig.wire), [(Kind::PutConditional, Some(200))]);
    assert_eq!(
        wire::committed_unobserved(&rig.wire.seen()),
        1,
        "it did commit"
    );
    let l = ledger.ledger();
    assert_eq!(l.amplification(Role::Payload), Some((1, 1)));
    assert_eq!(l.retries(), 0);
    assert_eq!(l.attempts_with(Outcome::Transport), 1);
    assert_agree(&ledger, &rig.wire);
}

#[test]
fn adapter_read_retries_agree_with_the_wire() {
    let Some(rig) = rig() else { return };
    rig.seed(b"obj", b"hello");
    let store = rig.adapter(3);
    rig.wire.arm(Kind::Head, Fault::Throttle);
    rig.wire.arm(Kind::Head, Fault::DropRequest);
    let head = run(store.head(&ctx(), &key(b"obj")));
    assert!(head.is_ok(), "{head:?}");
    assert_eq!(
        shape(&rig.wire),
        [
            (Kind::Head, Some(503)),
            (Kind::Head, None),
            (Kind::Head, Some(200))
        ]
    );
    assert_eq!(store.ledger().retries(), 2);
    assert_agree(&store, &rig.wire);
}

#[test]
fn adapter_writes_are_never_repeated_after_a_status_or_a_lost_response() {
    let Some(rig) = rig() else { return };
    let store = rig.adapter(3);
    let first = mutation(b"w1", b"k", b"v1", Precondition::Absent);
    rig.wire.arm(Kind::PutConditional, Fault::Throttle);
    assert_eq!(run(store.create(&ctx(), &first)), Err(Error::Throttled));
    rig.wire.arm(Kind::PutConditional, Fault::LoseResponse);
    assert_eq!(
        run(store.create(&ctx(), &first)),
        Err(Error::UnknownOutcome)
    );
    let puts = rig
        .wire
        .seen()
        .iter()
        .filter(|s| s.kind == Kind::PutConditional)
        .count();
    assert_eq!(puts, 2, "one PUT per call, none repeated inside a call");
    assert_eq!(store.ledger().retries(), 0);
    assert_agree(&store, &rig.wire);
}

#[test]
fn adapter_lost_race_and_its_resolution_reads_are_attributed_under_retry() {
    let Some(rig) = rig() else { return };
    rig.seed(b"obj", b"first");
    let store = rig.adapter(3);
    let second = mutation(b"w2", b"obj", b"second", Precondition::Absent);
    // The write loses (412); the resolution that follows is reads, and a throttled one is retried.
    rig.wire.arm(Kind::List, Fault::Throttle);
    let result = run(store.create(&ctx(), &second));
    assert_eq!(result, Err(Error::Conflict));
    let kinds: Vec<Kind> = rig.wire.seen().iter().map(|s| s.kind).collect();
    assert_eq!(kinds[0], Kind::PutConditional);
    assert!(kinds.contains(&Kind::List));
    let l = store.ledger();
    assert_eq!(l.attempts_with(Outcome::Conflict), 1);
    assert_eq!(l.retries(), 1, "exactly the throttled LIST was repeated");
    assert_agree(&store, &rig.wire);
}

#[test]
fn adapter_multipart_parts_and_controls_agree_with_the_wire() {
    let Some(rig) = rig() else { return };
    let store = rig.adapter(3);
    let ctx = ctx();
    let object = key(b"seg");
    let mut upload = store.begin_multipart(&ctx, &object).unwrap();
    rig.wire.arm(Kind::Part, Fault::Throttle);
    let part = vec![7u8; MIN_PART];
    assert_eq!(upload.put_part(&store, &ctx, &part), Err(Error::Throttled));
    upload
        .put_part(&store, &ctx, &part)
        .expect("replays the same number");
    upload.put_part(&store, &ctx, b"tail").unwrap();
    upload.complete(&store, &ctx).unwrap();
    assert_eq!(
        shape(&rig.wire),
        [
            (Kind::MultipartCreate, Some(200)),
            (Kind::Part, Some(503)),
            (Kind::Part, Some(200)),
            (Kind::Part, Some(200)),
            (Kind::MultipartComplete, Some(200)),
        ]
    );
    assert_agree(&store, &rig.wire);
}

#[test]
fn a_misclassifying_attribution_path_fails_the_reconciliation() {
    let Some(rig) = rig() else { return };
    rig.seed(b"obj", b"hello");
    let ledger = Arc::new(rig.adapter(1));
    let sdk = rig.sdk(Arc::clone(&ledger), 1);
    let rt = runtime();
    rt.block_on(sdk.get_object().bucket(&rig.bucket).key("obj").send())
        .unwrap();
    assert_agree(&ledger, &rig.wire);
    // The same GET attributed as a LIST, as a path with its own verb rules might.
    let wrong = rig.adapter(1);
    wrong.record_attempt(Role::Payload, Verb::List, Some(200), 100, 100, false);
    let mismatch = wire::attempt_mismatches(&wrong.ledger(), &rig.wire.seen());
    assert_eq!(mismatch.len(), 2, "{mismatch:?}");
    // An attempt the ledger lost entirely is also caught.
    let silent = rig.adapter(1);
    assert!(!wire::attempt_mismatches(&silent.ledger(), &rig.wire.seen()).is_empty());
    // An outcome misclassified: the wire gave a 200, the path recorded a 412.
    let outcome = rig.adapter(1);
    outcome.record_attempt(Role::Payload, Verb::Get, Some(412), 752, 584, false);
    let mismatch = wire::attempt_mismatches(&outcome.ledger(), &rig.wire.seen());
    assert_eq!(mismatch.len(), 2, "{mismatch:?}");
    // The retry flag is not on the wire: the tests above pin it with exact equalities.
}

#[test]
fn adapter_backoff_pauses_grow_and_a_refused_connection_sends_nothing() {
    let Some(rig) = rig() else { return };
    // Nothing listens on this port: every connect is refused, so nothing is ever sent, which is
    // the one case a write may be repeated.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let store = store_at(&closed, &rig.bucket, &rig.access, &rig.secret).with_retry(
        hsearch_s3::RetryPolicy {
            max_attempts: 3,
            backoff: Duration::from_millis(40),
        },
    );
    let first = mutation(b"w9", b"k", b"v1", Precondition::Absent);
    let started = std::time::Instant::now();
    assert_eq!(run(store.create(&ctx(), &first)), Err(Error::Unavailable));
    // Pauses of 40 ms then 80 ms between three attempts.
    assert!(
        started.elapsed() >= Duration::from_millis(120),
        "{:?}",
        started.elapsed()
    );
    let l = store.ledger();
    assert_eq!(l.attempts_with(Outcome::Transport), 3);
    assert_eq!(l.retries(), 2);
    let cell = wire::ledger_cell(&l, Verb::Put, Outcome::Transport);
    assert_eq!(
        (cell.attempts, cell.request_bytes),
        (3, 0),
        "nothing was sent"
    );
}

#[test]
fn adapter_retries_stop_at_the_attempt_limit_and_return_the_last_real_failure() {
    let Some(rig) = rig() else { return };
    rig.seed(b"obj", b"hello");
    let store = rig.adapter(3);
    for _ in 0..5 {
        rig.wire.arm(Kind::Head, Fault::Throttle);
    }
    let result = run(store.head(&ctx(), &key(b"obj")));
    assert_eq!(
        result,
        Err(Error::Throttled),
        "the real failure, not a budget error"
    );
    assert_eq!(
        rig.wire.seen().len(),
        3,
        "bounded by the policy, not by the faults"
    );
    assert_eq!(
        rig.wire.pending(),
        2,
        "the remaining faults were never reached"
    );
    assert_eq!(store.ledger().retries(), 2);
    assert_agree(&store, &rig.wire);
    // A context that allows two attempts stops there even though the policy allows three.
    rig.wire.clear();
    for _ in 0..5 {
        rig.wire.arm(Kind::Head, Fault::Throttle);
    }
    let tight = limited(64 << 20, 4 << 20, 2);
    let fresh = rig.adapter(3);
    let result = run(fresh.head(&tight, &key(b"obj")));
    assert_eq!(result, Err(Error::Throttled));
    assert_eq!(
        rig.wire.seen().len(),
        2,
        "the context's attempt budget wins"
    );
    assert_agree(&fresh, &rig.wire);
}

#[test]
fn adapter_abort_is_attributed_as_a_delete_and_agrees_with_the_wire() {
    let Some(rig) = rig() else { return };
    let store = rig.adapter(3);
    let ctx = ctx();
    let mut upload = store.begin_multipart(&ctx, &key(b"gone")).unwrap();
    upload.put_part(&store, &ctx, &vec![1u8; MIN_PART]).unwrap();
    upload.abort(&store, &ctx).unwrap();
    assert_eq!(
        shape(&rig.wire),
        [
            (Kind::MultipartCreate, Some(200)),
            (Kind::Part, Some(200)),
            (Kind::MultipartAbort, Some(204)),
        ]
    );
    assert_agree(&store, &rig.wire);
}
