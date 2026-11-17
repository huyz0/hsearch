//! Publishing a base: verified files, then manifest pages, then the root, then the head.
//!
//! Every object is created once and never changed (a retry of an attempt writes new objects under
//! a new attempt token), so a crash at any step leaves only unreferenced orphans for garbage
//! collection. Nothing names an object before it is complete: the root is written after every
//! file and page it lists exists, and [`Publisher::activate`] verifies the whole root before the
//! structural head's checkpoint transition names it. A base's `covered` coordinate is taken from
//! the applied [`Cut`] that the files were written at and must not pass what the commit path has
//! acknowledged, so a base never claims history that is not durably committed.
use std::sync::Arc;
use std::time::Duration;

use hsearch_engine::{LocalFiles, PublishError, UploadedFile, upload_local};
use hsearch_format::{
    CommitToken, Digest, FileEntry, FormatError, Id, Layout, MAX_PAGE_ENTRIES, MAX_ROOT_PAGES,
    ManifestPage, ManifestRoot, PageRef, StructuralHead,
};
use hsearch_index::barrier::Cut;
use hsearch_lane::{Advance, Change, LaneId, advance_head, block_on};
use hsearch_object::{
    Budget, ByteRange, Cancellation, Error as StoreError, LargeObjectWriter, Metadata,
    MetadataStore, Mutation, ObjectKey, Payload, Precondition, RequestContext, RequestId,
    VersionToken,
};
use sha2::{Digest as _, Sha256};

/// Largest manifest root or page read back (the format bounds both well below this).
const READ_BOUND: u64 = 1 << 20;

/// Why a publish or a verification stopped.
#[derive(Debug)]
pub enum Error {
    /// A native file is missing or fails its footer checksum; nothing was uploaded.
    Unverified,
    /// A file could not be uploaded, or its reply was lost (the same attempt may be published again
    /// for one-part files; a multipart file needs a new attempt).
    Upload(String),
    /// `covered` is above what the commit path acknowledged: the base would claim speculative state.
    Speculative,
    /// `covered` is below the previous base's, or the generation would not rise.
    Regression,
    /// A dependency the root names (mapping, key envelope) is no longer current.
    Stale,
    /// A named object is missing, the wrong length or fails its digest.
    Broken(&'static str),
    /// Stored bytes are not a valid manifest object.
    Format(FormatError),
    /// The store failed; nothing it holds was changed by this step, or the outcome is unknown.
    Store(StoreError),
    /// The structural head moved: read it again and decide again. Nothing was changed.
    Lost,
    /// Whether the head transition landed could not be established: re-read the head before acting.
    Unknown,
    /// The head transition was refused for a reason other than a lost race.
    Head(String),
    /// There are no files to publish.
    NoFiles,
    /// The spec and the published root disagree (index or covered prefix).
    Mismatch,
    /// More files than a root can name.
    TooManyFiles,
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

/// What a base is made of and where it stands in history.
pub struct BaseSpec<'a> {
    /// The index.
    pub index: Id,
    /// The shard.
    pub shard: Id,
    /// The shard's incarnation.
    pub incarnation: u64,
    /// The native format label recorded in the root.
    pub engine_format: &'a str,
    /// The mapping the files were written under.
    pub mapping: Digest,
    /// The key envelope the files are protected under.
    pub crypto_envelope: Id,
    /// The applied prefix the files hold. Only a replay barrier makes one.
    pub cut: Cut,
    /// The prefix the commit path has acknowledged for this index.
    pub committed: u64,
    /// The base this one replaces, if any.
    pub previous: Option<&'a ManifestRoot>,
    /// Unique to this attempt; a retry uses a new one.
    pub attempt: CommitToken,
}

/// Whether the mapping and key a root names are still the current ones.
pub trait Dependencies {
    /// True when both are still current.
    fn current(&self, mapping: &Digest, crypto: &Id) -> bool;
}

/// A dependency view that accepts everything (for stores with nothing to check).
pub struct AnyDependencies;
impl Dependencies for AnyDependencies {
    fn current(&self, _: &Digest, _: &Id) -> bool {
        true
    }
}

/// A structural head as read, with the version a transition must name.
pub struct HeadRead<'a> {
    /// The lane the head belongs to.
    pub lane: &'a LaneId,
    /// The head as read.
    pub head: &'a StructuralHead,
    /// The version it was read at.
    pub version: &'a VersionToken,
}

