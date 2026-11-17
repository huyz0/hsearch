//! The receive side of one connection, with no I/O: bytes in, events out, time as an argument.
//!
//! Memory is bounded three ways. The header is a fixed 20-byte array. A body is admitted against the
//! shared [`Budget`] from its header alone, and the buffer then grows only as bytes actually arrive
//! (doubling, never past the declared length), so a peer that declares a big body and sends nothing
//! holds a budget permit, not memory, and one that has sent N bytes holds at most 2N. A body that
//! cannot be admitted is skipped without being stored and answered `Overloaded`, so the connection
//! stays usable.
//!
//! Time is bounded too. A frame must finish its header within `header_ms` of its first byte and its
//! body within `body_base_ms` plus the time `min_rate` bytes a second would take; a connection with
//! nothing pending and no frame started for `idle_ms` is closed. [`Inbound::tick`] reports a
//! violation whether or not bytes are arriving, so a peer that stops sending is caught as surely as
//! one that trickles.
use hsearch_security::policy::PeerRpc;

use crate::budget::{Budget, FramePermit};
use crate::wire::{HEADER_LEN, Header, Kind, Limits, Status, WireError};

/// Which end of the connection this decoder is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    /// Receives requests and cancels.
    Server,
    /// Receives responses.
    Client,
}

/// Timing and queue caps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Which end this is.
    pub side: Side,
    /// Size and deadline caps.
    pub limits: Limits,
    /// From a frame's first byte to a complete header.
    pub header_ms: u64,
    /// The fixed part of a body's allowance.
    pub body_base_ms: u64,
    /// The slowest body rate accepted, bytes a second; at least 1.
    pub min_rate: u64,
    /// Longest a connection may sit with nothing pending and no frame started.
    pub idle_ms: u64,
    /// Requests accepted on one connection and not yet answered.
    pub max_pending: usize,
}

/// A request accepted for service.
#[derive(Debug)]
pub struct Request {
    /// The stream.
    pub stream: u32,
    /// The RPC.
    pub rpc: PeerRpc,
    /// The caller's deadline, milliseconds after `received_ms`.
    pub deadline_ms: u32,
    /// When the header completed, on the caller's clock.
    pub received_ms: u64,
    /// The body.
    pub body: Vec<u8>,
    /// The budget share held until this is dropped.
    pub permit: Option<FramePermit>,
}

/// What a received frame turned into.
#[derive(Debug)]
pub enum Event {
    /// A request to serve.
    Request(Request),
    /// The caller cancelled a stream.
    Cancel {
        /// The stream.
        stream: u32,
    },
    /// A request that was not accepted; answer it with `status`.
    Refused {
        /// The stream.
        stream: u32,
        /// Why.
        status: Status,
    },
    /// A response (client side).
    Response {
        /// The stream.
        stream: u32,
        /// How the call ended.
        status: Status,
        /// The body.
        body: Vec<u8>,
    },
}

/// Why the connection must end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fatal {
    /// A frame failed validation.
    Wire(WireError),
    /// A frame arrived too slowly, or stopped arriving.
    TooSlow,
    /// Nothing was happening.
    Idle,
}

enum State {
    Header {
        buf: [u8; HEADER_LEN],
        have: usize,
    },
    Body {
        header: Header,
        buf: Vec<u8>,
        permit: Option<FramePermit>,
    },
    Skip {
        remaining: u32,
    },
}

fn empty() -> State {
    State::Header {
        buf: [0; HEADER_LEN],
        have: 0,
    }
}

/// The decoder for one connection.
pub struct Inbound {
    cfg: Config,
    budget: Option<Budget>,
    state: State,
    /// When the frame in progress (or, between frames, the wait for one) began.
    since_ms: u64,
    /// When the body in progress began; its allowance runs from here.
    body_ms: u64,
    /// Declared length of the body in progress or being skipped.
    body_len: u32,
    pending: usize,
}

impl Inbound {
    /// A decoder at time `now_ms`. The server side charges `budget`; the client side passes `None`.
    #[must_use]
    pub fn new(cfg: Config, budget: Option<Budget>, now_ms: u64) -> Self {
        Self {
            cfg,
            budget,
            state: empty(),
            since_ms: now_ms,
            body_ms: now_ms,
            body_len: 0,
            pending: 0,
        }
    }

    /// A request finished (answered, shed or cancelled): its queue place is free.
    pub fn finished(&mut self) {
        self.pending = self.pending.saturating_sub(1);
    }

    /// Requests accepted and not yet finished.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Bytes allocated for a partly received body, for tests and reports.
    #[must_use]
    pub fn buffered(&self) -> usize {
        match &self.state {
            State::Body { buf, .. } => buf.capacity(),
            _ => 0,
        }
    }

    fn allowance_ms(&self, len: u32) -> u64 {
        self.cfg
            .body_base_ms
            .saturating_add(u64::from(len).saturating_mul(1000) / self.cfg.min_rate.max(1))
    }

    fn reset(&mut self, now_ms: u64) {
        self.state = empty();
        self.since_ms = now_ms;
    }

