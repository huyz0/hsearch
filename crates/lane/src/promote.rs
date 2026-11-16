//! Hot-index promotion: moving one index out of a shared lane into a lane of its own.
//!
//! Four durable steps, each resumable by running `promote` again after a crash:
//! 1. **Seal** `X` in the source lane's ordered log. From that position the source accepts nothing
//!    more for `X`, whatever any cached descriptor says; the seal is an *order*, not a membership
//!    view.
//! 2. **Create** the target lane with `X` as its only member, at a domain derived from the source,
//!    `X` and the seal position, so a repeat finds it instead of making another.
//! 3. **Establish** the target's structural head (create-only, activation 1): the record a writer
//!    opens the target from ([`crate::LaneWriter::open_with_head`]) and that [`route`] reads.
//! 4. **Leave**: remove `X` from the source descriptor, freeing its slot under the ceiling.
//!
//! `X`'s data is the records the source holds for it below the seal (readable with
//! [`crate::read_log`]) followed by the target lane's log.
//!
//! **Routing is derived, not stored.** The target's domain is a function of the source domain, the
//! policy and `X`, so [`route`] finds it by key: while `X` is in the source descriptor it routes to
//! the source; once the target's head exists and `X` has left, it routes to the target; between
//! the two (sealed, not yet established) or for an index that never lived here it routes nowhere.
//! Between establish and leave the descriptor still lists `X`, so [`route`] says `Shared` while
//! the source already refuses it with `Sealed`: the caller treats that refusal as "route again".
//! `X` then has **no** writer, which is temporary unavailability and never two authorities: the
//! seal is an order in the source log, so a router holding an old descriptor still cannot commit
//! `X` to the source.
use hsearch_format::{Id, KeyIdentity, Layout};
use hsearch_object::MetadataStore;

use crate::head::{create_head, read_head};
use crate::log::LaneWriter;
use crate::members::{LaneId, create_lane, dedicated_group, read_descriptor};
use crate::{Error, Result};

/// Where an index's writes go.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Route {
    /// The index is a member of the shared lane it was looked up in.
    Shared(LaneId),
    /// The index left its source for a lane of its own whose head is established.
    Promoted {
        /// The target lane at the activation its head names.
        lane: LaneId,
        /// The head's newest writer epoch (0 before any writer opened it).
        writer_epoch: u64,
    },
    /// Nothing is authorised to commit this index right now: it is not in the source and its
    /// promoted lane has no head (a promotion not finished, or an index that never lived here).
    Unrouted,
}

/// The lane domain a promotion of `index` out of `source` moves it to, a function of the source
/// domain, the source's policy and the index alone.
fn target_of(policy: &hsearch_format::PolicyTuple, source: &LaneId, index: &Id) -> (LaneId, Id) {
    let mut origin = b"promoted".to_vec();
    origin.extend_from_slice(&source.domain);
    let (domain, group) = dedicated_group(policy, index, &origin);
    (LaneId::new(domain), group)
}

/// Where `index`'s writes go, given the shared lane it was last known in. Reads the source
/// descriptor (1 HEAD, 1 GET) and, if the index has left it, the target's head (1 HEAD, 1 GET).
/// No LIST, no write.
pub async fn route<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    source: &LaneId,
    index: &Id,
) -> Result<Route> {
    let (descriptor, _) = read_descriptor(store, layout, source).await?;
    if descriptor.members.contains(index) {
        return Ok(Route::Shared(*source));
    }
    let (target, _) = target_of(&descriptor.policy, source, index);
    match read_head(store, layout, &target).await {
        Ok((head, _)) => Ok(Route::Promoted {
            lane: LaneId {
                domain: target.domain,
                incarnation: head.incarnation,
                activation: head.activation,
                lane: head.tail.lane,
            },
            writer_epoch: head.writer_epoch,
        }),
        Err(Error::NoHead) => Ok(Route::Unrouted),
        Err(error) => Err(error),
    }
}

/// The result of a completed promotion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Promotion {
    /// The index that moved.
    pub index: Id,
    /// The lane it left.
    pub source: LaneId,
    /// The source position of its seal: its records below this position are in the source.
    pub sealed_at: u64,
    /// The lane it now writes through; [`route`] reports it once its head exists.
    pub target: LaneId,
}

impl<S: MetadataStore> LaneWriter<S> {
    /// Promote `index` to a lane of its own. `last_seq` is the index's last sequence in this lane
    /// (recorded in the seal). Safe to repeat from any crash point; a writer reopened after a crash
    /// between the steps sees the seal in its replay and resumes at step 2.
    pub async fn promote(&mut self, index: Id, last_seq: u64) -> Result<Promotion> {
        if self.is_poisoned() {
            return Err(Error::Poisoned);
        }
        let policy = self.policy().clone();
        if matches!(policy.key, KeyIdentity::Byok(_)) {
            return Err(Error::Invalid("a BYOK index already has its own lane"));
        }
        let sealed_at = match self.sealed_at(&index) {
            Some(at) => at,
            None => self.seal_index(index, last_seq).await?.ordinal,
        };
        // One target per (source lane, index): a repeat after a crash finds it.
        let (target, group_id) = target_of(&policy, self.id(), &index);
        match create_lane(
            self.store_ref(),
            self.layout_ref(),
            &target,
            &policy,
            group_id,
            1,
            &[index],
        )
        .await
        {
            Ok(_) => {}
            Err(Error::Exists) => {
                let (descriptor, _) =
                    read_descriptor(self.store_ref(), self.layout_ref(), &target).await?;
                if descriptor.members != [index] {
                    return Err(Error::Exists);
                }
            }
            Err(error) => return Err(error),
        }
        match create_head(
            self.store_ref(),
            self.layout_ref(),
            &target,
            policy.durability,
        )
        .await
        {
            Ok(_) | Err(Error::Exists) => {}
            Err(error) => return Err(error),
        }
        self.remove_member(&index).await?;
        Ok(Promotion {
            index,
            source: *self.id(),
            sealed_at,
            target,
        })
    }
}
