//! A [`KeyProvider`] wrapper that counts every attempt and times it (M05.08).
//!
//! The KMS bill is requests, so the number that matters is how many calls reach the provider, including the
//! ones that failed, were throttled or timed out. [`MeteredProvider`] counts each call by operation and
//! outcome and keeps a latency histogram per operation. It changes nothing about a call: arguments, results
//! and errors pass through untouched, and nothing it records can hold key material (counts and durations
//! only). Wrap any provider, including the real ones in M15, so the same accounting runs against an emulator
//! and against a provider.
//!
//! Latency buckets are powers of two in microseconds (bucket `i` holds `[2^i, 2^(i+1))` us, the last bucket
//! everything above), so a snapshot is a fixed 24 numbers per operation however many calls were made.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::keys::{
    CallOptions, KeyContext, KeyInfo, KeyProvider, KeyRef, ProviderError, SecretKey, WrappedKey,
};

/// Latency histogram buckets.
pub const BUCKETS: usize = 24;

/// The provider operations that are counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// `key_info`.
    KeyInfo,
    /// `wrap`.
    Wrap,
    /// `unwrap`.
    Unwrap,
    /// `destroy_key`.
    Destroy,
}

impl Op {
    /// A stable label.
    pub fn as_str(self) -> &'static str {
        match self {
            Op::KeyInfo => "key_info",
            Op::Wrap => "wrap",
            Op::Unwrap => "unwrap",
            Op::Destroy => "destroy",
        }
    }

    const ALL: [Op; 4] = [Op::KeyInfo, Op::Wrap, Op::Unwrap, Op::Destroy];

    fn index(self) -> usize {
        self as usize
    }
}

/// A latency histogram: counts per power-of-two microsecond bucket, plus the exact maximum and sum.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Histogram {
    /// Counts per bucket.
    pub buckets: [u64; BUCKETS],
    /// Calls recorded.
    pub count: u64,
    /// Sum of durations, microseconds.
    pub sum_us: u64,
    /// Longest duration, microseconds.
    pub max_us: u64,
}

impl Histogram {
    fn bucket(us: u64) -> usize {
        (64 - us.max(1).leading_zeros() as usize - 1).min(BUCKETS - 1)
    }

    fn add(&mut self, elapsed: Duration) {
        let us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.buckets[Self::bucket(us)] += 1;
        self.count += 1;
        self.sum_us = self.sum_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }

    /// An upper bound, in microseconds, on the `q`th quantile (`0.0..=1.0`): the top of the bucket that
    /// holds it, never more than the true maximum. `None` with no calls.
    pub fn quantile_us(&self, q: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let rank = ((self.count as f64 * q.clamp(0.0, 1.0)).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                let top = if i + 1 >= BUCKETS {
                    u64::MAX
                } else {
                    (1u64 << (i + 1)) - 1
                };
                return Some(top.min(self.max_us));
            }
        }
        Some(self.max_us)
    }
}

/// What was observed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Calls per operation and outcome (`"ok"` or a [`ProviderError::code`]).
    pub calls: BTreeMap<(Op, &'static str), u64>,
    /// Latency per operation.
    pub latency: BTreeMap<Op, Histogram>,
}

impl Snapshot {
    /// Calls of `op` with any outcome.
    pub fn total(&self, op: Op) -> u64 {
        self.calls
            .iter()
            .filter(|((o, _), _)| *o == op)
            .map(|(_, n)| n)
            .sum()
    }

    /// Calls of `op` with `outcome`.
    pub fn outcome(&self, op: Op, outcome: &str) -> u64 {
        self.calls
            .iter()
            .filter(|((o, c), _)| *o == op && *c == outcome)
            .map(|(_, n)| n)
            .sum()
    }

    /// Every call of every operation: what the provider bills.
    pub fn all(&self) -> u64 {
        self.calls.values().sum()
    }
}

/// Counts and times every call to the wrapped provider.
pub struct MeteredProvider {
    inner: Arc<dyn KeyProvider>,
    attempts: [AtomicU64; 4],
    detail: Mutex<Snapshot>,
}

impl MeteredProvider {
    /// Wraps `inner`.
    pub fn new(inner: Arc<dyn KeyProvider>) -> Self {
        Self {
            inner,
            attempts: Default::default(),
            detail: Mutex::new(Snapshot::default()),
        }
    }

