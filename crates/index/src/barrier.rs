//! Apply and reader barriers (M04.04): commit, apply and search visibility are
//! three separate positions, and a reader can only ever cover a prefix that is
//! both applied and durably committed.
//!
//! Positions are slot-prefix lengths: `n` means slots `0..n`.
//!
//! - [`Replay`] applies slots in order, once. A duplicate is skipped, a gap is
//!   refused, and a failed apply poisons the cursor: the engine may be partly
//!   changed, so recovery rebuilds from the durable log, never continues.
//! - [`Readers`] holds the committed watermark and the published reader. A reader
//!   is published with the prefix it covers; the publish is refused when that
//!   prefix is beyond the committed watermark (a write whose persistence failed,
//!   or has not finished, is never searchable) or behind the published one. A
//!   query pins a generation and its fetch round reuses the same pin, so the
//!   documents ranked are the documents fetched. Generations held by pins are
//!   counted and bounded, which bounds the memory a slow fetch can keep alive.
//! - [`Waiters`] holds bounded waits for a prefix and releases them when a
//!   published reader covers them (`refresh=wait_for`, M06.07).
//!
//! Nothing here does I/O, reads a clock or blocks: the caller decides when to
//! refresh (a refresh is due only when a reader would cover more than the
//! published one, so refreshes batch themselves).
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// What to do with a slot offered to [`Replay::begin`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The next slot: apply it, then call [`Replay::done`].
    Apply,
    /// Already applied: skip it.
    Duplicate,
    /// A slot is missing before this one.
    Gap {
        /// The slot that must come next.
        expected: u64,
    },
}

/// Misuse of a [`Replay`] cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// A failed apply poisoned the cursor; rebuild from the log.
    Poisoned,
    /// `done` was called for a slot that is not the one in flight.
    Order,
}

/// In-order, exactly-once apply of slots.
#[derive(Debug)]
pub struct Replay {
    applied: u64,
    inflight: bool,
    poisoned: bool,
}

impl Replay {
    /// A cursor whose applied prefix is `start` slots (a checkpoint's cover).
    #[must_use]
    pub fn new(start: u64) -> Self {
        Self {
            applied: start,
            inflight: false,
            poisoned: false,
        }
    }
    /// The contiguous applied prefix.
    #[must_use]
    pub fn applied(&self) -> u64 {
        self.applied
    }
    /// Offer slot `seq`.
    ///
    /// # Errors
    /// [`ReplayError::Poisoned`] after [`Replay::fail`].
    pub fn begin(&mut self, seq: u64) -> Result<Step, ReplayError> {
        if self.poisoned {
            return Err(ReplayError::Poisoned);
        }
        Ok(match seq.cmp(&self.applied) {
            std::cmp::Ordering::Less => Step::Duplicate,
            std::cmp::Ordering::Greater => Step::Gap {
                expected: self.applied,
            },
            std::cmp::Ordering::Equal => {
                self.inflight = true;
                Step::Apply
            }
        })
    }
    /// Slot `seq` was applied; the prefix grows by one.
    ///
    /// # Errors
    /// [`ReplayError::Order`] unless `seq` is the slot [`Replay::begin`] returned
    /// [`Step::Apply`] for; [`ReplayError::Poisoned`] after a failure.
    pub fn done(&mut self, seq: u64) -> Result<(), ReplayError> {
        if self.poisoned {
            return Err(ReplayError::Poisoned);
        }
        if !self.inflight || seq != self.applied {
            return Err(ReplayError::Order);
        }
        self.inflight = false;
        self.applied += 1;
        Ok(())
    }
    /// The applied prefix as a proof for [`Readers::publish`]. Take it in the
    /// same step that opens the reader, with no apply in between.
    ///
    /// # Errors
    /// [`ReplayError::Poisoned`] after a failure, or while a slot is being
    /// applied (the engine is between prefixes).
    pub fn cut(&self) -> Result<Cut, ReplayError> {
        if self.poisoned {
            return Err(ReplayError::Poisoned);
        }
        if self.inflight {
            return Err(ReplayError::Order);
        }
        Ok(Cut(self.applied))
    }
    /// The apply failed and may have partly changed the engine.
    pub fn fail(&mut self) {
        self.poisoned = true;
    }
}

/// The applied prefix of a [`Replay`] at one moment: the proof a reader opened
/// on that engine covers exactly those slots. Only [`Replay::cut`] makes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cut(u64);

impl Cut {
    /// Slots `0..covers` are applied.
    #[must_use]
    pub fn covers(self) -> u64 {
        self.0
    }
}

/// A published reader and the exact prefix it covers.
#[derive(Debug)]
pub struct Gen<R> {
    /// The reader.
    pub reader: R,
    /// Slots `0..covers` are visible through it, and no others.
    pub covers: u64,
    /// Publication order, from 1.
    pub generation: u64,
}

/// Why a publish or commit was refused. Nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refuse {
    /// The reader covers slots that are not durably committed.
    Uncommitted,
    /// The reader covers less than the published one, or the watermark went back.
    Regress,
    /// Too many older generations are still pinned.
    Pinned,
}

/// A refused publish, returning the reader so the caller can retry with it.
#[derive(Debug)]
pub struct Refused<R> {
    /// Why.
    pub reason: Refuse,
    /// The reader that was offered.
    pub reader: R,
}

