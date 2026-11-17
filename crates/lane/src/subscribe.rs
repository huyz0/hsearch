//! Bounded subscriptions (M13.02): which nodes want to hear about a change to which index.
//!
//! A node that has an index active subscribes to it so a head change reaches it by push instead of
//! by polling; an index nobody has active has no subscription, so idle indexes cost nothing here.
//! Three caps keep the table bounded whatever the callers do: subscriptions in all, per subscriber
//! and per index (the fan-out of one notification). A request past a cap is refused and changes
//! nothing; the caller falls back to reading the head when it needs it.
use std::collections::{HashMap, HashSet};

use hsearch_format::Id;

/// Caps for one table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Subscriptions in all.
    pub total: usize,
    /// Subscriptions one subscriber may hold.
    pub per_subscriber: usize,
    /// Subscribers one index may have; the fan-out of a notification.
    pub per_index: usize,
}

/// Why a subscription was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// A cap is zero.
    Invalid,
    /// The table is full.
    Total,
    /// This subscriber holds its limit.
    Subscriber,
    /// This index already has its limit of subscribers.
    Index,
}

/// Subscriptions from subscribers (nodes) to indexes.
pub struct Subscriptions {
    limits: Limits,
    total: usize,
    by_index: HashMap<Id, HashSet<Id>>,
    by_subscriber: HashMap<Id, HashSet<Id>>,
}

impl Subscriptions {
    /// An empty table. [`Refusal::Invalid`] if a cap is zero.
    pub fn new(limits: Limits) -> Result<Self, Refusal> {
        if limits.total == 0 || limits.per_subscriber == 0 || limits.per_index == 0 {
            return Err(Refusal::Invalid);
        }
        Ok(Self {
            limits,
            total: 0,
            by_index: HashMap::new(),
            by_subscriber: HashMap::new(),
        })
    }

    /// Subscribe `subscriber` to `index`. `Ok(false)` if it already was.
    pub fn subscribe(&mut self, subscriber: Id, index: Id) -> Result<bool, Refusal> {
        if self
            .by_index
            .get(&index)
            .is_some_and(|set| set.contains(&subscriber))
        {
            return Ok(false);
        }
        if self.total >= self.limits.total {
            return Err(Refusal::Total);
        }
        if self
            .by_subscriber
            .get(&subscriber)
            .is_some_and(|set| set.len() >= self.limits.per_subscriber)
        {
            return Err(Refusal::Subscriber);
        }
        if self
            .by_index
            .get(&index)
            .is_some_and(|set| set.len() >= self.limits.per_index)
        {
            return Err(Refusal::Index);
        }
        self.by_index.entry(index).or_default().insert(subscriber);
        self.by_subscriber
            .entry(subscriber)
            .or_default()
            .insert(index);
        self.total += 1;
        Ok(true)
    }

    /// Remove one subscription; whether it existed.
    pub fn unsubscribe(&mut self, subscriber: &Id, index: &Id) -> bool {
        let removed = remove_pair(&mut self.by_index, index, subscriber);
        if removed {
            remove_pair(&mut self.by_subscriber, subscriber, index);
            self.total -= 1;
        }
        removed
    }

    /// Remove every subscription of a subscriber that left; how many there were.
    pub fn drop_subscriber(&mut self, subscriber: &Id) -> usize {
        let Some(indexes) = self.by_subscriber.remove(subscriber) else {
            return 0;
        };
        for index in &indexes {
            remove_pair(&mut self.by_index, index, subscriber);
        }
        self.total -= indexes.len();
        indexes.len()
    }

    /// The subscribers to tell about a change to `index`: at most `per_index` of them.
    pub fn subscribers(&self, index: &Id) -> impl Iterator<Item = &Id> {
        self.by_index.get(index).into_iter().flatten()
    }

    /// Subscriptions in all.
    #[must_use]
    pub fn len(&self) -> usize {
        self.total
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }
}

/// Remove `value` from `key`'s set, dropping the set when it empties.
fn remove_pair(map: &mut HashMap<Id, HashSet<Id>>, key: &Id, value: &Id) -> bool {
    let Some(set) = map.get_mut(key) else {
        return false;
    };
    let removed = set.remove(value);
    if set.is_empty() {
        map.remove(key);
    }
    removed
}
