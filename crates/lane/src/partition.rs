//! Catalog partition strategy (M13.01): how many bucket pages a catalog is given.
//!
//! A catalog page is created by its first write, so a catalog with `2^bits` buckets costs one root
//! object plus one page object per bucket that ever received a name. A tenant with a handful of
//! names therefore costs a root and at most as many pages as it has names, not `2^bits`, which is
//! what keeps a million mostly idle tenants cheap. The fan-out only has to be large enough that
//! the busiest bucket stays inside the page bound as names and tombstones accumulate.
use hsearch_format::{MAX_CATALOG_BUCKET_BITS, MAX_CATALOG_PAGE_ENTRIES};

/// Entries a page is planned to hold on average: half the format's bound, so hash skew and the
/// tombstones a catalog keeps for the life of a name do not overflow a page before it is re-planned.
pub const PAGE_TARGET: usize = MAX_CATALOG_PAGE_ENTRIES / 2;

/// The smallest fan-out (in bits, at least 1) at which `names` names average at most
/// [`PAGE_TARGET`] per page, clamped to the format ceiling.
#[must_use]
pub fn bucket_bits_for(names: u64) -> u8 {
    let mut bits = 1u8;
    while bits < MAX_CATALOG_BUCKET_BITS && names > (PAGE_TARGET as u64) << bits {
        bits += 1;
    }
    bits
}
