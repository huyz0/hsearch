//! What a module's callbacks reach: per-call cancellation and a bounded log sink.

use crate::sys::Callbacks;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// A module log line after the host bounded it, handed to the node's sink.
pub struct LogRecord<'a> {
    /// The module's declared name.
    pub module: &'a str,
    /// The call it was logged from.
    pub call_id: u64,
    /// `HS_LOG_*` as the module gave it.
    pub level: u32,
    /// At most 1 KiB, invalid UTF-8 replaced.
    pub message: &'a str,
}

/// Where module log lines go. Called on the worker thread that is running the
/// module, so it must not block.
pub type LogSink = Arc<dyn Fn(&LogRecord<'_>) + Send + Sync>;

/// A call slot's cancellation flag and the call id that owns it.
pub(crate) struct CancelSlot {
    id: AtomicU64,
    flag: AtomicBool,
}

impl CancelSlot {
    pub(crate) const fn new() -> Self {
        Self {
            id: AtomicU64::new(0),
            flag: AtomicBool::new(false),
        }
    }

    /// Hands the slot to a new call.
    pub(crate) fn begin(&self, call_id: u64) {
        self.flag.store(false, Ordering::Release);
        self.id.store(call_id, Ordering::Release);
    }

    /// The call returned; any later `cancelled` for its id answers "gone".
    pub(crate) fn end(&self) {
        self.id.store(0, Ordering::Release);
    }

    /// The caller gave up on the running call.
    pub(crate) fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }

    /// An id that no longer owns the slot is a call whose caller is gone.
    fn answer(&self, call_id: u64) -> bool {
        self.id.load(Ordering::Acquire) != call_id || self.flag.load(Ordering::Acquire)
    }
}

/// The low 16 bits of a call id name its slot, the rest is a sequence number.
pub(crate) const SLOT_BITS: u32 = 16;

pub(crate) struct HostState {
    name: OnceLock<String>,
    pub(crate) slots: Box<[CancelSlot]>,
    sink: Option<LogSink>,
    window: Mutex<(Instant, u32)>,
    per_second: u32,
    dropped: AtomicU64,
}

impl HostState {
    /// The table is sized before the module is initialised, because the module
    /// keeps the host struct from then on and the descriptor is not known yet.
    pub(crate) fn new(slots: usize, sink: Option<LogSink>, per_second: u32) -> Self {
        Self {
            name: OnceLock::new(),
            slots: (0..slots).map(|_| CancelSlot::new()).collect(),
            sink,
            window: Mutex::new((Instant::now(), 0)),
            per_second,
            dropped: AtomicU64::new(0),
        }
    }

    pub(crate) fn set_name(&self, name: &str) {
        let _ = self.name.set(name.to_owned());
    }

    pub(crate) fn dropped_logs(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn admit(&self) -> bool {
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        if now.duration_since(window.0) >= Duration::from_secs(1) {
            *window = (now, 0);
        }
        if window.1 >= self.per_second {
            return false;
        }
        window.1 += 1;
        true
    }
}

/// The `Callbacks` the module is handed; a thin owner of the shared state.
pub(crate) struct SharedCallbacks(pub(crate) Arc<HostState>);

impl Callbacks for SharedCallbacks {
    fn cancelled(&self, call_id: u64) -> bool {
        let slot = (call_id & ((1 << SLOT_BITS) - 1)) as usize;
        self.0.slots.get(slot).is_none_or(|s| s.answer(call_id))
    }

    fn log(&self, call_id: u64, level: u32, message: &[u8]) {
        let Some(sink) = &self.0.sink else { return };
        if !self.admit_or_count() {
            return;
        }
        let text = String::from_utf8_lossy(message);
        sink(&LogRecord {
            module: self.0.name.get().map_or("", String::as_str),
            call_id,
            level,
            message: &text,
        });
    }
}

impl SharedCallbacks {
    fn admit_or_count(&self) -> bool {
        let ok = self.0.admit();
        if !ok {
            self.0.dropped.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_follows_the_owning_call_id() {
        let state = Arc::new(HostState::new(4, None, 10));
        let cb = SharedCallbacks(Arc::clone(&state));
        let id = (7u64 << SLOT_BITS) | 2;
        assert!(cb.cancelled(id), "an idle slot means the call is gone");
        state.slots[2].begin(id);
        assert!(!cb.cancelled(id));
        assert!(
            cb.cancelled((8u64 << SLOT_BITS) | 2),
            "a different id for the slot is not this call"
        );
        state.slots[2].cancel();
        assert!(cb.cancelled(id));
        state.slots[2].end();
        state.slots[2].begin(id + (1 << SLOT_BITS));
        assert!(cb.cancelled(id), "the old id no longer owns the slot");
        assert!(cb.cancelled(3), "a slot beyond the table is gone");
    }

    #[test]
    fn log_is_rate_limited_and_counted() {
        let seen = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&seen);
        let sink: LogSink = Arc::new(move |r| {
            assert_eq!((r.module, r.level, r.message), ("m", 2, "hi\u{fffd}"));
            counter.fetch_add(1, Ordering::Relaxed);
        });
        let state = Arc::new(HostState::new(1, Some(sink), 3));
        state.set_name("m");
        let cb = SharedCallbacks(Arc::clone(&state));
        for _ in 0..5 {
            cb.log(1, 2, &[b'h', b'i', 0xff]);
        }
        assert_eq!(seen.load(Ordering::Relaxed), 3);
        assert_eq!(state.dropped_logs(), 2);
        let silent = SharedCallbacks(Arc::new(HostState::new(1, None, 3)));
        silent.log(1, 2, b"x");
        assert_eq!(silent.0.dropped_logs(), 0);
    }
}
