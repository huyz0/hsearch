//! An independent record of what crossed the wire (M03.13).
//!
//! A TCP proxy in front of the store that parses each HTTP/1.1 request itself and classifies it
//! by rules written here, **not** by the adapter's `Op` and `Verb` rules: a LIST is a GET on the
//! bucket, a multipart control request is told by its query, a conditional write by its headers.
//! It counts the bytes it actually read from the client and wrote back, records the status the
//! client was given, and can inject throttling, a refused request and a lost response. The adapter
//! and an SDK are both reconciled against this record.
#![allow(dead_code)]

use hsearch_s3::{Cell, Ledger, Outcome, Role, Verb};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};

/// What a request is, by the proxy's own reading of the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Get,
    Head,
    List,
    Put,
    /// A PUT carrying `If-None-Match` or `If-Match`.
    PutConditional,
    Part,
    BucketPut,
    MultipartCreate,
    MultipartComplete,
    MultipartAbort,
    Delete,
    Other,
}

impl Kind {
    /// The verb a billing line would use for this request, by this module's own table.
    pub fn verb(self) -> Verb {
        match self {
            Kind::Get => Verb::Get,
            Kind::Head => Verb::Head,
            Kind::List => Verb::List,
            Kind::Put | Kind::PutConditional | Kind::Part | Kind::BucketPut => Verb::Put,
            Kind::MultipartCreate | Kind::MultipartComplete => Verb::Post,
            Kind::MultipartAbort | Kind::Delete => Verb::Delete,
            Kind::Other => Verb::Get,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    /// Answer 503 without reaching the store.
    Throttle,
    /// Answer 500 without reaching the store.
    ServerError,
    /// Read the request, then close: the store never sees it, the client never hears.
    DropRequest,
    /// Forward the request, read the store's answer, then close without relaying it.
    LoseResponse,
}

/// One request as the proxy saw it.
#[derive(Clone, Debug)]
pub struct Seen {
    pub kind: Kind,
    /// Status the store (or the proxy) produced; `None` when no response existed.
    pub status: Option<u16>,
    /// Whether the client received the response.
    pub delivered: bool,
    pub request_bytes: u64,
    /// Bytes written back to the client: zero when not delivered.
    pub response_bytes: u64,
    pub fault: Option<Fault>,
}

#[derive(Default)]
struct State {
    script: Mutex<Vec<(Kind, Fault)>>,
    log: Mutex<Vec<Seen>>,
}

pub struct Wire {
    pub addr: String,
    state: Arc<State>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Wire {
    pub fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state = Arc::new(State::default());
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let (state, upstream) = (Arc::clone(&shared), upstream.clone());
                std::thread::spawn(move || serve(client, &upstream, &state));
            }
        });
        Self { addr, state }
    }

    /// Apply `fault` to the next request of `kind`, once. Faults of one kind fire in order.
    pub fn arm(&self, kind: Kind, fault: Fault) {
        lock(&self.state.script).push((kind, fault));
    }

    pub fn seen(&self) -> Vec<Seen> {
        lock(&self.state.log).clone()
    }

    pub fn clear(&self) {
        lock(&self.state.log).clear();
        lock(&self.state.script).clear();
    }

    /// Faults armed and not yet fired.
    pub fn pending(&self) -> usize {
        lock(&self.state.script).len()
    }
}