/// A root that is complete in the store but not yet named by the head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Published {
    /// The manifest's identity (the checkpoint root the head will name).
    pub manifest: Id,
    /// The root as written.
    pub root: ManifestRoot,
    /// The files it lists.
    pub files: usize,
}

/// Publishes bases of one deployment into one store.
pub struct Publisher<S> {
    store: Arc<S>,
    layout: Layout,
    part_size: usize,
    deadline: Duration,
}

fn context(deadline: Duration, body: usize) -> Result<RequestContext, StoreError> {
    RequestContext::new(
        deadline,
        Cancellation::default(),
        Budget {
            request_bytes: 4096 + body as u64,
            response_bytes: 4096 + READ_BOUND,
            attempts: 8,
        },
    )
}

/// A 16-byte identity that is a pure function of its inputs, so a name never leaks content.
fn derive(label: &str, parts: &[&[u8]]) -> Id {
    let mut h = Sha256::new();
    h.update(b"hs-publish\x01");
    h.update(label.as_bytes());
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    let d = h.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&d[..16]);
    id
}

/// The identity the manifest written under `token` has. Pages, the root and one-part files replay
/// by identity, so publishing the same attempt again after a crash finds what landed; a file of
/// more than one part is a create-only multipart upload with no replay, so for those a retry needs
/// a new token and the old attempt's objects are orphans for GC.
#[must_use]
pub fn manifest_id(index: &Id, shard: &Id, token: &CommitToken) -> Id {
    derive("manifest", &[index, shard, token.as_bytes()])
}

fn request_id(key: &str) -> Result<RequestId, StoreError> {
    let d = Sha256::digest(key.as_bytes());
    let mut text = String::from("pub-");
    for b in &d[..16] {
        text.push_str(&format!("{b:02x}"));
    }
    RequestId::new(text.as_bytes())
}

