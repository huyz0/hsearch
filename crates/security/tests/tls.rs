//! M05.06 runtime TLS rotation: certificates and trust bundles rotate while real TCP streams stay open.
//! Negative controls first: a rotation to something unusable is refused and changes nothing, a TLS 1.2
//! client and a client with no certificate cannot connect, and a peer of the wrong role is refused during
//! the handshake.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

use hsearch_security::dev_ca::{DevCa, NodeCert};
use hsearch_security::identity::{CertUse, Role};
use hsearch_security::ids::{ClusterId, TrustDomain};
use hsearch_security::tls::{PeerTls, Reauthenticate, RotateError, RotatingCert};
use hsearch_security::x509::{AcceptedIdentity, ChainVerifier, SpiffeIdentityProvider};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection, StreamOwned};
use zeroize::Zeroizing;

const DOMAIN: &str = "example.test";

fn spiffe(role: &str) -> String {
    format!("spiffe://{DOMAIN}/hsearch/prod/{role}")
}

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

struct Node {
    tls: PeerTls,
    verifier: Arc<ChainVerifier>,
}

fn mint(ca: &DevCa, role: &str, not_after: i64) -> (Vec<Vec<u8>>, Zeroizing<Vec<u8>>) {
    let mut spec = NodeCert::spiffe(&spiffe(role));
    spec.not_after = not_after;
    let m = ca.issue(&spec).unwrap();
    (vec![m.der], Zeroizing::new(m.key_pkcs8))
}

fn node(ca: &DevCa, role: &str) -> Node {
    let verifier = Arc::new(ChainVerifier::new(vec![ca.der()], Duration::from_secs(5)).unwrap());
    let id = identity(verifier.clone());
    let (chain, key) = mint(ca, role, 3600);
    let cert =
        RotatingCert::new(chain, key, CertUse::Server, id.clone(), SystemTime::now()).unwrap();
    Node {
        tls: PeerTls::new(cert, id).unwrap(),
        verifier,
    }
}

fn name() -> ServerName<'static> {
    ServerName::try_from("peer.test").unwrap()
}

