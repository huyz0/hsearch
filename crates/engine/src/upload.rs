//! Writing a segment file of any size to the object store with bounded memory.
//!
//! [`ObjectUploadSink`] is the `lucene_store::OutputSink` a write-capable `BlobStore::writer`
//! hands to Lucene. It holds at most one part (`part_size` bytes, at least the provider's
//! minimum) however large the file is. A file that fits in one part is one conditional `create`,
//! one request; a larger one is a create-only multipart upload, one request per part plus two
//! control requests, all attributed by the backend's own ledger. Nothing is visible until
//! `finish`, and a sink dropped before `finish`, or one whose part failed, aborts its upload.
//!
//! Which store and which manifest a publish path writes to is that path's choice (M04, M06);
//! this is only the sink it needs.
use std::io;
use std::sync::Arc;
use std::time::Duration;

use hsearch_object::{
    Budget, Cancellation, Error, LargeObjectWriter, MIN_PART, Metadata, Mutation, ObjectKey,
    Payload, Precondition, RequestContext, RequestId,
};
use lucene_store::OutputSink;

use crate::range::block_on;

/// Control and header bytes a request may carry beyond its body.
const OVERHEAD: u64 = 4096;

fn io_error(op: &str, error: Error) -> io::Error {
    let kind = match error {
        // The small path replays by a key-derived id, so different bytes under a taken key
        // read as an identity conflict: the key exists either way.
        Error::Conflict | Error::IdentityConflict => io::ErrorKind::AlreadyExists,
        Error::NotFound => io::ErrorKind::NotFound,
        Error::PermissionDenied => io::ErrorKind::PermissionDenied,
        Error::InvalidInput | Error::InvalidRange | Error::BudgetExceeded => {
            io::ErrorKind::InvalidInput
        }
        Error::Throttled | Error::DeadlineExceeded | Error::Unavailable => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("object write failed ({op}): {error:?}"))
}

