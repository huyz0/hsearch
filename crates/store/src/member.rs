//! Membership of an index in a shared lane: creating joins it, deleting seals and removes it, and
//! the lane's slots are reclaimed to the cover its members allow.
//!
//! The name catalog is where an index's incarnation is bound and kept: a name's generation is its
//! incarnation, so a name created again after a delete gets a higher one and with it another
//! identity ([`crate::served::index_id_in`]), and the deleted name's tombstone keeps the old
//! identity from being bound again. Every step can be repeated: a create or delete that stopped
//! half way is finished by calling it again.
use hsearch_format::{Id, Layout, PolicyTuple};
use hsearch_lane::{
    Catalog, Error as LaneError, LaneId, Reclaimed, Resolution, block_on, join_members,
};
use hsearch_object::{LargeObjectWriter, MetadataStore};

use crate::cover::SharedCover;
use crate::log::Log;
use crate::publish::{Error as PublishError, Publisher};
use crate::served::index_id_in;

/// An index joined to a lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Member {
    /// The identity the catalog binds the name to.
    pub index: Id,
    /// The name's incarnation: the catalog's generation of the name, which rises with every bind
    /// and every delete, so a recreate never repeats one.
    pub incarnation: u64,
}

/// What stopped a membership change.
#[derive(Debug)]
pub enum Error {
    /// The lane or the catalog refused or failed.
    Lane(LaneError),
    /// The head could not be read or moved.
    Publish(PublishError),
    /// No index is bound to the name.
    NotFound,
}

impl From<LaneError> for Error {
    fn from(e: LaneError) -> Self {
        Self::Lane(e)
    }
}

impl From<PublishError> for Error {
    fn from(e: PublishError) -> Self {
        Self::Publish(e)
    }
}

/// Bind `name` in the catalog under its next incarnation and join the index to `lane`. A name
/// that is live is the one bound by an earlier call (its join is finished if it stopped short),
/// so a repeated create returns the same member.
///
/// # Errors
/// [`Error::Lane`] with [`LaneError::Exists`] if the lane cannot host another member; otherwise
/// what the catalog or the descriptor change returned. The call may be repeated.
pub fn create<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    catalog: &Catalog,
    lane: &LaneId,
    policy: &PolicyTuple,
    name: &str,
) -> Result<Member, Error> {
    let member = match block_on(catalog.resolve(store, layout, name))? {
        Resolution::Live {
            index,
            name_generation,
        } => Member {
            index,
            incarnation: name_generation,
        },
        found => {
            let incarnation = match found {
                Resolution::Deleted {
                    name_generation, ..
                } => name_generation.saturating_add(1),
                _ => 1,
            };
            let index = index_id_in(name, incarnation);
            block_on(catalog.create(store, layout, name, index))?;
            Member { index, incarnation }
        }
    };
    block_on(join_members(
        store,
        layout,
        lane,
        policy,
        &[member.index],
        None,
    ))?;
    Ok(member)
}

/// Delete the index bound to `name`: the catalog leaves the tombstone first, so a create of the
/// name from then on is another identity, and then the lane seals the index at `last_seq` (the
/// last sequence the lane holds for it) and drops its membership, and the pins forget it. Calling
/// it again for a name that is deleted finishes whatever stopped; a delete whose lane step is
/// repeated keeps the seal it already made, so `last_seq` of the first attempt that sealed is the
/// one that stands.
///
/// # Errors
/// [`Error::NotFound`] if no index was ever bound to the name; otherwise what stopped a step. The
/// call may be repeated. Until the lane step finishes the index still pins the cover, and a node
/// that restarts in between finds it in the tail and settles it.
pub fn delete<S: MetadataStore, L: Log>(
    store: &S,
    layout: &Layout,
    catalog: &Catalog,
    log: &mut L,
    pins: &SharedCover,
    name: &str,
    last_seq: u64,
) -> Result<(), Error> {
    let index = match block_on(catalog.resolve(store, layout, name))? {
        Resolution::Live { index, .. } => {
            block_on(catalog.delete(store, layout, name, &index))?;
            index
        }
        Resolution::Deleted { last, .. } => last,
        Resolution::Absent => return Err(Error::NotFound),
    };
    log.leave(index, last_seq)?;
    pins.forget(&index);
    Ok(())
}

/// Move the head's cover to what the pinned set allows and remove the slots below it: one head
/// read and, when it moves, one head write, then one request per slot removed. Nothing is asked
/// of any index.
///
/// # Errors
/// [`Error`] when the head or a slot could not be handled; the cover stays where it was.
pub fn reclaim<S, L>(
    publisher: &Publisher<S>,
    pins: &SharedCover,
    log: &mut L,
) -> Result<Reclaimed, Error>
where
    S: MetadataStore + LargeObjectWriter + 'static,
    L: Log,
{
    publisher.advance_cover(pins)?;
    Ok(log.reclaim()?)
}

/// Ask each of `indexes` to publish a base through `publish`, which says whether the base is
/// named. An index that did not publish (its host failed, was evicted, or is not open) stays
/// pinned, so the cover waits for it. Returns how many published.
fn settle(indexes: &[Id], mut publish: impl FnMut(&Id) -> bool) -> usize {
    indexes.iter().filter(|index| publish(index)).count()
}

/// After a commit on the lane: publish the indexes whose pin has held the span bound, so an index
/// that stopped writing does not hold the cover.
pub fn settle_overdue(pins: &SharedCover, publish: impl FnMut(&Id) -> bool) -> usize {
    settle(&pins.overdue(), publish)
}

/// When a lane opens: publish a base for every index its tail left pinned, before the lane
/// serves. The caller opens each index (which replays its tail) and publishes it.
pub fn settle_tail(pins: &SharedCover, publish: impl FnMut(&Id) -> bool) -> usize {
    settle(&pins.pinned_indexes(), publish)
}
