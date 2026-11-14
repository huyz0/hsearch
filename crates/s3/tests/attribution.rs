//! M03.07: what the adapter attributes against what actually crossed the wire. A scripted
//! in-process S3 stands in for the provider and keeps its own transport log, so the adapter's
//! counts are compared with an independent record. No external store is needed.
#[path = "../../object/tests/support/mod.rs"]
mod support;

use hsearch_object::{
    Budget, Cancellation, Error, MetadataStore, ObjectBackend, ObjectKey, Precondition,
    RequestContext,
};
use hsearch_s3::RetryPolicy;
use hsearch_s3::{
    Class, Evidence, Outcome, Path, Payer, Place, Rate, Rates, Rejection, Role, S3Config, S3Store,
    Unit, Verb, Via, reconcile,
};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use support::{context, limited, mutation, run};

struct Reply {
    status: u16,
    headers: &'static [(&'static str, &'static str)],
    body: &'static str,
}

const CREATED: Reply = Reply {
    status: 200,
    headers: &[("ETag", "\"abc\""), ("x-amz-version-id", "v1")],
    body: "",
};
const PRECONDITION_FAILED: Reply = Reply {
    status: 412,
    headers: &[],
    body: "",
};
const NO_VERSIONS: Reply = Reply {
    status: 200,
    headers: &[],
    body: "<ListVersionsResult><IsTruncated>false</IsTruncated></ListVersionsResult>",
};
const MISSING: Reply = Reply {
    status: 404,
    headers: &[],
    body: "",
};
const REDIRECT: Reply = Reply {
    status: 307,
    headers: &[("Location", "http://127.0.0.1:1/elsewhere")],
    body: "",
};
const SLOW_DOWN: Reply = Reply {
    status: 503,
    headers: &[],
    body: "",
};

/// A provider that answers from a script and logs every request it receives.
struct Provider {
    addr: String,
    log: Arc<Mutex<Vec<(String, usize)>>>,
}

impl Provider {
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (seen, queue) = (
            Arc::clone(&log),
            Arc::new(Mutex::new(VecDeque::from(script))),
        );
        std::thread::spawn(move || {
            for mut client in listener.incoming().flatten() {
                let Some((method, size)) = read_request(&mut client) else {
                    continue;
                };
                seen.lock().unwrap().push((method, size));
                let reply = queue.lock().unwrap().pop_front().unwrap_or(Reply {
                    status: 200,
                    headers: &[],
                    body: "",
                });
                let mut text = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (name, value) in reply.headers {
                    text.push_str(&format!("{name}: {value}\r\n"));
                }
                text.push_str("\r\n");
                text.push_str(reply.body);
                let _ = client.write_all(text.as_bytes());
            }
        });
        Self { addr, log }
    }

    fn store(&self) -> S3Store {
        S3Store::new(S3Config {
            endpoint: self.addr.clone(),
            bucket: "b".into(),
            access_key: "k".into(),
            secret_key: "s".into(),
            region: "us-east-1".into(),
        })
        .unwrap()
    }

    fn verbs(&self) -> Vec<Verb> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|(m, _)| match m.as_str() {
                "GET" => Verb::Get,
                "LIST" => Verb::List,
                "PUT" => Verb::Put,
                "HEAD" => Verb::Head,
                "POST" => Verb::Post,
                "DELETE" => Verb::Delete,
                other => panic!("unexpected {other}"),
            })
            .collect()
    }

    fn wire_bytes(&self) -> usize {
        self.log.lock().unwrap().iter().map(|(_, n)| n).sum()
    }
}

fn read_request(client: &mut std::net::TcpStream) -> Option<(String, usize)> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = client.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        request.extend_from_slice(&chunk[..n]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < end + 4 + length {
                let n = client.read(&mut chunk).ok()?;
                if n == 0 {
                    return None;
                }
                request.extend_from_slice(&chunk[..n]);
            }
            // A provider bills a GET with a `versions` query as a LIST, so the log classifies it so.
            let mut line = head.split(' ');
            let method = line.next()?.to_ascii_uppercase();
            let target = line.next()?;
            let method = if method == "GET" && target.contains("versions") {
                "LIST".to_owned()
            } else {
                method
            };
            return Some((method, request.len()));
        }
    }
}

