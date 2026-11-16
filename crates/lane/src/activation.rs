//! Lazy activation of indexes under hard caps (M13.01).
//!
//! The set holds only the indexes that are active or loading. An idle index has no entry, no timer
//! and no counter here, so every operation does the same bounded work whether ten or ten million
//! indexes exist: a hash lookup and a constant number of list links. Memory is set by the caps,
//! not by the population. Sans-I/O: the caller loads (resolve the name, read the index's head,
//! open its engine) and tells the set what it got.
//!
//! Callers of the same cold index share one load: the first gets [`Begin::Load`], the rest
//! [`Begin::Pending`]. The number of loads in flight is capped, so a burst of cold indexes is
//! refused ([`Refusal::Loading`]) rather than started all at once. A full set evicts the least
//! recently used index that nobody holds a lease on; if every active index is leased it refuses
//! ([`Refusal::Capacity`]) instead of growing.
use std::collections::HashMap;

use hsearch_format::Id;

/// Caps on one set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Most indexes active or loading at once.
    pub max_active: usize,
    /// Most bytes of footprint, as reported by the caller, across active and loading indexes.
    pub max_bytes: u64,
    /// Most loads in flight at once.
    pub max_loading: usize,
}

/// Why an activation was refused. Nothing was changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The limits are zero somewhere.
    Invalid,
    /// This one index alone is larger than the byte cap.
    TooLarge,
    /// Every active index is leased and the set is full.
    Capacity,
    /// The cap on loads in flight is reached.
    Loading,
}

/// What [`ActiveSet::begin`] decided.
#[derive(Debug)]
pub enum Begin<H> {
    /// Already active; it is now the most recently used. Read it with [`ActiveSet::get`].
    Hit,
    /// Another caller is loading it; wait and ask again.
    Pending,
    /// This caller loads it. `evicted` left the set to make room: close them.
    Load {
        /// Hand back to [`ActiveSet::finish`] or [`ActiveSet::fail`].
        ticket: Ticket,
        /// Indexes removed to make room, with their handles.
        evicted: Vec<(Id, H)>,
    },
}

/// The right to finish one load.
#[derive(Debug, Eq, PartialEq)]
pub struct Ticket {
    id: Id,
}

/// Counters. `steps` counts hash lookups and list links touched, the unit of "fixed work".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    /// Loads started.
    pub activations: u64,
    /// Activations that found the index active.
    pub hits: u64,
    /// Indexes evicted.
    pub evictions: u64,
    /// Loads abandoned.
    pub failed: u64,
    /// Refused activations.
    pub refused: u64,
    /// Lookups and links touched.
    pub steps: u64,
}

const NONE: usize = usize::MAX;

struct Node<H> {
    id: Id,
    handle: Option<H>,
    bytes: u64,
    leases: u32,
    prev: usize,
    next: usize,
    listed: bool,
}

/// Active and loading indexes under [`Limits`].
pub struct ActiveSet<H> {
    limits: Limits,
    map: HashMap<Id, usize>,
    nodes: Vec<Node<H>>,
    free: Vec<usize>,
    head: usize,
    tail: usize,
    ready: usize,
    loading: usize,
    bytes: u64,
    /// Entries on the eviction list (active and unleased) and their bytes.
    listed_count: usize,
    listed_bytes: u64,
    stats: Stats,
}

impl<H> ActiveSet<H> {
    /// An empty set. [`Refusal::Invalid`] if any cap is zero.
    pub fn new(limits: Limits) -> Result<Self, Refusal> {
        if limits.max_active == 0 || limits.max_bytes == 0 || limits.max_loading == 0 {
            return Err(Refusal::Invalid);
        }
        Ok(Self {
            limits,
            map: HashMap::with_capacity(limits.max_active),
            nodes: Vec::with_capacity(limits.max_active),
            free: Vec::new(),
            head: NONE,
            tail: NONE,
            ready: 0,
            loading: 0,
            bytes: 0,
            listed_count: 0,
            listed_bytes: 0,
            stats: Stats::default(),
        })
    }

