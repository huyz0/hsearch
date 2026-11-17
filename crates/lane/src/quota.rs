//! Cache quotas (M13.02): a shared byte cache in which one tenant's cold reads cannot push out
//! another tenant's working set.
//!
//! Every tenant is guaranteed `floor` bytes, and no tenant may hold more than `ceiling`. Pressure
//! is taken from bytes above a tenant's floor, largest surplus first, so a tenant at or under its
//! floor is never a victim of another tenant. A tenant over its ceiling evicts itself. When no
//! room can be found the read is served without being cached ([`Bypass`]) rather than evicting a
//! protected byte.
//!
//! Sans-I/O: this does the accounting and names who gives up how many bytes; the cache evicts them
//! (its own recency order within a tenant) and calls [`CacheQuota::release`] as bytes leave. Only
//! tenants with bytes resident have a slot, bounded by `max_tenants`, so an idle tenant costs
//! nothing. Victims are chosen from a bounded sample of [`SAMPLE`] slots taken round-robin plus the
//! requester itself, so the work of a reservation does not grow with the number of tenants.
use std::collections::HashMap;

use hsearch_format::Id;

/// Slots examined for victims in one reservation, besides the requester's own.
pub const SAMPLE: usize = 32;

/// Caps for one cache.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Bytes the cache may hold.
    pub capacity: u64,
    /// Bytes each tenant keeps against other tenants' pressure.
    pub floor: u64,
    /// Most bytes one tenant may hold.
    pub ceiling: u64,
    /// Most tenants with a slot.
    pub max_tenants: usize,
}

/// Why a read was not cached. Nothing was changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bypass {
    /// The object is empty or larger than the tenant's ceiling.
    TooLarge,
    /// The table of tenants is full and this one has no slot.
    Tenants,
    /// There is no room that does not belong to a tenant at or under its floor.
    NoRoom,
}

/// Bytes a tenant must evict to make room. The accounting already counts them gone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Eviction {
    /// Whose bytes.
    pub tenant: Id,
    /// How many.
    pub bytes: u64,
}

/// Counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    /// Reservations granted.
    pub cached: u64,
    /// Reads left uncached.
    pub bypassed: u64,
    /// Bytes evicted from a tenant other than the requester.
    pub evicted_from_others: u64,
    /// Bytes a tenant evicted from itself.
    pub evicted_from_self: u64,
}

struct Slot {
    tenant: Id,
    bytes: u64,
}

/// The per-tenant accounting of one cache.
pub struct CacheQuota {
    config: Config,
    total: u64,
    slots: Vec<Slot>,
    index: HashMap<Id, usize>,
    hand: usize,
    stats: Stats,
}

impl CacheQuota {
    /// An empty cache. `None` if a cap is zero, the floor exceeds the ceiling or the ceiling the
    /// capacity.
    #[must_use]
    pub fn new(config: Config) -> Option<Self> {
        if config.capacity == 0
            || config.ceiling == 0
            || config.max_tenants == 0
            || config.floor > config.ceiling
            || config.ceiling > config.capacity
        {
            return None;
        }
        Some(Self {
            config,
            total: 0,
            slots: Vec::new(),
            index: HashMap::new(),
            hand: 0,
            stats: Stats::default(),
        })
    }

    /// Account `bytes` newly cached for `tenant`, returning what must be evicted to make room.
    pub fn reserve(&mut self, tenant: Id, bytes: u64) -> Result<Vec<Eviction>, Bypass> {
        let result = self.plan(tenant, bytes);
        match &result {
            Ok(_) => self.stats.cached += 1,
            Err(_) => self.stats.bypassed += 1,
        }
        result
    }

