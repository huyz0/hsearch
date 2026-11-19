//! Writing a batch that does not fit inline in a lane record as an external payload object.
//!
//! A record body above [`MAX_INLINE_RECORD_BYTES`] cannot ride in a slot. The serving writer
//! stores the whole encoded batch as one object under the index's payload prefix, with the
//! batch's SHA-256 and length, **before** the record is committed, and commits a record that names
//! it (`PayloadRef::External`). That is the write side of what recovery already reads: it fetches
//! the object in ranged reads of [`PAYLOAD_CHUNK`] and refuses an absent, short, long or altered
//! one at that sequence.
//!
//! Costs, all scaling with bytes and none with documents or indexes:
//!
//! * Up to one chunk (8 MiB): one conditional create. Above it, a create-only multipart upload:
//!   one request to begin, one per 8 MiB part, one to complete.
//! * Memory: the document being assembled is the caller's; beyond it the writer holds nothing
//!   larger than one part, and a multipart part is sent straight from the caller's bytes, with
//!   no copy. A small single-request payload is copied once into its request (the object
//!   contract's `Payload` owns its bytes), which is at most one chunk.
//! * The digest is one pass over the body, chunk by chunk, before the first request, so a body
//!   the store would refuse costs hashing and no request. SHA-256 uses the CPU's hardware
//!   instructions where present (SHA-NI on x86, the SHA2 extension on ARM), selected at run time
//!   by the `sha2` crate.
//!
//! An upload that fails is aborted and nothing is committed: the object is invisible until its
//! upload completes. A payload that completed but whose record was then refused or never settled is
//! an unreferenced object; the payload prefix is what garbage collection lists for those
//! ([`crate::sweep`]). The store is whatever the writer was given, so an encrypting decorator seals
//! the object and the lane seals nothing here.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hsearch_commit::{Committer, DependencyView, External, Outcome, Reject, Request};
use hsearch_engine::Error as EngineError;
use hsearch_format::{Id, Layout, MAX_DOCUMENT_BYTES, MAX_INLINE_RECORD_BYTES};
use hsearch_lane::block_on;
use hsearch_object::{
    Budget, Cancellation, Error as ObjectError, LargeObjectWriter, Metadata, MetadataStore,
    Mutation, ObjectKey, Payload, Precondition, RequestContext, RequestId,
};
use sha2::{Digest as _, Sha256};

use crate::recover::PAYLOAD_CHUNK;
use crate::serve::ServeError;

/// Control and header bytes a request may carry beyond its body.
const OVERHEAD: u64 = 4096;
/// Most times one request is tried by the store layer.
const ATTEMPTS: u32 = 8;

/// Why a payload was not stored. Nothing was committed.
#[derive(Debug, Eq, PartialEq)]
pub enum PayloadError {
    /// The body is empty or longer than [`MAX_DOCUMENT_BYTES`].
    Limit,
    /// The store did not take the object. Any open upload was aborted.
    Store(ObjectError),
}

/// Somewhere to put the object of a batch that is too large for a record.
pub trait PayloadSink: Send + Sync {
    /// Store `body` as a new payload object of `index` and name it. The returned reference is what
    /// the record carries; the object is complete and absent from no reader's view.
    ///
    /// # Errors
    /// [`PayloadError`]; nothing is left readable at the key.
    fn stage(&self, index: &Id, body: &[u8]) -> Result<External, PayloadError>;
}

/// Payload objects written to an object store through its multipart upload.
pub struct ObjectPayloads<P> {
    store: Arc<P>,
    layout: Layout,
    deadline: Duration,
}

