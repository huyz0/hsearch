//! An example provider that lives outside the engine and uses only `hsearch-object`'s public
//! contracts (M03.06): a directory-backed [`ObjectBackend`] and [`MetadataStore`].
//!
//! Its purpose is to show the contracts suffice for a provider the engine has never seen, and to
//! be qualified by the same suite as the in-memory fake and the S3 adapter
//! (`hsearch_testkit::provider::qualify`). It is **not** a production store:
//!
//! - One process owns the directory. A mutex orders mutations; two processes on one directory
//!   would race (a production provider would use the store's own conditional write).
//! - A mutation commits when its `CURRENT` pointer is renamed into place. The identity record is
//!   written after, so a crash between the two leaves a committed write whose `resolve` answers
//!   [`Error::ResolutionUnavailable`] (never a false "accepted") and whose retry reads as a
//!   conflict. That is the contract's honest-uncertainty case, not data loss.
//! - Keys are limited to 100 bytes (they become directory names); retention of identities and
//!   versions is unbounded.
//!
//! Layout under the root: `COUNTER`, `ids/<hex request id>` and `keys/<hex key>/{CURRENT,<hex
//! version>}`. A version file is `u32 metadata length, metadata, payload`.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hsearch_object::{
    ByteRange, Error, Metadata, MetadataStore, Mutation, ObjectBackend, ObjectInfo, ObjectKey,
    Operation, Payload, Precondition, RequestContext, RequestId, VersionToken, WriteReceipt,
};

/// Longest key this provider accepts: a key becomes a directory name.
pub const MAX_KEY: usize = 100;

/// A directory-backed conditional store.
#[derive(Debug)]
pub struct DirectoryStore {
    root: PathBuf,
    lock: Mutex<()>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn io(_: std::io::Error) -> Error {
    Error::Unavailable
}

/// Write `bytes` to `path` through a temporary file and a rename, so a reader sees all or none.
fn put(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    fs::rename(&tmp, path).map_err(io)?;
    // The rename is only durable once its directory is: without this a power loss could keep a
    // later commit's record while losing the earlier rename it depends on.
    if let Some(dir) = path.parent()
        && let Ok(dir) = fs::File::open(dir)
    {
        dir.sync_all().map_err(io)?;
    }
    Ok(())
}

fn field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(Error::Corrupt);
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn field(&mut self) -> Result<&'a [u8], Error> {
        let len = u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| Error::Corrupt)?);
        self.take(len as usize)
    }
}

/// The identity record: the whole request, then the receipt it earned.
fn encode_identity(request: &Mutation, receipt: &WriteReceipt) -> Vec<u8> {
    let mut out = Vec::new();
    field(&mut out, request.request_id.as_bytes());
    field(&mut out, request.key.as_bytes());
    field(&mut out, request.payload.as_bytes());
    field(&mut out, request.metadata.as_bytes());
    match &request.precondition {
        Precondition::Absent => field(&mut out, b""),
        Precondition::Matches(v) => field(&mut out, &[&[1][..], v.as_bytes()].concat()),
    }
    field(&mut out, receipt.object.version.as_bytes());
    out.extend_from_slice(&receipt.object.length.to_be_bytes());
    field(&mut out, receipt.object.metadata.as_bytes());
    out
}

fn decode_identity(bytes: &[u8]) -> Result<(Mutation, WriteReceipt), Error> {
    let mut c = Cursor(bytes);
    let request_id = RequestId::new(c.field()?)?;
    let key = ObjectKey::new(c.field()?)?;
    let payload = Payload::new(c.field()?)?;
    let metadata = Metadata::new(c.field()?)?;
    let precondition = match c.field()? {
        [] => Precondition::Absent,
        [1, rest @ ..] => Precondition::Matches(VersionToken::new(rest)?),
        _ => return Err(Error::Corrupt),
    };
    let version = VersionToken::new(c.field()?)?;
    let length = u64::from_be_bytes(c.take(8)?.try_into().map_err(|_| Error::Corrupt)?);
    let receipt_metadata = Metadata::new(c.field()?)?;
    Ok((
        Mutation {
            request_id: request_id.clone(),
            key,
            payload,
            metadata,
            precondition,
        },
        WriteReceipt {
            request_id,
            object: ObjectInfo {
                version,
                length,
                metadata: receipt_metadata,
            },
        },
    ))
}