    /// Calls started for `op` (counted before the call returns, so a call still in flight is included).
    pub fn attempts(&self, op: Op) -> u64 {
        self.attempts[op.index()].load(Ordering::Relaxed)
    }

    /// What has been observed so far.
    pub fn snapshot(&self) -> Snapshot {
        self.detail
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Forgets everything observed (the wrapped provider is untouched).
    pub fn reset(&self) {
        for a in &self.attempts {
            a.store(0, Ordering::Relaxed);
        }
        *self.detail.lock().unwrap_or_else(|p| p.into_inner()) = Snapshot::default();
    }

    fn observe<T>(
        &self,
        op: Op,
        call: impl FnOnce() -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        self.attempts[op.index()].fetch_add(1, Ordering::Relaxed);
        let started = Instant::now();
        let result = call();
        let elapsed = started.elapsed();
        let outcome = match &result {
            Ok(_) => "ok",
            Err(e) => e.code(),
        };
        let mut detail = self.detail.lock().unwrap_or_else(|p| p.into_inner());
        *detail.calls.entry((op, outcome)).or_insert(0) += 1;
        detail.latency.entry(op).or_default().add(elapsed);
        result
    }

    /// All operations, for reports.
    pub fn ops() -> [Op; 4] {
        Op::ALL
    }
}

impl KeyProvider for MeteredProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn is_test(&self) -> bool {
        self.inner.is_test()
    }

    fn key_info(&self, key: &KeyRef, opts: &CallOptions) -> Result<KeyInfo, ProviderError> {
        self.observe(Op::KeyInfo, || self.inner.key_info(key, opts))
    }

    fn wrap(
        &self,
        key: &KeyRef,
        ctx: &KeyContext,
        dek: &SecretKey,
        opts: &CallOptions,
    ) -> Result<WrappedKey, ProviderError> {
        self.observe(Op::Wrap, || self.inner.wrap(key, ctx, dek, opts))
    }

    fn unwrap(
        &self,
        wrapped: &WrappedKey,
        ctx: &KeyContext,
        opts: &CallOptions,
    ) -> Result<SecretKey, ProviderError> {
        self.observe(Op::Unwrap, || self.inner.unwrap(wrapped, ctx, opts))
    }

    fn destroy_key(&self, key: &KeyRef, opts: &CallOptions) -> Result<(), ProviderError> {
        self.observe(Op::Destroy, || self.inner.destroy_key(key, opts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_powers_of_two_and_clamp() {
        assert_eq!(Histogram::bucket(0), 0);
        assert_eq!(Histogram::bucket(1), 0);
        assert_eq!(Histogram::bucket(2), 1);
        assert_eq!(Histogram::bucket(3), 1);
        assert_eq!(Histogram::bucket(4), 2);
        assert_eq!(Histogram::bucket(1023), 9);
        assert_eq!(Histogram::bucket(1024), 10);
        assert_eq!(Histogram::bucket(u64::MAX), BUCKETS - 1);
    }

    #[test]
    fn quantiles_bound_the_true_value_and_never_exceed_the_maximum() {
        let mut h = Histogram::default();
        assert_eq!(h.quantile_us(0.5), None);
        for us in [100u64, 100, 100, 100, 100, 100, 100, 100, 100, 5_000] {
            h.add(Duration::from_micros(us));
        }
        assert_eq!(h.count, 10);
        assert_eq!(h.max_us, 5_000);
        // 100 us lives in [64,128): the median bound is 127.
        assert_eq!(h.quantile_us(0.5), Some(127));
        // The slowest call lives in [4096,8192) but the bound is clamped to the exact maximum.
        assert_eq!(h.quantile_us(1.0), Some(5_000));
        assert_eq!(h.quantile_us(0.99), Some(5_000));
        assert_eq!(h.sum_us, 9 * 100 + 5_000);
    }

    #[test]
    fn op_labels_are_stable() {
        let labels: Vec<_> = Op::ALL.iter().map(|o| o.as_str()).collect();
        assert_eq!(labels, ["key_info", "wrap", "unwrap", "destroy"]);
        assert_eq!(MeteredProvider::ops().len(), 4);
    }
}