impl<P: MetadataStore + LargeObjectWriter + 'static> ObjectPayloads<P> {
    /// Write payload objects to `store` under `layout`, each request bounded by `deadline`.
    pub fn new(store: Arc<P>, layout: Layout, deadline: Duration) -> Self {
        Self {
            store,
            layout,
            deadline,
        }
    }

    fn context(&self, key: &ObjectKey, body: usize) -> Result<RequestContext, ObjectError> {
        RequestContext::new(
            self.deadline,
            Cancellation::default(),
            Budget {
                request_bytes: OVERHEAD + key.len() as u64 + body as u64,
                response_bytes: OVERHEAD,
                attempts: ATTEMPTS,
            },
        )
    }

    /// One conditional create of a body that fits a chunk. The request identity names the key, so a
    /// lost reply is settled by the store as the same write.
    fn create(&self, key: &ObjectKey, id: &Id, body: &[u8]) -> Result<(), ObjectError> {
        let request = Mutation {
            request_id: RequestId::new(format!("pl-{}", hex(id)).as_bytes())?,
            key: key.clone(),
            payload: Payload::new(body)?,
            metadata: Metadata::new(b"")?,
            precondition: Precondition::Absent,
        };
        let context = self.context(key, body.len())?;
        match block_on(self.store.create(&context, &request)) {
            Ok(_) => Ok(()),
            Err(ObjectError::UnknownOutcome | ObjectError::DeadlineExceeded) => {
                block_on(self.store.resolve(&self.context(key, 0)?, &request)).map(|_| ())
            }
            Err(e) => Err(e),
        }
    }

    /// A multipart upload of `body` in parts of one chunk, hashing each as it goes.
    fn upload(&self, key: &ObjectKey, body: &[u8]) -> Result<(), ObjectError> {
        let mut upload = block_on(self.store.begin_upload(&self.context(key, 0)?, key))?;
        let sent = body.chunks(PAYLOAD_CHUNK as usize).try_for_each(|part| {
            block_on(
                self.store
                    .put_part(&self.context(key, part.len())?, &mut upload, part),
            )
        });
        let done = sent
            .and_then(|()| block_on(self.store.complete_upload(&self.context(key, 0)?, &upload)));
        match done {
            Ok((_, length)) if length == body.len() as u64 => Ok(()),
            Ok(_) => Err(ObjectError::Corrupt),
            Err(ObjectError::UnknownOutcome | ObjectError::DeadlineExceeded) => {
                // The object may exist: the key is ours alone, so one of the same length is ours.
                let info = block_on(self.store.head(&self.context(key, 0)?, key));
                match info {
                    Ok(info) if info.length == body.len() as u64 => Ok(()),
                    Ok(_) => Err(ObjectError::Corrupt),
                    Err(e) => {
                        self.abort(key, &upload);
                        Err(e)
                    }
                }
            }
            Err(e) => {
                self.abort(key, &upload);
                Err(e)
            }
        }
    }

    fn abort(&self, key: &ObjectKey, upload: &P::Upload) {
        if let Ok(context) = self.context(key, 0) {
            // Best effort: an upload left open holds parts but no visible object.
            let _ = block_on(self.store.abort_upload(&context, upload));
        }
    }
}

impl<P: MetadataStore + LargeObjectWriter + 'static> PayloadSink for ObjectPayloads<P> {
    fn stage(&self, index: &Id, body: &[u8]) -> Result<External, PayloadError> {
        let length = body.len() as u64;
        if !(1..=MAX_DOCUMENT_BYTES).contains(&length) {
            return Err(PayloadError::Limit);
        }
        let object = fresh_object();
        let key = ObjectKey::new(self.layout.payload(index, &object).as_bytes())
            .map_err(PayloadError::Store)?;
        let mut hash = Sha256::new();
        if length <= PAYLOAD_CHUNK {
            hash.update(body);
            self.create(&key, &object, body)
        } else {
            for part in body.chunks(PAYLOAD_CHUNK as usize) {
                hash.update(part);
            }
            self.upload(&key, body)
        }
        .map_err(PayloadError::Store)?;
        Ok(External {
            object,
            length,
            digest: hash.finalize().into(),
        })
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// A unique 16-byte object name: a randomly keyed hash of the clock and a process counter. It is
/// an identity, not a secret, and carries nothing of the document (the layout keeps names opaque
/// so equal content does not show as equal names).
fn fresh_object() -> Id {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let state = RandomState::new();
    let mut id = [0u8; 16];
    for (half, chunk) in id.chunks_mut(8).enumerate() {
        let mut hasher = state.build_hasher();
        hasher.write_u128(nanos);
        hasher.write_u64(n);
        hasher.write_usize(half);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    id
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Commit `request` as the lane record of one batch: inline when its body fits a record, otherwise
/// as an external payload staged through `sink` first. Without a sink a body over the inline bound
/// is [`Reject::BadBody`], as it was before payloads were written.
pub(crate) fn commit<S: MetadataStore, D: DependencyView>(
    committer: &mut Committer<S, D>,
    sink: Option<&dyn PayloadSink>,
    request: Request,
) -> Result<Outcome, ServeError> {
    let Some(sink) = sink.filter(|_| request.body.len() > MAX_INLINE_RECORD_BYTES) else {
        return Ok(block_on(committer.commit(vec![request])).remove(0));
    };
    let external = sink
        .stage(&request.index, &request.body)
        .map_err(|e| match e {
            PayloadError::Limit => ServeError::Refused(EngineError::Limit),
            // The object is not visible and no record was written: the writer is usable.
            PayloadError::Store(_) => ServeError::Rejected(Reject::Store),
        })?;
    let request = Request {
        body: Vec::new(),
        ..request
    };
    Ok(block_on(committer.commit_external(request, external)))
}