impl DirectoryStore {
    /// Open (creating if needed) a store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        fs::create_dir_all(root.join("ids")).map_err(io)?;
        fs::create_dir_all(root.join("keys")).map_err(io)?;
        Ok(Self {
            root,
            lock: Mutex::new(()),
        })
    }

    fn key_dir(&self, key: &ObjectKey) -> Result<PathBuf, Error> {
        if key.len() > MAX_KEY {
            return Err(Error::InvalidInput);
        }
        Ok(self.root.join("keys").join(hex(key.as_bytes())))
    }

    fn current(&self, key: &ObjectKey) -> Result<Option<VersionToken>, Error> {
        match fs::read(self.key_dir(key)?.join("CURRENT")) {
            Ok(bytes) => Ok(Some(VersionToken::new(&bytes)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(e)),
        }
    }

    /// `(metadata, payload offset, payload length)` of one stored version of `key`.
    fn stored(
        &self,
        key: &ObjectKey,
        version: &VersionToken,
    ) -> Result<(Vec<u8>, ObjectInfo), Error> {
        let path = self.key_dir(key)?.join(hex(version.as_bytes()));
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::NotFound),
            Err(e) => return Err(io(e)),
        };
        let mut c = Cursor(&bytes);
        let metadata = c.field()?.to_vec();
        let payload = c.0.to_vec();
        let info = ObjectInfo {
            version: version.clone(),
            length: payload.len() as u64,
            metadata: Metadata::new(&metadata)?,
        };
        Ok((payload, info))
    }

    fn identity(&self, id: &RequestId) -> Result<Option<(Mutation, WriteReceipt)>, Error> {
        // A request id that would not fit a file name is refused, not retried as `Unavailable`.
        if id.as_bytes().len() > MAX_KEY {
            return Err(Error::InvalidInput);
        }
        match fs::read(self.root.join("ids").join(hex(id.as_bytes()))) {
            Ok(bytes) => decode_identity(&bytes).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(e)),
        }
    }

    fn next_version(&self) -> Result<VersionToken, Error> {
        let path = self.root.join("COUNTER");
        let n = match fs::read(&path) {
            Ok(b) => u128::from_be_bytes(b.try_into().map_err(|_| Error::Corrupt)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(io(e)),
        }
        .checked_add(1)
        .ok_or(Error::BudgetExceeded)?;
        put(&path, &n.to_be_bytes())?;
        VersionToken::new(&n.to_be_bytes())
    }

    fn write(&self, context: &RequestContext, request: &Mutation) -> Result<WriteReceipt, Error> {
        context.charge_request(
            (request.key.len() + request.payload.len() + request.metadata.len()) as u64,
        )?;
        let _guard = self.lock.lock().map_err(|_| Error::Unavailable)?;
        context.check()?;
        // Replay precedes the precondition: a retry of an accepted create is its first receipt.
        if let Some((accepted, receipt)) = self.identity(&request.request_id)? {
            if accepted != *request {
                return Err(Error::IdentityConflict);
            }
            context.charge_response(receipt.object.metadata.len() as u64 + 40)?;
            return Ok(receipt);
        }
        let current = self.current(&request.key)?;
        match (&request.precondition, &current) {
            (Precondition::Absent, None) => {}
            (Precondition::Matches(expected), Some(actual)) if expected == actual => {}
            _ => return Err(Error::Conflict),
        }
        context.charge_response(request.metadata.len() as u64 + 40)?;
        let version = self.next_version()?;
        let dir = self.key_dir(&request.key)?;
        fs::create_dir_all(&dir).map_err(io)?;
        let mut body = Vec::new();
        field(&mut body, request.metadata.as_bytes());
        body.extend_from_slice(request.payload.as_bytes());
        put(&dir.join(hex(version.as_bytes())), &body)?;
        let receipt = WriteReceipt {
            request_id: request.request_id.clone(),
            object: ObjectInfo {
                version: version.clone(),
                length: request.payload.len() as u64,
                metadata: request.metadata.clone(),
            },
        };
        // The commit point. Past it the write is accepted whatever happens next.
        put(&dir.join("CURRENT"), version.as_bytes())?;
        put(
            &self
                .root
                .join("ids")
                .join(hex(request.request_id.as_bytes())),
            &encode_identity(request, &receipt),
        )
        .map_err(|_| Error::UnknownOutcome)?;
        Ok(receipt)
    }
}