impl<S: MetadataStore + LargeObjectWriter + 'static> Publisher<S> {
    /// The store this publisher writes through.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The deployment layout.
    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// A publisher writing through `store`. `part_size` bounds upload memory (at least 5 MiB).
    #[must_use]
    pub fn new(store: Arc<S>, layout: Layout, part_size: usize, deadline: Duration) -> Self {
        Self {
            store,
            layout,
            part_size,
            deadline,
        }
    }

    fn create(&self, key: &str, bytes: &[u8]) -> Result<(), Error> {
        let request = Mutation {
            request_id: request_id(key)?,
            key: ObjectKey::new(key.as_bytes())?,
            payload: Payload::new(bytes)?,
            metadata: Metadata::new(b"")?,
            precondition: Precondition::Absent,
        };
        let ctx = context(self.deadline, bytes.len())?;
        match block_on(self.store.create(&ctx, &request)) {
            Ok(_) => Ok(()),
            // A conflict may be our own accepted write seen again: ask by identity.
            Err(
                StoreError::Conflict | StoreError::UnknownOutcome | StoreError::DeadlineExceeded,
            ) => {
                let ctx = context(self.deadline, 0)?;
                block_on(self.store.resolve(&ctx, &request))
                    .map(|_| ())
                    .map_err(Error::Store)
            }
            Err(e) => Err(Error::Store(e)),
        }
    }

    fn read(&self, key: &str) -> Result<Vec<u8>, Error> {
        let key = ObjectKey::new(key.as_bytes())?;
        let ctx = context(self.deadline, 0)?;
        let info = block_on(self.store.head(&ctx, &key))?;
        if info.length > READ_BOUND || info.length == 0 {
            return Err(Error::Broken("object size"));
        }
        let ctx = context(self.deadline, 0)?;
        let part = block_on(self.store.read_range(
            &ctx,
            &key,
            &info.version,
            ByteRange::new(0, info.length)?,
        ))?;
        Ok(part.as_bytes().to_vec())
    }

    fn length(&self, key: &str) -> Result<u64, Error> {
        let key = ObjectKey::new(key.as_bytes())?;
        let ctx = context(self.deadline, 0)?;
        Ok(block_on(self.store.head(&ctx, &key))?.length)
    }

    /// Verify and upload the named files of `files`, then write the pages and the root. The
    /// head is not touched: until [`Publisher::activate`], nothing refers to the new objects.
    pub fn publish(
        &self,
        files: &LocalFiles,
        names: &[String],
        spec: &BaseSpec<'_>,
        dependencies: &dyn Dependencies,
    ) -> Result<Published, Error> {
        let covered = spec.cut.covers();
        if covered > spec.committed {
            return Err(Error::Speculative);
        }
        let generation = match spec.previous {
            Some(p) if covered < p.covered || p.generation == u64::MAX => {
                return Err(Error::Regression);
            }
            Some(p) => p.generation + 1,
            None => 1,
        };
        if names.is_empty() {
            return Err(Error::NoFiles);
        }
        if names.len() > MAX_PAGE_ENTRIES * MAX_ROOT_PAGES {
            return Err(Error::TooManyFiles);
        }
        if !dependencies.current(&spec.mapping, &spec.crypto_envelope) {
            return Err(Error::Stale);
        }
        let mut sorted: Vec<String> = names.to_vec();
        sorted.sort();
        sorted.dedup();
        let token = spec.attempt;
        let object_of = |name: &str| {
            derive(
                "file",
                &[&spec.index, &spec.shard, token.as_bytes(), name.as_bytes()],
            )
        };
        let key_of = |name: &str| {
            ObjectKey::new(
                self.layout
                    .payload(&spec.index, &object_of(name))
                    .as_bytes(),
            )
            .ok()
        };
        let uploaded: Vec<UploadedFile> = upload_local(
            files,
            &sorted,
            &self.store,
            &key_of,
            self.part_size,
            self.deadline,
        )
        .map_err(|e| match e {
            PublishError::Unverified => Error::Unverified,
            PublishError::Key(n) => Error::Upload(n),
            PublishError::Upload { name, .. } => Error::Upload(name),
        })?;
        // Each file must now be the length we sent: the store is where it must be readable from.
        for f in &uploaded {
            let key = self.layout.payload(&spec.index, &object_of(&f.name));
            if self.length(&key)? != f.length {
                return Err(Error::Broken("uploaded file length"));
            }
        }
        // A dependency may have changed while the files uploaded.
        if !dependencies.current(&spec.mapping, &spec.crypto_envelope) {
            return Err(Error::Stale);
        }
        let mut pages = Vec::new();
        for (n, chunk) in uploaded.chunks(MAX_PAGE_ENTRIES).enumerate() {
            let page = ManifestPage {
                entries: chunk
                    .iter()
                    .map(|f| FileEntry {
                        name: f.name.clone(),
                        object: object_of(&f.name),
                        length: f.length,
                        digest: f.digest,
                    })
                    .collect(),
            };
            let mut bytes = Vec::new();
            page.encode(&mut bytes)?;
            let id = derive(
                "page",
                &[
                    &spec.index,
                    &spec.shard,
                    token.as_bytes(),
                    &(n as u64).to_le_bytes(),
                ],
            );
            let key = self
                .layout
                .manifest_page(&spec.index, &spec.shard, spec.incarnation, &id);
            self.create(&key, &bytes)?;
            pages.push(PageRef {
                page: id,
                entry_count: u32::try_from(chunk.len()).map_err(|_| Error::TooManyFiles)?,
                digest: Sha256::digest(&bytes).into(),
            });
        }
        let root = ManifestRoot {
            generation,
            index: spec.index,
            engine_format: spec.engine_format.to_owned(),
            mapping: spec.mapping,
            covered,
            crypto_envelope: spec.crypto_envelope,
            pages,
            token,
        };
        let mut bytes = Vec::new();
        root.encode(&mut bytes)?;
        let manifest = manifest_id(&spec.index, &spec.shard, &token);
        let key = self
            .layout
            .manifest_root(&spec.index, &spec.shard, spec.incarnation, &manifest);
        self.create(&key, &bytes)?;
        Ok(Published {
            manifest,
            root,
            files: uploaded.len(),
        })
    }

    /// Read the root `manifest` and prove everything it depends on: the mapping and key it names
    /// are still current, every page's digest and entry count match, and every file object is
    /// present at its recorded length. One GET for the root, one per
    /// page and one HEAD per file; nothing is opened until this passes.
    pub fn verify_root(
        &self,
        index: &Id,
        shard: &Id,
        incarnation: u64,
        manifest: &Id,
        dependencies: &dyn Dependencies,
    ) -> Result<ManifestRoot, Error> {
        let key = self
            .layout
            .manifest_root(index, shard, incarnation, manifest);
        let root = ManifestRoot::decode_for(&self.read(&key)?, index)?;
        if !dependencies.current(&root.mapping, &root.crypto_envelope) {
            return Err(Error::Stale);
        }
        for (i, page_ref) in root.pages.iter().enumerate() {
            let key = self
                .layout
                .manifest_page(index, shard, incarnation, &page_ref.page);
            let page = root.verify_page(i, &self.read(&key)?)?;
            for entry in &page.entries {
                let key = self.layout.payload(index, &entry.object);
                if self.length(&key)? != entry.length {
                    return Err(Error::Broken("file length"));
                }
            }
        }
        Ok(root)
    }

    /// Name `published` as the checkpoint of `lane`'s structural head, covering lane slots below
    /// `lane_cover`. The root is verified first, so the head never names a base that cannot be
    /// opened, and the transition is a compare-and-swap on the version read: a lost race is
    /// [`Error::Lost`] and changes nothing.
    pub fn activate(
        &self,
        at: &HeadRead<'_>,
        published: &Published,
        spec: &BaseSpec<'_>,
        lane_cover: u64,
        dependencies: &dyn Dependencies,
    ) -> Result<(StructuralHead, VersionToken), Error> {
        self.transition(at, published, spec, dependencies, |root| {
            Change::Checkpoint {
                cover: lane_cover,
                root,
            }
        })
    }

    /// Replace the checkpoint the head names by `published`, a rewritten base of the same history
    /// (compaction): the cover stays and the publication generation rises. Verified and
    /// compare-and-swapped exactly like [`Publisher::activate`].
    pub fn replace(
        &self,
        at: &HeadRead<'_>,
        published: &Published,
        spec: &BaseSpec<'_>,
        dependencies: &dyn Dependencies,
    ) -> Result<(StructuralHead, VersionToken), Error> {
        let source = spec.previous.ok_or(Error::Mismatch)?;
        let from = manifest_id(&spec.index, &spec.shard, &source.token);
        self.transition(at, published, spec, dependencies, |root| Change::Replace {
            from,
            root,
        })
    }

    fn transition(
        &self,
        at: &HeadRead<'_>,
        published: &Published,
        spec: &BaseSpec<'_>,
        dependencies: &dyn Dependencies,
        change: impl FnOnce(Id) -> Change,
    ) -> Result<(StructuralHead, VersionToken), Error> {
        if spec.index != published.root.index
            || spec.cut.covers() != published.root.covered
            || published.root.covered > spec.committed
        {
            return Err(Error::Mismatch);
        }
        let stored = self.verify_root(
            &spec.index,
            &spec.shard,
            spec.incarnation,
            &published.manifest,
            dependencies,
        )?;
        if stored != published.root {
            return Err(Error::Mismatch);
        }
        match block_on(advance_head(
            &*self.store,
            &self.layout,
            at.lane,
            at.head,
            at.version,
            change(published.manifest),
        )) {
            Ok(Advance::Done(next, version)) => Ok((next, version)),
            Ok(Advance::Lost) => Err(Error::Lost),
            Err(hsearch_lane::Error::Contended) => Err(Error::Unknown),
            Err(hsearch_lane::Error::Store(e)) => Err(Error::Store(e)),
            Err(e) => Err(Error::Head(format!("{e:?}"))),
        }
    }
}
