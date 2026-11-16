//! Placement: which lane a new index joins, found by derived keys and CAS, never by LIST.
use std::collections::HashMap;

use hsearch_format::{DEFAULT_LANE_GROUP, Id, KeyIdentity, LaneDescriptor, Layout, PolicyTuple};
use hsearch_object::{MetadataStore, VersionToken};

use crate::members::{
    LaneId, create_lane, dedicated_group, join_members, lane_exists, read_descriptor, shared_group,
};
use crate::{Error, Result};

/// Most distinct policies a placer remembers; beyond it the oldest is dropped, which only costs a
/// re-discovery (a few HEADs), never a wrong answer.
const MAX_CACHED_POLICIES: usize = 256;
/// Groups tried for one placement before giving up.
const MAX_GROUP_STEPS: u32 = 64;
/// Groups are created in order, so the last one that exists is found by doubling and bisecting.
const MAX_GROUP_SEQ: u64 = 1 << 32;

/// Where an index was placed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Placement {
    /// The lane the index writes through.
    pub lane: LaneId,
    /// The group sequence within its policy (0 for a dedicated lane).
    pub group: u64,
    /// Whether this call created the lane.
    pub created: bool,
}

struct Cursor {
    group: u64,
    /// The descriptor and version last seen for `group`, so a join is one CAS with no read.
    seen: Option<(LaneDescriptor, VersionToken)>,
}

/// Assigns indexes to lanes. One placer per node; placers on different nodes race safely because
/// every membership change is a CAS on the lane's descriptor.
pub struct Placer<S> {
    store: std::sync::Arc<S>,
    layout: Layout,
    ceiling: u32,
    cursors: HashMap<Vec<u8>, Cursor>,
}

impl<S: MetadataStore> Placer<S> {
    /// A placer with the M03.09 default group ceiling (1,000).
    #[must_use]
    pub fn new(store: std::sync::Arc<S>, layout: Layout) -> Self {
        Self::with_ceiling(store, layout, DEFAULT_LANE_GROUP)
    }

    /// A placer with an explicit group ceiling, at most the format's ceiling.
    #[must_use]
    pub fn with_ceiling(store: std::sync::Arc<S>, layout: Layout, ceiling: u32) -> Self {
        Self {
            store,
            layout,
            ceiling,
            cursors: HashMap::new(),
        }
    }

    /// Place `index` under `policy`. A BYOK index gets its own lane (M03.09 decision 2); any other
    /// joins the policy's current group, opening the next group when it is full. Placing an index
    /// that is already a member of the open group returns its lane and writes nothing.
    ///
    /// For an index that has **no lane yet**: which lane an index lives in is recorded by the
    /// catalog (M03.05), and placing an index that already lives in an earlier group would make it
    /// a member of two.
    pub async fn place(&mut self, policy: &PolicyTuple, index: Id) -> Result<Placement> {
        match policy.key {
            KeyIdentity::Byok(_) => self.place_dedicated(policy, index).await,
            KeyIdentity::DeploymentDefault => self.place_shared(policy, index).await,
        }
    }

    async fn place_dedicated(&mut self, policy: &PolicyTuple, index: Id) -> Result<Placement> {
        let (domain, group_id) = dedicated_group(policy, &index, b"byok");
        let lane = LaneId::new(domain);
        match create_lane(
            &*self.store,
            &self.layout,
            &lane,
            policy,
            group_id,
            1,
            &[index],
        )
        .await
        {
            Ok(_) => Ok(Placement {
                lane,
                group: 0,
                created: true,
            }),
            Err(Error::Exists) => {
                let (descriptor, _) = read_descriptor(&*self.store, &self.layout, &lane).await?;
                if descriptor.members.binary_search(&index).is_ok() {
                    Ok(Placement {
                        lane,
                        group: 0,
                        created: false,
                    })
                } else {
                    Err(Error::Exists)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// The last group of `policy` that exists (`None` if group 0 does not): doubling, then
    /// bisection, `2 log2 n` HEADs and no LIST.
    async fn last_group(&self, policy: &PolicyTuple) -> Result<Option<u64>> {
        let exists = |group: u64| {
            let lane = LaneId::new(shared_group(policy, group).0);
            let (store, layout) = (&*self.store, &self.layout);
            async move { lane_exists(store, layout, &lane).await }
        };
        if !exists(0).await? {
            return Ok(None);
        }
        let (mut low, mut high) = (0u64, 1u64);
        while exists(high).await? {
            low = high;
            high = high.saturating_mul(2);
            if high >= MAX_GROUP_SEQ {
                return Err(Error::Capacity);
            }
        }
        while high - low > 1 {
            let mid = low + (high - low) / 2;
            if exists(mid).await? {
                low = mid;
            } else {
                high = mid;
            }
        }
        Ok(Some(low))
    }

    async fn place_shared(&mut self, policy: &PolicyTuple, index: Id) -> Result<Placement> {
        let mut tag = Vec::new();
        tag.extend_from_slice(&shared_group(policy, 0).0);
        if !self.cursors.contains_key(&tag) {
            if self.cursors.len() >= MAX_CACHED_POLICIES {
                self.cursors.clear();
            }
            let group = self.last_group(policy).await?.unwrap_or(0);
            self.cursors
                .insert(tag.clone(), Cursor { group, seen: None });
        }
        for _ in 0..MAX_GROUP_STEPS {
            let (group, seen) = match self.cursors.get_mut(&tag) {
                Some(cursor) => (cursor.group, cursor.seen.take()),
                None => return Err(Error::Capacity),
            };
            let (domain, group_id) = shared_group(policy, group);
            let lane = LaneId::new(domain);
            let step = match seen {
                Some(known) => Some(known),
                None => match read_descriptor(&*self.store, &self.layout, &lane).await {
                    Ok(found) => Some(found),
                    Err(Error::NoLane) => None,
                    Err(error) => return Err(error),
                },
            };
            let Some((descriptor, version)) = step else {
                match create_lane(
                    &*self.store,
                    &self.layout,
                    &lane,
                    policy,
                    group_id,
                    self.ceiling,
                    &[index],
                )
                .await
                {
                    Ok(known) => {
                        self.remember(&tag, group, Some(known));
                        return Ok(Placement {
                            lane,
                            group,
                            created: true,
                        });
                    }
                    // Another node created it first: read it and decide again.
                    Err(Error::Exists) => continue,
                    Err(error) => return Err(error),
                }
            };
            if descriptor.members.binary_search(&index).is_ok() {
                self.remember(&tag, group, Some((descriptor, version)));
                return Ok(Placement {
                    lane,
                    group,
                    created: false,
                });
            }
            if descriptor.can_host(policy) {
                match join_members(
                    &*self.store,
                    &self.layout,
                    &lane,
                    policy,
                    &[index],
                    Some((descriptor, version)),
                )
                .await
                {
                    Ok(known) => {
                        self.remember(&tag, group, Some(known));
                        return Ok(Placement {
                            lane,
                            group,
                            created: false,
                        });
                    }
                    // Filled while we raced: move on.
                    Err(Error::Exists) => self.remember(&tag, group + 1, None),
                    Err(error) => return Err(error),
                }
            } else {
                self.remember(&tag, group + 1, None);
            }
        }
        Err(Error::Contended)
    }

    fn remember(&mut self, tag: &[u8], group: u64, seen: Option<(LaneDescriptor, VersionToken)>) {
        if let Some(cursor) = self.cursors.get_mut(tag) {
            cursor.group = group;
            cursor.seen = seen;
        }
    }
}