impl ObjectBackend for DirectoryStore {
    fn head<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
    ) -> Operation<'a, ObjectInfo> {
        Box::pin(async move {
            context.charge_request(key.len() as u64)?;
            let _guard = self.lock.lock().map_err(|_| Error::Unavailable)?;
            context.check()?;
            let version = self.current(key)?.ok_or(Error::NotFound)?;
            let (_, info) = self.stored(key, &version)?;
            context.charge_response(info.metadata.len() as u64 + 40)?;
            Ok(info)
        })
    }

    fn create<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            if request.precondition != Precondition::Absent {
                return Err(Error::InvalidInput);
            }
            self.write(context, request)
        })
    }

    fn read_range<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
        version: &'a VersionToken,
        range: ByteRange,
    ) -> Operation<'a, Payload> {
        Box::pin(async move {
            context.charge_request(key.len() as u64 + version.len() as u64 + 16)?;
            let _guard = self.lock.lock().map_err(|_| Error::Unavailable)?;
            context.check()?;
            let (payload, info) = self.stored(key, version)?;
            range.validate(info.length)?;
            context.charge_response(range.length())?;
            let start = usize::try_from(range.offset()).map_err(|_| Error::InvalidRange)?;
            let length = usize::try_from(range.length()).map_err(|_| Error::InvalidRange)?;
            Payload::new(&payload[start..start + length])
        })
    }
}

impl MetadataStore for DirectoryStore {
    fn compare_exchange<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move { self.write(context, request) })
    }

    fn resolve<'a>(
        &'a self,
        context: &'a RequestContext,
        request: &'a Mutation,
    ) -> Operation<'a, WriteReceipt> {
        Box::pin(async move {
            context.charge_request(
                (request.key.len() + request.payload.len() + request.metadata.len()) as u64,
            )?;
            let _guard = self.lock.lock().map_err(|_| Error::Unavailable)?;
            context.check()?;
            match self.identity(&request.request_id)? {
                Some((accepted, _)) if accepted != *request => Err(Error::IdentityConflict),
                Some((_, receipt)) => {
                    context.charge_response(receipt.object.metadata.len() as u64 + 40)?;
                    Ok(receipt)
                }
                None => Err(Error::ResolutionUnavailable),
            }
        })
    }

    fn remove_version<'a>(
        &'a self,
        context: &'a RequestContext,
        key: &'a ObjectKey,
        version: &'a VersionToken,
    ) -> Operation<'a, ()> {
        Box::pin(async move {
            context.charge_request(key.len() as u64 + version.len() as u64)?;
            let _guard = self.lock.lock().map_err(|_| Error::Unavailable)?;
            context.check()?;
            let dir = self.key_dir(key)?;
            let file = dir.join(hex(version.as_bytes()));
            match fs::metadata(&file) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::NotFound),
                Err(e) => return Err(io(e)),
            }
            // The pointer goes first: a crash between the two steps leaves an unreferenced
            // version file, never a pointer to a missing one that nothing could clear.
            if self.current(key)?.as_ref() == Some(version) {
                fs::remove_file(dir.join("CURRENT")).map_err(io)?;
            }
            fs::remove_file(&file).map_err(io)?;
            Ok(())
        })
    }
}
