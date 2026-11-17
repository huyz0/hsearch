//! Blocking TCP + rustls transport for the peer protocol.
//!
//! One thread per connection, bounded by the shared [`Budget`]'s connection cap, each with a small
//! stack. The thread owns the TLS stream, reads in short ticks so that time rules fire whether or
//! not bytes arrive, and serves requests in order. Both ends authenticate: the server reads the
//! caller's role from its certificate and checks it against the RPC's allowed callers on every
//! request; the client checks the server's role against the roles it expects before sending a byte.
//!
//! There is no code path without TLS: `PeerTls` is the only source of a configuration, it is TLS 1.3
//! with mandatory client certificates, and any handshake or certificate error closes the socket.
//! An established connection is re-checked against the trust in force at every request, so a
//! certificate that expired or left the trust bundle ends its connections at the next message.
//!
//! Limits of this version: a request being served is not interrupted by a `Cancel` that arrives
//! after it started (the deadline in the frame is what stops a running handler, by [`Cx::is_done`]);
//! a cancel for a request still queued on the connection is honoured. Interrupting a running
//! handler needs a transport that reads while the handler runs, which this blocking one does not.
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use hsearch_security::identity::{CertUse, Role, VerifiedPrincipal};
use hsearch_security::policy::PeerRpc;
use hsearch_security::tls::{PeerSession, PeerTls};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection, StreamOwned};

use crate::budget::{Budget, ConnectionPermit};
use crate::inbound::{self, Event, Fatal, Inbound, Side};
use crate::wire::{HEADER_LEN, Header, Limits, Status};

/// Largest read.
const CHUNK: usize = 16 << 10;
/// Smallest stack a connection thread needs: it holds a TLS session and a 16 KiB read buffer.
const STACK: usize = 256 << 10;

/// Timing and size rules for a connection, either end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Frame size and deadline caps.
    pub limits: Limits,
    /// The TLS handshake must finish within this many milliseconds of the connection.
    pub handshake_ms: u64,
    /// A frame's header must complete within this long of its first byte.
    pub header_ms: u64,
    /// The fixed part of a body's time allowance.
    pub body_base_ms: u64,
    /// The slowest accepted body rate, bytes a second.
    pub min_rate: u64,
    /// Longest a connection may sit idle with nothing pending.
    pub idle_ms: u64,
    /// A blocked write ends the connection after this long.
    pub write_ms: u64,
    /// How often the read loop wakes to check time.
    pub tick_ms: u64,
    /// Requests queued on one connection.
    pub max_pending: usize,
    /// A channel older than this is dropped and must reconnect.
    pub max_channel_age: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            handshake_ms: 5_000,
            header_ms: 2_000,
            body_base_ms: 2_000,
            min_rate: 64 << 10,
            idle_ms: 60_000,
            write_ms: 10_000,
            tick_ms: 20,
            max_pending: 16,
            max_channel_age: hsearch_security::tls::DEFAULT_MAX_CHANNEL_AGE,
        }
    }
}

impl Config {
    fn inbound(&self, side: Side) -> inbound::Config {
        inbound::Config {
            side,
            limits: self.limits,
            header_ms: self.header_ms,
            body_base_ms: self.body_base_ms,
            min_rate: self.min_rate,
            idle_ms: self.idle_ms,
            max_pending: self.max_pending,
        }
    }
}

/// What a handler is told about the call's time.
pub struct Cx {
    deadline: Instant,
}

impl Cx {
    /// Whether the caller's deadline has passed; a handler that can stop early should.
    #[must_use]
    pub fn is_done(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Time left, zero once past.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// Serves requests. The caller's role has already been checked against the RPC.
pub trait Handler: Send + Sync + 'static {
    /// Answers one request.
    fn call(
        &self,
        caller: &VerifiedPrincipal,
        rpc: PeerRpc,
        body: &[u8],
        cx: &Cx,
    ) -> Result<Vec<u8>, Status>;
}

fn is_timeout(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

fn write_frame<W: Write>(w: &mut W, header: &Header, body: &[u8]) -> std::io::Result<()> {
    w.write_all(&header.encode())?;
    w.write_all(body)?;
    w.flush()
}

/// A running server. Dropping it stops the accept loop; open connections end at their next tick.
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    budget: Budget,
    accept: Option<JoinHandle<()>>,
}

impl Server {
    /// Starts accepting on `listener`.
    pub fn start(
        listener: TcpListener,
        tls: Arc<PeerTls>,
        handler: Arc<dyn Handler>,
        budget: Budget,
        cfg: Config,
    ) -> std::io::Result<Self> {
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let (stop, budget) = (stop.clone(), budget.clone());
            std::thread::Builder::new()
                .name("peer-accept".into())
                .spawn(move || accept_loop(&listener, &tls, &handler, &budget, cfg, &stop))?
        };
        Ok(Self {
            addr,
            stop,
            budget,
            accept: Some(accept),
        })
    }

