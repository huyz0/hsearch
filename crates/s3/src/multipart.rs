//! Create-only multipart upload for objects too large to hold in one request body.
//!
//! The upload is the adapter's own protocol, not the [`hsearch_object`] contract: segment files
//! are immutable and written once, so completion is always `If-None-Match: *` and an existing
//! key is a [`Error::Conflict`], never an overwrite. Every control request and every part goes
//! through the adapter's `send`, so each is attributed (verb, role, outcome, bytes) and spends
//! the caller's budget; a part that never reached the wire is not a free retry.
//!
//! Until [`Multipart::complete`] succeeds nothing is visible at the key. A failed or abandoned
//! upload must be [`Multipart::abort`]ed by the caller; a process that dies first leaves an
//! orphan the bucket's lifecycle rule must expire (nothing here LISTs to find one). A failed
//! [`Multipart::complete`] leaves the upload open, so it can still be aborted.
//!
//! A part whose reply was lost (`UnknownOutcome`) is not recorded; the next `put_part` reuses
//! its number, which S3 treats as a replacement. The 5 GiB part maximum is not enforced here.

use crate::ledger::Role;
use crate::sigv4::encode;
use crate::{Op, S3Store, token};
use hsearch_object::{Error, ObjectKey, RequestContext, VersionToken};

/// The smallest part S3 accepts, except for the last one.
pub const MIN_PART: usize = 5 * 1024 * 1024;
/// The most parts one upload may have.
pub const MAX_PARTS: u32 = 10_000;

/// An open multipart upload.
#[derive(Debug)]
pub struct Multipart<'a> {
    store: &'a S3Store,
    key: ObjectKey,
    id: String,
    etags: Vec<String>,
    length: u64,
    /// A part below [`MIN_PART`] was added: only the last part may be.
    short: bool,
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

impl S3Store {
    /// Begin an upload of `key`. One POST; nothing is visible until completion.
    pub fn begin_multipart(
        &self,
        context: &RequestContext,
        key: &ObjectKey,
    ) -> Result<Multipart<'_>, Error> {
        self.logical(Role::Payload);
        context.charge_request(key.len() as u64 + 16)?;
        context.charge_response(1024)?;
        let reply = self.send(
            context,
            Role::Payload,
            Op::Post,
            Some(key),
            "uploads=",
            &[],
            &[],
            1 << 16,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(crate::status_error(reply.status, false));
        }
        let body = String::from_utf8_lossy(&reply.body);
        let id = between(&body, "<UploadId>", "</UploadId>")
            .filter(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_graphic() && !b"&<>\"".contains(&b))
            })
            .ok_or(Error::Corrupt)?;
        Ok(Multipart {
            store: self,
            key: key.clone(),
            id: id.to_owned(),
            etags: Vec::new(),
            length: 0,
            short: false,
        })
    }
}

impl Multipart<'_> {
    /// The provider's upload id, for logs and for finding an orphan to abort.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Parts uploaded so far.
    #[must_use]
    pub fn parts(&self) -> u32 {
        self.etags.len() as u32
    }

    /// Bytes uploaded so far.
    #[must_use]
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Upload the next part. Every part but the last must be at least [`MIN_PART`] bytes, and
    /// that is refused here, before the wire, rather than at completion after the bytes were sent.
    pub fn put_part(&mut self, context: &RequestContext, bytes: &[u8]) -> Result<(), Error> {
        if bytes.is_empty() || self.short || self.parts() >= MAX_PARTS {
            return Err(Error::InvalidInput);
        }
        self.store.logical(Role::Payload);
        let query = format!(
            "partNumber={}&uploadId={}",
            self.parts() + 1,
            encode(self.id.as_bytes(), false)
        );
        context.charge_request(self.key.len() as u64 + query.len() as u64 + bytes.len() as u64)?;
        context.charge_response(1024)?;
        let reply = self.store.send(
            context,
            Role::Payload,
            Op::Put,
            Some(&self.key),
            &query,
            &[],
            bytes,
            1 << 16,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(crate::status_error(reply.status, true));
        }
        let etag = reply
            .header("etag")
            .filter(|etag| !etag.is_empty() && etag.bytes().all(|b| (0x20..0x7f).contains(&b)))
            .ok_or(Error::UnknownOutcome)?;
        self.etags.push(etag.to_owned());
        self.length += bytes.len() as u64;
        self.short = bytes.len() < MIN_PART;
        Ok(())
    }

    /// Complete the upload, creating the object only if the key is absent.
    ///
    /// The upload stays open after any error, so the caller can still [`Multipart::abort`] it.
    ///
    /// [`Error::Conflict`]: the key exists (also reported when a 200 reply carries a
    /// `PreconditionFailed` error body); abort the upload.
    /// [`Error::UnknownOutcome`]: no answer arrived; the object may exist, so the caller reads
    /// the key (HEAD) before deciding. A second completion of a finished upload is refused by the
    /// provider, so the call is not blindly repeated.
    pub fn complete(&self, context: &RequestContext) -> Result<(VersionToken, u64), Error> {
        if self.etags.is_empty() {
            return Err(Error::InvalidInput);
        }
        self.store.logical(Role::Payload);
        let mut xml = String::from("<CompleteMultipartUpload>");
        for (index, etag) in self.etags.iter().enumerate() {
            xml.push_str(&format!(
                "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
                index + 1,
                etag.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            ));
        }
        xml.push_str("</CompleteMultipartUpload>");
        let query = format!("uploadId={}", encode(self.id.as_bytes(), false));
        context.charge_request(self.key.len() as u64 + query.len() as u64 + xml.len() as u64)?;
        context.charge_response(4096)?;
        let reply = self.store.send(
            context,
            Role::Payload,
            Op::Post,
            Some(&self.key),
            &query,
            &[("if-none-match", "*".to_owned())],
            xml.as_bytes(),
            1 << 16,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(crate::status_error(reply.status, true));
        }
        let body = String::from_utf8_lossy(&reply.body);
        // S3 may answer 200 and put the failure in the body: not committed.
        if body.contains("<Error>") || body.contains("<Error ") {
            return Err(match between(&body, "<Code>", "</Code>") {
                Some("PreconditionFailed") => Error::Conflict,
                Some("InvalidPart" | "InvalidPartOrder" | "EntityTooSmall") => Error::InvalidInput,
                Some("NoSuchUpload") => Error::NotFound,
                _ => Error::Unavailable,
            });
        }
        let version = reply
            .header("x-amz-version-id")
            .zip(between(&body, "<ETag>", "</ETag>"))
            .and_then(|(id, etag)| {
                token(
                    id,
                    &etag
                        .replace("&quot;", "\"")
                        .replace("&#34;", "\"")
                        .replace("&#x22;", "\""),
                )
                .ok()
            })
            .ok_or(Error::UnknownOutcome)?;
        Ok((version, self.length))
    }

    /// Abort the upload and discard its parts. A missing upload is already gone.
    pub fn abort(self, context: &RequestContext) -> Result<(), Error> {
        self.store.logical(Role::Payload);
        let query = format!("uploadId={}", encode(self.id.as_bytes(), false));
        context.charge_request(self.key.len() as u64 + query.len() as u64)?;
        context.charge_response(1024)?;
        let reply = self.store.send(
            context,
            Role::Payload,
            Op::Delete,
            Some(&self.key),
            &query,
            &[],
            &[],
            1 << 16,
        )?;
        match reply.status {
            200..=299 | 404 => Ok(()),
            status => Err(crate::status_error(status, true)),
        }
    }
}
