//! The frame header.
//!
//! ```text
//! 0  2  magic "HP"
//! 2  1  version (1)
//! 3  1  kind: 1 request, 2 response, 3 cancel
//! 4  1  rpc: PeerRpc code on a request, 0 otherwise
//! 5  1  status: 0 on a request or cancel, a [`Status`] on a response
//! 6  2  reserved, zero
//! 8  4  stream id, big endian, never 0; requests use odd ids
//! 12 4  deadline in milliseconds from receipt, big endian; mandatory on a request, 0 otherwise
//! 16 4  body length, big endian
//! ```
//!
//! Every field is validated before the body is read. A request names its RPC, and the length cap is
//! the RPC's own, so a heartbeat cannot claim 16 MiB. A frame that fails validation ends the
//! connection: after a framing error there is no safe place to resynchronise.
use hsearch_security::policy::PeerRpc;

/// The first two bytes of every frame.
pub const MAGIC: [u8; 2] = *b"HP";
/// The only version.
pub const VERSION: u8 = 1;
/// Bytes in a header.
pub const HEADER_LEN: usize = 20;

/// What a frame is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    /// A call.
    Request,
    /// The answer to one.
    Response,
    /// The caller no longer wants the answer to a stream.
    Cancel,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Kind::Request => 1,
            Kind::Response => 2,
            Kind::Cancel => 3,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Kind::Request),
            2 => Some(Kind::Response),
            3 => Some(Kind::Cancel),
            _ => None,
        }
    }
}

/// How a call ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    /// The body is the answer.
    Ok,
    /// The caller's role may not make this call.
    WrongRole,
    /// The callee shed the request: a budget or queue was full.
    Overloaded,
    /// The deadline passed before or while it was served.
    DeadlineExceeded,
    /// The caller cancelled it.
    Cancelled,
    /// The request was well formed on the wire and wrong in content.
    BadRequest,
    /// The callee cannot serve this now.
    Unavailable,
    /// The callee failed.
    Internal,
}

impl Status {
    /// The wire code.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Status::Ok => 0,
            Status::WrongRole => 1,
            Status::Overloaded => 2,
            Status::DeadlineExceeded => 3,
            Status::Cancelled => 4,
            Status::BadRequest => 5,
            Status::Unavailable => 6,
            Status::Internal => 7,
        }
    }

    /// The status for a wire code.
    #[must_use]
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Status::Ok,
            1 => Status::WrongRole,
            2 => Status::Overloaded,
            3 => Status::DeadlineExceeded,
            4 => Status::Cancelled,
            5 => Status::BadRequest,
            6 => Status::Unavailable,
            7 => Status::Internal,
            _ => return None,
        })
    }
}

/// Why a header was refused. The connection ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireError {
    /// Not a peer frame.
    Magic,
    /// A version this build does not speak.
    Version,
    /// An unknown kind.
    Kind,
    /// An unknown RPC, or an RPC code where none belongs.
    Rpc,
    /// An unknown status, or a status where none belongs.
    Status,
    /// A reserved byte is not zero.
    Reserved,
    /// Stream id 0, or a request with an even id.
    Stream,
    /// A request with no deadline, or one past the cap, or a deadline where none belongs.
    Deadline,
    /// The declared body is longer than the cap for this frame.
    TooLong,
    /// A cancel with a body.
    Body,
}

/// Size caps. Every body is checked against these from the header alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Most bytes of response body.
    pub max_response: u32,
    /// Longest deadline a request may ask for, in milliseconds.
    pub max_deadline_ms: u32,
}

impl Limits {
    /// The most bytes of request body for `rpc`: sized to what the RPC carries, not to a shared
    /// maximum, so a heartbeat cannot make a node reserve a write's worth of memory.
    #[must_use]
    pub fn max_request(rpc: PeerRpc) -> u32 {
        match rpc {
            PeerRpc::Heartbeat => 256,
            PeerRpc::Drain => 1 << 10,
            PeerRpc::Membership => 64 << 10,
            PeerRpc::JournalFetch | PeerRpc::SegmentFetch | PeerRpc::ReplicationPull => 64 << 10,
            PeerRpc::ShardSearch => 1 << 20,
            PeerRpc::ShardWrite | PeerRpc::JournalAppend => 16 << 20,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_response: 64 << 20,
            max_deadline_ms: 60_000,
        }
    }
}

