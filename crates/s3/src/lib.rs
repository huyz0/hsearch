//! S3-compatible [`ObjectBackend`] and [`MetadataStore`] for RustFS and MinIO.
//!
//! # What this relies on
//!
//! The adapter assumes only an S3 subset, and **fails instead of falling back**
//! when a capability is missing:
//!
//! - `PutObject` with `If-None-Match: *` (conditional create) and `If-Match`
//!   (conditional update). A compare-and-set is never built from a read plus an
//!   unconditional write.
//! - Bucket versioning: the version token is `<versionId>\n<ETag>`, so reads are
//!   pinned to an exact immutable version and a stale version never reads newer
//!   bytes. A response without a version id is [`Error::Unsupported`].
//! - Ranged `GetObject` and `HeadObject`; `ListObjectVersions` on the recovery
//!   path only.
//!
//! # Honest limits
//!
//! - **ABA on identical bytes.** `If-Match` compares the `ETag`, which is a hash
//!   of the body. Replacing an object with identical bytes keeps the `ETag`, so a
//!   conditional update against the older version can still succeed. Callers that
//!   need a strict fence put a sequence number in the payload.
//! - **Identity retention is the version history.** Each version stores the
//!   request id and a digest of the full request as user metadata. Resolution
//!   scans at most [`SCAN_LIMIT`] versions newer than the precondition; beyond
//!   that it returns [`Error::ResolutionUnavailable`] rather than guess. A normal
//!   write costs one PUT; a lost race or an unknown outcome pays one LIST plus up
//!   to [`SCAN_LIMIT`] HEADs.
//! - **Metadata is capped at [`MAX_METADATA`] bytes** because S3 limits user
//!   metadata to 2 KiB; larger metadata is [`Error::BudgetExceeded`].
//! - **Plain HTTP only** and blocking I/O inside each operation's future: run it
//!   on a blocking thread pool. TLS arrives with the security milestones.
//! - **Cancellation** is observed before each call; a call already on the wire is
//!   bounded by the context deadline, not interrupted.

mod ledger;
mod sigv4;
mod traffic;
mod xml;

pub use ledger::{Cell, Ledger, Outcome, Role, Verb};
pub use traffic::{
    Class, Direction, Evidence, MAX_KEYS, MAX_NAME, MAX_TENANTS, MAX_TRANSFERS, Mismatch, Path,
    Payer, Place, Priced, Rate, Rates, Rejection, Totals, TrafficError, TrafficLedger, Transfer,
    Unit, Via, encoded_request_bytes, encoded_response_bytes, reconcile,
};

use hsearch_object::{
    ByteRange, Error, Metadata, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey,
    Operation, Payload, Precondition, RequestContext, VersionToken, WriteReceipt,
};
use sigv4::{Signer, amz_date, encode, hex, sha256_hex, unhex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

/// Most versions a recovery scan inspects before giving up.
pub const SCAN_LIMIT: usize = 16;
/// Largest opaque metadata this adapter stores (S3 user metadata is capped at 2 KiB).
pub const MAX_METADATA: usize = 700;
/// Upper bound charged to the response budget for one write receipt's version token.
const TOKEN_BOUND: u64 = 128;

const META_REQUEST: &str = "x-amz-meta-hs-request";
const META_DIGEST: &str = "x-amz-meta-hs-digest";
const META_OPAQUE: &str = "x-amz-meta-hs-meta";

/// How many times, and how far apart, the adapter repeats a request that certainly or probably
/// did not take effect. Attempts, not retries: one means no retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    /// Most attempts per request, at least one.
    pub max_attempts: u32,
    /// Pause before the second attempt; each later pause adds the same step.
    pub backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff: Duration::ZERO,
        }
    }
}