fn create_request() -> hsearch_object::Mutation {
    mutation(b"id-1", b"obj", b"payload-bytes", Precondition::Absent)
}

#[test]
fn a_clean_write_is_one_call_one_attempt_and_matches_the_transport_log() {
    let provider = Provider::start(vec![CREATED]);
    let store = provider.store();
    run(store.create(&context(), &create_request())).unwrap();
    let ledger = store.ledger();
    assert_eq!(ledger.logical_calls(Role::Payload), 1);
    assert_eq!(ledger.attempts(Role::Payload), 1);
    assert_eq!(
        ledger.cell(Role::Payload, Verb::Put, Outcome::Ok).attempts,
        1
    );
    assert!(reconcile(&ledger, &provider.verbs()).is_empty());
    // The adapter computes request size from the headers it signs; the HTTP client adds a few
    // of its own (length, agent), so the ledger is a lower bound on the wire, within a stated slack.
    let counted = ledger
        .cell(Role::Payload, Verb::Put, Outcome::Ok)
        .request_bytes as usize;
    let wire = provider.wire_bytes();
    println!("counted {counted}, wire {wire}");
    assert!(
        counted <= wire && wire - counted <= 80,
        "counted {counted}, wire {wire}"
    );
}

#[test]
fn a_lost_race_pays_for_its_resolution_and_that_is_visible() {
    // PUT refused by the precondition, then the adapter lists versions to see whether it was its own.
    let provider = Provider::start(vec![PRECONDITION_FAILED, NO_VERSIONS]);
    let store = provider.store();
    let outcome = run(store.compare_exchange(&context(), &create_request()));
    assert_eq!(outcome.unwrap_err(), Error::Conflict);
    let ledger = store.ledger();
    assert_eq!(ledger.logical_calls(Role::Metadata), 1);
    assert_eq!(ledger.attempts(Role::Metadata), 2);
    assert_eq!(ledger.amplification(Role::Metadata), Some((2, 1)));
    assert_eq!(
        ledger
            .cell(Role::Metadata, Verb::Put, Outcome::Conflict)
            .attempts,
        1
    );
    assert_eq!(
        ledger
            .cell(Role::Metadata, Verb::List, Outcome::Ok)
            .attempts,
        1
    );
    assert_eq!(ledger.attempts(Role::Payload), 0);
    // The listing is a GET on the wire.
    assert!(reconcile(&ledger, &provider.verbs()).is_empty());
    assert_eq!(provider.verbs(), vec![Verb::Put, Verb::List]);
}

#[test]
fn an_empty_read_and_a_throttled_write_are_attempts_with_their_own_outcome() {
    let provider = Provider::start(vec![MISSING, SLOW_DOWN, CREATED]);
    let store = provider.store();
    let key = ObjectKey::new(b"obj").unwrap();
    assert_eq!(
        run(store.head(&context(), &key)).unwrap_err(),
        Error::NotFound
    );
    assert_eq!(
        run(store.create(&context(), &create_request())).unwrap_err(),
        Error::Throttled
    );
    // The caller's retry is a second logical call: the adapter does not retry on its own.
    run(store.create(&context(), &create_request())).unwrap();
    let ledger = store.ledger();
    assert_eq!(ledger.attempts_with(Outcome::EmptyRead), 1);
    assert_eq!(ledger.attempts_with(Outcome::Throttled), 1);
    assert_eq!(ledger.attempts_with(Outcome::Ok), 1);
    assert_eq!(ledger.logical_calls(Role::Payload), 3);
    assert_eq!(ledger.attempts(Role::Payload), 3);
    assert!(reconcile(&ledger, &provider.verbs()).is_empty());
}

