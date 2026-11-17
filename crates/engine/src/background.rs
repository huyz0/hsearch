//! Process-wide bound on background merges: how many run at once, across
//! every engine, so merge I/O and CPU stay bounded however many indexes are
//! open.
use std::sync::{Condvar, Mutex};

/// Most merges running at once in this process.
pub(crate) const MAX_CONCURRENT_MERGES: usize = 2;

pub(crate) struct MergeSlots {
    free: Mutex<usize>,
    freed: Condvar,
}

pub(crate) struct MergeSlot<'a>(&'a MergeSlots);

impl MergeSlots {
    pub(crate) const fn new(slots: usize) -> Self {
        Self {
            free: Mutex::new(slots),
            freed: Condvar::new(),
        }
    }

    /// Takes a slot, waiting up to `wait` for one; `None` if none freed.
    pub(crate) fn acquire(&self, wait: std::time::Duration) -> Option<MergeSlot<'_>> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        if *free == 0 {
            let (guard, _) = self
                .freed
                .wait_timeout_while(free, wait, |n| *n == 0)
                .unwrap_or_else(|e| e.into_inner());
            free = guard;
        }
        if *free == 0 {
            return None;
        }
        *free -= 1;
        Some(MergeSlot(self))
    }
}

impl Drop for MergeSlot<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.0.freed.notify_one();
    }
}

/// The process's slots.
pub(crate) static SLOTS: MergeSlots = MergeSlots::new(MAX_CONCURRENT_MERGES);

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn slots_are_bounded_and_returned() {
        let slots = MergeSlots::new(1);
        let a = slots.acquire(Duration::ZERO).unwrap();
        assert!(slots.acquire(Duration::from_millis(5)).is_none());
        drop(a);
        assert!(slots.acquire(Duration::ZERO).is_some());
    }
}
