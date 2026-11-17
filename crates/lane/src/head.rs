//! The structural head of a commit domain (M03.05): one conditional object that binds authority.
//!
//! It changes on a structural transition (a new writer epoch, a checkpoint, a schema, publication
//! or membership change), **never per data append**: an append is one conditional create of the
//! next slot and does not touch the head. A transition names the version it read; a lost race is
//! reported as [`Advance::Lost`] and changes nothing, and the content rules in
//! [`StructuralHead::validate_successor`] keep every epoch, generation and the cover from falling
//! even for a writer holding an old copy. A head is never deleted: nothing here removes one.
use hsearch_format::{Durability, EpochEntry, Id, Layout, StructuralHead, TailLocator};
use hsearch_object::{MetadataStore, Precondition, VersionToken};

use crate::io::{Written, fresh_token, key, mutation, read_object, request_id, write};
use crate::members::LaneId;
use crate::{Error, Result};

/// Longest head read: the format's body bound plus the envelope.
const HEAD_READ_BOUND: u64 = 72 * 1024;

/// One structural transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Change {
    /// A writer establishes itself: the next epoch, in `activation` (which may only rise), with the
    /// fence it created at `fence`. The tail locator moves above that fence.
    Establish {
        /// The activation the new epoch writes in.
        activation: u64,
        /// Its lane.
        lane: u32,
        /// The ordinal of the fence the epoch created.
        fence: u64,
    },
    /// A checkpoint covering every slot below `cover` is published under `root`. Raising the cover
    /// is what lets a reader skip, and a reclaimer remove, the slots below it.
    Checkpoint {
        /// Slots below this are in the checkpoint.
        cover: u64,
        /// The checkpoint root's identity, never all zero.
        root: Id,
    },
    /// A rewritten base that covers exactly what the current checkpoint covers (a compaction)
    /// replaces the checkpoint root: the cover stays and the publication generation rises by one,
    /// which is what tells a validator the root changed on purpose. Needs a checkpoint to replace.
    Replace {
        /// The replacement root's identity, never all zero.
        root: Id,
    },
    /// The schema generation rises by one.
    Schema,
    /// The publication generation rises by one.
    Publication,
    /// The membership epoch rises by one.
    Membership,
}

/// How a transition ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Advance {
    /// The store holds exactly this successor, at this version.
    Done(StructuralHead, VersionToken),
    /// Another transition won: re-read and decide again. Nothing was changed.
    Lost,
}

/// Create the head of `id`'s incarnation by conditional create. [`Error::Exists`] if it is there:
/// a head is never recreated while its incarnation exists.
pub async fn create_head<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    mode: Durability,
) -> Result<(StructuralHead, VersionToken)> {
    let head = StructuralHead {
        generation: 1,
        domain: id.domain,
        incarnation: id.incarnation,
        activation: id.activation,
        mode,
        writer_epoch: 0,
        membership_epoch: 0,
        schema_generation: 1,
        publication_generation: 1,
        cover: 0,
        checkpoint: None,
        tail: TailLocator {
            lane: id.lane,
            ordinal: 0,
        },
        inventory: Vec::new(),
        token: fresh_token(),
    };
    let mut bytes = Vec::new();
    head.encode(&mut bytes)?;
    let k = layout.structural_head(&id.domain, id.incarnation);
    let request = mutation(
        &request_id("sh", &id.domain, head.generation, &head.token),
        &k,
        &bytes,
        Precondition::Absent,
    )?;
    match write(store, &request, false).await {
        Written::Done(receipt) => Ok((head, receipt.object.version)),
        Written::Occupied => Err(Error::Exists),
        Written::Refused(error) => Err(error.into()),
        Written::Unknown => Err(Error::Contended),
    }
}

/// Read the head and the version a transition must name, or [`Error::NoHead`].
pub async fn read_head<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
) -> Result<(StructuralHead, VersionToken)> {
    let k = key(&layout.structural_head(&id.domain, id.incarnation))?;
    let (bytes, version) = read_object(store, &k, HEAD_READ_BOUND)
        .await?
        .ok_or(Error::NoHead)?;
    Ok((
        StructuralHead::decode_at(bytes.as_bytes(), &id.domain, id.incarnation)?,
        version,
    ))
}

fn successor(current: &StructuralHead, change: Change) -> Result<StructuralHead> {
    let mut next = current.clone();
    next.generation = current
        .generation
        .checked_add(1)
        .ok_or(Error::Invalid("head generation"))?;
    next.token = fresh_token();
    match change {
        Change::Establish {
            activation,
            lane,
            fence,
        } => {
            let above = fence
                .checked_add(1)
                .ok_or(Error::Invalid("fence ordinal"))?;
            next.writer_epoch = current
                .writer_epoch
                .checked_add(1)
                .ok_or(Error::Invalid("writer epoch"))?;
            next.inventory.push(EpochEntry {
                epoch: next.writer_epoch,
                activation,
                lane,
                fence,
            });
            next.tail = TailLocator {
                lane,
                ordinal: if activation == current.activation {
                    current.tail.ordinal.max(above)
                } else {
                    above
                },
            };
            next.activation = activation;
        }
        Change::Checkpoint { cover, root } => {
            if root == [0; 16] {
                return Err(Error::Invalid("a checkpoint root cannot be the zero id"));
            }
            next.cover = cover;
            next.checkpoint = Some(root);
            next.tail.ordinal = current.tail.ordinal.max(cover);
        }
        Change::Replace { root } => {
            if root == [0; 16] || current.checkpoint.is_none() {
                return Err(Error::Invalid(
                    "a replacement needs a checkpoint and a real root",
                ));
            }
            next.checkpoint = Some(root);
            next.publication_generation = bump(current.publication_generation)?;
        }
        Change::Schema => next.schema_generation = bump(current.schema_generation)?,
        Change::Publication => next.publication_generation = bump(current.publication_generation)?,
        Change::Membership => next.membership_epoch = bump(current.membership_epoch)?,
    }
    current.validate_successor(&next)?;
    Ok(next)
}

fn bump(value: u64) -> Result<u64> {
    value.checked_add(1).ok_or(Error::Invalid("generation"))
}

/// Apply `change` to `current` (read at `version`) by compare-and-swap. The successor is checked
/// against [`StructuralHead::validate_successor`] before any request, so a regression is refused
/// without touching the store. A lost reply is resolved by the write's identity; if that cannot
/// be established the result is [`Error::Contended`] and the caller **must re-read** before it
/// acts, because the successor may or may not have landed.
pub async fn advance_head<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    current: &StructuralHead,
    version: &VersionToken,
    change: Change,
) -> Result<Advance> {
    let next = successor(current, change)?;
    let mut bytes = Vec::new();
    next.encode(&mut bytes)?;
    let k = layout.structural_head(&id.domain, id.incarnation);
    let request = mutation(
        &request_id("sh", &id.domain, next.generation, &next.token),
        &k,
        &bytes,
        Precondition::Matches(version.clone()),
    )?;
    match write(store, &request, true).await {
        Written::Done(receipt) => Ok(Advance::Done(next, receipt.object.version)),
        Written::Occupied => Ok(Advance::Lost),
        Written::Refused(error) => Err(error.into()),
        Written::Unknown => Err(Error::Contended),
    }
}
