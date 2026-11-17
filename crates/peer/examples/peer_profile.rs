//! M07.02 measurements. Markdown to stdout. Loopback, one process (server and clients share it), so
//! RSS figures include both ends and numbers are relative.
//!
//!     cargo run --release -p hsearch-peer --example peer_profile
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use hsearch_peer::budget::{Budget, BudgetLimits};
use hsearch_peer::transport::{Channel, Config, Cx, Handler, Server};
use hsearch_peer::wire::{Header, Status};
use hsearch_security::dev_ca::{DevCa, NodeCert};
use hsearch_security::identity::{CertUse, Role, VerifiedPrincipal};
use hsearch_security::ids::{ClusterId, TrustDomain};
use hsearch_security::policy::PeerRpc;
use hsearch_security::tls::{PeerTls, RotatingCert};
use hsearch_security::x509::{AcceptedIdentity, ChainVerifier, SpiffeIdentityProvider};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use zeroize::Zeroizing;

struct Echo;
impl Handler for Echo {
    fn call(
        &self,
        _: &VerifiedPrincipal,
        _: PeerRpc,
        body: &[u8],
        _: &Cx,
    ) -> Result<Vec<u8>, Status> {
        Ok(body.to_vec())
    }
}

fn node(ca: &DevCa, role: &str) -> Arc<PeerTls> {
    let verifier = Arc::new(ChainVerifier::new(vec![ca.der()], Duration::from_secs(5)).unwrap());
    let id = Arc::new(
        SpiffeIdentityProvider::new(
            verifier,
            vec![AcceptedIdentity {
                trust_domain: TrustDomain::new("example.test").unwrap(),
                cluster: ClusterId::new("prod").unwrap(),
                roles: BTreeSet::from([Role::IndexNode, Role::SearchNode, Role::Coordinator]),
            }],
        )
        .unwrap(),
    );
    let m = ca
        .issue(&NodeCert::spiffe(&format!(
            "spiffe://example.test/hsearch/prod/{role}"
        )))
        .unwrap();
    let cert = RotatingCert::new(
        vec![m.der],
        Zeroizing::new(m.key_pkcs8),
        CertUse::Server,
        id.clone(),
        SystemTime::now(),
    )
    .unwrap();
    Arc::new(PeerTls::new(cert, id).unwrap())
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

fn start(budget: Budget, cfg: Config) -> (Server, Arc<PeerTls>) {
    let ca = DevCa::new("root").unwrap();
    let (index, coordinator) = (node(&ca, "index"), node(&ca, "coordinator"));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = Server::start(listener, index, Arc::new(Echo), budget, cfg).unwrap();
    (server, coordinator)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p) as usize]
}

fn latency() {
    let (server, who) = start(
        Budget::new(BudgetLimits {
            connections: 64,
            frames: 256,
            bytes: 256 << 20,
        }),
        Config::default(),
    );
    let mut c =
        Channel::connect(server.addr(), &who, &[Role::IndexNode], Config::default()).unwrap();
    println!("## Round trips over mutual TLS 1.3 (loopback, one channel, sequential)\n");
    println!("| body | calls | p50 | p99 | throughput |");
    println!("|---|---|---|---|---|");
    for (rpc, size, n) in [
        (PeerRpc::Heartbeat, 0usize, 2000),
        (PeerRpc::ShardSearch, 4 << 10, 2000),
        (PeerRpc::ShardWrite, 1 << 20, 100),
    ] {
        let body = vec![1u8; size];
        let mut times = Vec::with_capacity(n);
        let start = Instant::now();
        for _ in 0..n {
            let t = Instant::now();
            c.call(rpc, &body, Duration::from_secs(10)).unwrap();
            times.push(t.elapsed().as_secs_f64() * 1e6);
        }
        let total = start.elapsed().as_secs_f64();
        times.sort_by(f64::total_cmp);
        println!(
            "| {size} B | {n} | {:.0} us | {:.0} us | {:.0} MiB/s (both directions) |",
            pct(&times, 0.5),
            pct(&times, 0.99),
            2.0 * (size * n) as f64 / total / (1 << 20) as f64
        );
    }
    println!();
}

fn connections() {
    println!("## Memory per connection (server and clients in one process)\n");
    println!("| open connections | handshake p50 | RSS growth | per connection | threads |");
    println!("|---|---|---|---|---|");
    for n in [100usize, 500, 1000] {
        let (server, who) = start(
            Budget::new(BudgetLimits {
                connections: 2000,
                frames: 4096,
                bytes: 256 << 20,
            }),
            Config::default(),
        );
        let before = rss_kib();
        let mut chans = Vec::new();
        let mut times = Vec::new();
        for _ in 0..n {
            let t = Instant::now();
            chans.push(
                Channel::connect(server.addr(), &who, &[Role::IndexNode], Config::default())
                    .unwrap(),
            );
            times.push(t.elapsed().as_secs_f64() * 1e3);
        }
        std::thread::sleep(Duration::from_millis(200));
        let grown = rss_kib().saturating_sub(before);
        times.sort_by(f64::total_cmp);
        let threads = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("Threads:"))
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        println!(
            "| {n} | {:.1} ms | {} KiB | {} KiB | {} |",
            pct(&times, 0.5),
            grown,
            grown / n as u64,
            threads.replace("Threads:", "").trim()
        );
        drop(chans);
    }
    println!();
}

fn slow_bodies() {
    println!("## Declared but unsent bodies (the allocation attack)\n");
    println!("200 connections each declare a 1 MiB ShardWrite body and send 1 KiB of it.\n");
    let (server, who) = start(
        Budget::new(BudgetLimits {
            connections: 400,
            frames: 400,
            bytes: 300 << 20,
        }),
        Config::default(),
    );
    let before = rss_kib();
    let mut raws = Vec::new();
    for _ in 0..200 {
        let tcp = TcpStream::connect(server.addr()).unwrap();
        let conn = ClientConnection::new(
            who.client_config(),
            ServerName::try_from("peer.invalid").unwrap(),
        )
        .unwrap();
        let mut tls = StreamOwned::new(conn, tcp);
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock).unwrap();
        }
        tls.write_all(&Header::request(1, PeerRpc::ShardWrite, 30_000, 1 << 20).encode())
            .unwrap();
        tls.write_all(&[0u8; 1024]).unwrap();
        tls.flush().unwrap();
        raws.push(tls);
    }
    std::thread::sleep(Duration::from_millis(300));
    let r = server.budget().report();
    let grown = rss_kib().saturating_sub(before);
    println!(
        "| declared (budget bytes held) | frames held | RSS growth | RSS growth per connection |"
    );
    println!("|---|---|---|---|");
    println!(
        "| {} MiB | {} | {} KiB | {} KiB |\n",
        r.bytes >> 20,
        r.frames,
        grown,
        grown / 200
    );
    let mut sink = [0u8; 1];
    for mut t in raws {
        let _ = t.sock.set_read_timeout(Some(Duration::from_millis(1)));
        let _ = t.read(&mut sink);
    }
}

fn main() {
    latency();
    connections();
    slow_bodies();
}