struct Inner<R> {
    committed: u64,
    next: u64,
    published: Option<Arc<Gen<R>>>,
    live: Vec<Weak<Gen<R>>>,
    max_pinned: usize,
}

/// The committed watermark and the reader queries run on.
pub struct Readers<R> {
    inner: Mutex<Inner<R>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl<R> Readers<R> {
    /// At most `max_pinned` generations held outside the manager, plus the new one.
    #[must_use]
    pub fn new(max_pinned: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                committed: 0,
                next: 1,
                published: None,
                live: Vec::new(),
                max_pinned: max_pinned.max(1),
            }),
        }
    }

    /// Raise the durable watermark to `prefix` slots.
    ///
    /// # Errors
    /// [`Refuse::Regress`] if it would go back.
    pub fn commit(&self, prefix: u64) -> Result<(), Refuse> {
        let mut g = lock(&self.inner);
        if prefix < g.committed {
            return Err(Refuse::Regress);
        }
        g.committed = prefix;
        Ok(())
    }

    /// Publish `reader`, opened on an engine whose applied prefix is `cut`.
    ///
    /// The previous generation is dropped after the lock is released, so a slow
    /// `R::drop` never blocks a pin. A generation a caller never drops counts
    /// against `max_pinned` forever: once the bound is reached every publish is
    /// refused with [`Refuse::Pinned`] until a pin is released, so holders must
    /// be bounded in time by the caller (a fetch deadline).
    ///
    /// # Errors
    /// [`Refuse::Uncommitted`], [`Refuse::Regress`] or [`Refuse::Pinned`], with
    /// the reader returned.
    pub fn publish(&self, reader: R, cut: Cut) -> Result<Arc<Gen<R>>, Refused<R>> {
        let covers = cut.0;
        let (new, old) = {
            let mut g = lock(&self.inner);
            let refuse = |reason, reader| Err(Refused { reason, reader });
            if covers > g.committed {
                return refuse(Refuse::Uncommitted, reader);
            }
            if g.published.as_ref().is_some_and(|p| covers < p.covers) {
                return refuse(Refuse::Regress, reader);
            }
            g.live.retain(|w| w.strong_count() > 0);
            // Generations a caller holds. The manager's own references (the
            // field, and for the published one the clone below) and the
            // `upgrade` temporary are not holders.
            let published = g.published.clone();
            let held = g
                .live
                .iter()
                .filter_map(Weak::upgrade)
                .filter(|a| {
                    let manager = published.as_ref().is_some_and(|p| Arc::ptr_eq(a, p));
                    Arc::strong_count(a) > 1 + 2 * usize::from(manager)
                })
                .count();
            drop(published);
            if held + 1 > g.max_pinned {
                return refuse(Refuse::Pinned, reader);
            }
            let generation = g.next;
            g.next += 1;
            let new = Arc::new(Gen {
                reader,
                covers,
                generation,
            });
            g.live.push(Arc::downgrade(&new));
            let old = g.published.replace(Arc::clone(&new));
            (new, old)
        };
        drop(old);
        Ok(new)
    }

    /// The current reader, pinned until the returned handle is dropped. Use the
    /// same handle for the fetch round of a query.
    #[must_use]
    pub fn pin(&self) -> Option<Arc<Gen<R>>> {
        lock(&self.inner).published.clone()
    }

    /// Generations alive, held by the manager or by a pin.
    #[must_use]
    pub fn live(&self) -> usize {
        lock(&self.inner)
            .live
            .iter()
            .filter(|w| w.strong_count() > 0)
            .count()
    }

    /// Whether a refresh would publish a reader covering more than the current
    /// one, given `applied` slots applied: nothing is due until commit and apply
    /// both run ahead, which batches refreshes without a timer.
    #[must_use]
    pub fn refresh_due(&self, applied: u64) -> bool {
        let g = lock(&self.inner);
        let covers = g.published.as_ref().map_or(0, |p| p.covers);
        applied.min(g.committed) > covers
    }
}

/// The wait set is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Full;

/// Bounded waits for a published prefix.
#[derive(Debug)]
pub struct Waiters {
    max: usize,
    next: u64,
    needs: BTreeMap<u64, u64>,
}

impl Waiters {
    /// At most `max` waits at once.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            max: max.max(1),
            next: 1,
            needs: BTreeMap::new(),
        }
    }
    /// Wait for a reader covering `need` slots; returns the wait's id.
    ///
    /// # Errors
    /// [`Full`] at the bound: the caller answers busy instead of queueing.
    pub fn add(&mut self, need: u64) -> Result<u64, Full> {
        if self.needs.len() >= self.max {
            return Err(Full);
        }
        let id = self.next;
        self.next += 1;
        self.needs.insert(id, need);
        Ok(id)
    }
    /// Remove and return the waits a reader covering `covers` slots satisfies.
    pub fn release(&mut self, covers: u64) -> Vec<u64> {
        let ready: Vec<u64> = self
            .needs
            .iter()
            .filter(|(_, n)| **n <= covers)
            .map(|(id, _)| *id)
            .collect();
        for id in &ready {
            self.needs.remove(id);
        }
        ready
    }
    /// Waits outstanding.
    #[must_use]
    pub fn len(&self) -> usize {
        self.needs.len()
    }
    /// Whether none are outstanding.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.needs.is_empty()
    }
}