/// Connection settings. Secrets are never printed by `Debug`.
#[derive(Clone)]
pub struct S3Config {
    /// `host:port` of the S3 endpoint, without a scheme.
    pub endpoint: String,
    /// Bucket name, addressed path-style.
    pub bucket: String,
    /// Access key id.
    pub access_key: String,
    /// Secret access key.
    pub secret_key: String,
    /// Signing region, `us-east-1` for RustFS and MinIO defaults.
    pub region: String,
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

/// Requests sent, by S3 operation type. Counted per attempt, including failures.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RequestCounts {
    /// `GetObject` and bucket reads.
    pub get: u64,
    /// `PutObject` and bucket creation or configuration.
    pub put: u64,
    /// `HeadObject`.
    pub head: u64,
    /// `ListObjectVersions`.
    pub list: u64,
    /// `DeleteObject`.
    pub delete: u64,
}

#[derive(Default)]
struct Counters {
    get: AtomicU64,
    put: AtomicU64,
    head: AtomicU64,
    list: AtomicU64,
    delete: AtomicU64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Get,
    Put,
    Head,
    List,
    Delete,
    Post,
}

impl Op {
    fn verb(self) -> Verb {
        match self {
            Op::Get => Verb::Get,
            Op::Put => Verb::Put,
            Op::Head => Verb::Head,
            Op::List => Verb::List,
            Op::Delete => Verb::Delete,
            Op::Post => Verb::Post,
        }
    }

    fn method(self) -> &'static str {
        match self {
            Op::Get | Op::List => "GET",
            Op::Put => "PUT",
            Op::Post => "POST",
            Op::Head => "HEAD",
            Op::Delete => "DELETE",
        }
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Status, headers and body of a probe request.
#[doc(hidden)]
pub type ProbeReply = (u16, Vec<(String, String)>, Vec<u8>);

/// An S3-compatible object store.
pub struct S3Store {
    config: S3Config,
    signer: Signer,
    agent: ureq::Agent,
    counters: Counters,
    attribution: Mutex<Attribution>,
    retry: RetryPolicy,
}

/// What the adapter has attributed so far.
struct Attribution {
    ledger: Ledger,
    traffic: TrafficLedger,
    path: Path,
    local: Place,
    next_transfer: u64,
}

impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Store")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn token(version_id: &str, etag: &str) -> Result<VersionToken, Error> {
    VersionToken::new(format!("{version_id}\n{etag}").as_bytes()).map_err(|_| Error::Corrupt)
}

fn split_token(token: &VersionToken) -> Result<(&str, &str), Error> {
    std::str::from_utf8(token.as_bytes())
        .ok()
        .and_then(|text| text.split_once('\n'))
        // Both halves travel in headers or query strings: refuse control bytes.
        .filter(|(id, etag)| {
            !id.is_empty()
                && !etag.is_empty()
                && id
                    .bytes()
                    .chain(etag.bytes())
                    .all(|b| (0x20..0x7f).contains(&b))
        })
        .ok_or(Error::InvalidInput)
}

fn digest(request: &Mutation) -> String {
    let mut data = Vec::new();
    for part in [
        request.key.as_bytes(),
        request.payload.as_bytes(),
        request.metadata.as_bytes(),
    ] {
        data.extend_from_slice(&(part.len() as u64).to_be_bytes());
        data.extend_from_slice(part);
    }
    match &request.precondition {
        Precondition::Absent => data.push(0),
        Precondition::Matches(token) => {
            data.push(1);
            data.extend_from_slice(token.as_bytes());
        }
    }
    sha256_hex(&data)
}

fn request_bytes(request: &Mutation) -> u64 {
    let precondition = match &request.precondition {
        Precondition::Absent => 1,
        Precondition::Matches(token) => 1 + token.len() as u64,
    };
    request.request_id.len() as u64
        + request.key.len() as u64
        + request.payload.len() as u64
        + request.metadata.len() as u64
        + precondition
}

