//! Process-wide caps shared by every connection.
//!
//! Three caps bound what an open port can cost: connections (each holds a thread, a socket and a
//! TLS session), queued frames and queued bytes (bodies accepted and not yet answered). Permits give
//! their share back when dropped, so a connection that dies mid-frame cannot leak any of it. The
//! high-water marks are what the "budgets are reported" criterion reads: they show what the caps
//! actually held under load, not what they were set to.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};

/// The caps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetLimits {
    /// Open connections, authenticated or not.
    pub connections: u64,
    /// Of those, connections still in their TLS handshake. A flood of sockets that never
    /// authenticate can hold at most this many of `connections`, so peers that did authenticate keep
    /// the rest.
    pub handshaking: u64,
    /// Frames accepted and not yet answered, in all.
    pub frames: u64,
    /// Body bytes accepted and not yet answered, in all.
    pub bytes: u64,
}

#[derive(Default)]
struct Frames {
    frames: u64,
    bytes: u64,
    peak_frames: u64,
    peak_bytes: u64,
    refused: u64,
}

#[derive(Default)]
struct Counters {
    connections: AtomicU64,
    peak_connections: AtomicU64,
    refused_connections: AtomicU64,
    handshaking: AtomicU64,
    peak_handshaking: AtomicU64,
    /// Frames and bytes move together under one lock, so a refusal never leaves a trace in a peak
    /// and a concurrent claim is never refused because of another's rolled-back attempt.
    frames: Mutex<Frames>,
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
    /// Most connections in their handshake at once.
    pub peak_handshaking: u64,
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

/// A connection's place in the handshake. Dropping it (when the handshake ends) gives it back.
#[must_use]
pub struct HandshakePermit(Arc<Counters>);

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

    /// A connection's share and its place in the handshake, or `None` at either cap.
    pub fn connection(&self) -> Option<(ConnectionPermit, HandshakePermit)> {
        let c = &self.counters;
        if !claim(
            &c.handshaking,
            &c.peak_handshaking,
            1,
            self.limits.handshaking,
        ) {
            c.refused_connections.fetch_add(1, Relaxed);
            return None;
        }
        if claim(
            &c.connections,
            &c.peak_connections,
            1,
            self.limits.connections,
        ) {
            Some((ConnectionPermit(c.clone()), HandshakePermit(c.clone())))
        } else {
            c.handshaking.fetch_sub(1, Relaxed);
            c.refused_connections.fetch_add(1, Relaxed);
            None
        }
    }

    /// A frame of `bytes` of body, or `None` if either cap would be passed. All or nothing.
    pub fn frame(&self, bytes: u64) -> Option<FramePermit> {
        let mut f = self
            .counters
            .frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let frames = f.frames.checked_add(1).filter(|n| *n <= self.limits.frames);
        let total = f
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= self.limits.bytes);
        let (Some(frames), Some(total)) = (frames, total) else {
            f.refused += 1;
            return None;
        };
        f.frames = frames;
        f.bytes = total;
        f.peak_frames = f.peak_frames.max(frames);
        f.peak_bytes = f.peak_bytes.max(total);
        Some(FramePermit {
            counters: self.counters.clone(),
            bytes,
        })
    }

    /// What the budget holds and held.
    #[must_use]
    pub fn report(&self) -> BudgetReport {
        let c = &self.counters;
        let f = c.frames.lock().unwrap_or_else(PoisonError::into_inner);
        BudgetReport {
            peak_connections: c.peak_connections.load(Relaxed),
            peak_handshaking: c.peak_handshaking.load(Relaxed),
            peak_frames: f.peak_frames,
            peak_bytes: f.peak_bytes,
            refused_connections: c.refused_connections.load(Relaxed),
            refused_frames: f.refused,
            connections: c.connections.load(Relaxed),
            frames: f.frames,
            bytes: f.bytes,
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

impl Drop for HandshakePermit {
    fn drop(&mut self) {
        self.0.handshaking.fetch_sub(1, Relaxed);
    }
}

impl Drop for FramePermit {
    fn drop(&mut self) {
        let mut f = self
            .counters
            .frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        f.frames = f.frames.saturating_sub(1);
        f.bytes = f.bytes.saturating_sub(self.bytes);
    }
}
