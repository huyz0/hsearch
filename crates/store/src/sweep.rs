//! Collecting unreachable objects: a dry run, and a sweep that is safe to stop and run again.
//!
//! A sweep publishes the boundary (the live generation of every shard) **first**, then scans pins,
//! then lists the roots. A root is kept when its generation is at or above the boundary (the live
//! base and any base still being published) or a pin holds it; every other root is a candidate,
//! and the files and pages only candidates name are deleted, **files, then pages, then the root
//! last**, so a sweep stopped anywhere leaves a root that still names what remains and the next
//! run finishes the job. Nothing is deleted when a root, page or pin that must be read is
//! unreadable, or when the live root is missing: an unreadable view is an unknown reference.
//!
//! LIST is used here and only here, one page per 1,000 keys, over the roots and pages of a shard and
//! the payloads of an index; a version comes back with each key, so a deletion needs no HEAD.
//! Memory is bounded by [`Config::max_tracked`]: a sweep that would track more refuses.
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use hsearch_format::{FormatError, Id, Layout, ManifestRoot, PinRecord};
use hsearch_lane::{LaneId, block_on, read_head};
use hsearch_object::{Error as StoreError, Inventory, MetadataStore, VersionToken};

use crate::boundary::{self, Boundary};
use crate::io::{Io, Requests};
use crate::pointer::{self, BaseSource};

/// One listed object, with its key as text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Listed {
    pub(crate) key: String,
    pub(crate) version: VersionToken,
    pub(crate) modified_unix_ms: u64,
}

/// One shard incarnation of the index being collected.
#[derive(Clone, Copy, Debug)]
pub struct ShardRef {
    /// The shard.
    pub shard: Id,
    /// Its incarnation.
    pub incarnation: u64,
    /// The lane whose structural head names its live root.
    pub lane: LaneId,
    /// Whether the live root is the head's checkpoint or the shard's own pointer.
    pub base: BaseSource,
}

/// Whether a run changes anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Count what a sweep would delete; write and delete nothing, boundary included.
    DryRun,
    /// Publish the boundary and delete.
    Sweep,
}

/// What one run may do.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Dry run or sweep.
    pub mode: Mode,
    /// The collector's clock, in Unix milliseconds (pins expire against it).
    pub now_unix_ms: u64,
    /// Keys asked of the inventory per page (at most 1,000 on S3).
    pub page: usize,
    /// Most object identities remembered at once; a sweep needing more refuses.
    pub max_tracked: usize,
    /// Most roots taken as candidates in one run; the rest wait for the next.
    pub max_roots: usize,
    /// Most objects deleted in one run; the rest wait for the next.
    pub max_deletes: usize,
    /// Also delete payloads and pages no root names once they are this old. Off by default:
    /// payloads are per index, so this is only correct when `shards` lists **every** shard
    /// incarnation of the index, and an upload older than this that is still in flight loses its
    /// objects (its publish then fails verification rather than naming them).
    pub orphan_grace_ms: Option<u64>,
}

/// What one run found or did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Report {
    /// Roots kept (live, in flight, or pinned).
    pub roots_kept: usize,
    /// Roots deleted (or that a dry run would delete).
    pub roots_swept: usize,
    /// Pages deleted by candidacy.
    pub pages_swept: usize,
    /// Files deleted by candidacy.
    pub files_swept: usize,
    /// Pages deleted for being unreferenced and old.
    pub orphan_pages: usize,
    /// Files deleted for being unreferenced and old.
    pub orphan_files: usize,
    /// Pins that were holding roots.
    pub pins_active: usize,
    /// Expired pins removed.
    pub pins_expired: usize,
    /// False when a limit stopped the run short; running again continues.
    pub complete: bool,
    /// The requests the run made.
    pub requests: Requests,
}

