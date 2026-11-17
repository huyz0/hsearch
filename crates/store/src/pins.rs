//! Durable pins: registered before a generation is handed to a reader, honoured by collection.
//!
//! Registration is create pin, then re-read the boundary: a generation below the boundary is
//! refused and its pin removed, because a collector that published that boundary may already have
//! finished scanning pins. A pin that survives the re-read is seen by any scan that follows a
//! boundary above its generation. A pin outlives only until `expires_unix_ms`; a reader must stop
//! using a generation before then (see [`Pin::usable`]), because collection may reclaim after it.
use std::sync::Arc;
use std::time::Duration;

use hsearch_format::{FormatError, Id, Layout, PinRecord};
use hsearch_object::{Error as StoreError, MetadataStore, Precondition};

use crate::boundary::{self, Boundary};
use crate::io::{Io, request_id};

/// Why a pin was not registered or released.
#[derive(Debug)]
pub enum Error {
    /// The pin's expiry is not in the future.
    Expired,
    /// The generation is below the collection boundary; read the head again and pin the current one.
    Swept {
        /// The boundary that refused it.
        boundary: u64,
    },
    /// A pin or boundary object is not valid.
    Corrupt,
    /// The record could not be encoded.
    Format(FormatError),
    /// The store failed; a pin may exist that was not confirmed, and it expires on its own.
    Store(StoreError),
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
impl From<FormatError> for Error {
    fn from(e: FormatError) -> Self {
        Self::Format(e)
    }
}
impl From<boundary::Error> for Error {
    fn from(e: boundary::Error) -> Self {
        match e {
            boundary::Error::Corrupt => Self::Corrupt,
            boundary::Error::Store(e) => Self::Store(e),
        }
    }
}

/// A pin that is registered and safe to read under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pin {
    /// What was registered.
    pub record: PinRecord,
}

impl Pin {
    /// Whether the generation may still be read at `now_unix_ms`, leaving `skew_ms` for clock error
    /// between this node and the collector.
    #[must_use]
    pub fn usable(&self, now_unix_ms: u64, skew_ms: u64) -> bool {
        now_unix_ms.saturating_add(skew_ms) < self.record.expires_unix_ms
    }
}

/// Registers and releases pins of one deployment.
pub struct Pins<S> {
    io: Io<S>,
    layout: Layout,
}

impl<S: MetadataStore> Pins<S> {
    /// Pins stored through `store`.
    #[must_use]
    pub fn new(store: Arc<S>, layout: Layout, deadline: Duration) -> Self {
        Self {
            io: Io::new(store, deadline),
            layout,
        }
    }

    /// Register `record` for a reader of `shard`'s generation `record.manifest_generation`.
    pub fn register(
        &self,
        record: &PinRecord,
        shard: &Id,
        incarnation: u64,
        now_unix_ms: u64,
    ) -> Result<Pin, Error> {
        if record.expires_unix_ms <= now_unix_ms {
            return Err(Error::Expired);
        }
        let key = self.layout.pin(&record.index, &record.pin);
        let mut bytes = Vec::new();
        record.encode(&mut bytes)?;
        let identity = request_id(&[b"pin", key.as_bytes(), record.token.as_bytes()])?;
        self.io
            .write(&key, &bytes, Precondition::Absent, identity)?;
        let (boundary, _) =
            Boundary::new(&self.layout, &record.index, shard, incarnation).read(&self.io)?;
        if record.manifest_generation < boundary {
            self.release(&record.index, &record.pin)?;
            return Err(Error::Swept { boundary });
        }
        Ok(Pin { record: *record })
    }

    /// Remove a pin. Releasing one that is already gone is not an error.
    pub fn release(&self, index: &Id, pin: &Id) -> Result<(), Error> {
        let key = self.layout.pin(index, pin);
        if let Some((_, version)) = self.io.read(&key)? {
            self.io.remove(&key, &version)?;
        }
        Ok(())
    }
}
