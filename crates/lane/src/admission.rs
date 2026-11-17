//! Per-tenant admission on one node (M13.02): weighted fair shares of a bounded concurrency budget.
//!
//! A node has `capacity` units of work it will run at once. A tenant is guaranteed its weighted
//! share of the capacity among the tenants that are running something right now, and may borrow
//! beyond that only while a reserve stays free for tenants that are below their share. One hot
//! tenant therefore cannot take the last of the capacity from many small ones: it is refused
//! ([`Refusal::Share`]) while they are still admitted.
//!
//! A tenant has an entry only while it has work in flight, so the table holds at most `capacity`
//! entries and an idle tenant costs nothing: no entry, no timer, no counter. Every operation is a
//! hash lookup and a few integer operations. Sans-I/O: the caller runs the work and releases the
//! permit; refusals are returned for the caller to meter, so rejected work is counted by tenant.
use std::collections::HashMap;

use hsearch_format::Id;

/// Caps for one node's admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Units of work in flight at once, across all tenants.
    pub capacity: u64,
    /// Parts per thousand of `capacity` kept free of borrowing (0 to 999): the room tenants below
    /// their share can still claim when borrowers have taken everything else.
    pub reserve_permille: u16,
}

/// Why work was refused. Nothing was changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// A cap is zero, the weight is zero, or the reserve is 1000 or more.
    Invalid,
    /// This one request costs more than the node's capacity.
    TooLarge,
    /// The tenant is at or past its fair share and the only room left is the reserve.
    Share,
    /// The tenant is within its share but the node is full of work already running.
    Saturated,
}

/// The right to run one unit of work, to be given back with [`Admission::release`].
#[derive(Debug, Eq, PartialEq)]
#[must_use = "a permit that is not released holds capacity forever"]
pub struct Permit {
    tenant: Id,
    cost: u64,
}

impl Permit {
    /// The tenant it belongs to.
    #[must_use]
    pub fn tenant(&self) -> &Id {
        &self.tenant
    }
}

/// Counters over the life of the node.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    /// Requests admitted.
    pub admitted: u64,
    /// Of those, admitted beyond the tenant's fair share (borrowed).
    pub borrowed: u64,
    /// Refused as [`Refusal::Share`].
    pub refused_share: u64,
    /// Refused as [`Refusal::Saturated`].
    pub refused_saturated: u64,
    /// Refused as [`Refusal::TooLarge`] or [`Refusal::Invalid`].
    pub refused_other: u64,
}

struct Entry {
    inflight: u64,
    weight: u32,
}

/// Admission control for one node.
pub struct Admission {
    config: Config,
    used: u64,
    active_weight: u64,
    tenants: HashMap<Id, Entry>,
    stats: Stats,
}

impl Admission {
    /// A controller with nothing in flight. [`Refusal::Invalid`] if the config is unusable.
    pub fn new(config: Config) -> Result<Self, Refusal> {
        if config.capacity == 0 || config.reserve_permille >= 1000 {
            return Err(Refusal::Invalid);
        }
        Ok(Self {
            config,
            used: 0,
            active_weight: 0,
            tenants: HashMap::new(),
            stats: Stats::default(),
        })
    }

    /// Ask to run `cost` units for `tenant`, whose weight is `weight` (at least 1; a tenant's
    /// weight while it has work in flight is the last one given).
    pub fn admit(&mut self, tenant: Id, weight: u32, cost: u64) -> Result<Permit, Refusal> {
        let result = self.decide(tenant, weight, cost);
        match result {
            Ok(borrowed) => {
                self.stats.admitted += 1;
                self.stats.borrowed += u64::from(borrowed);
                Ok(Permit { tenant, cost })
            }
            Err(refusal) => {
                match refusal {
                    Refusal::Share => self.stats.refused_share += 1,
                    Refusal::Saturated => self.stats.refused_saturated += 1,
                    Refusal::Invalid | Refusal::TooLarge => self.stats.refused_other += 1,
                }
                Err(refusal)
            }
        }
    }

    /// Decide and, if admitted, record. Returns whether the work was borrowed.
    fn decide(&mut self, tenant: Id, weight: u32, cost: u64) -> Result<bool, Refusal> {
        if weight == 0 || cost == 0 {
            return Err(Refusal::Invalid);
        }
        let capacity = self.config.capacity;
        if cost > capacity {
            return Err(Refusal::TooLarge);
        }
        let (inflight, old_weight) = self
            .tenants
            .get(&tenant)
            .map_or((0, None), |e| (e.inflight, Some(u64::from(e.weight))));
        // The weight the share is computed against: everyone running, this tenant at its new weight.
        let total_weight = self.active_weight - old_weight.unwrap_or(0) + u64::from(weight);
        let reserve = capacity * u64::from(self.config.reserve_permille) / 1000;
        // A tenant alone would be owed the whole node; the reserve is never part of a share, so a
        // newcomer always finds it free.
        let share = (u128::from(capacity) * u128::from(weight) / u128::from(total_weight))
            .min(u128::from(capacity - reserve));
        let wanted = u128::from(inflight) + u128::from(cost);
        let free = capacity - self.used;
        let borrowed = wanted > share;
        if cost > free {
            return Err(Refusal::Saturated);
        }
        if borrowed && free - cost < reserve {
            return Err(Refusal::Share);
        }
        self.used += cost;
        self.active_weight = total_weight;
        let entry = self.tenants.entry(tenant).or_insert(Entry {
            inflight: 0,
            weight,
        });
        entry.inflight += cost;
        entry.weight = weight;
        Ok(borrowed)
    }

    /// Give back a permit. The tenant's entry disappears with its last unit.
    pub fn release(&mut self, permit: Permit) {
        let Some(entry) = self.tenants.get_mut(&permit.tenant) else {
            return;
        };
        entry.inflight = entry.inflight.saturating_sub(permit.cost);
        self.used = self.used.saturating_sub(permit.cost);
        if entry.inflight == 0 {
            self.active_weight -= u64::from(entry.weight);
            self.tenants.remove(&permit.tenant);
        }
    }

    /// Units in flight now.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used
    }

    /// Tenants with work in flight now; never more than the capacity.
    #[must_use]
    pub fn active_tenants(&self) -> usize {
        self.tenants.len()
    }

    /// Units in flight for `tenant`.
    #[must_use]
    pub fn inflight(&self, tenant: &Id) -> u64 {
        self.tenants.get(tenant).map_or(0, |e| e.inflight)
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats
    }
}