    fn plan(&mut self, tenant: Id, bytes: u64) -> Result<Vec<Eviction>, Bypass> {
        let config = self.config;
        if bytes == 0 || bytes > config.ceiling {
            return Err(Bypass::TooLarge);
        }
        let own = self.index.get(&tenant).copied();
        if own.is_none() && self.slots.len() >= config.max_tenants {
            return Err(Bypass::Tenants);
        }
        let current = own.map_or(0, |i| self.slots[i].bytes);
        // Past its ceiling a tenant gives up its own bytes first.
        let self_evict = (current + bytes).saturating_sub(config.ceiling);
        let mut need =
            (self.total - self_evict.min(self.total) + bytes).saturating_sub(config.capacity);
        let mut taken: Vec<(usize, u64)> = Vec::new();
        if need > 0 {
            let mut candidates = self.sample(own);
            // The requester's surplus counts, less what it is already giving up.
            if let Some(i) = own {
                let left = current - self_evict;
                candidates.push((i, left.saturating_sub(config.floor)));
            }
            candidates.sort_by_key(|&(_, surplus)| std::cmp::Reverse(surplus));
            for (slot, surplus) in candidates {
                if need == 0 {
                    break;
                }
                let take = surplus.min(need);
                if take > 0 {
                    taken.push((slot, take));
                    need -= take;
                }
            }
            if need > 0 {
                return Err(Bypass::NoRoom);
            }
        }
        // Commit.
        let mut evictions = Vec::with_capacity(taken.len() + 1);
        if self_evict > 0 {
            evictions.push(Eviction {
                tenant,
                bytes: self_evict,
            });
            self.stats.evicted_from_self += self_evict;
        }
        let mut gone: Vec<Id> = Vec::new();
        for &(slot, take) in &taken {
            let victim = &mut self.slots[slot];
            victim.bytes -= take;
            self.total -= take;
            if victim.tenant == tenant {
                self.stats.evicted_from_self += take;
            } else {
                self.stats.evicted_from_others += take;
            }
            evictions.push(Eviction {
                tenant: victim.tenant,
                bytes: take,
            });
            if victim.bytes == 0 && victim.tenant != tenant {
                gone.push(victim.tenant);
            }
        }
        // The requester's slot: created, or reduced by its own eviction, then grown by the bytes.
        let slot = match self.index.get(&tenant).copied() {
            Some(i) => i,
            None => {
                self.slots.push(Slot { tenant, bytes: 0 });
                self.index.insert(tenant, self.slots.len() - 1);
                self.slots.len() - 1
            }
        };
        let cut = self_evict.min(self.slots[slot].bytes);
        self.slots[slot].bytes -= cut;
        self.total -= cut;
        self.slots[slot].bytes += bytes;
        self.total += bytes;
        for victim in gone {
            self.drop_slot(&victim);
        }
        Ok(evictions)
    }

    /// Up to [`SAMPLE`] slots after the hand, skipping the requester's, as (slot, surplus).
    fn sample(&mut self, skip: Option<usize>) -> Vec<(usize, u64)> {
        let n = self.slots.len();
        let mut out = Vec::with_capacity(SAMPLE.min(n));
        let mut visited = 0;
        while visited < n && out.len() < SAMPLE {
            let i = (self.hand + visited) % n;
            visited += 1;
            if Some(i) == skip {
                continue;
            }
            let surplus = self.slots[i].bytes.saturating_sub(self.config.floor);
            if surplus > 0 {
                out.push((i, surplus));
            }
        }
        self.hand = if n == 0 { 0 } else { (self.hand + visited) % n };
        out
    }

    /// `bytes` of `tenant`'s left the cache (evicted by the cache itself, or invalidated).
    pub fn release(&mut self, tenant: &Id, bytes: u64) {
        let Some(&i) = self.index.get(tenant) else {
            return;
        };
        let cut = bytes.min(self.slots[i].bytes);
        self.slots[i].bytes -= cut;
        self.total -= cut;
        if self.slots[i].bytes == 0 {
            self.drop_slot(tenant);
        }
    }

    fn drop_slot(&mut self, tenant: &Id) {
        let Some(i) = self.index.remove(tenant) else {
            return;
        };
        self.slots.swap_remove(i);
        if i < self.slots.len() {
            let moved = self.slots[i].tenant;
            self.index.insert(moved, i);
        }
        if self.hand > self.slots.len() {
            self.hand = 0;
        }
    }

    /// Bytes cached for `tenant`.
    #[must_use]
    pub fn resident(&self, tenant: &Id) -> u64 {
        self.index.get(tenant).map_or(0, |&i| self.slots[i].bytes)
    }

    /// Bytes cached in all.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Tenants with bytes resident.
    #[must_use]
    pub fn tenants(&self) -> usize {
        self.slots.len()
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats
    }
}
