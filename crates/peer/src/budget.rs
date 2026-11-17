//! Process-wide caps shared by every connection.
//!
//! Three caps bound what an open port can cost: connections (each holds a thread, a socket and a
//! TLS session), queued frames and queued bytes (bodies accepted and not yet answered). Permits give
//! their share back when dropped, so a connection that dies mid-frame cannot leak any of it. The
//! high-water marks are what the "budgets are reported" criterion reads: they show what the caps
//! actually held under load, not what they were set to.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The caps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetLimits {
    /// Open connections.
    pub connections: u64,
    /// Frames accepted and not yet answered, in all.
    pub frames: u64,
    /// Body bytes accepted and not yet answered, in all.
    pub bytes: u64,
}

#[derive(Default)]
struct Counters {
    connections: AtomicU64,
    frames: AtomicU64,
    bytes: AtomicU64,
    peak_connections: AtomicU64,
    peak_frames: AtomicU64,
    peak_bytes: AtomicU64,
    refused_connections: AtomicU64,
    refused_frames: AtomicU64,
}

/// The shared budget. Cloning shares it.
#[derive(Clone)]
pub struct Budget {
    limits: BudgetLimits,
    counters: Arc<Counters>,
}

/// What the budget held.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetReport {
    /// Most connections open at once.
    pub peak_connections: u64,
    /// Most frames queued at once.
    pub peak_frames: u64,
    /// Most body bytes queued at once.
    pub peak_bytes: u64,
    /// Connections refused at the cap.
    pub refused_connections: u64,
    /// Frames shed at a cap.
    pub refused_frames: u64,
    /// Connections open now.
    pub connections: u64,
    /// Frames queued now.
    pub frames: u64,
    /// Body bytes queued now.
    pub bytes: u64,
}

/// A connection's share. Dropping it gives the share back.
#[must_use]
pub struct ConnectionPermit(Arc<Counters>);

/// A frame's share. Dropping it gives the share back.
#[must_use]
pub struct FramePermit {
    counters: Arc<Counters>,
    bytes: u64,
}

fn claim(cell: &AtomicU64, peak: &AtomicU64, amount: u64, cap: u64) -> bool {
    let mut seen = cell.load(Relaxed);
    loop {
        let Some(next) = seen.checked_add(amount).filter(|n| *n <= cap) else {
            return false;
        };
        match cell.compare_exchange_weak(seen, next, Relaxed, Relaxed) {
            Ok(_) => {
                peak.fetch_max(next, Relaxed);
                return true;
            }
            Err(now) => seen = now,
        }
    }
}

impl Budget {
    /// A budget with `limits`.
    #[must_use]
    pub fn new(limits: BudgetLimits) -> Self {
        Self {
            limits,
            counters: Arc::new(Counters::default()),
        }
    }

    /// The caps.
    #[must_use]
    pub fn limits(&self) -> BudgetLimits {
        self.limits
    }

    /// A connection's share, or `None` at the cap.
    pub fn connection(&self) -> Option<ConnectionPermit> {
        let c = &self.counters;
        if claim(
            &c.connections,
            &c.peak_connections,
            1,
            self.limits.connections,
        ) {
            Some(ConnectionPermit(c.clone()))
        } else {
            c.refused_connections.fetch_add(1, Relaxed);
            None
        }
    }

    /// A frame of `bytes` of body, or `None` if either cap would be passed. All or nothing.
    pub fn frame(&self, bytes: u64) -> Option<FramePermit> {
        let c = &self.counters;
        if !claim(&c.frames, &c.peak_frames, 1, self.limits.frames) {
            c.refused_frames.fetch_add(1, Relaxed);
            return None;
        }
        if !claim(&c.bytes, &c.peak_bytes, bytes, self.limits.bytes) {
            c.frames.fetch_sub(1, Relaxed);
            c.refused_frames.fetch_add(1, Relaxed);
            return None;
        }
        Some(FramePermit {
            counters: c.clone(),
            bytes,
        })
    }

    /// What the budget holds and held.
    #[must_use]
    pub fn report(&self) -> BudgetReport {
        let c = &self.counters;
        BudgetReport {
            peak_connections: c.peak_connections.load(Relaxed),
            peak_frames: c.peak_frames.load(Relaxed),
            peak_bytes: c.peak_bytes.load(Relaxed),
            refused_connections: c.refused_connections.load(Relaxed),
            refused_frames: c.refused_frames.load(Relaxed),
            connections: c.connections.load(Relaxed),
            frames: c.frames.load(Relaxed),
            bytes: c.bytes.load(Relaxed),
        }
    }
}

impl std::fmt::Debug for FramePermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FramePermit({} bytes)", self.bytes)
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Relaxed);
    }
}

impl Drop for FramePermit {
    fn drop(&mut self) {
        self.counters.frames.fetch_sub(1, Relaxed);
        self.counters.bytes.fetch_sub(self.bytes, Relaxed);
    }
}
