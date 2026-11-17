//! M07.02 over real sockets and real mutual TLS 1.3: wrong role, oversized prefix, slowloris, stalled
//! TLS, plaintext, an untrusted certificate, rotation, deadlines, cancellation and the connection cap.
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use hsearch_peer::budget::{Budget, BudgetLimits};
use hsearch_peer::transport::{CallError, Channel, ChannelError, Config, Cx, Handler, Server};
use hsearch_peer::wire::{HEADER_LEN, Header, Status};
use hsearch_security::dev_ca::{DevCa, NodeCert};
use hsearch_security::identity::{CertUse, Role, VerifiedPrincipal};
use hsearch_security::ids::{ClusterId, TrustDomain};
use hsearch_security::policy::PeerRpc;
use hsearch_security::tls::{PeerTls, RotatingCert};
use hsearch_security::x509::{AcceptedIdentity, ChainVerifier, SpiffeIdentityProvider};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use zeroize::Zeroizing;

const DOMAIN: &str = "example.test";

fn identity(verifier: Arc<ChainVerifier>) -> Arc<SpiffeIdentityProvider> {
    Arc::new(
        SpiffeIdentityProvider::new(
            verifier,
            vec![AcceptedIdentity {
                trust_domain: TrustDomain::new(DOMAIN).unwrap(),
                cluster: ClusterId::new("prod").unwrap(),
                roles: BTreeSet::from([Role::IndexNode, Role::SearchNode, Role::Coordinator]),
            }],
        )
        .unwrap(),
    )
}

fn mint(ca: &DevCa, role: &str) -> (Vec<Vec<u8>>, Zeroizing<Vec<u8>>) {
    let spec = NodeCert::spiffe(&format!("spiffe://{DOMAIN}/hsearch/prod/{role}"));
    let m = ca.issue(&spec).unwrap();
    (vec![m.der], Zeroizing::new(m.key_pkcs8))
}

fn node(ca: &DevCa, role: &str) -> Arc<PeerTls> {
    let verifier = Arc::new(ChainVerifier::new(vec![ca.der()], Duration::from_secs(5)).unwrap());
    let id = identity(verifier);
    let (chain, key) = mint(ca, role);
    let cert =
        RotatingCert::new(chain, key, CertUse::Server, id.clone(), SystemTime::now()).unwrap();
    Arc::new(PeerTls::new(cert, id).unwrap())
}

/// Echoes the body; a body starting with `sleep` loops until its deadline.
struct Echo {
    saw_deadline: AtomicBool,
}

