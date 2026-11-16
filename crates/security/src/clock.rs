//! Time, injectable. Lifetimes of cached keys use the monotonic reading, so stepping the wall clock
//! backwards cannot extend a key's life; audit records use the wall clock.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// A source of time.
pub trait Clock: Send + Sync {
    /// Monotonic time since an arbitrary origin.
    fn monotonic(&self) -> Duration;
    /// Wall-clock time.
    fn wall(&self) -> SystemTime;
}

/// The real clock.
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn monotonic(&self) -> Duration {
        self.origin.elapsed()
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock a test moves by hand.
#[derive(Default)]
pub struct ManualClock {
    offset: Mutex<Duration>,
}

impl ManualClock {
    /// A clock at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves time forward.
    pub fn advance(&self, by: Duration) {
        *self.offset.lock().unwrap_or_else(|p| p.into_inner()) += by;
    }
}

impl Clock for ManualClock {
    fn monotonic(&self) -> Duration {
        *self.offset.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn wall(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000) + self.monotonic()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manual_clock_moves_only_when_told() {
        let clock = ManualClock::new();
        let (m0, w0) = (clock.monotonic(), clock.wall());
        assert_eq!((clock.monotonic(), clock.wall()), (m0, w0));
        clock.advance(Duration::from_secs(5));
        assert_eq!(clock.monotonic() - m0, Duration::from_secs(5));
        assert_eq!(
            clock.wall().duration_since(w0).unwrap(),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn the_system_clock_is_monotonic_and_wall_is_recent() {
        let clock = SystemClock::default();
        let a = clock.monotonic();
        assert!(clock.monotonic() >= a);
        assert!(
            clock.wall().duration_since(SystemTime::UNIX_EPOCH).unwrap()
                > Duration::from_secs(1_700_000_000)
        );
    }
}
