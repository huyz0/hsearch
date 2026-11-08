use crate::Error;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

/// Shared cancellation flag, containing no request or payload data.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    /// Request cancellation. Accepted mutations still require honest resolution.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Observe the cancellation request.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Finite aggregate request-context caps; retries share the same counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Budget {
    /// Total admitted request bytes, including failed attempts.
    pub request_bytes: u64,
    /// Total admitted response bytes.
    pub response_bytes: u64,
    /// Total admitted calls, including identity resolution and reads.
    pub attempts: u32,
}

/// Monotonic usage charged before copying bytes or accepting an operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    /// Request bytes admitted so far.
    pub request_bytes: u64,
    /// Response bytes admitted so far.
    pub response_bytes: u64,
    /// Calls admitted so far.
    pub attempts: u32,
}

/// Deadline, cancellation and shared byte/attempt accounting for one workflow.
#[derive(Debug)]
pub struct RequestContext {
    deadline: Instant,
    cancellation: Cancellation,
    expired: AtomicBool,
    budget: Budget,
    usage: Mutex<Usage>,
}

impl RequestContext {
    /// Start a finite context with a maximum five-minute duration.
    ///
    /// A zero duration is valid and immediately expired. Zero budgets refuse
    /// their corresponding work. No provider operation may reset these counters.
    pub fn new(duration: Duration, cancellation: Cancellation, budget: Budget) -> Result<Self, Error> {
        if duration > Duration::from_secs(300) {
            return Err(Error::InvalidInput);
        }
        let deadline = Instant::now().checked_add(duration).ok_or(Error::InvalidInput)?;
        Ok(Self { deadline, cancellation, expired: AtomicBool::new(false), budget, usage: Mutex::new(Usage::default()) })
    }

    /// Cancel future admission, without rolling back accepted mutations.
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    /// Shorten this context's deadline to now. It can never be extended.
    pub fn expire(&self) {
        self.expired.store(true, Ordering::Release);
    }

    /// Check whether a new operation can still be admitted.
    pub fn check(&self) -> Result<(), Error> {
        if self.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if self.expired.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return Err(Error::DeadlineExceeded);
        }
        Ok(())
    }

    /// Charge one call and its complete input before external work or copying.
    pub fn charge_request(&self, bytes: u64) -> Result<(), Error> {
        self.check()?;
        let mut used = self.usage.lock().map_err(|_| Error::Unavailable)?;
        let request_bytes = used.request_bytes.checked_add(bytes).ok_or(Error::BudgetExceeded)?;
        let attempts = used.attempts.checked_add(1).ok_or(Error::BudgetExceeded)?;
        if request_bytes > self.budget.request_bytes || attempts > self.budget.attempts {
            return Err(Error::BudgetExceeded);
        }
        used.request_bytes = request_bytes;
        used.attempts = attempts;
        Ok(())
    }

    /// Charge response bytes before allocating or copying a response.
    ///
    /// This does not check cancellation: an already accepted mutation's known
    /// outcome must not be replaced with a false pre-acceptance cancellation.
    pub fn charge_response(&self, bytes: u64) -> Result<(), Error> {
        let mut used = self.usage.lock().map_err(|_| Error::Unavailable)?;
        let total = used.response_bytes.checked_add(bytes).ok_or(Error::BudgetExceeded)?;
        if total > self.budget.response_bytes {
            return Err(Error::BudgetExceeded);
        }
        used.response_bytes = total;
        Ok(())
    }

    /// Read current counters, or fail if accounting was poisoned.
    pub fn usage(&self) -> Result<Usage, Error> {
        self.usage.lock().map(|value| *value).map_err(|_| Error::Unavailable)
    }
}