fn classify(method: &str, target: &str, head: &str) -> Kind {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let has = |name: &str| {
        query
            .split('&')
            .any(|p| p == name || p.starts_with(&format!("{name}=")))
    };
    // `/bucket` and `/bucket/` address the bucket; anything deeper is an object.
    let bucket_only = path
        .trim_start_matches('/')
        .trim_end_matches('/')
        .find('/')
        .is_none();
    let conditional = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("if-none-match:") || l.starts_with("if-match:")
    });
    match method {
        "GET" if bucket_only => Kind::List,
        "GET" => Kind::Get,
        "HEAD" => Kind::Head,
        "PUT" if bucket_only => Kind::BucketPut,
        "PUT" if has("partNumber") && has("uploadId") => Kind::Part,
        "PUT" if conditional => Kind::PutConditional,
        "PUT" => Kind::Put,
        "POST" if has("uploads") => Kind::MultipartCreate,
        "POST" if has("uploadId") => Kind::MultipartComplete,
        "DELETE" if has("uploadId") => Kind::MultipartAbort,
        "DELETE" => Kind::Delete,
        _ => Kind::Other,
    }
}

/// The class a status falls in for a request of `kind`, by this module's own table.
pub fn outcome(kind: Kind, status: Option<u16>) -> Outcome {
    let Some(status) = status else {
        return Outcome::Transport;
    };
    let read = matches!(kind, Kind::Get | Kind::Head);
    match status {
        200..=299 => Outcome::Ok,
        404 if read || kind == Kind::Delete => Outcome::EmptyRead,
        412 | 304 if read => Outcome::EmptyRead,
        404 | 409 | 412 => Outcome::Conflict,
        429 | 503 => Outcome::Throttled,
        500..=599 => Outcome::ServerError,
        _ => Outcome::ClientError,
    }
}

