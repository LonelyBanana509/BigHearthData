//! Admission control and the commit-latency watch (spec `write-path.md`, section 7).
//!
//! Both are plain values that are told the time (a `Duration` on the clock's monotonic scale), so
//! they run on a fake clock in tests. A command that is refused here did no database work and has
//! no record.

use std::collections::VecDeque;
use std::time::Duration;

use bhdata_pg::errors::OVERLOAD_RETRY_AFTER;

/// The longest the retry hint grows to, as a multiple of the base hint.
const MAX_HINT_FACTOR: u32 = 8;
/// A queued command that has waited this many times `max_queue_wait` is dropped at the front.
const DROP_FACTOR: u32 = 4;

/// What the queue looks like right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueState {
    /// Commands waiting.
    pub len: usize,
    /// How long the oldest has waited (`None` if the queue is empty).
    pub oldest_wait: Option<Duration>,
}

/// The decision for a new command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Queue it.
    Admit,
    /// Refuse it with `overloaded` and this hint.
    Reject {
        /// The `retry_after` of the reply.
        retry_after: Duration,
    },
}

/// The admission rule of one shard's queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admission {
    /// Commands that may wait.
    pub capacity: usize,
    /// The longest the oldest queued command may have waited before new ones are refused.
    pub max_wait: Duration,
}

impl Admission {
    /// Decides for a new command. Refused when the queue is full, when the oldest queued command
    /// has waited longer than `max_wait` (a queue that is not full but not moving is overload too),
    /// or when the commit-latency alarm is on and the queue is more than half full.
    #[must_use]
    pub fn decide(&self, queue: QueueState, latency_alarm: bool) -> Verdict {
        let full = queue.len >= self.capacity;
        let stuck = queue.oldest_wait.is_some_and(|w| w > self.max_wait);
        let slow_disk = latency_alarm && queue.len.saturating_mul(2) > self.capacity;
        if full || stuck || slow_disk {
            Verdict::Reject {
                retry_after: self.hint(queue),
            }
        } else {
            Verdict::Admit
        }
    }

    /// The hint grows with how far over its limits the queue is: the base hint times the larger of
    /// "queue length over capacity" and "oldest wait over `max_wait`", rounded up, from 1 to 8.
    #[must_use]
    pub fn hint(&self, queue: QueueState) -> Duration {
        let by_len = ratio_ceil(queue.len as u128, self.capacity as u128);
        let by_wait = queue
            .oldest_wait
            .map_or(0, |w| ratio_ceil(w.as_nanos(), self.max_wait.as_nanos()));
        let factor = by_len.max(by_wait).clamp(1, u128::from(MAX_HINT_FACTOR));
        OVERLOAD_RETRY_AFTER * u32::try_from(factor).unwrap_or(MAX_HINT_FACTOR)
    }

    /// Whether a command that was admitted and has waited `waited` is dropped at the front of the
    /// queue, so a stuck shard does not hold requests for minutes.
    #[must_use]
    pub fn drop_waiting(&self, waited: Duration) -> bool {
        waited > self.max_wait * DROP_FACTOR
    }
}

/// `ceil(n / d)`, with `d == 0` giving the largest value (everything is over a zero limit).
fn ratio_ceil(n: u128, d: u128) -> u128 {
    if d == 0 {
        u128::MAX
    } else {
        n.div_ceil(d)
    }
}

/// The span the latency watch looks back over.
pub const WATCH_WINDOW: Duration = Duration::from_secs(10);
/// The most samples kept; when more commits than this happen in one window, the oldest are dropped
/// (the 99th percentile of the newest 4,096 is still the 99th percentile of the busiest moment).
pub const WATCH_MAX_SAMPLES: usize = 4_096;

/// The 99th-percentile `COMMIT` time over the last 10 seconds, and whether it is above the alarm.
#[derive(Debug, Clone)]
pub struct CommitLatency {
    alarm_above: Duration,
    samples: VecDeque<(Duration, Duration)>,
}

impl CommitLatency {
    /// A watch that raises its alarm above `alarm_above`.
    #[must_use]
    pub fn new(alarm_above: Duration) -> Self {
        CommitLatency {
            alarm_above,
            samples: VecDeque::new(),
        }
    }

    /// Records a commit that finished at `at` (monotonic time) and took `took`.
    pub fn record(&mut self, at: Duration, took: Duration) {
        self.expire(at);
        if self.samples.len() == WATCH_MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((at, took));
    }

    fn expire(&mut self, now: Duration) {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) > WATCH_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// The 99th percentile at `now`, or `None` with no commit in the window.
    pub fn p99(&mut self, now: Duration) -> Option<Duration> {
        self.expire(now);
        if self.samples.is_empty() {
            return None;
        }
        let mut took: Vec<Duration> = self.samples.iter().map(|(_, t)| *t).collect();
        took.sort_unstable();
        // Nearest rank: the smallest value with at least 99% of the samples at or below it.
        let rank = (took.len() * 99).div_ceil(100).max(1);
        took.get(rank - 1).copied()
    }