/// A request id that is a pure function of the key, stable across builds and processes: a retry
/// after a lost reply, even from a restarted process, finds the original accepted write by it.
/// (128 bits from two FNV-1a passes; a collision only changes which error a clash reports.)
fn key_request_id(key: &ObjectKey) -> String {
    fn fnv(seed: u64, bytes: &[u8]) -> u64 {
        bytes.iter().fold(seed, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }
    let bytes = key.as_bytes();
    format!(
        "obj-{:016x}{:016x}",
        fnv(0xcbf2_9ce4_8422_2325, bytes),
        fnv(0x8422_2325_cbf2_9ce4, bytes)
    )
}

/// An `OutputSink` that streams one new object to `B`.
pub struct ObjectUploadSink<B: LargeObjectWriter> {
    store: Arc<B>,
    key: ObjectKey,
    part_size: usize,
    deadline: Duration,
    buffer: Vec<u8>,
    upload: Option<B::Upload>,
    parts: u32,
    peak: usize,
    failed: bool,
}

impl<B: LargeObjectWriter> ObjectUploadSink<B> {
    /// A sink for `key`. `part_size` is the memory bound and must be at least [`MIN_PART`].
    pub fn new(
        store: Arc<B>,
        key: ObjectKey,
        part_size: usize,
        deadline: Duration,
    ) -> Result<Self, Error> {
        if !(MIN_PART..=16 * 1024 * 1024).contains(&part_size) {
            return Err(Error::InvalidInput);
        }
        Ok(Self {
            store,
            key,
            part_size,
            deadline,
            buffer: Vec::new(),
            upload: None,
            parts: 0,
            peak: 0,
            failed: false,
        })
    }

    /// The most buffer capacity this sink ever held at once (at most `part_size`). A file of one
    /// part or less is then copied once more into its single request.
    #[must_use]
    pub fn peak_buffered(&self) -> usize {
        self.peak
    }

    /// Make room for `extra` more bytes by doubling, but never past `part_size`: the buffer's
    /// capacity, not just its length, is the memory bound.
    fn grow(&mut self, extra: usize) {
        let need = self.buffer.len() + extra;
        if need > self.buffer.capacity() {
            let target = need
                .max(self.buffer.capacity() * 2)
                .max(64 * 1024)
                .min(self.part_size);
            self.buffer.reserve_exact(target - self.buffer.len());
        }
    }

    fn context(&self, body: usize) -> Result<RequestContext, Error> {
        RequestContext::new(
            self.deadline,
            Cancellation::default(),
            Budget {
                request_bytes: OVERHEAD + self.key.len() as u64 + body as u64,
                response_bytes: OVERHEAD,
                attempts: 8,
            },
        )
    }

    /// Send the full buffer as the next part, beginning the upload on the first one.
    fn flush_part(&mut self) -> Result<(), Error> {
        // One context per request, so one request's budget never pays for another's.
        if self.upload.is_none() {
            let context = self.context(0)?;
            self.upload = Some(block_on(self.store.begin_upload(&context, &self.key))?);
        }
        let context = self.context(self.buffer.len())?;
        let upload = self.upload.as_mut().ok_or(Error::Corrupt)?;
        block_on(self.store.put_part(&context, upload, &self.buffer))?;
        self.parts += 1;
        self.buffer.clear();
        Ok(())
    }

    /// Abort what is open, best effort: the original failure is what the caller needs.
    fn abort(&mut self) {
        if let Some(upload) = self.upload.take()
            && let Ok(context) = self.context(0)
        {
            let _ = block_on(self.store.abort_upload(&context, &upload));
        }
    }

    fn complete(&mut self) -> Result<(), Error> {
        match self.upload.take() {
            None => {
                // One part or less: one conditional create, replayable by an id derived from the
                // key alone (the adapter separately refuses the same id with different bytes).
                let request = Mutation {
                    request_id: RequestId::new(key_request_id(&self.key).as_bytes())?,
                    key: self.key.clone(),
                    payload: Payload::new(&self.buffer)?,
                    metadata: Metadata::new(b"")?,
                    precondition: Precondition::Absent,
                };
                let context = self.context(self.buffer.len())?;
                block_on(self.store.create(&context, &request)).map(|_| ())
            }
            Some(mut upload) => {
                let result = (|| {
                    let context = self.context(self.buffer.len())?;
                    block_on(self.store.put_part(&context, &mut upload, &self.buffer))?;
                    // The completion body names every part: about 100 bytes each.
                    let context = self.context(100 * (self.parts as usize + 1))?;
                    block_on(self.store.complete_upload(&context, &upload)).map(|_| ())
                })();
                // After an unknown completion the object may exist and an abort could race it:
                // leave the upload for the caller (who reads the key) and the lifecycle rule.
                if matches!(&result, Err(error) if *error != Error::UnknownOutcome)
                    && let Ok(context) = self.context(0)
                {
                    let _ = block_on(self.store.abort_upload(&context, &upload));
                }
                result
            }
        }
    }
}

impl<B: LargeObjectWriter> OutputSink for ObjectUploadSink<B> {
    fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("an earlier part failed; upload aborted"));
        }
        while !bytes.is_empty() {
            // A full buffer is sent only when more bytes follow, so the last part is never
            // empty and a file of exactly one part stays a single request.
            if self.buffer.len() == self.part_size
                && let Err(error) = self.flush_part()
            {
                self.failed = true;
                self.abort();
                return Err(io_error("part", error));
            }
            let take = bytes.len().min(self.part_size - self.buffer.len());
            self.grow(take);
            self.buffer.extend_from_slice(&bytes[..take]);
            self.peak = self.peak.max(self.buffer.capacity());
            bytes = &bytes[take..];
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> io::Result<()> {
        if self.failed {
            // A part was lost: what remains in the buffer is not the file.
            return Err(io::Error::other("an earlier part failed; upload aborted"));
        }
        if self.buffer.is_empty() && self.upload.is_none() {
            // Lucene files carry a codec header, so an empty one is a bug upstream of here.
            return Err(io_error("empty file", Error::InvalidInput));
        }
        self.complete().map_err(|error| io_error("finish", error))
    }
}

impl<B: LargeObjectWriter> Drop for ObjectUploadSink<B> {
    fn drop(&mut self) {
        // Not finished: leave no visible file and no billable orphan.
        self.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_id_is_a_pinned_function_of_the_key() {
        // Pinned: a restarted or rebuilt process must derive the same id to find its own write.
        assert_eq!(
            key_request_id(&ObjectKey::new(b"a").unwrap()),
            "obj-af63dc4c8601ec8c789e3e398d3ff5ff"
        );
        assert_ne!(
            key_request_id(&ObjectKey::new(b"a").unwrap()),
            key_request_id(&ObjectKey::new(b"b").unwrap())
        );
        assert!(key_request_id(&ObjectKey::new(&[7u8; 1024]).unwrap()).len() <= 128);
    }
}