#[test]
fn a_connection_that_never_answers_is_still_an_attempt() {
    // The listener accepts and drops the connection without a reply.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            drop(client);
        }
    });
    let store = S3Store::new(S3Config {
        endpoint: addr,
        bucket: "b".into(),
        access_key: "k".into(),
        secret_key: "s".into(),
        region: "us-east-1".into(),
    })
    .unwrap();
    assert!(run(store.create(&context(), &create_request())).is_err());
    let ledger = store.ledger();
    assert_eq!(ledger.attempts_with(Outcome::Transport), 1);
    assert_eq!(ledger.logical_calls(Role::Payload), 1);
    assert_eq!(
        ledger
            .cell(Role::Payload, Verb::Put, Outcome::Transport)
            .response_bytes,
        0
    );
}

#[test]
fn traffic_is_unknown_until_the_path_is_known_and_never_free() {
    let provider = Provider::start(vec![CREATED, CREATED]);
    let store = provider.store();
    run(store.create(&context(), &create_request())).unwrap();
    let unknown = store.traffic();
    let rates = Rates {
        same_zone: Some(Rate::per(0, Unit::GiB)),
        ..Rates::default()
    };
    let priced = unknown.price(&rates);
    assert!(!priced.complete() && priced.unknown_bytes > 0);
    assert!(matches!(
        unknown.acceptance(),
        Err(Rejection::UnknownPath(_))
    ));

    // With an inspected same-zone path and a rate, the same kind of traffic is classified and priced.
    let local = Place {
        region: "r".into(),
        zone: "a".into(),
    };
    let path = Path {
        endpoint: provider.addr.clone(),
        region: Some("r".into()),
        zone: Some("a".into()),
        payer: Some(Payer::Us),
        via: Via::Direct,
        evidence: Evidence::RouteInspected,
    };
    let known = Provider::start(vec![CREATED]);
    let store = known.store().with_path(path, local);
    run(store.create(&context(), &create_request())).unwrap();
    let ledger = store.ledger();
    let cell = ledger.cell(Role::Payload, Verb::Put, Outcome::Ok);
    let traffic = store.traffic();
    assert_eq!(
        traffic.bytes(Role::Payload, Class::SameZone),
        cell.request_bytes + cell.response_bytes
    );
    assert_eq!(traffic.acceptance(), Ok(()));
    let rates = Rates {
        same_zone: Some(Rate::per(1_000_000, Unit::GiB)),
        ..Rates::default()
    };
    let priced = traffic.price(&rates);
    assert!(priced.complete());
    // Each direction is its own entry and each rounds up to one micro-dollar: a request is never free.
    assert_eq!(priced.known_micro_usd, 2);
}

#[test]
fn a_dns_only_path_fails_acceptance_even_with_a_matching_name() {
    let provider = Provider::start(vec![CREATED]);
    let path = Path {
        endpoint: provider.addr.clone(),
        region: Some("r".into()),
        zone: Some("a".into()),
        payer: Some(Payer::Us),
        via: Via::Direct,
        evidence: Evidence::DnsOnly,
    };
    let store = provider.store().with_path(
        path,
        Place {
            region: "r".into(),
            zone: "a".into(),
        },
    );
    run(store.create(&context(), &create_request())).unwrap();
    assert_eq!(store.traffic().acceptance(), Err(Rejection::DnsOnly));
}

fn same_zone_path(endpoint: &str) -> (Path, Place) {
    (
        Path {
            endpoint: endpoint.to_owned(),
            region: Some("r".into()),
            zone: Some("a".into()),
            payer: Some(Payer::Us),
            via: Via::Direct,
            evidence: Evidence::RouteInspected,
        },
        Place {
            region: "r".into(),
            zone: "a".into(),
        },
    )
}

#[test]
fn calls_refused_before_the_wire_are_logical_calls_with_no_attempt() {
    let provider = Provider::start(vec![]);
    let store = provider.store();
    // A conditional update through the create path is refused locally.
    let stale = mutation(
        b"id-2",
        b"obj",
        b"x",
        Precondition::Matches(hsearch_object::VersionToken::new(b"v\ne").unwrap()),
    );
    assert_eq!(
        run(store.create(&context(), &stale)).unwrap_err(),
        Error::InvalidInput
    );
    // A request budget too small for the call is refused locally.
    let key = ObjectKey::new(b"obj").unwrap();
    assert!(run(store.head(&limited(0, 1 << 20, 10), &key)).is_err());
    let ledger = store.ledger();
    assert_eq!(ledger.logical_calls(Role::Payload), 2);
    assert_eq!(ledger.attempts(Role::Payload), 0);
    assert_eq!(ledger.amplification(Role::Payload), Some((0, 2)));
    assert!(provider.verbs().is_empty());
}