    /// Whether the alarm is on at `now`. With no commits in the window it is off, which is how it
    /// clears once the load (or the slow disk) is gone.
    pub fn alarmed(&mut self, now: Duration) -> bool {
        self.p99(now).is_some_and(|p| p > self.alarm_above)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn adm() -> Admission {
        Admission {
            capacity: 256,
            max_wait: MS(500),
        }
    }

    fn q(len: usize, oldest_ms: Option<u64>) -> QueueState {
        QueueState {
            len,
            oldest_wait: oldest_ms.map(MS),
        }
    }

    #[test]
    fn an_empty_or_moving_queue_admits() {
        assert_eq!(adm().decide(q(0, None), false), Verdict::Admit);
        assert_eq!(adm().decide(q(255, Some(500)), false), Verdict::Admit);
    }

    #[test]
    fn a_full_queue_refuses() {
        assert!(matches!(
            adm().decide(q(256, Some(10)), false),
            Verdict::Reject { .. }
        ));
    }

    #[test]
    fn a_queue_half_empty_but_not_moving_refuses() {
        assert!(matches!(
            adm().decide(q(100, Some(501)), false),
            Verdict::Reject { .. }
        ));
        assert_eq!(adm().decide(q(100, Some(500)), false), Verdict::Admit);
    }

    #[test]
    fn the_latency_alarm_only_matters_when_the_queue_is_over_half_full() {
        assert_eq!(adm().decide(q(128, Some(1)), true), Verdict::Admit);
        assert!(matches!(
            adm().decide(q(129, Some(1)), true),
            Verdict::Reject { .. }
        ));
        assert_eq!(adm().decide(q(129, Some(1)), false), Verdict::Admit);
    }

    #[test]
    fn the_hint_grows_with_the_overload_and_stops_at_eight_times_the_base() {
        let hint = |len, w| adm().hint(q(len, w));
        let base = OVERLOAD_RETRY_AFTER;
        assert_eq!(hint(256, Some(10)), base);
        assert_eq!(hint(100, Some(501)), base * 2);
        assert_eq!(hint(10, Some(1_000)), base * 2);
        assert_eq!(hint(10, Some(1_001)), base * 3);
        assert_eq!(hint(10, Some(60_000)), base * 8);
        let mut last = Duration::ZERO;
        for w in (500..5_000).step_by(100) {
            let h = hint(10, Some(w));
            assert!(h >= last, "{w}");
            last = h;
        }
    }

    #[test]
    fn a_rejection_carries_the_hint() {
        let verdict = adm().decide(q(100, Some(1_200)), false);
        assert_eq!(
            verdict,
            Verdict::Reject {
                retry_after: OVERLOAD_RETRY_AFTER * 3
            }
        );
    }

    #[test]
    fn a_zero_limit_refuses_everything_with_the_largest_hint() {
        let a = Admission {
            capacity: 0,
            max_wait: Duration::ZERO,
        };
        assert_eq!(
            a.decide(q(0, None), false),
            Verdict::Reject {
                retry_after: OVERLOAD_RETRY_AFTER * MAX_HINT_FACTOR
            }
        );
    }

    #[test]
    fn a_command_is_dropped_after_four_times_the_limit() {
        assert!(!adm().drop_waiting(MS(2_000)));
        assert!(adm().drop_waiting(MS(2_001)));
    }

    #[test]
    fn p99_uses_the_nearest_rank() {
        let mut w = CommitLatency::new(MS(250));
        let t = Duration::from_secs(1);
        for i in 1..=100u64 {
            w.record(t, MS(i));
        }
        assert_eq!(w.p99(t), Some(MS(99)));
        w.record(t, MS(1_000));
        // 101 samples: rank ceil(101 * 0.99) = 100, the 100th smallest is 100 ms.
        assert_eq!(w.p99(t), Some(MS(100)));
        assert_eq!(CommitLatency::new(MS(1)).p99(t), None);
    }

    #[test]
    fn the_alarm_rises_with_slow_commits_and_clears_when_they_age_out() {
        let mut w = CommitLatency::new(MS(250));
        let mut now = Duration::ZERO;
        // Normal load: 5 ms commits.
        for _ in 0..200 {
            now += MS(10);
            w.record(now, MS(5));
        }
        assert!(!w.alarmed(now));
        // A slow disk: 400 ms commits take over the 99th percentile.
        for _ in 0..30 {
            now += MS(100);
            w.record(now, MS(400));
        }
        assert!(w.alarmed(now));
        // The disk recovers: fast commits push the slow ones out of the 10 s window.
        for _ in 0..1_100 {
            now += MS(10);
            w.record(now, MS(5));
        }
        assert!(!w.alarmed(now));
        // With no commits at all, an old alarm does not stay on. (One slow commit among a thousand
        // fast ones does not move the 99th percentile, so this uses a window of slow ones only.)
        let mut idle = CommitLatency::new(MS(250));
        idle.record(now, MS(900));
        assert!(idle.alarmed(now));
        assert!(!idle.alarmed(now + WATCH_WINDOW + MS(1)));
    }

    #[test]
    fn memory_is_bounded() {
        let mut w = CommitLatency::new(MS(250));
        for i in 0..(WATCH_MAX_SAMPLES * 3) {
            w.record(MS(i as u64 % 1_000), MS(1));
        }
        assert!(w.samples.len() <= WATCH_MAX_SAMPLES);
    }
}