    /// The address it listens on.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The budget it charges.
    #[must_use]
    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    /// Stops accepting and waits for the accept thread.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: &TcpListener,
    tls: &Arc<PeerTls>,
    handler: &Arc<dyn Handler>,
    budget: &Budget,
    cfg: Config,
    stop: &Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((tcp, _)) => {
                // Over the cap the socket is dropped before any TLS work is done for it.
                let Some(permit) = budget.connection() else {
                    continue;
                };
                let (tls, handler, budget, stop) =
                    (tls.clone(), handler.clone(), budget.clone(), stop.clone());
                let spawned = std::thread::Builder::new()
                    .name("peer-conn".into())
                    .stack_size(STACK)
                    .spawn(move || {
                        let _ = serve(tcp, &tls, &*handler, &budget, cfg, &stop, permit);
                    });
                if spawned.is_err() {
                    continue;
                }
            }
            Err(e) if is_timeout(e.kind()) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Why a connection ended. For tests and logs; the connection is closed either way.
#[derive(Debug, Eq, PartialEq)]
pub enum Closed {
    /// The handshake failed or did not finish in time.
    Handshake,
    /// The peer's certificate did not verify as a caller.
    Identity,
    /// The decoder ended it.
    Rule(Fatal),
    /// The peer's certificate stopped being trusted, or its channel aged out.
    Reauthenticate,
    /// The peer closed, or the socket failed.
    Io,
    /// The server was stopping.
    Stopping,
}

fn serve(
    tcp: TcpStream,
    tls: &PeerTls,
    handler: &dyn Handler,
    budget: &Budget,
    cfg: Config,
    stop: &AtomicBool,
    _permit: ConnectionPermit,
) -> Closed {
    let t0 = Instant::now();
    let ms = || u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
    let tick = Duration::from_millis(cfg.tick_ms.max(1));
    if tcp.set_nodelay(true).is_err()
        || tcp.set_read_timeout(Some(tick)).is_err()
        || tcp
            .set_write_timeout(Some(Duration::from_millis(cfg.write_ms.max(1))))
            .is_err()
    {
        return Closed::Io;
    }
    let Ok(mut conn) = ServerConnection::new(tls.server_config()) else {
        return Closed::Handshake;
    };
    let mut tcp = tcp;
    while conn.is_handshaking() {
        if ms() > cfg.handshake_ms || stop.load(Ordering::Relaxed) {
            return Closed::Handshake;
        }
        match conn.complete_io(&mut tcp) {
            Ok(_) => {}
            Err(e) if is_timeout(e.kind()) => {}
            Err(_) => return Closed::Handshake,
        }
    }
    let Some(chain) = conn.peer_certificates() else {
        return Closed::Identity;
    };
    let Ok(session) = tls.session(
        chain,
        CertUse::Client,
        SystemTime::now(),
        cfg.max_channel_age,
    ) else {
        return Closed::Identity;
    };
    let mut stream = StreamOwned::new(conn, tcp);
    let mut inbound = Inbound::new(cfg.inbound(Side::Server), Some(budget.clone()), ms());
    let mut chunk = vec![0u8; CHUNK];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Closed::Stopping;
        }
        let events = match stream.read(&mut chunk) {
            Ok(0) => return Closed::Io,
            Ok(n) => match inbound.feed(&chunk[..n], ms()) {
                Ok(events) => events,
                Err(fatal) => return Closed::Rule(fatal),
            },
            Err(e) if is_timeout(e.kind()) => Vec::new(),
            Err(_) => return Closed::Io,
        };
        if let Err(fatal) = inbound.tick(ms()) {
            return Closed::Rule(fatal);
        }
        if events.is_empty() {
            continue;
        }
        if let Some(closed) = serve_batch(
            &mut stream,
            &session,
            handler,
            &mut inbound,
            events,
            cfg,
            &ms,
        ) {
            return closed;
        }
    }
}

