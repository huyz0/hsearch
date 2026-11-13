//! M03.10: what the real store does when a response is lost after a commit and when
//! a multipart upload fails, observed through a fault-injecting TCP proxy and the
//! adapter's own signed requests. Needs the same environment as `rustfs.rs`.
#[path = "../../object/tests/support/mod.rs"]
mod support;

use hsearch_object::{
    Error, MetadataStore, ObjectBackend, ObjectKey, Precondition, RequestContext,
};
use hsearch_s3::{S3Config, S3Store};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::time::SystemTime;
use support::{limited, mutation, run};

const PASS: u8 = 0;
/// Forward the next PUT, read the store's answer, then close without relaying it.
const LOSE_RESPONSE: u8 = 1;
/// Close the next PUT's connection before anything reaches the store.
const LOSE_REQUEST: u8 = 2;

static NEXT: AtomicU32 = AtomicU32::new(0);

struct Proxy {
    addr: String,
    mode: Arc<AtomicU8>,
    /// Faults actually applied, so a test can assert the one it armed was the one that fired.
    fired: Arc<AtomicU32>,
}

impl Proxy {
    fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let mode = Arc::new(AtomicU8::new(PASS));
        let shared = Arc::clone(&mode);
        let fired = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&fired);
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let (mode, upstream, fired) =
                    (Arc::clone(&shared), upstream.clone(), Arc::clone(&counter));
                std::thread::spawn(move || serve(client, &upstream, &mode, &fired));
            }
        });
        Self { addr, mode, fired }
    }
}

/// One request per connection: the proxy asks the store to close after answering.
fn serve(mut client: TcpStream, upstream: &str, mode: &AtomicU8, fired: &AtomicU32) {
    let mut request = Vec::new();
    let mut chunk = [0u8; 65536];
    let (head_end, length) = loop {
        let n = client.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..n]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (end + 4, length);
        }
    };
    while request.len() < head_end + length {
        let n = client.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..n]);
    }
    let is_put = request.starts_with(b"PUT ");
    let fault = if is_put {
        mode.compare_exchange(LOSE_RESPONSE, PASS, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| LOSE_RESPONSE)
            .or_else(|_| {
                mode.compare_exchange(LOSE_REQUEST, PASS, Ordering::SeqCst, Ordering::SeqCst)
                    .map(|_| LOSE_REQUEST)
            })
            .ok()
    } else {
        None
    };
    if fault.is_some() {
        fired.fetch_add(1, Ordering::SeqCst);
    }
    if fault == Some(LOSE_REQUEST) {
        return;
    }
    let mut upstream = TcpStream::connect(upstream).unwrap();
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let mut rewritten: Vec<&str> = head
        .trim_end()
        .lines()
        .filter(|l| !l.to_ascii_lowercase().starts_with("connection:"))
        .collect();
    rewritten.push("Connection: close");
    upstream
        .write_all(format!("{}\r\n\r\n", rewritten.join("\r\n")).as_bytes())
        .unwrap();
    upstream.write_all(&request[head_end..]).unwrap();
    let mut response = Vec::new();
    let _ = upstream.read_to_end(&mut response);
    if fault == Some(LOSE_RESPONSE) {
        return; // the store committed; the client never hears
    }
    let _ = client.write_all(&response);
}

