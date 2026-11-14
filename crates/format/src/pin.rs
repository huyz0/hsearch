//! Durable pins and bounded cursors: opaque, versioned, size-bounded identities.
use crate::codec::{Reader, Writer};
use crate::envelope::{Kind, open};
use crate::limits::MAX_CURSOR_ENTRIES;
use crate::{CommitToken, FormatError, Id, Result};

/// A durable pin: registered before a snapshot or PIT is handed out, honoured by GC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinRecord {
    /// The pin's own id.
    pub pin: Id,
    /// The index it protects.
    pub index: Id,
    /// The manifest root it holds live.
    pub manifest: Id,
    /// That manifest's generation.
    pub manifest_generation: u64,
    /// Expiry in Unix milliseconds; GC may reclaim only after it.
    pub expires_unix_ms: u64,
    /// Unique per commit (M03.11).
    pub token: CommitToken,
}

impl PinRecord {
    /// Decode and require the pin and index the key named.
    pub fn decode_for(bytes: &[u8], index: &Id, pin: &Id) -> Result<Self> {
        let p = Self::decode(bytes)?;
        if &p.index != index || &p.pin != pin {
            return Err(FormatError::Mismatch("pin identity"));
        }
        Ok(p)
    }

    /// Append the encoded pin to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut w = Writer::begin(out, Kind::Pin, 1, 0);
        w.bytes(&self.pin);
        w.bytes(&self.index);
        w.bytes(&self.manifest);
        w.u64(self.manifest_generation);
        w.u64(self.expires_unix_ms);
        w.bytes(self.token.as_bytes());
        w.finish()
    }

    /// Decode and validate one whole object.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::Pin)?;
        let mut r = Reader::new(body);
        let p = Self {
            pin: r.id()?,
            index: r.id()?,
            manifest: r.id()?,
            manifest_generation: r.u64()?,
            expires_unix_ms: r.u64()?,
            token: CommitToken::new(r.array()?)?,
        };
        r.finish()?;
        Ok(p)
    }
}

/// What a cursor holds: a bounded vector of shard positions, or a reference to a durable pin.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CursorBody {
    /// At most [`MAX_CURSOR_ENTRIES`] `(shard, ordinal)` pairs.
    Vector(Vec<(Id, u64)>),
    /// A durable pin that holds the snapshot vector.
    Pin(Id),
}

/// A cursor token, small enough for an HTTP header after base64 (about 1.1 KiB at most).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cursor {
    // Unauthenticated: a client holds it and the CRC is forgeable. M05 owns the MAC or AEAD;
    // until then a decoded cursor names nothing the caller may trust, and authorization of the
    // index and pin it names is checked independently.
    /// The index the cursor reads.
    pub index: Id,
    /// Catalog/routing generation the vector was taken at.
    pub routing_generation: u64,
    /// The positions or the pin.
    pub body: CursorBody,
}

impl Cursor {
    /// Append the encoded cursor to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        let mut w = Writer::begin(out, Kind::Cursor, 1, 0);
        w.bytes(&self.index);
        w.u64(self.routing_generation);
        match &self.body {
            CursorBody::Pin(p) => {
                w.u8(2);
                w.bytes(p);
            }
            CursorBody::Vector(v) => {
                w.u8(1);
                if let Err(e) = w.count(v.len(), MAX_CURSOR_ENTRIES, "cursor entries") {
                    return w.abort(e);
                }
                for (shard, ord) in v {
                    w.bytes(shard);
                    w.u64(*ord);
                }
            }
        }
        w.finish()
    }

    /// Decode and validate one whole object.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, body) = open(bytes, Kind::Cursor)?;
        let mut r = Reader::new(body);
        let index = r.id()?;
        let routing_generation = r.u64()?;
        let body = match r.u8()? {
            2 => CursorBody::Pin(r.id()?),
            1 => {
                let n = r.count(MAX_CURSOR_ENTRIES, 24, "cursor entries")?;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push((r.id()?, r.u64()?));
                }
                CursorBody::Vector(v)
            }
            _ => return Err(FormatError::Malformed("cursor body")),
        };
        r.finish()?;
        Ok(Self {
            index,
            routing_generation,
            body,
        })
    }
}
