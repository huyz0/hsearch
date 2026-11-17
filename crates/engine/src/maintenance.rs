//! Admission for merges: bounded concurrency, a disk reservation per merge and an
//! orderly shutdown.
//!
//! A merge writes a new segment before it frees the old ones, so it needs room for
//! about the bytes it reads. [`MergeScheduler::admit`] takes that room from the
//! shared [`DiskBudget`] first and refuses ([`Error::Limit`] for no room,
//! [`Error::Busy`] for no slot) before any native work starts. After
//! [`MergeScheduler::shutdown`] nothing is admitted and
//! [`MergeScheduler::wait_idle`] returns once the merges in flight have ended.
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::budget::{DiskBudget, DiskReservation};
use crate::{Error, Result};

struct State {
    running: usize,
    closed: bool,
}

struct Shared {
    max_running: usize,
    budget: DiskBudget,
    state: Mutex<State>,
    idle: Condvar,
}

/// Decides whether a merge may start now.
#[derive(Clone)]
pub struct MergeScheduler {
    shared: Arc<Shared>,
}

/// Permission to run one merge. Dropping it ends the merge's claim on a slot and
/// on its disk reservation, whether the merge succeeded or not.
pub struct MergeTicket {
    shared: Arc<Shared>,
    reservation: Option<DiskReservation>,
}

fn lock(shared: &Shared) -> Result<std::sync::MutexGuard<'_, State>> {
    shared.state.lock().map_err(|_| Error::Poisoned)
}

impl MergeScheduler {
    /// At most `max_running` merges (at least one) at once, each reserving from `budget`.
    #[must_use]
    pub fn new(max_running: usize, budget: DiskBudget) -> Self {
        Self {
            shared: Arc::new(Shared {
                max_running: max_running.max(1),
                budget,
                state: Mutex::new(State {
                    running: 0,
                    closed: false,
                }),
                idle: Condvar::new(),
            }),
        }
    }

    /// Admit a merge that will write about `output_bytes`, or refuse it.
    pub fn admit(&self, output_bytes: u64) -> Result<MergeTicket> {
        let mut state = lock(&self.shared)?;
        if state.closed {
            return Err(Error::Unsupported);
        }
        if state.running >= self.shared.max_running {
            return Err(Error::Busy);
        }
        let reservation = self.shared.budget.reserve(output_bytes)?;
        state.running += 1;
        Ok(MergeTicket {
            shared: Arc::clone(&self.shared),
            reservation: Some(reservation),
        })
    }

    /// Merges running now.
    #[must_use]
    pub fn running(&self) -> usize {
        lock(&self.shared).map_or(0, |s| s.running)
    }

    /// Refuse every later admission. Merges already admitted run to their end.
    pub fn shutdown(&self) {
        if let Ok(mut state) = lock(&self.shared) {
            state.closed = true;
        }
    }

    /// Wait until no merge runs, or `timeout` passes. True when idle.
    #[must_use]
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        let Ok(mut state) = lock(&self.shared) else {
            return false;
        };
        while state.running > 0 {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = match self.shared.idle.wait_timeout(state, left) {
                Ok((guard, _)) => guard,
                Err(_) => return false,
            };
        }
        true
    }
}

impl std::fmt::Debug for MergeTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MergeTicket")
    }
}

impl Drop for MergeTicket {
    fn drop(&mut self) {
        // Return the bytes before waking a waiter, so an idle scheduler holds none.
        drop(self.reservation.take());
        // Recover a poisoned lock: the count must still fall or shutdown hangs.
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.running -= 1;
        if state.running == 0 {
            self.shared.idle.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_and_disk_are_both_enforced() {
        let sched = MergeScheduler::new(2, DiskBudget::new(100));
        let a = sched.admit(40).unwrap();
        let _b = sched.admit(40).unwrap();
        assert_eq!(sched.admit(1).unwrap_err(), Error::Busy);
        drop(a);
        assert_eq!(sched.admit(61).unwrap_err(), Error::Limit);
        assert_eq!(sched.running(), 1);
        let _c = sched.admit(20).unwrap();
        assert_eq!(sched.running(), 2);
    }

    #[test]
    fn a_refused_disk_reservation_takes_no_slot() {
        let sched = MergeScheduler::new(1, DiskBudget::new(10));
        assert_eq!(sched.admit(11).unwrap_err(), Error::Limit);
        assert_eq!(sched.running(), 0);
        assert!(sched.admit(10).is_ok());
    }

    #[test]
    fn dropping_a_ticket_returns_slot_and_bytes() {
        let budget = DiskBudget::new(50);
        let sched = MergeScheduler::new(1, budget.clone());
        drop(sched.admit(50).unwrap());
        assert_eq!((sched.running(), budget.used()), (0, 0));
    }

    #[test]
    fn shutdown_refuses_new_work_and_waits_for_running_work() {
        let sched = MergeScheduler::new(2, DiskBudget::new(100));
        let ticket = sched.admit(10).unwrap();
        sched.shutdown();
        assert_eq!(sched.admit(1).unwrap_err(), Error::Unsupported);
        assert!(!sched.wait_idle(Duration::from_millis(20)));
        let waiter = {
            let sched = sched.clone();
            std::thread::spawn(move || sched.wait_idle(Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(30));
        drop(ticket);
        assert!(waiter.join().unwrap());
        assert!(sched.wait_idle(Duration::ZERO));
    }
}