/// A validated header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    /// What the frame is.
    pub kind: Kind,
    /// The RPC, on a request.
    pub rpc: Option<PeerRpc>,
    /// The outcome, on a response.
    pub status: Status,
    /// The stream.
    pub stream: u32,
    /// The deadline in milliseconds, on a request.
    pub deadline_ms: u32,
    /// Body bytes that follow.
    pub len: u32,
}

fn rpc_code(rpc: PeerRpc) -> u8 {
    // ALL is the single list of RPCs; the code is the position plus one.
    PeerRpc::ALL
        .iter()
        .position(|r| *r == rpc)
        .map_or(0, |i| u8::try_from(i + 1).unwrap_or(0))
}

fn rpc_from_code(code: u8) -> Option<PeerRpc> {
    usize::from(code)
        .checked_sub(1)
        .and_then(|i| PeerRpc::ALL.get(i))
        .copied()
}

impl Header {
    /// A request.
    #[must_use]
    pub fn request(stream: u32, rpc: PeerRpc, deadline_ms: u32, len: u32) -> Self {
        Self {
            kind: Kind::Request,
            rpc: Some(rpc),
            status: Status::Ok,
            stream,
            deadline_ms,
            len,
        }
    }

    /// A response.
    #[must_use]
    pub fn response(stream: u32, status: Status, len: u32) -> Self {
        Self {
            kind: Kind::Response,
            rpc: None,
            status,
            stream,
            deadline_ms: 0,
            len,
        }
    }

    /// A cancel.
    #[must_use]
    pub fn cancel(stream: u32) -> Self {
        Self {
            kind: Kind::Cancel,
            rpc: None,
            status: Status::Ok,
            stream,
            deadline_ms: 0,
            len: 0,
        }
    }

    /// The header's bytes.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..2].copy_from_slice(&MAGIC);
        out[2] = VERSION;
        out[3] = self.kind.code();
        out[4] = self.rpc.map_or(0, rpc_code);
        out[5] = self.status.code();
        out[8..12].copy_from_slice(&self.stream.to_be_bytes());
        out[12..16].copy_from_slice(&self.deadline_ms.to_be_bytes());
        out[16..20].copy_from_slice(&self.len.to_be_bytes());
        out
    }

    /// Validates `bytes`. The body length is checked here, before any body is read or reserved.
    /// `from_caller` is true for frames a server receives (requests and cancels, odd stream ids)
    /// and false for frames a client receives (responses).
    pub fn parse(
        bytes: &[u8; HEADER_LEN],
        limits: &Limits,
        from_caller: bool,
    ) -> Result<Self, WireError> {
        if bytes[..2] != MAGIC {
            return Err(WireError::Magic);
        }
        if bytes[2] != VERSION {
            return Err(WireError::Version);
        }
        let kind = Kind::from_code(bytes[3]).ok_or(WireError::Kind)?;
        if bytes[6] != 0 || bytes[7] != 0 {
            return Err(WireError::Reserved);
        }
        let word = |at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let (stream, deadline_ms, len) = (word(8), word(12), word(16));
        if stream == 0 {
            return Err(WireError::Stream);
        }
        match (kind, from_caller) {
            (Kind::Request | Kind::Cancel, true) | (Kind::Response, false) => {}
            _ => return Err(WireError::Kind),
        }
        if from_caller && stream % 2 == 0 {
            return Err(WireError::Stream);
        }
        let rpc = match kind {
            Kind::Request => Some(rpc_from_code(bytes[4]).ok_or(WireError::Rpc)?),
            _ if bytes[4] != 0 => return Err(WireError::Rpc),
            _ => None,
        };
        let status = if kind == Kind::Response {
            Status::from_code(bytes[5]).ok_or(WireError::Status)?
        } else if bytes[5] != 0 {
            return Err(WireError::Status);
        } else {
            Status::Ok
        };
        match kind {
            Kind::Request => {
                if deadline_ms == 0 || deadline_ms > limits.max_deadline_ms {
                    return Err(WireError::Deadline);
                }
                if rpc.is_some_and(|r| len > Limits::max_request(r)) {
                    return Err(WireError::TooLong);
                }
            }
            Kind::Response => {
                if deadline_ms != 0 {
                    return Err(WireError::Deadline);
                }
                if len > limits.max_response {
                    return Err(WireError::TooLong);
                }
            }
            Kind::Cancel => {
                if deadline_ms != 0 {
                    return Err(WireError::Deadline);
                }
                if len != 0 {
                    return Err(WireError::Body);
                }
            }
        }
        Ok(Self {
            kind,
            rpc,
            status,
            stream,
            deadline_ms,
            len,
        })
    }
}