fn env() -> Option<(String, String, String)> {
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
    Some((
        endpoint,
        std::env::var("HSEARCH_S3_ACCESS_KEY").expect("access key"),
        std::env::var("HSEARCH_S3_SECRET_KEY").expect("secret key"),
    ))
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

/// A direct store on a fresh versioned bucket, and a second store on the same
/// bucket reached through the fault proxy.
fn pair() -> Option<(S3Store, S3Store, Proxy)> {
    let (endpoint, access, secret) = env()?;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let bucket = format!("hf-{nanos}-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    let direct = store_at(&endpoint, &bucket, &access, &secret);
    direct
        .prepare_bucket(&limited(1 << 20, 1 << 20, 100))
        .unwrap();
    let proxy = Proxy::start(endpoint);
    let proxied = store_at(&proxy.addr, &bucket, &access, &secret);
    Some((direct, proxied, proxy))
}

fn context() -> RequestContext {
    limited(64 << 20, 4 << 20, 100)
}

#[test]
fn a_lost_response_after_commit_resolves_to_the_original_receipt() {
    let Some((direct, proxied, proxy)) = pair() else {
        return;
    };
    let ctx = context();
    let first = mutation(b"req-1", b"obj", b"v1", Precondition::Absent);

    proxy.mode.store(LOSE_RESPONSE, Ordering::SeqCst);
    let lost = run(proxied.create(&ctx, &first));
    assert_eq!(lost, Err(Error::UnknownOutcome), "the client cannot tell");
    assert_eq!(
        proxy.fired.load(Ordering::SeqCst),
        1,
        "exactly the armed PUT was faulted"
    );

    // The write happened: resolution finds it by identity and returns its receipt.
    let receipt = run(direct.resolve(&ctx, &first)).expect("resolves to the committed write");
    assert_eq!(receipt.request_id, first.request_id);
    assert_eq!(receipt.object.length, 2);
    // Retrying the very same request replays rather than writing a second version.
    let replay = run(proxied.create(&ctx, &first)).expect("replay");
    assert_eq!(replay.object.version, receipt.object.version);
}

#[test]
fn a_lost_request_is_unknown_to_the_client_and_absent_in_the_store() {
    let Some((direct, proxied, proxy)) = pair() else {
        return;
    };
    let ctx = context();
    let first = mutation(b"req-2", b"obj", b"v1", Precondition::Absent);
    proxy.mode.store(LOSE_REQUEST, Ordering::SeqCst);
    assert_eq!(
        run(proxied.create(&ctx, &first)),
        Err(Error::UnknownOutcome)
    );
    assert_eq!(
        proxy.fired.load(Ordering::SeqCst),
        1,
        "exactly the armed PUT was faulted"
    );
    // Resolution reports it cannot find an accepted write; the same request then succeeds once.
    assert_eq!(
        run(direct.resolve(&ctx, &first)),
        Err(Error::ResolutionUnavailable)
    );
    let receipt = run(proxied.create(&ctx, &first)).expect("the retry is the first write");
    assert_eq!(
        run(direct.resolve(&ctx, &first)).unwrap().object.version,
        receipt.object.version
    );
}

#[test]
fn a_lost_response_to_a_compare_and_set_replays_without_a_second_version() {
    let Some((direct, proxied, proxy)) = pair() else {
        return;
    };
    let ctx = context();
    let base = run(direct.create(
        &ctx,
        &mutation(b"req-3", b"root", b"a", Precondition::Absent),
    ))
    .unwrap();
    let update = mutation(
        b"req-4",
        b"root",
        b"b",
        Precondition::Matches(base.object.version.clone()),
    );
    proxy.mode.store(LOSE_RESPONSE, Ordering::SeqCst);
    assert_eq!(
        run(proxied.compare_exchange(&ctx, &update)),
        Err(Error::UnknownOutcome)
    );
    assert_eq!(
        proxy.fired.load(Ordering::SeqCst),
        1,
        "exactly the armed PUT was faulted"
    );
    let receipt = run(direct.resolve(&ctx, &update)).expect("committed");
    let again = run(proxied.compare_exchange(&ctx, &update)).expect("replay");
    assert_eq!(again.object.version, receipt.object.version);
    // The old version is stale: a competing writer with the old token is refused.
    let rival = mutation(
        b"req-5",
        b"root",
        b"c",
        Precondition::Matches(base.object.version),
    );
    assert_eq!(
        run(direct.compare_exchange(&ctx, &rival)),
        Err(Error::Conflict)
    );
}

fn text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> &'a str {
    let start = text.find(open).expect(open) + open.len();
    &text[start..start + text[start..].find(close).expect(close)]
}

struct Upload<'a> {
    store: &'a S3Store,
    key: ObjectKey,
    id: String,
}

impl<'a> Upload<'a> {
    fn start(store: &'a S3Store, key: &[u8]) -> Self {
        let key = ObjectKey::new(key).unwrap();
        let (status, _, body) = store
            .probe_request(&context(), "POST", &key, "uploads=", &[], &[])
            .unwrap();
        assert_eq!(status, 200, "{}", text(&body));
        let id = between(&text(&body), "<UploadId>", "</UploadId>").to_owned();
        Self { store, key, id }
    }