    /// Takes `data` received at `now_ms`.
    pub fn feed(&mut self, mut data: &[u8], now_ms: u64) -> Result<Vec<Event>, Fatal> {
        let mut events = Vec::new();
        while !data.is_empty() {
            match &mut self.state {
                State::Header { buf, have } => {
                    if *have == 0 {
                        self.since_ms = now_ms;
                    }
                    let take = (HEADER_LEN - *have).min(data.len());
                    buf[*have..*have + take].copy_from_slice(&data[..take]);
                    *have += take;
                    data = &data[take..];
                    if *have == HEADER_LEN {
                        let bytes = *buf;
                        self.on_header(&bytes, now_ms, &mut events)?;
                    }
                }
                State::Body { header, buf, .. } => {
                    let len = header.len as usize;
                    let take = (len - buf.len()).min(data.len());
                    let need = buf.len() + take;
                    if buf.capacity() < need {
                        let grown = buf.capacity().saturating_mul(2).max(need).min(len);
                        buf.reserve_exact(grown - buf.len());
                    }
                    buf.extend_from_slice(&data[..take]);
                    data = &data[take..];
                    if buf.len() == len {
                        self.complete_body(now_ms, &mut events);
                    }
                }
                State::Skip { remaining } => {
                    let take = (*remaining as usize).min(data.len());
                    data = &data[take..];
                    *remaining -= u32::try_from(take).unwrap_or(0);
                    if *remaining == 0 {
                        self.reset(now_ms);
                    }
                }
            }
        }
        Ok(events)
    }

    fn on_header(
        &mut self,
        bytes: &[u8; HEADER_LEN],
        now: u64,
        events: &mut Vec<Event>,
    ) -> Result<(), Fatal> {
        let header = Header::parse(bytes, &self.cfg.limits, self.cfg.side == Side::Server)
            .map_err(Fatal::Wire)?;
        self.body_ms = now;
        self.body_len = header.len;
        match header.kind {
            Kind::Cancel => {
                events.push(Event::Cancel {
                    stream: header.stream,
                });
                self.reset(now);
            }
            Kind::Request => {
                let over_queue = self.pending >= self.cfg.max_pending;
                let permit = if over_queue {
                    None
                } else {
                    self.budget
                        .as_ref()
                        .and_then(|b| b.frame(u64::from(header.len)))
                };
                let refused = over_queue || (self.budget.is_some() && permit.is_none());
                if refused {
                    events.push(Event::Refused {
                        stream: header.stream,
                        status: Status::Overloaded,
                    });
                    if header.len == 0 {
                        self.reset(now);
                    } else {
                        self.state = State::Skip {
                            remaining: header.len,
                        };
                    }
                } else if header.len == 0 {
                    self.pending += 1;
                    events.push(Event::Request(Request {
                        stream: header.stream,
                        rpc: header.rpc.unwrap_or(PeerRpc::Heartbeat),
                        deadline_ms: header.deadline_ms,
                        received_ms: now,
                        body: Vec::new(),
                        permit,
                    }));
                    self.reset(now);
                } else {
                    self.state = State::Body {
                        header,
                        buf: Vec::new(),
                        permit,
                    };
                }
            }
            Kind::Response => {
                if header.len == 0 {
                    events.push(Event::Response {
                        stream: header.stream,
                        status: header.status,
                        body: Vec::new(),
                    });
                    self.reset(now);
                } else {
                    self.state = State::Body {
                        header,
                        buf: Vec::new(),
                        permit: None,
                    };
                }
            }
        }
        Ok(())
    }

    fn complete_body(&mut self, now: u64, events: &mut Vec<Event>) {
        let State::Body {
            header,
            buf,
            permit,
        } = std::mem::replace(&mut self.state, empty())
        else {
            return;
        };
        match header.kind {
            Kind::Request => {
                self.pending += 1;
                events.push(Event::Request(Request {
                    stream: header.stream,
                    rpc: header.rpc.unwrap_or(PeerRpc::Heartbeat),
                    deadline_ms: header.deadline_ms,
                    received_ms: self.body_ms,
                    body: buf,
                    permit,
                }));
            }
            _ => events.push(Event::Response {
                stream: header.stream,
                status: header.status,
                body: buf,
            }),
        }
        self.since_ms = now;
    }

    /// Whether the connection has broken a time rule at `now_ms`. Call it whenever time passes,
    /// including when no bytes arrive.
    pub fn tick(&self, now_ms: u64) -> Result<(), Fatal> {
        match &self.state {
            State::Header { have, .. } if *have > 0 => {
                if now_ms.saturating_sub(self.since_ms) > self.cfg.header_ms {
                    return Err(Fatal::TooSlow);
                }
            }
            State::Header { .. } => {
                if self.pending == 0 && now_ms.saturating_sub(self.since_ms) > self.cfg.idle_ms {
                    return Err(Fatal::Idle);
                }
            }
            State::Body { .. } | State::Skip { .. } => {
                if now_ms.saturating_sub(self.body_ms) > self.allowance_ms(self.body_len) {
                    return Err(Fatal::TooSlow);
                }
            }
        }
        Ok(())
    }
}