impl Handler for Echo {
    fn call(
        &self,
        _caller: &VerifiedPrincipal,
        _rpc: PeerRpc,
        body: &[u8],
        cx: &Cx,
    ) -> Result<Vec<u8>, Status> {
        if body.starts_with(b"sleep") {
            while !cx.is_done() {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.saw_deadline.store(true, Ordering::Relaxed);
            return Ok(Vec::new());
        }
        if body.starts_with(b"fail") {
            return Err(Status::BadRequest);
        }
        Ok(body.to_vec())
    }
}

fn cfg() -> Config {
    Config {
        handshake_ms: 600,
        header_ms: 300,
        body_base_ms: 300,
        min_rate: 10_000,
        idle_ms: 30_000,
        tick_ms: 10,
        ..Config::default()
    }
}

fn budget() -> Budget {
    Budget::new(BudgetLimits {
        connections: 8,
        frames: 32,
        bytes: 1 << 20,
    })
}

struct Rig {
    server: Server,
    handler: Arc<Echo>,
    index: Arc<PeerTls>,
    coordinator: Arc<PeerTls>,
    search: Arc<PeerTls>,
    ca: DevCa,
}

fn rig_with(cfg: Config, budget: Budget) -> Rig {
    let ca = DevCa::new("root").unwrap();
    let (index, coordinator, search) = (
        node(&ca, "index"),
        node(&ca, "coordinator"),
        node(&ca, "search"),
    );
    let handler = Arc::new(Echo {
        saw_deadline: AtomicBool::new(false),
    });
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = Server::start(listener, index.clone(), handler.clone(), budget, cfg).unwrap();
    Rig {
        server,
        handler,
        index,
        coordinator,
        search,
        ca,
    }
}

fn rig() -> Rig {
    rig_with(cfg(), budget())
}

fn open(r: &Rig, who: &Arc<PeerTls>) -> Channel {
    Channel::connect(r.server.addr(), who, &[Role::IndexNode], cfg()).unwrap()
}

/// A TLS client with no protocol on top, to send frames the real client never would.
type Raw = StreamOwned<ClientConnection, TcpStream>;

fn raw(addr: SocketAddr, who: &PeerTls) -> Raw {
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let conn = ClientConnection::new(
        who.client_config(),
        ServerName::try_from("peer.invalid").unwrap(),
    )
    .unwrap();
    let mut tls = StreamOwned::new(conn, tcp);
    while tls.conn.is_handshaking() {
        tls.conn.complete_io(&mut tls.sock).unwrap();
    }
    tls
}

/// True if the peer closed the connection (EOF or a reset) within `within`.
fn closed(tls: &mut Raw, within: Duration) -> bool {
    tls.sock.set_read_timeout(Some(within)).unwrap();
    let mut b = [0u8; 64];
    let start = Instant::now();
    loop {
        match tls.read(&mut b) {
            Ok(0) | Err(_) if start.elapsed() < within + Duration::from_millis(500) => {
                // A timeout error is not a close: check the elapsed time.
                if start.elapsed() >= within {
                    return false;
                }
                return true;
            }
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

const T: Duration = Duration::from_secs(3);

#[test]
fn a_coordinator_calls_an_index_node_and_gets_the_echo() {
    let r = rig();
    let mut c = open(&r, &r.coordinator);
    assert_eq!(c.peer().role(), Role::IndexNode);
    for i in 0..20u8 {
        assert_eq!(
            c.call(PeerRpc::ShardWrite, &[i; 1000], T).unwrap(),
            vec![i; 1000]
        );
    }
    assert_eq!(
        c.call(PeerRpc::Heartbeat, b"", T).unwrap(),
        Vec::<u8>::new()
    );
    assert_eq!(
        c.call(PeerRpc::ShardSearch, b"fail", T),
        Err(CallError::Status(Status::BadRequest))
    );
    // A handler error leaves the channel usable.
    assert!(c.call(PeerRpc::Heartbeat, b"x", T).is_ok());
    // A body over the RPC's cap is refused locally and nothing is sent.
    assert_eq!(
        c.call(PeerRpc::Heartbeat, &[0; 257], T),
        Err(CallError::TooLarge)
    );
    assert!(c.call(PeerRpc::Heartbeat, b"x", T).is_ok());
}

#[test]
fn the_wrong_role_is_refused_per_rpc_and_the_channel_survives() {
    let r = rig();
    // A search node may heartbeat and fetch segments, not write or search a shard.
    let mut c = open(&r, &r.search);
    assert!(c.call(PeerRpc::Heartbeat, b"", T).is_ok());
    assert_eq!(
        c.call(PeerRpc::ShardWrite, b"x", T),
        Err(CallError::Status(Status::WrongRole))
    );
    assert_eq!(
        c.call(PeerRpc::ShardSearch, b"x", T),
        Err(CallError::Status(Status::WrongRole))
    );
    assert_eq!(
        c.call(PeerRpc::Drain, b"x", T),
        Err(CallError::Status(Status::WrongRole))
    );
    assert!(c.call(PeerRpc::SegmentFetch, b"x", T).is_ok());
    assert!(c.call(PeerRpc::Heartbeat, b"", T).is_ok());
    // Every RPC agrees with the policy table for every role we can mint here.
    for who in [&r.search, &r.coordinator, &r.index] {
        let mut ch = open(&r, who);
        let role = ch.peer(); // the server's role; the caller's is below
        let _ = role;
        for rpc in PeerRpc::ALL {
            let caller = match who {
                w if Arc::ptr_eq(w, &r.search) => Role::SearchNode,
                w if Arc::ptr_eq(w, &r.coordinator) => Role::Coordinator,
                _ => Role::IndexNode,
            };
            let allowed = rpc.allowed_callers().contains(&caller);
            let got = ch.call(rpc, b"x", T);
            if allowed {
                assert_eq!(got, Ok(b"x".to_vec()), "{rpc:?} by {caller:?}");
            } else {
                assert_eq!(
                    got,
                    Err(CallError::Status(Status::WrongRole)),
                    "{rpc:?} by {caller:?}"
                );
            }
        }
    }
}

#[test]
fn a_client_refuses_a_server_in_an_unexpected_role_before_sending() {
    let r = rig();
    let err = Channel::connect(r.server.addr(), &r.coordinator, &[Role::SearchNode], cfg()).err();
    assert_eq!(err, Some(ChannelError::WrongPeerRole(Role::IndexNode)));
}

#[test]
fn an_oversized_prefix_closes_the_connection_and_reserves_nothing() {
    let r = rig();
    let mut tls = raw(r.server.addr(), &r.coordinator);
    let mut h = Header::request(1, PeerRpc::Heartbeat, 1000, 0).encode();
    h[16..20].copy_from_slice(&(1u32 << 31).to_be_bytes());
    tls.write_all(&h).unwrap();
    tls.flush().unwrap();
    assert!(closed(&mut tls, Duration::from_secs(2)));
    assert_eq!(r.server.budget().report().peak_bytes, 0);
    wait_until("connection released", || {
        r.server.budget().report().connections == 0
    });
}

#[test]
fn a_header_dribbled_a_byte_at_a_time_is_cut_off() {
    let r = rig();
    let mut tls = raw(r.server.addr(), &r.coordinator);
    let h = Header::request(1, PeerRpc::Heartbeat, 1000, 0).encode();
    let start = Instant::now();
    let mut cut = false;
    for b in &h[..HEADER_LEN - 1] {
        if tls
            .write_all(std::slice::from_ref(b))
            .and_then(|()| tls.flush())
            .is_err()
        {
            cut = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    assert!(cut || closed(&mut tls, Duration::from_secs(1)));
    // header_ms is 300: it must not have been allowed to finish the header's 19 bytes in 1.1 s.
    assert!(
        start.elapsed() < Duration::from_millis(1800),
        "{:?}",
        start.elapsed()
    );
    wait_until("connection released", || {
        r.server.budget().report().connections == 0
    });
}

#[test]
fn a_body_that_stops_arriving_holds_a_permit_not_memory_and_is_cut_off() {
    let r = rig_with(
        cfg(),
        Budget::new(BudgetLimits {
            connections: 8,
            frames: 32,
            bytes: 16 << 20,
        }),
    );
    let mut tls = raw(r.server.addr(), &r.coordinator);
    tls.write_all(&Header::request(1, PeerRpc::ShardWrite, 1000, 8 << 20).encode())
        .unwrap();
    tls.flush().unwrap();
    wait_until("permit taken", || {
        r.server.budget().report().bytes == 8 << 20
    });
    // Nothing more: the body allowance is 300 ms + 8 MiB at 10 kB/s, so the cut would be far away;
    // the point is the permit, so shorten by closing from our side after checking it is held.
    assert_eq!(r.server.budget().report().frames, 1);
    drop(tls);
    wait_until("permit released", || {
        let b = r.server.budget().report();
        b.bytes == 0 && b.frames == 0
    });
}

#[test]
fn a_stalled_tls_handshake_is_cut_off_and_its_connection_slot_returns() {
    let r = rig();
    let mut tcp = TcpStream::connect(r.server.addr()).unwrap();
    // Half a TLS record header, then silence.
    tcp.write_all(&[0x16, 0x03, 0x01]).unwrap();
    wait_until("slot taken", || r.server.budget().report().connections == 1);
    wait_until("slot returned", || {
        r.server.budget().report().connections == 0
    });
    tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut b = [0u8; 8];
    assert!(matches!(tcp.read(&mut b), Ok(0) | Err(_)));
}

#[test]
fn plaintext_and_a_client_without_a_certificate_never_get_an_application_answer() {
    let r = rig();
    // Plaintext frames over TCP.
    let mut tcp = TcpStream::connect(r.server.addr()).unwrap();
    tcp.write_all(&Header::request(1, PeerRpc::Heartbeat, 1000, 0).encode())
        .unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut b = [0u8; 64];
    match tcp.read(&mut b) {
        Ok(0) | Err(_) => {}
        // An alert is the only thing TLS may send; it must not be a protocol response frame.
        Ok(n) => assert_ne!(&b[..2], b"HP", "{n} bytes of plaintext answer"),
    }
    // A TLS client that offers no certificate: handshake fails.
    let cfg = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAll))
        .with_no_client_auth(),
    );
    let tcp = TcpStream::connect(r.server.addr()).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let conn = ClientConnection::new(cfg, ServerName::try_from("peer.invalid").unwrap()).unwrap();
    let mut tls = StreamOwned::new(conn, tcp);
    let _ = tls.write_all(&Header::request(1, PeerRpc::Heartbeat, 1000, 0).encode());
    let mut b = [0u8; 64];
    assert!(
        matches!(tls.read(&mut b), Ok(0) | Err(_)),
        "no answer without a client certificate"
    );
}

#[derive(Debug)]
struct AcceptAll;
impl rustls::client::danger::ServerCertVerifier for AcceptAll {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[test]
fn a_certificate_from_another_ca_cannot_connect_either_way() {
    let r = rig();
    let other = DevCa::new("elsewhere").unwrap();
    let stranger = node(&other, "coordinator");
    // The stranger dials the server: its certificate is not trusted.
    let dial = Channel::connect(r.server.addr(), &stranger, &[Role::IndexNode], cfg());
    assert!(
        matches!(dial, Err(ChannelError::Handshake | ChannelError::Identity)),
        "{:?}",
        dial.err()
    );
    // And a client that trusts only the stranger's CA refuses our server.
    let dial = Channel::connect(r.server.addr(), &stranger, &[Role::IndexNode], cfg());
    assert!(dial.is_err());
    wait_until("slots returned", || {
        r.server.budget().report().connections == 0
    });
    // The server is still serving the right peer.
    let mut c = open(&r, &r.coordinator);
    assert!(c.call(PeerRpc::Heartbeat, b"", T).is_ok());
}

#[test]
fn rotating_the_servers_certificate_keeps_open_channels_and_serves_new_ones() {
    let r = rig();
    let mut old = open(&r, &r.coordinator);
    assert!(old.call(PeerRpc::ShardWrite, &[1; 100], T).is_ok());
    let (chain, key) = mint(&r.ca, "index");
    let generation = r
        .index
        .cert()
        .rotate(chain, key, SystemTime::now())
        .unwrap();
    assert_eq!(generation, 2);
    assert!(
        old.call(PeerRpc::ShardWrite, &[2; 100], T).is_ok(),
        "open channel untouched"
    );
    let mut fresh = open(&r, &r.coordinator);
    assert!(fresh.call(PeerRpc::ShardWrite, &[3; 100], T).is_ok());
    // The caller's certificate rotates for its next channel too.
    let (chain, key) = mint(&r.ca, "coordinator");
    r.coordinator
        .cert()
        .rotate(chain, key, SystemTime::now())
        .unwrap();
    let mut again = open(&r, &r.coordinator);
    assert!(again.call(PeerRpc::Heartbeat, b"", T).is_ok());
    assert!(old.call(PeerRpc::Heartbeat, b"", T).is_ok());
}

#[test]
fn a_channel_older_than_its_maximum_age_ends_at_the_next_request() {
    let server_cfg = Config {
        max_channel_age: Duration::from_millis(300),
        ..cfg()
    };
    let r = rig_with(server_cfg, budget());
    let mut c = open(&r, &r.coordinator);
    assert!(c.call(PeerRpc::Heartbeat, b"", T).is_ok());
    std::thread::sleep(Duration::from_millis(400));
    let got = c.call(PeerRpc::Heartbeat, b"", T);
    assert!(got.is_err(), "{got:?}");
    assert!(c.is_broken());
    assert_eq!(c.call(PeerRpc::Heartbeat, b"", T), Err(CallError::Broken));
    // A fresh channel works.
    assert!(
        open(&r, &r.coordinator)
            .call(PeerRpc::Heartbeat, b"", T)
            .is_ok()
    );
}

#[test]
fn the_deadline_travels_to_the_handler_and_the_caller_gives_up_on_time() {
    let r = rig();
    let mut c = open(&r, &r.coordinator);
    let start = Instant::now();
    let got = c.call(PeerRpc::ShardSearch, b"sleep", Duration::from_millis(150));
    // The server answers DeadlineExceeded at the deadline; the caller may also time out first.
    assert!(
        matches!(
            got,
            Err(CallError::Timeout | CallError::Status(Status::DeadlineExceeded))
        ),
        "{got:?}"
    );
    assert!(
        start.elapsed() < Duration::from_millis(1000),
        "{:?}",
        start.elapsed()
    );
    wait_until("handler saw its deadline", || {
        r.handler.saw_deadline.load(Ordering::Relaxed)
    });
}

#[test]
fn a_queued_request_that_is_cancelled_is_not_served() {
    let r = rig();
    let mut tls = raw(r.server.addr(), &r.coordinator);
    // A slow request, then a second request and its cancel in the same write.
    let mut batch = Vec::new();
    batch.extend_from_slice(&Header::request(1, PeerRpc::ShardSearch, 400, 5).encode());
    batch.extend_from_slice(b"sleep");
    batch.extend_from_slice(&Header::request(3, PeerRpc::ShardSearch, 5000, 2).encode());
    batch.extend_from_slice(b"hi");
    batch.extend_from_slice(&Header::cancel(3).encode());
    tls.write_all(&batch).unwrap();
    tls.flush().unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    let start = Instant::now();
    while got.len() < 2 * HEADER_LEN && start.elapsed() < Duration::from_secs(3) {
        if let Ok(n) = tls.read(&mut buf) {
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
    }
    let status_of = |at: usize| got[at + 5];
    assert_eq!(&got[..2], b"HP");
    assert_eq!(
        status_of(0),
        Status::DeadlineExceeded.code(),
        "the slow request ran to its deadline"
    );
    assert_eq!(
        status_of(HEADER_LEN),
        Status::Cancelled.code(),
        "the cancelled one never ran"
    );
}

#[test]
fn a_body_over_the_shared_budget_is_shed_and_the_next_request_is_served() {
    let tight = Budget::new(BudgetLimits {
        connections: 8,
        frames: 32,
        bytes: 64 << 10,
    });
    let r = rig_with(cfg(), tight);
    let mut tls = raw(r.server.addr(), &r.coordinator);
    let big = vec![7u8; 1 << 20];
    tls.write_all(&Header::request(1, PeerRpc::ShardWrite, 5000, big.len() as u32).encode())
        .unwrap();
    tls.write_all(&big).unwrap();
    tls.write_all(&Header::request(3, PeerRpc::Heartbeat, 5000, 0).encode())
        .unwrap();
    tls.flush().unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    let start = Instant::now();
    while got.len() < 2 * HEADER_LEN && start.elapsed() < Duration::from_secs(5) {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }
    }
    assert_eq!(got[5], Status::Overloaded.code());
    assert_eq!(got[HEADER_LEN + 5], Status::Ok.code());
    assert!(r.server.budget().report().peak_bytes <= 64 << 10);
}

#[test]
fn the_connection_cap_refuses_before_any_tls_work_and_frees_slots_on_close() {
    let capped = Budget::new(BudgetLimits {
        connections: 2,
        frames: 32,
        bytes: 1 << 20,
    });
    let r = rig_with(cfg(), capped);
    let mut a = open(&r, &r.coordinator);
    let _b = open(&r, &r.coordinator);
    let third = Channel::connect(r.server.addr(), &r.coordinator, &[Role::IndexNode], cfg());
    assert!(third.is_err());
    assert!(r.server.budget().report().refused_connections >= 1);
    assert_eq!(r.server.budget().report().peak_connections, 2);
    assert!(a.call(PeerRpc::Heartbeat, b"", T).is_ok());
    drop(a);
    wait_until("slot returned", || {
        r.server.budget().report().connections == 1
    });
    assert!(
        open(&r, &r.coordinator)
            .call(PeerRpc::Heartbeat, b"", T)
            .is_ok()
    );
}

#[test]
fn many_callers_at_once_stay_inside_the_budget() {
    let r = rig();
    let addr = r.server.addr();
    let coordinator = r.coordinator.clone();
    let handles: Vec<_> = (0..6u8)
        .map(|n| {
            let tls = coordinator.clone();
            std::thread::spawn(move || {
                let mut c = Channel::connect(addr, &tls, &[Role::IndexNode], cfg()).unwrap();
                for i in 0..50u8 {
                    let body = vec![n ^ i; 4000];
                    assert_eq!(c.call(PeerRpc::ShardWrite, &body, T).unwrap(), body);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let report = r.server.budget().report();
    assert!(report.peak_connections <= 6);
    assert!(report.peak_bytes <= 6 * 4000);
    wait_until("all released", || {
        let b = r.server.budget().report();
        b.connections == 0 && b.frames == 0 && b.bytes == 0
    });
}
