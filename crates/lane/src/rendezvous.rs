//! Weighted placement (M13.02): which nodes serve a tenant, as a pure function of the fleet.
//!
//! Weighted rendezvous hashing: every node scores `weight / -ln(u)` for a tenant, where `u` is a
//! hash of the pair mapped into (0, 1], and the highest scores win. A node wins in proportion to
//! its weight, a node leaving or changing weight moves only the tenants that scored it highest, and
//! asking for more nodes only appends: the nodes for `k` are always among those for `k + 1`, so a
//! tenant that turns hot gains replicas without moving the ones it has. Nodes are spread across
//! zones first, then filled by score, so losing a zone loses at most one replica of a tenant.
//!
//! The logarithm is integer arithmetic (Q16 binary logarithm by repeated squaring), so two nodes
//! on different CPUs agree on every placement. No state, no I/O and **no authority**: the result
//! names nodes that may serve a tenant's index and nothing else. Who may write is decided by the
//! lane head's conditional transition (M03.05), which placement never reads or writes, so moving
//! compute cannot change who is authoritative.
use hsearch_format::Id;

/// Most nodes one placement returns.
pub const MAX_REPLICAS: usize = 8;
/// Most distinct zones in a fleet.
pub const MAX_ZONES: usize = 64;

/// One serving node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Node {
    /// Its identity.
    pub id: Id,
    /// The availability zone it runs in.
    pub zone: u16,
    /// Capacity in any unit; only the ratios matter.
    pub weight: u32,
}

/// What a tenant needs from placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ask {
    /// The tenant (an index id).
    pub tenant: Id,
    /// Nodes wanted, 1 to [`MAX_REPLICAS`].
    pub replicas: usize,
}

/// A placement that could not be computed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// No node has weight, or `replicas` is zero or past [`MAX_REPLICAS`].
    Invalid,
    /// More than [`MAX_ZONES`] distinct zones.
    Zones,
}

/// A fleet to place against.
pub struct Fleet<'a> {
    nodes: &'a [Node],
}

impl<'a> Fleet<'a> {
    /// A view of `nodes`. [`Refusal::Zones`] if it spans too many zones.
    pub fn new(nodes: &'a [Node]) -> Result<Self, Refusal> {
        let mut zones: Vec<u16> = Vec::with_capacity(MAX_ZONES);
        for node in nodes {
            if !zones.contains(&node.zone) {
                if zones.len() == MAX_ZONES {
                    return Err(Refusal::Zones);
                }
                zones.push(node.zone);
            }
        }
        Ok(Self { nodes })
    }

    /// The nodes for `ask`, best first. `factor` scales each node's weight in parts per thousand
    /// (load feedback; 1000 is neutral). Fewer than asked if the fleet has fewer usable nodes. One
    /// pass over the nodes and no allocation beyond the result.
    pub fn place(&self, ask: Ask, factor: impl Fn(&Id) -> u32) -> Result<Vec<Id>, Refusal> {
        if ask.replicas == 0 || ask.replicas > MAX_REPLICAS {
            return Err(Refusal::Invalid);
        }
        let mut best_per_zone: Vec<(u16, u128, usize)> = Vec::with_capacity(MAX_ZONES);
        let mut top: Vec<(u128, usize)> = Vec::with_capacity(ask.replicas + 1);
        for (i, node) in self.nodes.iter().enumerate() {
            let weight = u64::from(node.weight) * u64::from(factor(&node.id)) / 1000;
            if weight == 0 {
                continue;
            }
            let score = score(&ask.tenant, &node.id, weight);
            match best_per_zone.iter_mut().find(|z| z.0 == node.zone) {
                Some(z) if score > z.1 => *z = (node.zone, score, i),
                Some(_) => {}
                None => best_per_zone.push((node.zone, score, i)),
            }
            let at = top.partition_point(|&(s, _)| s > score);
            if at < ask.replicas {
                top.insert(at, (score, i));
                top.truncate(ask.replicas);
            }
        }
        if top.is_empty() {
            return Err(Refusal::Invalid);
        }
        best_per_zone.sort_by_key(|&(_, s, _)| std::cmp::Reverse(s));
        let mut chosen: Vec<usize> = best_per_zone
            .iter()
            .take(ask.replicas)
            .map(|z| z.2)
            .collect();
        for &(_, i) in &top {
            if chosen.len() == ask.replicas {
                break;
            }
            if !chosen.contains(&i) {
                chosen.push(i);
            }
        }
        Ok(chosen.into_iter().map(|i| self.nodes[i].id).collect())
    }
}

