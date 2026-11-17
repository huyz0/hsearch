//! A shared, finite local-disk budget with RAII reservations.
//!
//! Cache fills and merges both write local bytes. Each takes a [`DiskReservation`]
//! before it writes, so the sum of what is promised never passes the cap and a
//! full disk is a refusal at the start of the work, not an error half way through.
//! Dropping a reservation returns its bytes, so a failed or abandoned operation
//! cannot leak budget.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{Error, Result};

/// The most bytes that may be reserved at once.
#[derive(Clone, Debug)]
pub struct DiskBudget {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    cap: u64,
    used: AtomicU64,
}

/// Bytes held against a [`DiskBudget`] until dropped.
#[derive(Debug)]
pub struct DiskReservation {
    budget: Arc<Inner>,
    bytes: u64,
}

impl DiskBudget {
    /// A budget of `cap` bytes. A zero cap refuses every nonzero reservation.
    #[must_use]
    pub fn new(cap: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                cap,
                used: AtomicU64::new(0),
            }),
        }
    }

    /// Reserve `bytes`, or refuse with [`Error::Limit`] without changing anything.
    pub fn reserve(&self, bytes: u64) -> Result<DiskReservation> {
        let mut used = self.inner.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes).ok_or(Error::Limit)?;
            if next > self.inner.cap {
                return Err(Error::Limit);
            }
            match self.inner.used.compare_exchange_weak(
                used,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(DiskReservation {
                        budget: Arc::clone(&self.inner),
                        bytes,
                    });
                }
                Err(seen) => used = seen,
            }
        }
    }

    /// Bytes currently reserved.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.inner.used.load(Ordering::Acquire)
    }

    /// The cap.
    #[must_use]
    pub fn cap(&self) -> u64 {
        self.inner.cap
    }
}

impl DiskReservation {
    /// Bytes this reservation holds.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_never_pass_the_cap_and_return_on_drop() {
        let budget = DiskBudget::new(100);
        let a = budget.reserve(60).unwrap();
        assert_eq!(budget.reserve(41).unwrap_err(), Error::Limit);
        let b = budget.reserve(40).unwrap();
        assert_eq!(budget.used(), 100);
        drop(a);
        assert_eq!(budget.used(), 40);
        let c = budget.reserve(60).unwrap();
        assert_eq!((b.bytes(), c.bytes(), budget.cap()), (40, 60, 100));
    }

    #[test]
    fn overflow_and_zero_cap_are_refused() {
        let budget = DiskBudget::new(u64::MAX);
        let _held = budget.reserve(u64::MAX - 1).unwrap();
        assert_eq!(budget.reserve(2).unwrap_err(), Error::Limit);
        assert_eq!(DiskBudget::new(0).reserve(1).unwrap_err(), Error::Limit);
        assert!(DiskBudget::new(0).reserve(0).is_ok());
    }

    #[test]
    fn concurrent_reservations_stay_within_the_cap() {
        let budget = DiskBudget::new(1000);
        let granted = std::sync::atomic::AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let mut held = Vec::new();
                    for _ in 0..50 {
                        if let Ok(r) = budget.reserve(10) {
                            granted.fetch_add(10, Ordering::SeqCst);
                            held.push(r);
                        }
                    }
                    assert!(budget.used() <= 1000);
                });
            }
        });
        assert_eq!(budget.used(), 0);
        assert!(granted.load(Ordering::SeqCst) >= 1000);
    }
}