    fn part(&self, number: u32, bytes: &[u8]) -> (u16, String) {
        let query = format!("partNumber={number}&uploadId={}", self.id);
        let (status, headers, _) = self
            .store
            .probe_request(&context(), "PUT", &self.key, &query, &[], bytes)
            .unwrap();
        let etag = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("etag"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        (status, etag)
    }

    fn complete(&self, parts: &[(u32, String)], condition: Option<(&str, &str)>) -> (u16, String) {
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (number, etag) in parts {
            xml.push_str(&format!(
                "<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>"
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let headers: Vec<(&str, String)> = condition
            .into_iter()
            .map(|(k, v)| (k, v.to_owned()))
            .collect();
        let query = format!("uploadId={}", self.id);
        let (status, _, body) = self
            .store
            .probe_request(
                &context(),
                "POST",
                &self.key,
                &query,
                &headers,
                xml.as_bytes(),
            )
            .unwrap();
        (status, text(&body))
    }

    fn abort(&self) -> u16 {
        let query = format!("uploadId={}", self.id);
        self.store
            .probe_request(&context(), "DELETE", &self.key, &query, &[], &[])
            .unwrap()
            .0
    }
}

const PART: usize = 5 * 1024 * 1024;

#[test]
fn an_unfinished_or_failed_multipart_upload_leaves_no_visible_object() {
    let Some((direct, _, _)) = pair() else { return };
    let ctx = context();
    let key = ObjectKey::new(b"big").unwrap();
    let body = vec![7u8; PART];

    // Parts uploaded, never completed: the key does not exist.
    let open = Upload::start(&direct, b"big");
    let (status, etag1) = open.part(1, &body);
    assert_eq!(status, 200);
    assert_eq!(
        run(direct.head(&ctx, &key)),
        Err(Error::NotFound),
        "no partial object"
    );

    // Completion naming a part that was never uploaded is refused; still nothing visible.
    let (status, reply) = open.complete(&[(1, etag1.clone()), (2, "\"0000\"".into())], None);
    eprintln!(
        "complete with a missing part: HTTP {status} {}",
        reply.chars().take(120).collect::<String>()
    );
    assert_eq!(status, 400, "a bad completion is InvalidPart: {reply}");
    assert!(reply.contains("InvalidPart"), "{reply}");
    assert_eq!(run(direct.head(&ctx, &key)), Err(Error::NotFound));

    // Abort removes the upload; a late part is refused and nothing appears.
    assert!((200..300).contains(&open.abort()));
    let (late, _) = open.part(2, b"tail");
    eprintln!("part after abort: HTTP {late}");
    assert_eq!(late, 404, "a part for an aborted upload is NoSuchUpload");
    assert_eq!(run(direct.head(&ctx, &key)), Err(Error::NotFound));
}

#[test]
fn a_completed_multipart_upload_is_one_conditional_version() {
    let Some((direct, _, _)) = pair() else { return };
    let ctx = context();
    let key = ObjectKey::new(b"whole").unwrap();
    let upload = Upload::start(&direct, b"whole");
    let (_, etag1) = upload.part(1, &vec![1u8; PART]);
    let (_, etag2) = upload.part(2, b"tail");
    let (status, reply) = upload.complete(&[(1, etag1), (2, etag2)], Some(("if-none-match", "*")));
    eprintln!(
        "complete with If-None-Match: HTTP {status} {}",
        reply.chars().take(120).collect::<String>()
    );
    assert_eq!(status, 200);
    let info = run(direct.head(&ctx, &key)).expect("the object exists after completion");
    assert_eq!(info.length, PART as u64 + 4);

    // A second upload completing onto the existing key with If-None-Match is refused.
    let second = Upload::start(&direct, b"whole");
    let (_, etag) = second.part(1, &vec![2u8; PART]);
    let (status, reply) = second.complete(&[(1, etag)], Some(("if-none-match", "*")));
    eprintln!(
        "second complete with If-None-Match on an existing key: HTTP {status} {}",
        reply.chars().take(120).collect::<String>()
    );
    assert_eq!(status, 412, "conditional completion must not overwrite");
    assert_eq!(run(direct.head(&ctx, &key)).unwrap().version, info.version);
}