/// Why a run stopped.
#[derive(Debug)]
pub enum Error {
    /// A shard's head names a root that is not in the store: nothing was deleted.
    LiveRootMissing,
    /// A limit that cannot be met safely: nothing was deleted by this step.
    Budget(&'static str),
    /// An object that must be read is not valid: nothing was deleted by this step.
    Corrupt(&'static str),
    /// A head could not be read.
    Head(String),
    /// A stored object failed its format checks.
    Format(FormatError),
    /// The store or the inventory failed; running again is safe.
    Store(StoreError),
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
impl From<pointer::Error> for Error {
    fn from(e: pointer::Error) -> Self {
        match e {
            pointer::Error::Store(e) => Self::Store(e),
            pointer::Error::Corrupt(e) => Self::Format(e),
            pointer::Error::Refused(_) | pointer::Error::Lost => Self::Corrupt("base pointer"),
        }
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
            boundary::Error::Corrupt => Self::Corrupt("boundary"),
            boundary::Error::Store(e) => Self::Store(e),
        }
    }
}

/// Collects one index's unreachable objects.
pub struct Sweeper<S> {
    io: Io<S>,
    layout: Layout,
}

struct Candidate {
    key: String,
    version: VersionToken,
    root: ManifestRoot,
}

/// The identity a listed key ends in (32 hex digits after `prefix`).
fn id_after(key: &str, prefix: &str) -> Option<Id> {
    let tail = key.strip_prefix(prefix)?;
    if tail.len() != 32 {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(tail.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(id)
}

fn track(set: &mut HashSet<Id>, id: Id, max: usize) -> Result<(), Error> {
    if set.len() >= max && !set.contains(&id) {
        return Err(Error::Budget("tracked objects"));
    }
    set.insert(id);
    Ok(())
}

impl<S: MetadataStore + Inventory> Sweeper<S> {
    /// A sweeper reading, listing and deleting through `store`.
    #[must_use]
    pub fn new(store: Arc<S>, layout: Layout, deadline: Duration) -> Self {
        Self {
            io: Io::new(store, deadline),
            layout,
        }
    }

    fn list_all(
        &self,
        prefix: &str,
        page: usize,
        mut each: impl FnMut(&Listed) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let page = page.clamp(1, 1000);
        let mut after: Option<String> = None;
        loop {
            self.io.counters.list();
            let ctx = self
                .io
                .context(prefix.len() + after.as_ref().map_or(0, String::len))?;
            let got = block_on(self.io.store.list(&ctx, prefix, after.as_deref(), page))?;
            let mut last = None;
            for o in &got.objects {
                let key = String::from_utf8(o.key.as_bytes().to_vec())
                    .map_err(|_| Error::Corrupt("listed key"))?;
                each(&Listed {
                    key: key.clone(),
                    version: o.version.clone(),
                    modified_unix_ms: o.modified_unix_ms,
                })?;
                last = Some(key);
            }
            match (got.more, last) {
                (false, _) => return Ok(()),
                (true, Some(key)) => after = Some(key),
                // A page that says more follows but names nothing would loop forever.
                (true, None) => return Err(Error::Corrupt("a listing page made no progress")),
            }
        }
    }

    fn root(&self, key: &str, index: &Id) -> Result<Option<(ManifestRoot, VersionToken)>, Error> {
        match self.io.read(key)? {
            None => Ok(None),
            Some((bytes, version)) => Ok(Some((ManifestRoot::decode_for(&bytes, index)?, version))),
        }
    }

    /// The object identities one root's pages name, or `None` when a page is already gone.
    fn files_of(
        &self,
        root: &ManifestRoot,
        shard: &ShardRef,
        index: &Id,
    ) -> Result<Option<Vec<Id>>, Error> {
        let mut out = Vec::new();
        for (i, page_ref) in root.pages.iter().enumerate() {
            let key =
                self.layout
                    .manifest_page(index, &shard.shard, shard.incarnation, &page_ref.page);
            let Some((bytes, _)) = self.io.read(&key)? else {
                return Ok(None);
            };
            out.extend(
                root.verify_page(i, &bytes)?
                    .entries
                    .iter()
                    .map(|e| e.object),
            );
        }
        Ok(Some(out))
    }

    /// Collect what is unreachable for `index`, whose shard incarnations are `shards`.
    pub fn run(&self, index: &Id, shards: &[ShardRef], cfg: &Config) -> Result<Report, Error> {
        let sweep = cfg.mode == Mode::Sweep;
        let mut report = Report::default();
        // 1. The live generation of every shard, from its head.
        let mut live: Vec<Option<(u64, String)>> = Vec::new();
        for s in shards {
            let (head, _) = block_on(read_head(&*self.io.store, &self.layout, &s.lane))
                .map_err(|e| Error::Head(format!("{e:?}")))?;
            let Some((checkpoint, _)) = pointer::named(
                &self.io,
                &self.layout,
                s.base,
                &head,
                index,
                &s.shard,
                s.incarnation,
            )?
            else {
                live.push(None);
                continue;
            };
            let key = self
                .layout
                .manifest_root(index, &s.shard, s.incarnation, &checkpoint);
            let (root, _) = self.root(&key, index)?.ok_or(Error::LiveRootMissing)?;
            live.push(Some((root.generation, key)));
        }
        // 2. Publish every boundary before any pin is read.
        for (s, l) in shards.iter().zip(&live) {
            if let Some((generation, _)) = l
                && sweep
            {
                Boundary::new(&self.layout, index, &s.shard, s.incarnation)
                    .raise(&self.io, *generation)?;
            }
        }
        // 3. Pins.
        let mut protected: HashSet<Id> = HashSet::new();
        let prefix = self.layout.pin_prefix(index);
        let mut expired: Vec<Listed> = Vec::new();
        let mut complete = true;
        self.list_all(&prefix, cfg.page, |l| {
            // A pin released (or refused and removed) since the listing no longer holds anything.
            let Some((bytes, _)) = self.io.read(&l.key)? else {
                return Ok(());
            };
            let id = id_after(&l.key, &prefix).ok_or(Error::Corrupt("pin key"))?;
            let pin = PinRecord::decode_for(&bytes, index, &id)?;
            if pin.index != *index {
                return Err(Error::Corrupt("pin names another index"));
            }
            if pin.expires_unix_ms > cfg.now_unix_ms {
                report.pins_active += 1;
                track(&mut protected, pin.manifest, cfg.max_tracked)
            } else {
                // Bounded like everything else: the rest are removed by a later run.
                if expired.len() < cfg.max_tracked {
                    expired.push(l.clone());
                } else {
                    complete = false;
                }
                Ok(())
            }
        })?;
        // 4. Roots: keep the live, the in-flight and the pinned; the rest are candidates.
        let mut files: HashSet<Id> = HashSet::new();
        let mut pages: HashSet<Id> = HashSet::new();
        let mut candidates: Vec<(usize, Candidate)> = Vec::new();
        for (n, (s, l)) in shards.iter().zip(&live).enumerate() {
            let boundary = l.as_ref().map_or(0, |(g, _)| *g);
            let prefix = self.layout.manifest_prefix(index, &s.shard, s.incarnation);
            self.list_all(&prefix, cfg.page, |listed| {
                let Some(id) = id_after(&listed.key, &prefix) else {
                    return Ok(());
                };
                let Some((root, version)) = self.root(&listed.key, index)? else {
                    return Ok(());
                };
                if root.generation >= boundary || protected.contains(&id) {
                    report.roots_kept += 1;
                    for p in &root.pages {
                        track(&mut pages, p.page, cfg.max_tracked)?;
                    }
                    let names = self
                        .files_of(&root, s, index)?
                        .ok_or(Error::Corrupt("a kept root names a missing page"))?;
                    for object in names {
                        track(&mut files, object, cfg.max_tracked)?;
                    }
                } else if candidates.len() < cfg.max_roots {
                    candidates.push((
                        n,
                        Candidate {
                            key: listed.key.clone(),
                            version,
                            root,
                        },
                    ));
                } else {
                    complete = false;
                }
                Ok(())
            })?;
        }
        // 5. What only candidates name.
        let mut dead_files: HashSet<Id> = HashSet::new();
        let mut dead_pages: HashSet<Id> = HashSet::new();
        for (n, c) in &candidates {
            let Some(s) = shards.get(*n) else { continue };
            for p in &c.root.pages {
                if !pages.contains(&p.page) {
                    track(&mut dead_pages, p.page, cfg.max_tracked)?;
                }
            }
            if let Some(names) = self.files_of(&c.root, s, index)? {
                for object in names {
                    if !files.contains(&object) {
                        track(&mut dead_files, object, cfg.max_tracked)?;
                    }
                }
            }
        }
        // 6. Delete. The live roots must still exist, or the view this ran on was wrong.
        for (_, key) in live.iter().flatten() {
            if !self.io.exists(key)? {
                return Err(Error::LiveRootMissing);
            }
        }
        let mut left = cfg.max_deletes;
        let mut spend = |what: &mut usize, l: &Listed, io: &Io<S>| -> Result<bool, Error> {
            if left == 0 {
                return Ok(false);
            }
            left -= 1;
            *what += 1;
            if sweep {
                io.remove(&l.key, &l.version)?;
            }
            Ok(true)
        };
        let old = |l: &Listed| {
            cfg.orphan_grace_ms
                .is_some_and(|g| l.modified_unix_ms.saturating_add(g) < cfg.now_unix_ms)
        };
        let prefix = self.layout.payload_prefix(index);
        let (mut swept, mut orphans) = (0, 0);
        self.list_all(&prefix, cfg.page, |l| {
            let Some(id) = id_after(&l.key, &prefix) else {
                return Ok(());
            };
            let dead = dead_files.contains(&id);
            if (dead || (old(l) && !files.contains(&id)))
                && !spend(if dead { &mut swept } else { &mut orphans }, l, &self.io)?
            {
                complete = false;
            }
            Ok(())
        })?;
        report.files_swept = swept;
        report.orphan_files = orphans;
        let (mut swept, mut orphans) = (0, 0);
        for s in shards {
            let prefix = self.layout.page_prefix(index, &s.shard, s.incarnation);
            self.list_all(&prefix, cfg.page, |l| {
                let Some(id) = id_after(&l.key, &prefix) else {
                    return Ok(());
                };
                let dead = dead_pages.contains(&id);
                if (dead || (old(l) && !pages.contains(&id)))
                    && !spend(if dead { &mut swept } else { &mut orphans }, l, &self.io)?
                {
                    complete = false;
                }
                Ok(())
            })?;
        }
        report.pages_swept = swept;
        report.orphan_pages = orphans;
        // Roots go last, and only when every file and page they name is gone.
        if complete {
            for (_, c) in &candidates {
                let l = Listed {
                    key: c.key.clone(),
                    version: c.version.clone(),
                    modified_unix_ms: 0,
                };
                if !spend(&mut report.roots_swept, &l, &self.io)? {
                    complete = false;
                    break;
                }
            }
        }
        for l in &expired {
            if spend(&mut report.pins_expired, l, &self.io)? {
                continue;
            }
            complete = false;
            break;
        }
        report.complete = complete;
        report.requests = self.io.counters.snapshot();
        Ok(report)
    }
}