fn serve_batch(
    stream: &mut StreamOwned<ServerConnection, TcpStream>,
    session: &PeerSession,
    handler: &dyn Handler,
    inbound: &mut Inbound,
    events: Vec<Event>,
    cfg: Config,
    ms: &dyn Fn() -> u64,
) -> Option<Closed> {
    let cancelled: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            Event::Cancel { stream } => Some(*stream),
            _ => None,
        })
        .collect();
    for event in events {
        let (stream_id, status, body) = match event {
            Event::Cancel { .. } | Event::Response { .. } => continue,
            Event::Refused { stream, status } => (stream, status, Vec::new()),
            Event::Request(request) => {
                let result = if cancelled.contains(&request.stream) {
                    Err(Status::Cancelled)
                } else if session.check(SystemTime::now()).is_err() {
                    return Some(Closed::Reauthenticate);
                } else if !request
                    .rpc
                    .allowed_callers()
                    .contains(&session.principal().role())
                {
                    Err(Status::WrongRole)
                } else {
                    let waited = ms().saturating_sub(request.received_ms);
                    let allowed = u64::from(request.deadline_ms);
                    if waited >= allowed {
                        Err(Status::DeadlineExceeded)
                    } else {
                        let cx = Cx {
                            deadline: Instant::now() + Duration::from_millis(allowed - waited),
                        };
                        let out =
                            handler.call(session.principal(), request.rpc, &request.body, &cx);
                        match out {
                            _ if cx.is_done() => Err(Status::DeadlineExceeded),
                            Ok(body) if body.len() > cfg.limits.max_response as usize => {
                                Err(Status::Internal)
                            }
                            other => other,
                        }
                    }
                };
                inbound.finished();
                match result {
                    Ok(body) => (request.stream, Status::Ok, body),
                    Err(status) => (request.stream, status, Vec::new()),
                }
            }
        };
        let header = Header::response(stream_id, status, u32::try_from(body.len()).unwrap_or(0));
        if write_frame(stream, &header, &body).is_err() {
            return Some(Closed::Io);
        }
    }
    None
}

/// Why a channel could not be opened. No variant means "fell back to something weaker".
#[derive(Debug, Eq, PartialEq)]
pub enum ChannelError {
    /// The socket could not connect.
    Connect(ErrorKind),
    /// The TLS handshake failed or timed out, including a certificate this node does not trust.
    Handshake,
    /// The server's certificate did not verify as a server.
    Identity,
    /// The server is not in a role the caller expected.
    WrongPeerRole(Role),
}

/// Why a call did not return an answer.
#[derive(Debug, Eq, PartialEq)]
pub enum CallError {
    /// The server answered with a status other than `Ok`.
    Status(Status),
    /// No answer by the deadline; the channel is closed.
    Timeout,
    /// The request body is over the RPC's cap; nothing was sent.
    TooLarge,
    /// The channel is closed.
    Broken,
    /// The server broke the protocol; the channel is closed.
    Protocol(Fatal),
    /// The socket failed; the channel is closed.
    Io(ErrorKind),
    /// The server's certificate stopped being trusted or the channel aged out.
    Reauthenticate,
}

/// A client connection to one peer.
pub struct Channel {
    stream: StreamOwned<ClientConnection, TcpStream>,
    inbound: Inbound,
    session: PeerSession,
    t0: Instant,
    next_stream: u32,
    cfg: Config,
    broken: bool,
}

