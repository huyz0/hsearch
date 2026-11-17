//! Bounded authenticated peer RPC (M07.02).
//!
//! Roles talk to each other with typed request/response frames over mutual TLS 1.3, using the
//! rotating certificates of `hsearch-security`. Nothing here can speak without TLS or with
//! verification off: the only way to build a [`transport::Server`] or [`transport::Channel`] is from
//! a `PeerTls`, which has no plaintext or insecure mode, and a certificate error ends the
//! connection rather than falling back to anything.
//!
//! Layers, each testable without the one above:
//!
//! - [`wire`]: the 20-byte frame header and its validation. The declared length is checked against a
//!   per-RPC cap before a single body byte is allocated.
//! - [`budget`]: process-wide caps on connections, queued frames and queued bytes, shared by every
//!   connection, with the high-water marks that report what the caps held.
//! - [`inbound`]: a sans-I/O decoder and slow-peer clock. It owns the memory of a partly received
//!   frame, charges the budget before accepting a body, skips (without storing) what it refuses,
//!   and ends a connection that is too slow to start, finish or follow a frame.
//! - [`transport`]: blocking TCP + rustls, one thread per connection (bounded by the budget), the
//!   role check for every request, deadlines carried in the frame and cooperative cancellation.
//!
//! Design call, recorded here because the task named HTTP/2: this is a small framed protocol, not
//! HTTP/2. HTTP/2's HPACK tables, CONTINUATION frames, priority trees and flow-control windows are a
//! family of unbounded-memory and rapid-reset attacks that each need a mitigation, and an async
//! runtime plus an `h2` stack is a large dependency surface for six message shapes. Streams, a
//! mandatory deadline and cancellation are kept; the rest is not. If a gateway ever needs HTTP/2 it
//! sits in front of this, not inside it.
pub mod budget;
pub mod inbound;
pub mod transport;
pub mod wire;

pub use budget::{Budget, BudgetLimits, BudgetReport};
pub use wire::{Header, Kind, Limits, Status, WireError};
