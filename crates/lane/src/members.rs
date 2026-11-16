//! Lane identity and membership: the descriptor object, written only on a membership change
//! (never per append), and the derived identities that let a node find a lane without a LIST.
use hsearch_format::{
    Durability, Id, KeyIdentity, LaneDescriptor, Layout, MAX_LANE_GROUP, PolicyTuple,
};
use hsearch_object::{MetadataStore, Precondition, VersionToken};
use sha2::{Digest as _, Sha256};

use crate::io::{Written, fresh_token, key, mutation, read_object, request_id, write};
use crate::{Error, Result};

/// Where a lane lives: its domain, and the incarnation, activation and lane number its keys carry.
///
/// M03.08 runs one ordered log per lane at activation 1, lane 0; the other fields are the format's
/// room for structural re-activation (M03.05) and are carried, never invented here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneId {
    /// Opaque lane domain.
    pub domain: Id,
    /// Incarnation of the domain (a recreated lane starts a new key space).
    pub incarnation: u64,
    /// Activation of the log within the incarnation.
    pub activation: u64,
    /// Lane number within the activation.
    pub lane: u32,
}

impl LaneId {
    /// The first incarnation and activation of `domain`, lane 0.
    #[must_use]
    pub fn new(domain: Id) -> Self {
        Self {
            domain,
            incarnation: 1,
            activation: 1,
            lane: 0,
        }
    }
}

fn policy_bytes(policy: &PolicyTuple, out: &mut Vec<u8>) {
    match &policy.key {
        KeyIdentity::DeploymentDefault => out.push(1),
        KeyIdentity::Byok(id) => {
            out.push(2);
            out.extend_from_slice(&(id.len() as u32).to_le_bytes());
            out.extend_from_slice(id.as_bytes());
        }
    }
    out.extend_from_slice(&(policy.residency.len() as u32).to_le_bytes());
    out.extend_from_slice(policy.residency.as_bytes());
    out.push(match policy.durability {
        Durability::Object => 1,
        Durability::Quorum => 2,
    });
    out.extend_from_slice(&policy.format_version.to_le_bytes());
}

/// The derived identity of a shared group: its domain and its group id, both functions of the
/// policy tuple and the group's sequence number, so a node finds group `n` of a policy by key alone.
#[must_use]
pub fn shared_group(policy: &PolicyTuple, group: u64) -> (Id, Id) {
    derive(policy, b"shared", &group.to_le_bytes())
}

/// The derived identity of a lane that holds one index (a BYOK lane, or a promoted hot index).
/// `origin` names why it exists so a BYOK lane and a promoted lane of the same index differ.
#[must_use]
pub fn dedicated_group(policy: &PolicyTuple, index: &Id, origin: &[u8]) -> (Id, Id) {
    let mut tail = origin.to_vec();
    tail.extend_from_slice(index);
    derive(policy, b"dedicated", &tail)
}

fn derive(policy: &PolicyTuple, label: &[u8], tail: &[u8]) -> (Id, Id) {
    let mut bytes = b"hs-lane-domain\0\x01".to_vec();
    bytes.extend_from_slice(label);
    bytes.push(0);
    policy_bytes(policy, &mut bytes);
    bytes.extend_from_slice(tail);
    let digest = Sha256::digest(&bytes);
    let mut domain = [0u8; 16];
    let mut group = [0u8; 16];
    domain.copy_from_slice(digest.get(..16).unwrap_or(&[0; 16]));
    group.copy_from_slice(digest.get(16..32).unwrap_or(&[0; 16]));
    (domain, group)
}

/// Longest descriptor read: the format's body bound plus the envelope.
const DESCRIPTOR_READ_BOUND: u64 = 160 * 1024;
/// A descriptor CAS that loses to another joiner is re-read and repeated this many times.
const MAX_CAS_ATTEMPTS: u32 = 16;

/// Read a lane's descriptor and the version a CAS must name, or [`Error::NoLane`].
pub async fn read_descriptor<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
) -> Result<(LaneDescriptor, VersionToken)> {
    let k = key(&layout.lane_head(&id.domain, id.incarnation))?;
    let (bytes, version) = read_object(store, &k, DESCRIPTOR_READ_BOUND)
        .await?
        .ok_or(Error::NoLane)?;
    Ok((
        LaneDescriptor::decode_at(bytes.as_bytes(), &id.domain)?,
        version,
    ))
}