fn status_error(status: u16, mutation: bool) -> Error {
    match status {
        400 => Error::InvalidInput,
        401 | 403 => Error::PermissionDenied,
        404 => Error::NotFound,
        412 | 409 => Error::Conflict,
        416 => Error::InvalidRange,
        429 | 503 => Error::Throttled,
        501 => Error::Unsupported,
        _ if mutation => Error::UnknownOutcome,
        _ => Error::Unavailable,
    }
}

impl S3Store {
    /// Build a client. No network I/O happens until the first operation.
    pub fn new(config: S3Config) -> Result<Self, Error> {
        if config.endpoint.is_empty()
            || config.endpoint.contains('/')
            || config.bucket.is_empty()
            || config.bucket.contains('/')
        {
            return Err(Error::InvalidInput);
        }
        let signer = Signer::new(&config.access_key, &config.secret_key, &config.region, "s3");
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                // A redirect would send a second request inside one call that no counter sees.
                .max_redirects(0)
                .proxy(None)
                .build(),
        );
        Ok(Self {
            signer,
            agent,
            counters: Counters::default(),
            attribution: Mutex::new(Attribution {
                ledger: Ledger::default(),
                traffic: TrafficLedger::default(),
                // Until told otherwise the path is unknown, so its traffic is unpriced, not free.
                path: Path {
                    endpoint: config.endpoint.clone(),
                    region: None,
                    zone: None,
                    payer: None,
                    via: Via::Direct,
                    evidence: Evidence::Declared,
                },
                local: Place {
                    region: String::new(),
                    zone: String::new(),
                },
                next_transfer: 0,
            }),
            config,
            retry: RetryPolicy::default(),
        })
    }

    /// Tell the adapter where it runs and what is known about the path to its endpoint, so that
    /// traffic is classified and priced. Without this the path is unknown.
    #[must_use]
    pub fn with_path(self, path: Path, local: Place) -> Self {
        {
            let mut guard = self.attribution();
            guard.path = path;
            guard.local = local;
        }
        self
    }

    /// Retry throttled, failed and unconnected requests as `policy` says. The default is a single
    /// attempt, so a store behaves as before until a caller opts in.
    #[must_use]
    pub fn with_retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }

    fn attribution(&self) -> std::sync::MutexGuard<'_, Attribution> {
        self.attribution
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// A snapshot of logical calls and attempts, by role, verb and outcome.
    #[must_use]
    pub fn ledger(&self) -> Ledger {
        self.attribution().ledger.clone()
    }

    /// A snapshot of the directed traffic ledger.
    #[must_use]
    pub fn traffic(&self) -> TrafficLedger {
        self.attribution().traffic.clone()
    }

    fn logical(&self, role: Role) {
        self.attribution().ledger.logical(role);
    }

    fn attribute(
        &self,
        role: Role,
        op: Op,
        status: Option<u16>,
        request: u64,
        response: u64,
        retry: bool,
    ) {
        let verb = op.verb();
        let mut guard = self.attribution();
        let state = &mut *guard;
        state.ledger.attempt(
            role,
            verb,
            Outcome::classify(verb, status),
            request,
            response,
            retry,
        );
        for (direction, bytes) in [(Direction::Egress, request), (Direction::Ingress, response)] {
            state.next_transfer += 1;
            // A name refusal keeps the bytes as unknown traffic inside the ledger; it must not
            // fail the storage operation.
            let _ = state.traffic.record_unique(Transfer {
                id: state.next_transfer,
                role,
                direction,
                path: &state.path,
                local: &state.local,
                tenant: None,
                bytes,
            });
        }
    }

    /// Requests sent so far, by operation type.
    #[must_use]
    pub fn counts(&self) -> RequestCounts {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        RequestCounts {
            get: load(&self.counters.get),
            put: load(&self.counters.put),
            head: load(&self.counters.head),
            list: load(&self.counters.list),
            delete: load(&self.counters.delete),
        }
    }

    fn count(&self, op: Op) {
        let counter = match op {
            Op::Get => &self.counters.get,
            Op::Put | Op::Post => &self.counters.put,
            Op::Head => &self.counters.head,
            Op::List => &self.counters.list,
            Op::Delete => &self.counters.delete,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Send a request, retrying under the store's [`RetryPolicy`]. Every attempt is attributed and a
    /// repeat is flagged as a retry. A read is retried after a 429, a 5xx (except 501) or a failure
    /// to connect. A mutation is retried **only when no connection was made** (nothing was sent): a
    /// status code, even a 503, does not prove a write did not commit (a gateway or a store with a
    /// quorum failure can answer after applying it), so a refused status is returned and the caller
    /// replays the same request id, which the conditional-write path resolves. Each retry is
    /// charged to the context's attempt and byte budget. When the budget is spent or the deadline
    /// has passed, the last real result is returned, never a budget error in its place.
    #[allow(clippy::too_many_arguments)]
    fn send(
        &self,
        context: &RequestContext,
        role: Role,
        op: Op,
        key: Option<&ObjectKey>,
        query: &str,
        headers: &[(&str, String)],
        body: &[u8],
        response_limit: u64,
    ) -> Result<Reply, Error> {
        let mutation = matches!(op, Op::Put | Op::Post | Op::Delete);
        let mut attempt = 0u32;
        loop {
            let result = self.send_once(
                context,
                role,
                op,
                key,
                query,
                headers,
                body,
                response_limit,
                attempt > 0,
            );
            attempt += 1;
            let retryable = match &result {
                Ok(reply) => !mutation && matches!(reply.status, 429 | 500 | 502..=599),
                // `Unavailable` is returned for a mutation only when the connection was never made.
                Err(Error::Unavailable) => true,
                Err(_) => false,
            };
            if !retryable || attempt >= self.retry.max_attempts {
                return result;
            }
            let pause = self
                .retry
                .backoff
                .saturating_mul(attempt)
                .min(context.remaining());
            if !pause.is_zero() {
                std::thread::sleep(pause);
            }
            let again = body.len() as u64 + key.map_or(0, |k| k.len() as u64) + query.len() as u64;
            if context.check().is_err() || context.charge_request(again).is_err() {
                return result;
            }
        }
    }

    /// Send one signed request. `key` is the object key, or `None` for the bucket.
    ///
    /// A failure before the request is on the wire is [`Error::Unavailable`]; once
    /// it may have been delivered a mutation reports [`Error::UnknownOutcome`].
    #[allow(clippy::too_many_arguments)]
    fn send_once(
        &self,
        context: &RequestContext,
        role: Role,
        op: Op,
        key: Option<&ObjectKey>,
        query: &str,
        headers: &[(&str, String)],
        body: &[u8],
        response_limit: u64,
        retry: bool,
    ) -> Result<Reply, Error> {
        context.check()?;
        let mutation = matches!(op, Op::Put | Op::Post | Op::Delete);
        let mut path = format!("/{}", encode(self.config.bucket.as_bytes(), false));
        if let Some(key) = key {
            path.push('/');
            path.push_str(&encode(key.as_bytes(), true));
        }
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let date = amz_date(now);
        let payload_hash = sha256_hex(body);
        let mut signed: Vec<(String, String)> = vec![
            ("host".into(), self.config.endpoint.clone()),
            ("x-amz-content-sha256".into(), payload_hash.clone()),
            ("x-amz-date".into(), date.clone()),
        ];
        for (name, value) in headers {
            signed.push(((*name).to_owned(), value.clone()));
        }
        let authorization =
            self.signer
                .authorization(op.method(), &path, query, &signed, &payload_hash, &date);
        let url = if query.is_empty() {
            format!("http://{}{path}", self.config.endpoint)
        } else {
            format!("http://{}{path}?{query}", self.config.endpoint)
        };
        let timeout = context.remaining();
        if timeout.is_zero() {
            return Err(Error::DeadlineExceeded);
        }
        let target = if query.is_empty() {
            path.clone()
        } else {
            format!("{path}?{query}")
        };
        let mut on_wire = signed.clone();
        on_wire.push(("authorization".to_owned(), authorization.clone()));
        let request_size = encoded_request_bytes(op.method(), &target, &on_wire, body.len() as u64);
        self.count(op);
        let outcome = self.dispatch(op, &url, &signed, &authorization, body, timeout);
        let mut response = match outcome {
            Ok(response) => response,
            Err(error) => {
                // Refused or unresolvable connections sent nothing; any other failure may have
                // sent the whole request.
                let sent = match &error {
                    ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => 0,
                    ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => 0,
                    _ => request_size,
                };
                self.attribute(role, op, None, sent, 0, retry);
                return Err(match error {
                    ureq::Error::ConnectionFailed | ureq::Error::HostNotFound => Error::Unavailable,
                    // Refused before any byte was accepted: certified not executed.
                    ureq::Error::Io(ref io)
                        if io.kind() == std::io::ErrorKind::ConnectionRefused =>
                    {
                        Error::Unavailable
                    }
                    _ if mutation => Error::UnknownOutcome,
                    ureq::Error::Timeout(_) => Error::DeadlineExceeded,
                    _ => Error::Unavailable,
                });
            }
        };
        let status = response.status().as_u16();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect();
        let body = if op == Op::Head {
            Ok(Vec::new())
        } else {
            response
                .body_mut()
                .with_config()
                .limit(response_limit)
                .read_to_vec()
                .map_err(|error| match error {
                    ureq::Error::BodyExceedsLimit(_) if !mutation => Error::Corrupt,
                    _ if mutation => Error::UnknownOutcome,
                    _ => Error::Unavailable,
                })
        };
        let body_len = body.as_ref().map_or(0, Vec::len) as u64;
        self.attribute(
            role,
            op,
            Some(status),
            request_size,
            encoded_response_bytes(&headers, body_len),
            retry,
        );
        let body = body?;
        Ok(Reply {
            status,
            headers,
            body,
        })
    }

    fn dispatch(
        &self,
        op: Op,
        url: &str,
        signed: &[(String, String)],
        authorization: &str,
        body: &[u8],
        timeout: Duration,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        macro_rules! finish {
            ($request:expr) => {{
                let mut request = $request;
                for (name, value) in signed.iter().filter(|(n, _)| n != "host") {
                    request = request.header(name.as_str(), value.as_str());
                }
                request = request.header("authorization", authorization);
                request
            }};
        }
        match op {
            Op::Get | Op::List => finish!(
                self.agent
                    .get(url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
            )
            .call(),
            Op::Head => finish!(
                self.agent
                    .head(url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
            )
            .call(),
            Op::Delete => finish!(
                self.agent
                    .delete(url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
            )
            .call(),
            Op::Post => finish!(
                self.agent
                    .post(url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
            )
            .send(body),
            Op::Put => finish!(
                self.agent
                    .put(url)
                    .config()
                    .timeout_global(Some(timeout))
                    .build()
            )
            .send(body),
        }
    }

    /// One signed request outside the adapter's own protocol, for probes of
    /// provider behaviour the adapter does not use (multipart upload, listings).
    /// Returns the status, headers and body. Not part of the storage contract.
    #[doc(hidden)]
    pub fn probe_request(
        &self,
        context: &RequestContext,
        method: &str,
        key: &ObjectKey,
        query: &str,
        headers: &[(&str, String)],
        body: &[u8],
    ) -> Result<ProbeReply, Error> {
        let op = match method {
            "GET" => Op::Get,
            "PUT" => Op::Put,
            "POST" => Op::Post,
            "HEAD" => Op::Head,
            "DELETE" => Op::Delete,
            _ => return Err(Error::InvalidInput),
        };
        self.logical(Role::Payload);
        context.charge_request(key.len() as u64 + body.len() as u64 + 64)?;
        let reply = self.send(
            context,
            Role::Payload,
            op,
            Some(key),
            query,
            headers,
            body,
            1 << 20,
        )?;
        Ok((reply.status, reply.headers, reply.body))
    }

    /// Create the bucket if absent and turn versioning on. Setup helper for tests,
    /// compose runs and local development; production buckets are provisioned
    /// outside the data path.
    pub fn prepare_bucket(&self, context: &RequestContext) -> Result<(), Error> {
        self.logical(Role::Metadata);
        context.charge_request(0)?;
        let created = self.send(context, Role::Metadata, Op::Put, None, "", &[], &[], 4096)?;
        // 409 means the bucket already exists and is ours.
        if !(200..300).contains(&created.status) && created.status != 409 {
            return Err(status_error(created.status, false));
        }
        context.charge_request(0)?;
        let body = b"<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Status>Enabled</Status></VersioningConfiguration>";
        let enabled = self.send(
            context,
            Role::Metadata,
            Op::Put,
            None,
            "versioning=",
            &[],
            body,
            4096,
        )?;
        if !(200..300).contains(&enabled.status) {
            return Err(status_error(enabled.status, false));
        }
        self.verify_versioning(context)
    }

    /// Require bucket versioning. Without it exact-version reads and identity
    /// resolution are unsound, so this is [`Error::Unsupported`], not a fallback.
    pub fn verify_versioning(&self, context: &RequestContext) -> Result<(), Error> {
        self.logical(Role::Metadata);
        context.charge_request(0)?;
        let reply = self.send(
            context,
            Role::Metadata,
            Op::Get,
            None,
            "versioning=",
            &[],
            &[],
            4096,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(status_error(reply.status, false));
        }
        if String::from_utf8_lossy(&reply.body).contains("<Status>Enabled</Status>") {
            Ok(())
        } else {
            Err(Error::Unsupported)
        }
    }

    /// Permanently remove one object version, for garbage collection and test
    /// cleanup. Not part of the object traits: stores never overwrite in place.
    pub fn delete_version(
        &self,
        context: &RequestContext,
        key: &ObjectKey,
        version: &VersionToken,
    ) -> Result<(), Error> {
        self.logical(Role::Payload);
        let (version_id, _) = split_token(version)?;
        context.charge_request(key.len() as u64 + version.len() as u64)?;
        let query = format!("versionId={}", encode(version_id.as_bytes(), false));
        let reply = self.send(
            context,
            Role::Payload,
            Op::Delete,
            Some(key),
            &query,
            &[],
            &[],
            4096,
        )?;
        match reply.status {
            200..=299 => Ok(()),
            status => Err(status_error(status, true)),
        }
    }

    fn info_from(reply: &Reply, length: u64) -> Result<ObjectInfo, Error> {
        let etag = reply.header("etag").ok_or(Error::Unsupported)?;
        let version_id = reply.header("x-amz-version-id").ok_or(Error::Unsupported)?;
        let metadata = match reply.header(META_OPAQUE) {
            Some(text) => {
                Metadata::new(&unhex(text).ok_or(Error::Corrupt)?).map_err(|_| Error::Corrupt)?
            }
            None => Metadata::new(b"").map_err(|_| Error::Corrupt)?,
        };
        Ok(ObjectInfo {
            version: token(version_id, etag)?,
            length,
            metadata,
        })
    }

    fn head_version(
        &self,
        context: &RequestContext,
        role: Role,
        key: &ObjectKey,
        version_id: Option<&str>,
    ) -> Result<(Reply, ObjectInfo), Error> {
        let query = version_id.map_or_else(String::new, |id| {
            format!("versionId={}", encode(id.as_bytes(), false))
        });
        let reply = self.send(context, role, Op::Head, Some(key), &query, &[], &[], 0)?;
        if !(200..300).contains(&reply.status) {
            return Err(status_error(reply.status, false));
        }
        let length = reply
            .header("content-length")
            .and_then(|v| v.parse().ok())
            .ok_or(Error::Corrupt)?;
        let info = Self::info_from(&reply, length)?;
        Ok((reply, info))
    }

    /// Find the version this exact request created, if any, scanning the versions
    /// newer than its precondition. `Ok(None)` is returned only when the scan was
    /// complete and no version carries the request id.
    fn find_accepted(
        &self,
        context: &RequestContext,
        role: Role,
        request: &Mutation,
    ) -> Result<Option<WriteReceipt>, Error> {
        let boundary = match &request.precondition {
            Precondition::Absent => None,
            Precondition::Matches(token) => Some(split_token(token)?.0.to_owned()),
        };
        context.charge_request(request.key.len() as u64 + 32)?;
        let query = format!(
            "max-keys={}&prefix={}&versions=",
            SCAN_LIMIT + 1,
            encode(request.key.as_bytes(), false)
        );
        let reply = self.send(context, role, Op::List, None, &query, &[], &[], 1 << 20)?;
        if !(200..300).contains(&reply.status) {
            return Err(status_error(reply.status, false));
        }
        let text = String::from_utf8(reply.body).map_err(|_| Error::Corrupt)?;
        let page = xml::parse_versions(&text).ok_or(Error::Corrupt)?;
        let wanted = hex(request.request_id.as_bytes());
        let mut reached = false;
        let candidates: Vec<_> = page
            .versions
            .iter()
            .filter(|v| v.key == request.key.as_bytes())
            .collect();
        // Versions sort by key, so history can continue past the page only when the
        // page ends inside this key's versions; siblings sharing the prefix come later.
        let ends_in_key = page
            .versions
            .last()
            .is_some_and(|v| v.key == request.key.as_bytes());
        let more = (page.truncated && ends_in_key) || candidates.len() > SCAN_LIMIT;
        for listed in candidates.into_iter().take(SCAN_LIMIT) {
            if boundary.as_deref() == Some(listed.version_id.as_str()) {
                reached = true;
                break;
            }
            context.charge_request(request.key.len() as u64 + 40)?;
            let (reply, info) =
                self.head_version(context, role, &request.key, Some(&listed.version_id))?;
            if reply.header(META_REQUEST) != Some(wanted.as_str()) {
                continue;
            }
            if reply.header(META_DIGEST) != Some(digest(request).as_str()) {
                return Err(Error::IdentityConflict);
            }
            return Ok(Some(WriteReceipt {
                request_id: request.request_id.clone(),
                object: info,
            }));
        }
        if reached || !more {
            return Ok(None);
        }
        // The precondition version was not reached within the bound and history
        // continues: absence cannot be certified.
        Err(Error::ResolutionUnavailable)
    }

    fn write(
        &self,
        context: &RequestContext,
        role: Role,
        request: &Mutation,
    ) -> Result<WriteReceipt, Error> {
        if request.metadata.len() > MAX_METADATA {
            return Err(Error::BudgetExceeded);
        }
        context.charge_request(request_bytes(request))?;
        // Reserve the whole known receipt before accepting, so no budget check can
        // fail after commit and masquerade as a refused mutation.
        context.charge_response(
            request.request_id.len() as u64 + TOKEN_BOUND + 8 + request.metadata.len() as u64,
        )?;
        let condition = match &request.precondition {
            Precondition::Absent => ("if-none-match", "*".to_owned()),
            Precondition::Matches(token) => ("if-match", split_token(token)?.1.to_owned()),
        };
        let headers = [
            condition,
            (META_REQUEST, hex(request.request_id.as_bytes())),
            (META_DIGEST, digest(request)),
            (META_OPAQUE, hex(request.metadata.as_bytes())),
        ];
        let reply = self.send(
            context,
            role,
            Op::Put,
            Some(&request.key),
            "",
            &headers,
            request.payload.as_bytes(),
            1 << 16,
        )?;
        match reply.status {
            200..=299 => {
                // Accepted: a receipt we cannot build is an unknown outcome to resolve,
                // never a refusal. (Missing version headers mean versioning is off;
                // `verify_versioning` reports that as Unsupported.)
                let version = reply
                    .header("etag")
                    .zip(reply.header("x-amz-version-id"))
                    .and_then(|(etag, id)| token(id, etag).ok())
                    .ok_or(Error::UnknownOutcome)?;
                Ok(WriteReceipt {
                    request_id: request.request_id.clone(),
                    object: ObjectInfo {
                        version,
                        length: request.payload.len() as u64,
                        metadata: request.metadata.clone(),
                    },
                })
            }
            // A failed condition may be this very request, accepted earlier whose
            // response was lost: replay precedes conflict, as in the memory double.
            404 | 409 | 412 => match self.find_accepted(context, role, request)? {
                Some(receipt) => Ok(receipt),
                None => Err(Error::Conflict),
            },
            status => Err(status_error(status, true)),
        }
    }
}

impl ObjectBackend for S3Store {
    fn head<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
    ) -> Operation<'a, ObjectInfo> {
        Box::pin(async move {
            self.logical(Role::Payload);
            context.charge_request(key.len() as u64)?;
            context.charge_response(TOKEN_BOUND + 8 + MAX_METADATA as u64)?;
            self.head_version(context, Role::Payload, key, None)
                .map(|(_, info)| info)
        })
    }

    fn create<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            self.logical(Role::Payload);
            if request.precondition != Precondition::Absent {
                return Err(Error::InvalidInput);
            }
            self.write(context, Role::Payload, request)
        })
    }

    fn read_range<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
        version: &'a VersionToken,
        range: ByteRange,
    ) -> Operation<'a, Payload> {
        Box::pin(async move {
            self.logical(Role::Payload);
            context.charge_request(key.len() as u64 + version.len() as u64 + 16)?;
            let (version_id, _) = split_token(version)?;
            if range.length() > 16 * 1024 * 1024 {
                return Err(Error::BudgetExceeded);
            }
            context.charge_response(range.length())?;
            let query = format!("versionId={}", encode(version_id.as_bytes(), false));
            let end = range.offset() + range.length() - 1;
            let headers = [("range", format!("bytes={}-{end}", range.offset()))];
            let reply = self.send(
                context,
                Role::Payload,
                Op::Get,
                Some(key),
                &query,
                &headers,
                &[],
                range.length().max(8192),
            )?;
            if reply.status != 206 && reply.status != 200 {
                // A bad version id is a missing version for this contract.
                return Err(match reply.status {
                    400 => Error::NotFound,
                    status => status_error(status, false),
                });
            }
            // A range running past the end is silently truncated by S3; the
            // contract returns exactly the requested bytes or fails.
            if reply.status == 200 && range.offset() != 0 {
                return Err(Error::Corrupt);
            }
            if reply.body.len() as u64 > range.length() {
                return Err(Error::Corrupt);
            }
            if reply.body.len() as u64 != range.length() {
                return Err(Error::InvalidRange);
            }
            Payload::new(&reply.body)
        })
    }
}

impl MetadataStore for S3Store {
    fn compare_exchange<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            self.logical(Role::Metadata);
            self.write(context, Role::Metadata, request)
        })
    }

    fn resolve<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            self.logical(Role::Metadata);
            let receipt = self
                .find_accepted(context, Role::Metadata, request)?
                .ok_or(Error::ResolutionUnavailable)?;
            context.charge_response(
                request.request_id.len() as u64
                    + TOKEN_BOUND
                    + 8
                    + receipt.object.metadata.len() as u64,
            )?;
            Ok(receipt)
        })
    }
}