/// How many replicas a tenant needs for `rate` (requests a second) when a node serves `per_node`
/// comfortably: enough that none runs past `fill_permille` of that, clamped to
/// `1..=max_replicas` (and [`MAX_REPLICAS`]). A pure function of a load figure the caller
/// smooths; the placement it feeds keeps the replicas it already has.
#[must_use]
pub fn replicas_for(rate: u64, per_node: u64, fill_permille: u32, max_replicas: usize) -> usize {
    let usable = u128::from(per_node) * u128::from(fill_permille) / 1000;
    let max = max_replicas.clamp(1, MAX_REPLICAS);
    if usable == 0 {
        return max;
    }
    let needed = u128::from(rate).div_ceil(usable).max(1);
    usize::try_from(needed).map_or(max, |n| n.min(max))
}

/// Weighted rendezvous score: `weight / -ln(u)`, as a fixed-point integer.
fn score(tenant: &Id, node: &Id, weight: u64) -> u128 {
    let h = pair_hash(tenant, node);
    // -log2(u) for u = h / 2^64, in Q16, never zero.
    let neg_log = (64u64 << 16).saturating_sub(log2_q16(h | 1)).max(1);
    (u128::from(weight) << 48) / u128::from(neg_log)
}

fn pair_hash(a: &Id, b: &Id) -> u64 {
    let word = |id: &Id, at: usize| {
        let mut w = [0u8; 8];
        w.copy_from_slice(&id[at..at + 8]);
        u64::from_le_bytes(w)
    };
    let mut h = 0x9E37_79B9_7F4A_7C15u64;
    for w in [word(a, 0), word(a, 8), word(b, 0), word(b, 8)] {
        h = mix(h ^ w);
    }
    h
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Mantissa bits the logarithm table is indexed by: 12 gives 4,096 entries (16 KiB) and a score
/// error under 0.02%, far inside the sampling noise of any fleet this places.
const TABLE_BITS: u32 = 12;

/// `log2(1 + i / 2^TABLE_BITS)` in Q16, built at compile time by repeated squaring so that every
/// build of every platform holds the same table.
const LOG2_FRACTION: [u32; 1 << TABLE_BITS] = {
    let mut table = [0u32; 1 << TABLE_BITS];
    let mut i = 0;
    while i < table.len() {
        // The mantissa 1 + i/2^bits in Q63.
        let mut m: u128 = (1u128 << 63) + ((i as u128) << (63 - TABLE_BITS));
        let mut frac = 0u32;
        let mut bit = 0;
        while bit < 16 {
            m = (m * m) >> 63;
            frac <<= 1;
            if m >= 1u128 << 64 {
                frac |= 1;
                m >>= 1;
            }
            bit += 1;
        }
        table[i] = frac;
        i += 1;
    }
    table
};

/// log2 of `x` (non-zero) in Q16 fixed point, integer only.
fn log2_q16(x: u64) -> u64 {
    let lz = x.leading_zeros();
    let whole = u64::from(63 - lz);
    // The bits after the leading one, top `TABLE_BITS` of them.
    let normalised = x << lz;
    let index = (normalised << 1) >> (64 - TABLE_BITS);
    (whole << 16) | u64::from(LOG2_FRACTION[index as usize])
}