#[test]
fn a_redirect_is_not_followed_so_it_cannot_hide_a_second_request() {
    let provider = Provider::start(vec![REDIRECT]);
    let store = provider.store();
    assert!(run(store.create(&context(), &create_request())).is_err());
    assert_eq!(provider.verbs(), vec![Verb::Put]);
    let ledger = store.ledger();
    assert_eq!(ledger.attempts(Role::Payload), 1);
    assert_eq!(ledger.attempts_with(Outcome::ClientError), 1);
    assert!(reconcile(&ledger, &provider.verbs()).is_empty());
}

#[test]
fn a_refused_connection_sent_nothing() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener); // nothing listens: the connection is refused
    let store = S3Store::new(S3Config {
        endpoint: addr,
        bucket: "b".into(),
        access_key: "k".into(),
        secret_key: "s".into(),
        region: "us-east-1".into(),
    })
    .unwrap();
    assert!(run(store.create(&context(), &create_request())).is_err());
    let cell = store
        .ledger()
        .cell(Role::Payload, Verb::Put, Outcome::Transport);
    assert_eq!(
        (cell.attempts, cell.request_bytes, cell.response_bytes),
        (1, 0, 0)
    );
}

#[test]
fn traffic_keeps_recording_past_the_transfer_table_and_matches_the_ledger() {
    // 2,100 requests make 4,200 transfers, more than the de-duplication table holds.
    let provider = Provider::start(vec![]);
    let (path, local) = same_zone_path(&provider.addr);
    let store = provider.store().with_path(path, local);
    let key = ObjectKey::new(b"obj").unwrap();
    for _ in 0..2_100 {
        let _ = run(store.head(&context(), &key));
    }
    let ledger = store.ledger();
    assert_eq!(ledger.attempts(Role::Payload), 2_100);
    let (mut sent, mut received) = (0, 0);
    for outcome in [
        Outcome::Ok,
        Outcome::ClientError,
        Outcome::EmptyRead,
        Outcome::ServerError,
    ] {
        let cell = ledger.cell(Role::Payload, Verb::Head, outcome);
        sent += cell.request_bytes;
        received += cell.response_bytes;
    }
    let traffic = store.traffic();
    assert_eq!(
        traffic.bytes(Role::Payload, Class::SameZone),
        sent + received
    );
    assert_eq!(traffic.overflow_bytes(), 0);
    assert_eq!(traffic.acceptance(), Ok(()));
}

fn retrying(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        backoff: std::time::Duration::ZERO,
    }
}

#[test]
fn a_throttled_read_is_retried_and_every_attempt_is_attributed() {
    let provider = Provider::start(vec![SLOW_DOWN, SLOW_DOWN, CREATED]);
    let store = provider.store().with_retry(retrying(3));
    let key = ObjectKey::new(b"obj").unwrap();
    run(store.head(&context(), &key)).unwrap();
    let ledger = store.ledger();
    assert_eq!(ledger.logical_calls(Role::Payload), 1);
    assert_eq!(ledger.attempts(Role::Payload), 3);
    assert_eq!(ledger.retries(), 2);
    assert_eq!(ledger.amplification(Role::Payload), Some((3, 1)));
    assert_eq!(ledger.attempts_with(Outcome::Throttled), 2);
    assert_eq!(ledger.attempts_with(Outcome::Ok), 1);
    assert_eq!(provider.verbs(), vec![Verb::Head; 3]);
    assert!(reconcile(&ledger, &provider.verbs()).is_empty());
}

#[test]
fn a_throttled_write_is_not_repeated_because_a_status_does_not_prove_it_did_not_commit() {
    let provider = Provider::start(vec![SLOW_DOWN, CREATED]);
    let store = provider.store().with_retry(retrying(5));
    assert_eq!(
        run(store.create(&context(), &create_request())).unwrap_err(),
        Error::Throttled
    );
    assert_eq!(store.ledger().attempts(Role::Payload), 1);
    assert_eq!(store.ledger().retries(), 0);
    assert_eq!(provider.verbs(), vec![Verb::Put]);
}

