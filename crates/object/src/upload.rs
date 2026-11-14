//! Create-only upload of one large immutable object, in ordered parts.
//!
//! A segment file can be larger than any single request body, and holding it whole to `create`
//! it would put the file's size on the heap. An upload streams it instead: `begin`, then parts
//! in order, then `complete`. Nothing is visible at the key until `complete` succeeds, and
//! `complete` never overwrites: an existing key is [`crate::Error::Conflict`]. The handle is plain
//! data owned by the caller (it borrows nothing), so a writer that outlives a call stack can
//! hold it next to an `Arc` of the store.
//!
//! Failure contract, the same for every provider:
//! - Any error from `complete` leaves the upload open; the caller may `abort` it.
//! - [`crate::Error::UnknownOutcome`] from `complete` means the object may exist; read the key before
//!   deciding. The call is not repeated blindly.
//! - After any error from `put_part` the upload is not to be continued: abort it. (A part whose
//!   reply was lost may or may not be held by the provider; the next part must not assume.)
//! - `abort` is idempotent: an upload that is already gone is success.
//! - [`MIN_PART`] and [`MAX_PARTS`] are S3's limits, used for every provider here; a provider
//!   with other limits would need them declared on the trait.
//! - A part that is not the last must be at least [`MIN_PART`] bytes, and nothing may follow a
//!   shorter one; both are refused before any request is made.

use crate::{ObjectBackend, ObjectKey, Operation, RequestContext, VersionToken};

/// The smallest part accepted unless it is the last.
pub const MIN_PART: usize = 5 * 1024 * 1024;
/// The most parts one upload may have.
pub const MAX_PARTS: u32 = 10_000;

/// An object store that accepts create-only multipart uploads.
pub trait LargeObjectWriter: ObjectBackend {
    /// The caller-owned state of one open upload. It borrows nothing from the store.
    type Upload: Send + 'static;

    /// Open an upload for `key`. Nothing is visible until [`Self::complete_upload`].
    fn begin_upload<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
    ) -> Operation<'a, Self::Upload>;

    /// Append the next part. Refused before any request when empty, after a short part, or
    /// past [`MAX_PARTS`] ([`crate::Error::InvalidInput`]).
    fn put_part<'a>(
        &'a self,
        context: &'a RequestContext,
        upload: &'a mut Self::Upload,
        bytes: &'a [u8],
    ) -> Operation<'a, ()>;

    /// Create the object, only if the key is absent. Returns its version and total length.
    fn complete_upload<'a>(
        &'a self,
        context: &'a RequestContext,
        upload: &'a Self::Upload,
    ) -> Operation<'a, (VersionToken, u64)>;

    /// Discard the upload and its parts. Idempotent.
    fn abort_upload<'a>(
        &'a self,
        context: &'a RequestContext,
        upload: &'a Self::Upload,
    ) -> Operation<'a, ()>;
}
