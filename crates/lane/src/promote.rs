//! Hot-index promotion: moving one index out of a shared lane into a lane of its own.
//!
//! Three durable steps, each resumable by running `promote` again after a crash:
//! 1. **Seal** `X` in the source lane's ordered log. From that position the source accepts nothing
//!    more for `X`, whatever any cached descriptor says; the seal is an *order*, not a membership
//!    view.
//! 2. **Create** the target lane with `X` as its only member, at a domain derived from the source,
//!    `X` and the seal position, so a repeat finds it instead of making another.
//! 3. **Leave**: remove `X` from the source descriptor, freeing its slot under the ceiling.
//!
//! `X`'s data is the records the source holds for it below the seal (readable with
//! [`crate::read_log`]) followed by the target lane's log. The routing change that makes the target
//! authoritative is the structural head's job (M03.05); until it is published `X` has **no**
//! writer, which is temporary unavailability and never two authorities.
use hsearch_format::{Id, KeyIdentity};
use hsearch_object::MetadataStore;

use crate::log::LaneWriter;
use crate::members::{LaneId, create_lane, dedicated_group, read_descriptor};
use crate::{Error, Result};

/// The result of a completed promotion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Promotion {
    /// The index that moved.
    pub index: Id,
    /// The lane it left.
    pub source: LaneId,
    /// The source position of its seal: its records below this position are in the source.
    pub sealed_at: u64,
    /// The lane it now writes through (once routing is published).
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
        let mut origin = b"promoted".to_vec();
        origin.extend_from_slice(&self.id().domain);
        let (domain, group_id) = dedicated_group(&policy, &index, &origin);
        let target = LaneId::new(domain);
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
        self.remove_member(&index).await?;
        Ok(Promotion {
            index,
            source: *self.id(),
            sealed_at,
            target,
        })
    }
}