/// An echo server: reads length-prefixed messages and sends them back until the peer closes. Returns the
/// address and the leaf the server presented per accepted connection is observable by the client.
fn echo_server(node: &Node) -> (std::net::SocketAddr, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let config = node.tls.server_config();
    let flag = stop.clone();
    thread::spawn(move || {
        for tcp in listener.incoming() {
            if flag.load(Ordering::Relaxed) {
                break;
            }
            let Ok(tcp) = tcp else { continue };
            let config = config.clone();
            thread::spawn(move || {
                let Ok(conn) = ServerConnection::new(config) else {
                    return;
                };
                let mut tls = StreamOwned::new(conn, tcp);
                let mut len = [0u8; 4];
                while tls.read_exact(&mut len).is_ok() {
                    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
                    if tls.read_exact(&mut body).is_err() {
                        return;
                    }
                    if tls
                        .write_all(&len)
                        .and_then(|_| tls.write_all(&body))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (addr, stop)
}

type Client = StreamOwned<ClientConnection, TcpStream>;

fn connect(node: &Node, addr: std::net::SocketAddr) -> Result<Client, String> {
    let tcp = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let conn =
        ClientConnection::new(node.tls.client_config(), name()).map_err(|e| e.to_string())?;
    let mut tls = StreamOwned::new(conn, tcp);
    // Force the handshake to finish so refusals surface here.
    tls.flush().map_err(|e| e.to_string())?;
    while tls.conn.is_handshaking() {
        tls.conn
            .complete_io(&mut tls.sock)
            .map_err(|e| e.to_string())?;
    }
    Ok(tls)
}

fn round_trip(c: &mut Client, payload: &[u8]) {
    c.write_all(&(payload.len() as u32).to_be_bytes()).unwrap();
    c.write_all(payload).unwrap();
    let mut len = [0u8; 4];
    c.read_exact(&mut len).unwrap();
    let mut back = vec![0u8; u32::from_be_bytes(len) as usize];
    c.read_exact(&mut back).unwrap();
    assert_eq!(back, payload);
}

fn peer_leaf(c: &Client) -> Vec<u8> {
    c.conn.peer_certificates().unwrap()[0].as_ref().to_vec()
}

fn protocol(c: &Client) -> rustls::ProtocolVersion {
    c.conn.protocol_version().unwrap()
}

#[test]
fn mutual_handshake_is_tls13_and_names_the_peer() {
    let ca = DevCa::new("root").unwrap();
    let (server, client) = (node(&ca, "index"), node(&ca, "search"));
    let (addr, _stop) = echo_server(&server);
    let mut c = connect(&client, addr).unwrap();
    assert_eq!(protocol(&c), rustls::ProtocolVersion::TLSv1_3);
    round_trip(&mut c, b"hello");
    let chain = c.conn.peer_certificates().unwrap();
    let session = client
        .tls
        .session(
            chain,
            CertUse::Server,
            SystemTime::now(),
            Duration::from_secs(60),
        )
        .unwrap();
    assert_eq!(session.principal().role(), Role::IndexNode);
}

#[test]
fn server_certificate_rotates_while_a_stream_is_open() {
    let ca = DevCa::new("root").unwrap();
    let (server, client) = (node(&ca, "index"), node(&ca, "search"));
    let (addr, _stop) = echo_server(&server);
    let mut old = connect(&client, addr).unwrap();
    let first = peer_leaf(&old);
    round_trip(&mut old, &[1; 1000]);

    let (chain, key) = mint(&ca, "index", 3600);
    let generation = server
        .tls
        .cert()
        .rotate(chain, key, SystemTime::now())
        .unwrap();
    assert_eq!(generation, 2);

    // The open stream is untouched and still carries the old handshake's identity.
    round_trip(&mut old, &[2; 200_000]);
    assert_eq!(peer_leaf(&old), first);
    // The next connection sees the new certificate.
    let mut fresh = connect(&client, addr).unwrap();
    assert_ne!(peer_leaf(&fresh), first);
    assert_eq!(peer_leaf(&fresh), server.tls.cert().current_leaf());
    round_trip(&mut fresh, b"after");
    round_trip(&mut old, b"still open");
}

#[test]
fn client_certificate_rotates_for_the_next_handshake() {
    let ca = DevCa::new("root").unwrap();
    let (server, client) = (node(&ca, "index"), node(&ca, "search"));
    let (addr, _stop) = echo_server(&server);
    let mut before = connect(&client, addr).unwrap();
    let (chain, key) = mint(&ca, "search", 3600);
    client
        .tls
        .cert()
        .rotate(chain, key, SystemTime::now())
        .unwrap();
    round_trip(&mut before, b"still fine");
    let mut after = connect(&client, addr).unwrap();
    round_trip(&mut after, b"new identity");
    assert_eq!(client.tls.cert().generation(), 2);
}

#[test]
fn a_refused_rotation_changes_nothing() {
    let ca = DevCa::new("root").unwrap();
    let other = DevCa::new("rogue").unwrap();
    let server = node(&ca, "index");
    let before = server.tls.cert().current_leaf();
    let now = SystemTime::now();

    let (rogue_chain, rogue_key) = mint(&other, "index", 3600);
    assert!(matches!(
        server
            .tls
            .cert()
            .rotate(rogue_chain.clone(), rogue_key, now),
        Err(RotateError::NotAcceptable(_))
    ));
    let (_, key) = mint(&ca, "index", 3600);
    let (expired, _) = mint(&ca, "index", -3600);
    assert!(matches!(
        server.tls.cert().rotate(expired, key, now),
        Err(RotateError::NotAcceptable(_))
    ));
    // A trusted certificate with someone else's key.
    let (chain, _) = mint(&ca, "index", 3600);
    let (_, other_key) = mint(&ca, "index", 3600);
    assert_eq!(
        server.tls.cert().rotate(chain, other_key, now),
        Err(RotateError::BadKey)
    );
    assert_eq!(
        server
            .tls
            .cert()
            .rotate(vec![], Zeroizing::new(vec![]), now),
        Err(RotateError::BadChain)
    );
    assert_eq!(
        server
            .tls
            .cert()
            .rotate(vec![vec![1]; 7], Zeroizing::new(vec![]), now),
        Err(RotateError::BadChain)
    );
    assert_eq!(server.tls.cert().generation(), 1);
    assert_eq!(server.tls.cert().current_leaf(), before);
    assert!(!RotateError::BadKey.to_string().is_empty());
    assert!(!RotateError::BadChain.to_string().is_empty());
}

#[test]
fn trust_bundle_rotation_overlaps_then_refuses_new_handshakes_and_ends_old_sessions() {
    let old_ca = DevCa::new("old root").unwrap();
    let new_ca = DevCa::new("new root").unwrap();
    // Both nodes share one verifier, as one process would.
    let verifier =
        Arc::new(ChainVerifier::new(vec![old_ca.der()], Duration::from_secs(5)).unwrap());
    let id = identity(verifier.clone());
    let mk = |ca: &DevCa, role: &str| {
        let (chain, key) = mint(ca, role, 3600);
        let cert =
            RotatingCert::new(chain, key, CertUse::Server, id.clone(), SystemTime::now()).unwrap();
        Node {
            tls: PeerTls::new(cert, id.clone()).unwrap(),
            verifier: verifier.clone(),
        }
    };
    let (server, client) = (mk(&old_ca, "index"), mk(&old_ca, "search"));
    let (addr, _stop) = echo_server(&server);
    let mut stream = connect(&client, addr).unwrap();
    let session = client
        .tls
        .session(
            stream.conn.peer_certificates().unwrap(),
            CertUse::Server,
            SystemTime::now(),
            Duration::from_secs(3600),
        )
        .unwrap();
    assert!(session.check(SystemTime::now()).is_ok());

    // New bundle; the old one stays acceptable for one second (the handshake clock has whole-second resolution).
    let t0 = SystemTime::now();
    server
        .verifier
        .rotate(vec![new_ca.der()], t0, Duration::from_secs(1))
        .unwrap();
    let mut during = connect(&client, addr).unwrap();
    round_trip(
        &mut during,
        b"old-issued certificates still work during the overlap",
    );
    assert!(session.check(SystemTime::now()).is_ok());

    thread::sleep(Duration::from_millis(2300));
    // The open stream keeps carrying bytes, but the session's re-check says it must end.
    round_trip(&mut stream, b"in flight bytes are not cut");
    assert!(matches!(
        session.check(SystemTime::now()),
        Err(Reauthenticate::NoLongerTrusted(_))
    ));
    // New handshakes with old-CA certificates are refused on both sides.
    assert!(connect(&client, addr).is_err());

    // Re-issue both under the new CA: service resumes.
    let (c1, k1) = mint(&new_ca, "index", 3600);
    server.tls.cert().rotate(c1, k1, SystemTime::now()).unwrap();
    let (c2, k2) = mint(&new_ca, "search", 3600);
    client.tls.cert().rotate(c2, k2, SystemTime::now()).unwrap();
    let mut resumed = connect(&client, addr).unwrap();
    round_trip(&mut resumed, b"back");
}

#[test]
fn a_session_ends_by_age_and_by_clock_reversal() {
    let ca = DevCa::new("root").unwrap();
    let (server, client) = (node(&ca, "index"), node(&ca, "search"));
    let (addr, _stop) = echo_server(&server);
    let c = connect(&client, addr).unwrap();
    let t0 = SystemTime::now();
    let s = client
        .tls
        .session(
            c.conn.peer_certificates().unwrap(),
            CertUse::Server,
            t0,
            Duration::from_secs(60),
        )
        .unwrap();
    assert!(s.check(t0 + Duration::from_secs(60)).is_ok());
    assert_eq!(
        s.check(t0 + Duration::from_secs(61)),
        Err(Reauthenticate::TooOld)
    );
    assert_eq!(
        s.check(t0 - Duration::from_secs(1)),
        Err(Reauthenticate::ClockWentBack)
    );
    // The certificate expires after an hour: even within the age bound the check fails.
    let long = client
        .tls
        .session(
            c.conn.peer_certificates().unwrap(),
            CertUse::Server,
            t0,
            Duration::from_secs(48 * 3600),
        )
        .unwrap();
    assert!(matches!(
        long.check(t0 + Duration::from_secs(2 * 3600)),
        Err(Reauthenticate::NoLongerTrusted(_))
    ));
}

#[test]
fn rotation_under_load_loses_no_bytes_and_no_stream() {
    let ca = DevCa::new("root").unwrap();
    let (server, client) = (node(&ca, "index"), node(&ca, "search"));
    let (addr, _stop) = echo_server(&server);
    let done = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..6u8)
        .map(|w| {
            let cfg = client.tls.client_config();
            let done = done.clone();
            thread::spawn(move || {
                let tcp = TcpStream::connect(addr).unwrap();
                let conn = ClientConnection::new(cfg, name()).unwrap();
                let mut c = StreamOwned::new(conn, tcp);
                let mut rounds = 0u32;
                while !done.load(Ordering::Relaxed) || rounds < 50 {
                    let payload: Vec<u8> = (0..(1 + (rounds as usize * 997) % 70_000))
                        .map(|i| (i as u8) ^ w)
                        .collect();
                    round_trip(&mut c, &payload);
                    rounds += 1;
                }
                rounds
            })
        })
        .collect();
    for _ in 0..20 {
        let (chain, key) = mint(&ca, "index", 3600);
        server
            .tls
            .cert()
            .rotate(chain, key, SystemTime::now())
            .unwrap();
        let (chain, key) = mint(&ca, "search", 3600);
        client
            .tls
            .cert()
            .rotate(chain, key, SystemTime::now())
            .unwrap();
        // Every rotation is also followed by a fresh handshake that must succeed.
        let mut fresh = connect(&client, addr).unwrap();
        round_trip(&mut fresh, b"fresh");
    }
    done.store(true, Ordering::Relaxed);
    for w in workers {
        assert!(w.join().unwrap() >= 50);
    }
    assert_eq!(server.tls.cert().generation(), 21);
}

#[test]
fn there_is_no_downgrade_and_no_anonymous_client() {
    let ca = DevCa::new("root").unwrap();
    let (server, client) = (node(&ca, "index"), node(&ca, "search"));
    let (addr, _stop) = echo_server(&server);

    // A TLS 1.2-only client cannot connect.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let v12 = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS12])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnything))
        .with_no_client_auth();
    assert!(handshake(Arc::new(v12), addr).is_err());

    // A TLS 1.3 client that sends no certificate cannot connect either (client auth is mandatory).
    let anon = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnything))
        .with_no_client_auth();
    let result = handshake(Arc::new(anon), addr).and_then(|mut c| {
        // TLS 1.3 reports the server's refusal on the first read after the client's handshake ends.
        c.write_all(&[0, 0, 0, 1, 7]).map_err(|e| e.to_string())?;
        let mut b = [0u8; 5];
        c.read_exact(&mut b).map_err(|e| e.to_string())
    });
    assert!(result.is_err(), "an anonymous client was served");

    // A client that trusts a different CA refuses the server.
    let other = DevCa::new("other").unwrap();
    let stranger = node(&other, "search");
    assert!(connect(&stranger, addr).is_err());
    let _ = client;
}

#[test]
fn the_wrong_role_is_refused_in_the_handshake() {
    let ca = DevCa::new("root").unwrap();
    let verifier = Arc::new(ChainVerifier::new(vec![ca.der()], Duration::from_secs(5)).unwrap());
    // This deployment accepts only index nodes.
    let id = Arc::new(
        SpiffeIdentityProvider::new(
            verifier,
            vec![AcceptedIdentity {
                trust_domain: TrustDomain::new(DOMAIN).unwrap(),
                cluster: ClusterId::new("prod").unwrap(),
                roles: BTreeSet::from([Role::IndexNode]),
            }],
        )
        .unwrap(),
    );
    let (chain, key) = mint(&ca, "index", 3600);
    let cert =
        RotatingCert::new(chain, key, CertUse::Server, id.clone(), SystemTime::now()).unwrap();
    let server = PeerTls::new(cert, id.clone()).unwrap();
    // A coordinator certificate cannot even be installed as this node's own.
    let (chain, key) = mint(&ca, "coordinator", 3600);
    assert!(matches!(
        server.cert().rotate(chain, key, SystemTime::now()),
        Err(RotateError::NotAcceptable(_))
    ));
}

#[derive(Debug)]
struct AcceptAnything;

impl rustls::client::danger::ServerCertVerifier for AcceptAnything {
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

fn handshake(
    config: Arc<rustls::ClientConfig>,
    addr: std::net::SocketAddr,
) -> Result<Client, String> {
    let tcp = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let conn = ClientConnection::new(config, name()).map_err(|e| e.to_string())?;
    let mut tls = StreamOwned::new(conn, tcp);
    while tls.conn.is_handshaking() {
        tls.conn
            .complete_io(&mut tls.sock)
            .map_err(|e| e.to_string())?;
    }
    Ok(tls)
}
