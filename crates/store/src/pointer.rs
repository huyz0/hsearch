//! The per-index base pointer in the store: read it, replace it under the successor rule, and name
//! which source a reader takes its base from.
//!
//! A lane's structural head names one checkpoint, which is right for a lane with one index and
//! wrong for a shared lane. [`BaseSource::Pointer`] reads the shard's own conditional object
//! instead; [`BaseSource::Head`] keeps the head's checkpoint, so a lane that has no pointer behaves
//! as before.
use hsearch_format::PointerRefusal as Refusal;
use hsearch_format::{BasePointer, FormatError, Id, Layout, StructuralHead};
use hsearch_object::{Error as StoreError, MetadataStore, Precondition, VersionToken};

use crate::io::{Io, request_id};

/// Where a reader takes the base it recovers from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BaseSource {
    /// The checkpoint the lane's structural head names (a lane of one index).
    #[default]
    Head,
    /// The shard's own base pointer (a lane shared by several indexes).
    Pointer,
}

/// Why a pointer could not be read or replaced.
#[derive(Debug)]
pub enum Error {
    /// The stored pointer is not a valid pointer for this shard.
    Corrupt(FormatError),
    /// The replacement breaks the successor rule.
    Refused(Refusal),
    /// Another writer replaced the pointer first; nothing changed.
    Lost,
    /// The store failed.
    Store(StoreError),
}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

/// A pointer and the version it was read at.
pub type Read = (BasePointer, VersionToken);

/// Read the pointer of one shard incarnation: one HEAD and one GET, none when no base is named yet.
pub(crate) fn read<S: MetadataStore>(
    io: &Io<S>,
    layout: &Layout,
    index: &Id,
    shard: &Id,
    incarnation: u64,
) -> Result<Option<Read>, Error> {
    let key = layout.base_pointer(index, shard, incarnation);
    match io.read(&key)? {
        None => Ok(None),
        Some((bytes, version)) => BasePointer::decode_for(&bytes, index, shard, incarnation)
            .map(|p| Some((p, version)))
            .map_err(Error::Corrupt),
    }
}

/// Replace the pointer read as `current` (none: create it) by `next`, on a lane whose head covers
/// slots below `head_cover`. The successor rule is checked first and the store enforces it again by
/// compare-and-swap on the version read, so a paused writer's older view changes nothing.
pub(crate) fn write<S: MetadataStore>(
    io: &Io<S>,
    layout: &Layout,
    next: &BasePointer,
    current: Option<&Read>,
    head_cover: u64,
) -> Result<(), Error> {
    next.validate_successor(current.map(|(p, _)| p), head_cover)
        .map_err(Error::Refused)?;
    let mut bytes = Vec::new();
    next.encode(&mut bytes).map_err(Error::Corrupt)?;
    let key = layout.base_pointer(&next.index, &next.shard, next.incarnation);
    let (precondition, version) = match current {
        None => (Precondition::Absent, &[][..]),
        Some((_, v)) => (Precondition::Matches(v.clone()), v.as_bytes()),
    };
    let identity = request_id(&[
        b"base-pointer",
        key.as_bytes(),
        version,
        next.token.as_bytes(),
    ])?;
    match io.write(&key, &bytes, precondition, identity) {
        Ok(()) => Ok(()),
        Err(StoreError::Conflict) => Err(Error::Lost),
        Err(e) => Err(Error::Store(e)),
    }
}

/// What `source` names for a shard whose lane head is `head`: the manifest root and, for a pointer,
/// the pointer itself. None before the first base.
pub(crate) fn named<S: MetadataStore>(
    io: &Io<S>,
    layout: &Layout,
    source: BaseSource,
    head: &StructuralHead,
    index: &Id,
    shard: &Id,
    incarnation: u64,
) -> Result<Option<(Id, Option<BasePointer>)>, Error> {
    match source {
        BaseSource::Head => Ok(head.checkpoint.map(|m| (m, None))),
        BaseSource::Pointer => {
            Ok(read(io, layout, index, shard, incarnation)?.map(|(p, _)| (p.manifest, Some(p))))
        }
    }
}