#[test]
fn retries_stop_at_the_limit_and_report_the_last_failure() {
    let provider = Provider::start(vec![SLOW_DOWN, SLOW_DOWN, CREATED]);
    let store = provider.store().with_retry(retrying(2));
    let key = ObjectKey::new(b"obj").unwrap();
    assert_eq!(
        run(store.head(&context(), &key)).unwrap_err(),
        Error::Throttled
    );
    assert_eq!(store.ledger().attempts(Role::Payload), 2);
    assert_eq!(store.ledger().retries(), 1);
    assert_eq!(provider.verbs().len(), 2);
}

#[test]
fn the_default_and_a_zero_policy_make_one_attempt() {
    for policy in [RetryPolicy::default(), retrying(0)] {
        let provider = Provider::start(vec![SLOW_DOWN, CREATED]);
        let store = provider.store().with_retry(policy);
        let key = ObjectKey::new(b"obj").unwrap();
        assert_eq!(
            run(store.head(&context(), &key)).unwrap_err(),
            Error::Throttled
        );
        assert_eq!(store.ledger().attempts(Role::Payload), 1);
        assert_eq!(store.ledger().retries(), 0);
    }
}

#[test]
fn a_read_is_retried_after_a_server_error_but_a_write_is_not() {
    let reads = Provider::start(vec![
        Reply {
            status: 500,
            headers: &[],
            body: "",
        },
        CREATED,
    ]);
    let store = reads.store().with_retry(retrying(3));
    let key = ObjectKey::new(b"obj").unwrap();
    run(store.head(&context(), &key)).unwrap();
    let ledger = store.ledger();
    assert_eq!(
        ledger
            .cell(Role::Payload, Verb::Head, Outcome::ServerError)
            .attempts,
        1
    );
    assert_eq!(
        ledger.cell(Role::Payload, Verb::Head, Outcome::Ok).attempts,
        1
    );
    assert_eq!(ledger.retries(), 1);

    // A 500 on a write may have executed, so it is not repeated.
    let writes = Provider::start(vec![
        Reply {
            status: 500,
            headers: &[],
            body: "",
        },
        CREATED,
    ]);
    let store = writes.store().with_retry(retrying(3));
    assert!(run(store.create(&context(), &create_request())).is_err());
    assert_eq!(store.ledger().attempts(Role::Payload), 1);
    assert_eq!(writes.verbs().len(), 1);
}

#[test]
fn a_write_whose_outcome_is_unknown_is_never_retried() {
    // The connection is accepted and dropped after the request is read: it may have executed.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let accepted = Arc::new(Mutex::new(0));
    let count = Arc::clone(&accepted);
    std::thread::spawn(move || {
        for mut client in listener.incoming().flatten() {
            let _ = read_request(&mut client);
            *count.lock().unwrap() += 1;
        }
    });
    let store = S3Store::new(S3Config {
        endpoint: addr,
        bucket: "b".into(),
        access_key: "k".into(),
        secret_key: "s".into(),
        region: "us-east-1".into(),
    })
    .unwrap()
    .with_retry(retrying(3));
    assert_eq!(
        run(store.create(&context(), &create_request())).unwrap_err(),
        Error::UnknownOutcome
    );
    assert_eq!(store.ledger().attempts(Role::Payload), 1);
    assert_eq!(*accepted.lock().unwrap(), 1);
}

#[test]
fn a_refused_connection_is_retried_even_for_a_write_and_sends_nothing() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    let store = S3Store::new(S3Config {
        endpoint: addr,
        bucket: "b".into(),
        access_key: "k".into(),
        secret_key: "s".into(),
        region: "us-east-1".into(),
    })
    .unwrap()
    .with_retry(retrying(3));
    assert_eq!(
        run(store.create(&context(), &create_request())).unwrap_err(),
        Error::Unavailable
    );
    let cell = store
        .ledger()
        .cell(Role::Payload, Verb::Put, Outcome::Transport);
    assert_eq!((cell.attempts, cell.request_bytes), (3, 0));
    assert_eq!(store.ledger().retries(), 2);
}

