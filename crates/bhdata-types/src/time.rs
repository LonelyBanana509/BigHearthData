//! Time, behind a trait so tests control it.
//!
//! Code that needs "now" takes a [`Clock`] instead of calling the system clock, so a test can move
//! time by hand with [`ManualClock`] (retry-window expiry, admission control, lease timeouts).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// A point in wall-clock time: milliseconds since the Unix epoch.
///
/// Requests carry their creation time as a `Timestamp`, so an expired retry is rejected rather than
/// re-applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The epoch itself.
    pub const EPOCH: Timestamp = Timestamp(0);

    /// Wraps a number of milliseconds since the Unix epoch.
    #[must_use]
    pub const fn from_unix_millis(ms: u64) -> Self {
        Timestamp(ms)
    }

    /// Milliseconds since the Unix epoch.
    #[must_use]
    pub const fn as_unix_millis(self) -> u64 {
        self.0
    }

    /// This time plus `d`, stopping at the largest representable time instead of wrapping.
    #[must_use]
    pub fn saturating_add(self, d: Duration) -> Timestamp {
        Timestamp(self.0.saturating_add(millis(d)))
    }

    /// How long after `earlier` this time is; zero if `earlier` is not earlier.
    #[must_use]
    pub fn saturating_duration_since(self, earlier: Timestamp) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

/// A source of time.
pub trait Clock: Send + Sync + fmt::Debug {
    /// Wall-clock time. Can jump; use for creation times and expiry, never to measure a wait.
    fn now(&self) -> Timestamp;

    /// Time since an arbitrary fixed start, never going backwards. Use to measure how long
    /// something waited (for example the oldest queued write).
    fn monotonic(&self) -> Duration;
}

/// The real clock.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    started: Instant,
}

impl SystemClock {
    /// Starts a clock whose [`Clock::monotonic`] counts from now.
    #[must_use]
    pub fn new() -> Self {
        SystemClock {
            started: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        Timestamp(millis(since_epoch))
    }

    fn monotonic(&self) -> Duration {
        self.started.elapsed()
    }
}

/// A clock that only moves when told to. For tests.
#[derive(Debug)]
pub struct ManualClock {
    start: Timestamp,
    elapsed_ms: AtomicU64,
}

impl ManualClock {
    /// A clock showing `start`, with zero monotonic time elapsed.
    #[must_use]
    pub fn new(start: Timestamp) -> Self {
        ManualClock {
            start,
            elapsed_ms: AtomicU64::new(0),
        }
    }

    /// Moves both readings forward by `by`.
    pub fn advance(&self, by: Duration) {
        let add = millis(by);
        let mut current = self.elapsed_ms.load(Ordering::SeqCst);
        loop {
            let next = current.saturating_add(add);
            match self.elapsed_ms.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        self.start.saturating_add(Duration::from_millis(
            self.elapsed_ms.load(Ordering::SeqCst),
        ))
    }

    fn monotonic(&self) -> Duration {
        Duration::from_millis(self.elapsed_ms.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn timestamp_arithmetic_saturates() {
        let t = Timestamp::from_unix_millis(1_000);
        assert_eq!(
            t.saturating_add(Duration::from_secs(2)).as_unix_millis(),
            3_000
        );
        assert_eq!(
            Timestamp::from_unix_millis(u64::MAX)
                .saturating_add(Duration::from_secs(1))
                .as_unix_millis(),
            u64::MAX
        );
        assert_eq!(t.saturating_add(Duration::MAX).as_unix_millis(), u64::MAX);
        let later = Timestamp::from_unix_millis(4_000);
        assert_eq!(later.saturating_duration_since(t), Duration::from_secs(3));
        assert_eq!(t.saturating_duration_since(later), Duration::ZERO);
    }

    #[test]
    fn manual_clock_moves_only_when_told() {
        let clock = ManualClock::new(Timestamp::from_unix_millis(10_000));
        assert_eq!(clock.now().as_unix_millis(), 10_000);
        assert_eq!(clock.monotonic(), Duration::ZERO);
        clock.advance(Duration::from_secs(90));
        assert_eq!(clock.now().as_unix_millis(), 100_000);
        assert_eq!(clock.monotonic(), Duration::from_secs(90));
    }

    #[test]
    fn a_clock_can_be_shared_as_a_trait_object() {
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(Timestamp::EPOCH));
        let other = Arc::clone(&clock);
        assert_eq!(clock.now(), other.now());
    }

    #[test]
    fn system_clock_is_after_2020_and_monotonic_does_not_go_back() {
        let clock = SystemClock::new();
        assert!(clock.now().as_unix_millis() > 1_577_836_800_000);
        let a = clock.monotonic();
        let b = clock.monotonic();
        assert!(b >= a);
    }
}