impl Channel {
    /// Connects, completes mutual TLS and checks that the server's role is one of `expect`.
    pub fn connect(
        addr: SocketAddr,
        tls: &PeerTls,
        expect: &[Role],
        cfg: Config,
    ) -> Result<Self, ChannelError> {
        let t0 = Instant::now();
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_millis(cfg.handshake_ms))
            .map_err(|e| ChannelError::Connect(e.kind()))?;
        let tick = Duration::from_millis(cfg.tick_ms.max(1));
        tcp.set_nodelay(true)
            .and_then(|()| tcp.set_read_timeout(Some(tick)))
            .and_then(|()| tcp.set_write_timeout(Some(Duration::from_millis(cfg.write_ms.max(1)))))
            .map_err(|e| ChannelError::Connect(e.kind()))?;
        let name = ServerName::try_from("peer.invalid").map_err(|_| ChannelError::Handshake)?;
        let mut conn = ClientConnection::new(tls.client_config(), name)
            .map_err(|_| ChannelError::Handshake)?;
        let mut tcp = tcp;
        while conn.is_handshaking() {
            if t0.elapsed() > Duration::from_millis(cfg.handshake_ms) {
                return Err(ChannelError::Handshake);
            }
            match conn.complete_io(&mut tcp) {
                Ok(_) => {}
                Err(e) if is_timeout(e.kind()) => {}
                Err(_) => return Err(ChannelError::Handshake),
            }
        }
        let chain = conn.peer_certificates().ok_or(ChannelError::Identity)?;
        let session = tls
            .session(
                chain,
                CertUse::Server,
                SystemTime::now(),
                cfg.max_channel_age,
            )
            .map_err(|_| ChannelError::Identity)?;
        let role = session.principal().role();
        if !expect.contains(&role) {
            return Err(ChannelError::WrongPeerRole(role));
        }
        Ok(Self {
            stream: StreamOwned::new(conn, tcp),
            inbound: Inbound::new(cfg.inbound(Side::Client), None, 0),
            session,
            t0: Instant::now(),
            next_stream: 1,
            cfg,
            broken: false,
        })
    }

    /// The server's verified identity.
    #[must_use]
    pub fn peer(&self) -> &VerifiedPrincipal {
        self.session.principal()
    }

    fn ms(&self) -> u64 {
        u64::try_from(self.t0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Calls `rpc` with `body`, giving up `timeout` from now. The deadline travels in the frame, so
    /// the server stops working on a request nobody is waiting for.
    pub fn call(
        &mut self,
        rpc: PeerRpc,
        body: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, CallError> {
        if self.broken {
            return Err(CallError::Broken);
        }
        if body.len() > Limits::max_request(rpc) as usize {
            return Err(CallError::TooLarge);
        }
        if self.session.check(SystemTime::now()).is_err() {
            self.broken = true;
            return Err(CallError::Reauthenticate);
        }
        let stream = self.next_stream;
        self.next_stream = self.next_stream.wrapping_add(2).max(1);
        let started = Instant::now();
        let deadline_ms = u32::try_from(timeout.as_millis())
            .unwrap_or(u32::MAX)
            .clamp(1, self.cfg.limits.max_deadline_ms);
        let header = Header::request(
            stream,
            rpc,
            deadline_ms,
            u32::try_from(body.len()).map_err(|_| CallError::TooLarge)?,
        );
        if let Err(e) = write_frame(&mut self.stream, &header, body) {
            self.broken = true;
            return Err(CallError::Io(e.kind()));
        }
        let mut chunk = vec![0u8; CHUNK];
        loop {
            if started.elapsed() >= Duration::from_millis(u64::from(deadline_ms)) {
                self.give_up(stream);
                return Err(CallError::Timeout);
            }
            let events = match self.stream.read(&mut chunk) {
                Ok(0) => {
                    self.broken = true;
                    return Err(CallError::Io(ErrorKind::UnexpectedEof));
                }
                Ok(n) => {
                    let now = self.ms();
                    match self.inbound.feed(&chunk[..n], now) {
                        Ok(events) => events,
                        Err(fatal) => {
                            self.broken = true;
                            return Err(CallError::Protocol(fatal));
                        }
                    }
                }
                Err(e) if is_timeout(e.kind()) => Vec::new(),
                Err(e) => {
                    self.broken = true;
                    return Err(CallError::Io(e.kind()));
                }
            };
            let now = self.ms();
            if let Err(fatal) = self.inbound.tick(now)
                && fatal != Fatal::Idle
            {
                self.broken = true;
                return Err(CallError::Protocol(fatal));
            }
            for event in events {
                if let Event::Response {
                    stream: id,
                    status,
                    body,
                } = event
                {
                    if id != stream {
                        self.broken = true;
                        return Err(CallError::Protocol(Fatal::Wire(
                            crate::wire::WireError::Stream,
                        )));
                    }
                    return if status == Status::Ok {
                        Ok(body)
                    } else {
                        Err(CallError::Status(status))
                    };
                }
            }
        }
    }

    /// Tells the server the answer is no longer wanted and closes the channel: the late answer
    /// would otherwise arrive in the middle of the next call.
    fn give_up(&mut self, stream: u32) {
        let _ = write_frame(&mut self.stream, &Header::cancel(stream), &[]);
        self.broken = true;
    }

    /// Whether the channel is closed.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        self.broken
    }
}

/// The header length, for callers building raw frames in tests and tools.
pub const FRAME_HEADER: usize = HEADER_LEN;