/// Whether a lane's descriptor exists: one HEAD, no read.
pub async fn lane_exists<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
) -> Result<bool> {
    let k = key(&layout.lane_head(&id.domain, id.incarnation))?;
    match store.head(&crate::io::context(), &k).await {
        Ok(_) => Ok(true),
        Err(hsearch_object::Error::NotFound) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Create a lane with `members` (strictly ascending, within the ceiling) by conditional create.
/// [`Error::Exists`] if the descriptor is already there.
pub async fn create_lane<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    policy: &PolicyTuple,
    group_id: Id,
    ceiling: u32,
    members: &[Id],
) -> Result<(LaneDescriptor, VersionToken)> {
    if ceiling == 0 || ceiling > MAX_LANE_GROUP {
        return Err(Error::Invalid("lane group ceiling"));
    }
    let mut sorted = members.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let descriptor = LaneDescriptor {
        generation: 1,
        domain: id.domain,
        policy: policy.clone(),
        group_id,
        group_ceiling: ceiling,
        members: sorted,
        token: fresh_token(),
    };
    let mut bytes = Vec::new();
    descriptor.encode(&mut bytes)?;
    let k = layout.lane_head(&id.domain, id.incarnation);
    let request = mutation(
        &request_id("hd", &id.domain, descriptor.generation, &descriptor.token),
        &k,
        &bytes,
        Precondition::Absent,
    )?;
    match write(store, &request, false).await {
        Written::Done(receipt) => Ok((descriptor, receipt.object.version)),
        Written::Occupied => Err(Error::Exists),
        Written::Refused(error) => Err(error.into()),
        Written::Unknown => Err(Error::Contended),
    }
}

/// Replace the descriptor by CAS on `version`. `Ok(None)` means the CAS lost (someone else changed
/// the descriptor): re-read and decide again.
pub(crate) async fn swap_descriptor<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    next: &LaneDescriptor,
    version: &VersionToken,
) -> Result<Option<VersionToken>> {
    let mut bytes = Vec::new();
    next.encode(&mut bytes)?;
    let k = layout.lane_head(&id.domain, id.incarnation);
    let request = mutation(
        &request_id("hd", &id.domain, next.generation, &next.token),
        &k,
        &bytes,
        Precondition::Matches(version.clone()),
    )?;
    match write(store, &request, true).await {
        Written::Done(receipt) => Ok(Some(receipt.object.version)),
        Written::Occupied => Ok(None),
        Written::Refused(error) => Err(error.into()),
        Written::Unknown => Err(Error::Contended),
    }
}

/// Add `indexes` to a lane in one CAS (one PUT for any number of joiners, which is how creating
/// many indexes at once avoids one head rewrite each). Indexes already members are left alone.
/// [`Error::Exists`] if the lane cannot host them (policy mismatch or no room).
pub async fn join_members<S: MetadataStore>(
    store: &S,
    layout: &Layout,
    id: &LaneId,
    policy: &PolicyTuple,
    indexes: &[Id],
    known: Option<(LaneDescriptor, VersionToken)>,
) -> Result<(LaneDescriptor, VersionToken)> {
    let mut state = known;
    for _ in 0..MAX_CAS_ATTEMPTS {
        let (current, version) = match state.take() {
            Some(known) => known,
            None => read_descriptor(store, layout, id).await?,
        };
        let mut next = current.clone();
        let mut changed = false;
        for index in indexes {
            if next.members.binary_search(index).is_ok() {
                continue;
            }
            if !next.can_host(policy) {
                return Err(Error::Exists);
            }
            next = next.with_member(*index, fresh_token())?;
            changed = true;
        }
        if !changed {
            return Ok((current, version));
        }
        if let Some(version) = swap_descriptor(store, layout, id, &next, &version).await? {
            return Ok((next, version));
        }
    }
    Err(Error::Contended)
}

/// The successor descriptor without `index`: a membership change, next generation, new token.
pub(crate) fn without_member(current: &LaneDescriptor, index: &Id) -> Result<LaneDescriptor> {
    let Ok(at) = current.members.binary_search(index) else {
        return Err(Error::NotMember(*index));
    };
    let mut next = current.clone();
    next.generation = current
        .generation
        .checked_add(1)
        .ok_or(Error::Invalid("generation"))?;
    next.token = fresh_token();
    next.members.remove(at);
    Ok(next)
}