fn serve(mut client: TcpStream, upstream: &str, state: &State) {
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
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let mut first = head.lines().next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let kind = classify(method, target, &head);
    let request_bytes = request.len() as u64;
    let fault = {
        let mut script = lock(&state.script);
        script
            .iter()
            .position(|(k, _)| *k == kind)
            .map(|i| script.remove(i).1)
    };
    // The entry is written before the response is, so a client that returns the moment it has
    // the answer never reads the log ahead of the record; a failed write corrects it afterwards.
    let record = |status, delivered, response_bytes| {
        let mut log = lock(&state.log);
        log.push(Seen {
            kind,
            status,
            delivered,
            request_bytes,
            response_bytes,
            fault,
        });
        log.len() - 1
    };
    let undeliver = |index: usize| {
        let mut log = lock(&state.log);
        log[index].delivered = false;
        log[index].response_bytes = 0;
    };
    match fault {
        Some(Fault::DropRequest) => {
            record(None, false, 0);
            return;
        }
        Some(f @ (Fault::Throttle | Fault::ServerError)) => {
            let (code, text, error) = if f == Fault::Throttle {
                (503, "Service Unavailable", "SlowDown")
            } else {
                (500, "Internal Server Error", "InternalError")
            };
            // A HEAD answer never carries a body.
            let body = if kind == Kind::Head {
                String::new()
            } else {
                format!("<Error><Code>{error}</Code></Error>")
            };
            let response = format!(
                "HTTP/1.1 {code} {text}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let index = record(Some(code), true, response.len() as u64);
            if client.write_all(response.as_bytes()).is_err() {
                undeliver(index);
            }
            return;
        }
        _ => {}
    }
    let mut store = TcpStream::connect(upstream).unwrap();
    let mut rewritten: Vec<&str> = head
        .trim_end()
        .lines()
        .filter(|l| !l.to_ascii_lowercase().starts_with("connection:"))
        .collect();
    rewritten.push("Connection: close");
    store
        .write_all(format!("{}\r\n\r\n", rewritten.join("\r\n")).as_bytes())
        .unwrap();
    store.write_all(&request[head_end..]).unwrap();
    let mut response = Vec::new();
    let _ = store.read_to_end(&mut response);
    let status = std::str::from_utf8(&response[..response.len().min(16)])
        .ok()
        .and_then(|s| s.split(' ').nth(1))
        .and_then(|s| s.parse::<u16>().ok());
    if fault == Some(Fault::LoseResponse) {
        record(status, false, 0);
        return;
    }
    // Tell the client the connection ends here, so it never reuses a closed socket.
    let end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(response.len());
    let head_text = String::from_utf8_lossy(&response[..end]).into_owned();
    let mut lines: Vec<&str> = head_text
        .lines()
        .filter(|l| !l.to_ascii_lowercase().starts_with("connection:"))
        .collect();
    lines.push("Connection: close");
    let mut out = format!("{}\r\n\r\n", lines.join("\r\n")).into_bytes();
    out.extend_from_slice(&response[(end + 4).min(response.len())..]);
    let index = record(status, true, out.len() as u64);
    if client.write_all(&out).is_err() {
        undeliver(index);
    }
}

impl Seen {
    /// The status the client observed: none when the response never reached it, even if the
    /// store had already committed the request (`status` still holds what the store said).
    pub fn observed(&self) -> Option<u16> {
        self.status.filter(|_| self.delivered)
    }
}

/// Requests the store applied and answered with a success the client never received: the writes
/// an unknown outcome may hide.
pub fn committed_unobserved(seen: &[Seen]) -> usize {
    seen.iter()
        .filter(|s| !s.delivered && matches!(s.status, Some(200..=299)))
        .count()
}

/// One row of the comparison: a (verb, outcome) class.
#[derive(Debug)]
pub struct Row {
    pub verb: Verb,
    pub outcome: Outcome,
    pub ledger: Cell,
    pub wire_attempts: u64,
    pub wire_request_bytes: u64,
    pub wire_response_bytes: u64,
}

pub const ROLES: [Role; 3] = [Role::Payload, Role::Metadata, Role::Key];
pub const VERBS: [Verb; 6] = [
    Verb::Get,
    Verb::Put,
    Verb::Head,
    Verb::List,
    Verb::Delete,
    Verb::Post,
];
pub const OUTCOMES: [Outcome; 7] = [
    Outcome::Ok,
    Outcome::Conflict,
    Outcome::EmptyRead,
    Outcome::Throttled,
    Outcome::ServerError,
    Outcome::ClientError,
    Outcome::Transport,
];

/// Sum a ledger's cells over every role for one class.
pub fn ledger_cell(ledger: &Ledger, verb: Verb, outcome: Outcome) -> Cell {
    ROLES.iter().fold(Cell::default(), |acc, role| {
        let c = ledger.cell(*role, verb, outcome);
        Cell {
            attempts: acc.attempts + c.attempts,
            request_bytes: acc.request_bytes + c.request_bytes,
            response_bytes: acc.response_bytes + c.response_bytes,
        }
    })
}

/// Every non-empty class of either record, side by side.
pub fn rows(ledger: &Ledger, seen: &[Seen]) -> Vec<Row> {
    let mut out = Vec::new();
    for verb in VERBS {
        for outcome_class in OUTCOMES {
            let ledger_side = ledger_cell(ledger, verb, outcome_class);
            let wire: Vec<&Seen> = seen
                .iter()
                .filter(|s| s.kind.verb() == verb && outcome(s.kind, s.observed()) == outcome_class)
                .collect();
            if ledger_side.attempts == 0 && wire.is_empty() {
                continue;
            }
            out.push(Row {
                verb,
                outcome: outcome_class,
                ledger: ledger_side,
                wire_attempts: wire.len() as u64,
                wire_request_bytes: wire.iter().map(|s| s.request_bytes).sum(),
                wire_response_bytes: wire.iter().map(|s| s.response_bytes).sum(),
            });
        }
    }
    out
}

/// Classes whose attempt counts differ. Empty means the ledger and the wire agree exactly.
pub fn attempt_mismatches(ledger: &Ledger, seen: &[Seen]) -> Vec<String> {
    rows(ledger, seen)
        .into_iter()
        .filter(|r| r.ledger.attempts != r.wire_attempts)
        .map(|r| {
            format!(
                "{:?}/{:?}: ledger {} wire {}",
                r.verb, r.outcome, r.ledger.attempts, r.wire_attempts
            )
        })
        .collect()
}