    /// Active indexes (not those still loading).
    #[must_use]
    pub fn len(&self) -> usize {
        self.ready
    }

    /// Whether no index is active.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ready == 0
    }

    /// Footprint of everything active or loading, as the caller reported it.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Memory this set itself holds, from its allocations: set by the caps, not the population.
    #[must_use]
    pub fn footprint_bytes(&self) -> usize {
        self.nodes.capacity() * std::mem::size_of::<Node<H>>()
            + self.map.capacity() * (std::mem::size_of::<Id>() + std::mem::size_of::<usize>() + 8)
            + self.free.capacity() * std::mem::size_of::<usize>()
    }

    fn unlink(&mut self, at: usize) {
        let (prev, next) = {
            let n = &mut self.nodes[at];
            n.listed = false;
            (n.prev, n.next)
        };
        self.listed_count -= 1;
        self.listed_bytes -= self.nodes[at].bytes;
        if prev == NONE {
            self.head = next;
        } else {
            self.nodes[prev].next = next;
        }
        if next == NONE {
            self.tail = prev;
        } else {
            self.nodes[next].prev = prev;
        }
        self.stats.steps += 2;
    }

    fn push_front(&mut self, at: usize) {
        let old = self.head;
        {
            let n = &mut self.nodes[at];
            n.prev = NONE;
            n.next = old;
            n.listed = true;
        }
        self.listed_count += 1;
        self.listed_bytes += self.nodes[at].bytes;
        if old == NONE {
            self.tail = at;
        } else {
            self.nodes[old].prev = at;
        }
        self.head = at;
        self.stats.steps += 2;
    }

    fn remove(&mut self, at: usize) -> (Id, Option<H>, u64) {
        let (id, handle, bytes) = {
            let n = &mut self.nodes[at];
            (n.id, n.handle.take(), n.bytes)
        };
        self.map.remove(&id);
        self.free.push(at);
        self.bytes -= bytes;
        self.stats.steps += 1;
        (id, handle, bytes)
    }

    fn evict_one(&mut self) -> Option<(Id, H)> {
        if self.tail == NONE {
            return None;
        }
        let at = self.tail;
        self.unlink(at);
        self.ready -= 1;
        let (id, handle, _) = self.remove(at);
        self.stats.evictions += 1;
        handle.map(|h| (id, h))
    }

    /// Whether one more entry of `bytes` fits if every unleased active entry were evicted. Checked
    /// before anything is evicted, so a refusal never costs an index its place.
    fn fits_after_eviction(&self, bytes: u64) -> bool {
        let pinned = self.ready + self.loading - self.listed_count;
        pinned < self.limits.max_active
            && self.bytes - self.listed_bytes + bytes <= self.limits.max_bytes
    }

    /// Ask to use `id`, which will cost about `bytes`.
    pub fn begin(&mut self, id: Id, bytes: u64) -> Result<Begin<H>, Refusal> {
        self.stats.steps += 1;
        if let Some(&at) = self.map.get(&id) {
            if self.nodes[at].handle.is_none() {
                return Ok(Begin::Pending);
            }
            if self.nodes[at].listed {
                self.unlink(at);
                self.push_front(at);
            }
            self.stats.hits += 1;
            return Ok(Begin::Hit);
        }
        if bytes > self.limits.max_bytes {
            self.stats.refused += 1;
            return Err(Refusal::TooLarge);
        }
        if self.loading >= self.limits.max_loading {
            self.stats.refused += 1;
            return Err(Refusal::Loading);
        }
        if !self.fits_after_eviction(bytes) {
            self.stats.refused += 1;
            return Err(Refusal::Capacity);
        }
        let mut evicted = Vec::new();
        while self.ready + self.loading >= self.limits.max_active
            || self.bytes + bytes > self.limits.max_bytes
        {
            // Feasible by the check above: an unleased entry is always there to evict.
            let Some(pair) = self.evict_one() else { break };
            evicted.push(pair);
        }
        let node = Node {
            id,
            handle: None,
            bytes,
            leases: 0,
            prev: NONE,
            next: NONE,
            listed: false,
        };
        let at = if let Some(slot) = self.free.pop() {
            self.nodes[slot] = node;
            slot
        } else {
            self.nodes.push(node);
            self.nodes.len() - 1
        };
        self.map.insert(id, at);
        self.loading += 1;
        self.bytes += bytes;
        self.stats.activations += 1;
        self.stats.steps += 1;
        Ok(Begin::Load {
            ticket: Ticket { id },
            evicted,
        })
    }

    /// The load succeeded: `handle` is `id`'s engine, costing `bytes` (which may differ from the
    /// estimate given to [`begin`](Self::begin)). Returns indexes evicted to fit the difference.
    /// Refuses ([`Refusal::Capacity`]) if it cannot fit; the load is then abandoned.
    pub fn finish(
        &mut self,
        ticket: Ticket,
        handle: H,
        bytes: u64,
    ) -> Result<Vec<(Id, H)>, Refusal> {
        let at = *self.map.get(&ticket.id).ok_or(Refusal::Invalid)?;
        self.stats.steps += 1;
        let reserved = self.nodes[at].bytes;
        let mut evicted = Vec::new();
        if bytes > reserved {
            // Grew past the estimate: evict only if that makes it fit, otherwise give it up whole.
            if self.bytes - reserved - self.listed_bytes + bytes > self.limits.max_bytes {
                self.loading -= 1;
                self.remove(at);
                self.stats.failed += 1;
                self.stats.refused += 1;
                return Err(Refusal::Capacity);
            }
            self.bytes -= reserved;
            self.nodes[at].bytes = 0;
            while self.bytes + bytes > self.limits.max_bytes {
                let Some(pair) = self.evict_one() else { break };
                evicted.push(pair);
            }
            self.bytes += bytes;
        } else {
            self.bytes -= reserved - bytes;
        }
        self.nodes[at].bytes = bytes;
        self.loading -= 1;
        self.ready += 1;
        self.nodes[at].handle = Some(handle);
        self.push_front(at);
        Ok(evicted)
    }

    /// The load failed or was cancelled; its place is free again and the index may be tried again.
    pub fn fail(&mut self, ticket: Ticket) {
        if let Some(&at) = self.map.get(&ticket.id)
            && self.nodes[at].handle.is_none()
        {
            self.loading -= 1;
            self.remove(at);
            self.stats.failed += 1;
        }
    }

    /// The active index's handle, as the most recently used. `None` if it is not active.
    pub fn get(&mut self, id: &Id) -> Option<&H> {
        self.stats.steps += 1;
        let at = *self.map.get(id)?;
        self.nodes[at].handle.as_ref()?;
        if self.nodes[at].listed {
            self.unlink(at);
            self.push_front(at);
        }
        self.nodes[at].handle.as_ref()
    }

    /// Hold `id` active: it is not evicted until every lease is [`release`](Self::release)d.
    pub fn lease(&mut self, id: &Id) -> Result<(), Refusal> {
        self.stats.steps += 1;
        let at = *self.map.get(id).ok_or(Refusal::Invalid)?;
        if self.nodes[at].handle.is_none() {
            return Err(Refusal::Invalid);
        }
        if self.nodes[at].leases == 0 && self.nodes[at].listed {
            self.unlink(at);
        }
        self.nodes[at].leases += 1;
        Ok(())
    }

    /// Give back one lease; with the last, `id` becomes the most recently used and evictable.
    pub fn release(&mut self, id: &Id) -> Result<(), Refusal> {
        self.stats.steps += 1;
        let at = *self.map.get(id).ok_or(Refusal::Invalid)?;
        if self.nodes[at].leases == 0 {
            return Err(Refusal::Invalid);
        }
        self.nodes[at].leases -= 1;
        if self.nodes[at].leases == 0 {
            self.push_front(at);
        }
        Ok(())
    }
}