#[test]
fn retries_spend_the_attempt_budget_and_the_last_real_failure_is_returned() {
    let provider = Provider::start(vec![SLOW_DOWN, SLOW_DOWN, CREATED]);
    let store = provider.store().with_retry(retrying(5));
    let key = ObjectKey::new(b"obj").unwrap();
    // One attempt is charged by the call, one by the first retry, then the budget is spent: the
    // caller sees the throttle, not a budget error standing in for it.
    let result = run(store.head(&limited(1 << 20, 1 << 20, 2), &key));
    assert_eq!(result.unwrap_err(), Error::Throttled);
    assert_eq!(store.ledger().attempts(Role::Payload), 2);
    assert_eq!(provider.verbs().len(), 2);
}

#[test]
fn retries_spend_the_byte_budget_too() {
    let provider = Provider::start(vec![SLOW_DOWN, CREATED]);
    let store = provider.store().with_retry(retrying(5));
    let key = ObjectKey::new(b"obj").unwrap(); // 3 key bytes charged per attempt
    let result = run(store.head(&limited(5, 1 << 20, 10), &key));
    assert_eq!(result.unwrap_err(), Error::Throttled);
    assert_eq!(store.ledger().attempts(Role::Payload), 1);
}

#[test]
fn a_backoff_never_sleeps_past_the_deadline() {
    let provider = Provider::start(vec![SLOW_DOWN, CREATED]);
    let policy = RetryPolicy {
        max_attempts: 3,
        backoff: std::time::Duration::from_secs(30),
    };
    let store = provider.store().with_retry(policy);
    let context = RequestContext::new(
        std::time::Duration::from_millis(150),
        Cancellation::default(),
        Budget {
            request_bytes: 1 << 20,
            response_bytes: 1 << 20,
            attempts: 10,
        },
    )
    .unwrap();
    let key = ObjectKey::new(b"obj").unwrap();
    let started = std::time::Instant::now();
    let result = run(store.head(&context, &key));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(result.unwrap_err(), Error::Throttled);
    assert_eq!(store.ledger().attempts(Role::Payload), 1);
}

#[test]
fn a_post_is_attributed_as_a_write_verb_and_matches_the_transport_log() {
    let provider = Provider::start(vec![Reply {
        status: 200,
        headers: &[],
        body: "",
    }]);
    let store = provider.store();
    let key = ObjectKey::new(b"big").unwrap();
    let (status, _, _) = store
        .probe_request(&context(), "POST", &key, "uploads=", &[], b"")
        .unwrap();
    assert_eq!(status, 200);
    let ledger = store.ledger();
    assert_eq!(ledger.attempts_by_verb(Verb::Post), 1);
    assert_eq!(ledger.attempts_by_verb(Verb::Put), 0);
    assert_eq!(provider.verbs(), vec![Verb::Post]);
    assert!(reconcile(&ledger, &provider.verbs()).is_empty());
    assert_eq!(store.counts().put, 1); // billed as a write
}

#[test]
fn a_hairpin_path_through_the_adapter_is_classified_and_priced_apart() {
    let provider = Provider::start(vec![CREATED]);
    let (mut path, local) = same_zone_path(&provider.addr);
    path.via = Via::Hairpin;
    let store = provider.store().with_path(path, local);
    run(store.create(&context(), &create_request())).unwrap();
    let cell = store.ledger().cell(Role::Payload, Verb::Put, Outcome::Ok);
    let traffic = store.traffic();
    let bytes = cell.request_bytes + cell.response_bytes;
    assert_eq!(traffic.bytes(Role::Payload, Class::NatHairpin), bytes);
    assert_eq!(traffic.bytes(Role::Payload, Class::SameZone), 0);
    assert!(!traffic.price(&Rates::default()).complete());
    let rates = Rates {
        nat_hairpin: Some(Rate::per(45_000, Unit::GB)),
        ..Rates::default()
    };
    assert!(traffic.price(&rates).complete());
}
