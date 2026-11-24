//! The per-login token bucket and the per-login connection cap (spec, decision 11). Lane S.
//!
//! Pure state: every method that depends on time takes `now_ms`, milliseconds on a monotonic clock
//! (the gateway's start is zero; tests pass a number). Nothing here reads a clock, locks or sleeps.
//! The session code keeps one [`RateLimiter`] and one [`Connections`] for the gateway copy behind a
//! mutex; counters are per gateway copy, as the spec says.

use std::collections::HashMap;
use std::time::Duration;

use bhdata_api::types::config::GatewayConfig;
use bhdata_api::types::Error;

/// One token is this many milli-tokens. Integer maths keeps the bucket exact: `rate` tokens a second
/// is exactly `rate` milli-tokens a millisecond.
const TOKEN: u64 = 1_000;

/// A request was refused by the bucket. The hint is how long until a token is free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimited {
    /// When the same request is likely to pass. The SDK waits this long times a random 1 to 1.5.
    pub retry_after: Duration,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Milli-tokens now, as of `at_ms`.
    tokens: u64,
    at_ms: u64,
    rate: u64,
    capacity: u64,
}

impl Bucket {
    fn full(rate: u32, burst: u32, now_ms: u64) -> Self {
        let capacity = u64::from(burst) * TOKEN;
        Bucket {
            tokens: capacity,
            at_ms: now_ms,
            rate: u64::from(rate),
            capacity,
        }
    }

    /// Brings the bucket up to `now_ms` with the given limits (a changed limit applies at once).
    fn refill(&mut self, rate: u32, burst: u32, now_ms: u64) {
        self.rate = u64::from(rate);
        self.capacity = u64::from(burst) * TOKEN;
        // A clock that goes backwards adds nothing and loses nothing.
        let elapsed = now_ms.saturating_sub(self.at_ms);
        self.at_ms = self.at_ms.max(now_ms);
        self.tokens = self
            .tokens
            .saturating_add(elapsed.saturating_mul(self.rate))
            .min(self.capacity);
    }
}

/// A token bucket per login.
#[derive(Debug)]
pub struct RateLimiter {
    default_rate: u32,
    default_burst: u32,
    buckets: HashMap<String, Bucket>,
}

impl RateLimiter {
    /// A limiter whose logins get `default_rate` requests a second with a burst of `default_burst`,
    /// unless a login record says otherwise. A zero is raised to one: a bucket that never fills would
    /// refuse a login for ever with a hint that never comes true.
    #[must_use]
    pub fn new(default_rate: u32, default_burst: u32) -> Self {
        RateLimiter {
            default_rate: default_rate.max(1),
            default_burst: default_burst.max(1),
            buckets: HashMap::new(),
        }
    }

    /// From `[gateway]`: `rate_per_sec` and `rate_burst`.
    #[must_use]
    pub fn from_config(config: &GatewayConfig) -> Self {
        RateLimiter::new(config.rate_per_sec, config.rate_burst)
    }

    /// Takes one token for `login`, or says how long until there is one. `rate` and `burst` are the
    /// login record's own limits (`None` means the default).
    ///
    /// The caller asks only for requests that passed the permission check, and every such request
    /// counts, including one that fails later (decision 11).
    ///
    /// # Errors
    /// [`RateLimited`] with the retry hint.
    pub fn check(
        &mut self,
        login: &str,
        rate: Option<u32>,
        burst: Option<u32>,
        now_ms: u64,
    ) -> Result<(), RateLimited> {
        let rate = rate.unwrap_or(self.default_rate).max(1);
        let burst = burst.unwrap_or(self.default_burst).max(1);
        let bucket = self
            .buckets
            .entry(login.to_owned())
            .or_insert_with(|| Bucket::full(rate, burst, now_ms));
        bucket.refill(rate, burst, now_ms);
        if bucket.tokens >= TOKEN {
            bucket.tokens -= TOKEN;
            return Ok(());
        }
        // Whole milliseconds until one token is there, rounded up and never zero.
        let missing = TOKEN - bucket.tokens;
        let wait_ms = missing.div_ceil(bucket.rate).max(1);
        Err(RateLimited {
            retry_after: Duration::from_millis(wait_ms),
        })
    }

    /// Forgets the buckets that have refilled completely: a full bucket is the same as a new one, so
    /// nothing is lost. The session calls this now and then so the map does not grow with the number of
    /// logins that ever called.
    pub fn sweep(&mut self, now_ms: u64) {
        self.buckets.retain(|_, b| {
            let elapsed = now_ms.saturating_sub(b.at_ms);
            b.tokens.saturating_add(elapsed.saturating_mul(b.rate)) < b.capacity
        });
    }

    /// How many logins have a bucket right now.
    #[must_use]
    pub fn tracked_logins(&self) -> usize {
        self.buckets.len()
    }
}

/// The count of open connections per login (decision 11: at most 32 by default).
#[derive(Debug)]
pub struct Connections {
    default_max: u32,
    retry_hint: Duration,
    open: HashMap<String, u32>,
}

impl Connections {
    /// A counter whose logins may hold `default_max` connections unless their record says otherwise.
    /// `retry_hint` goes with the `overloaded` that a refused connection gets.
    #[must_use]
    pub fn new(default_max: u32, retry_hint: Duration) -> Self {
        Connections {
            default_max,
            retry_hint,
            open: HashMap::new(),
        }
    }

    /// From `[gateway]`: `max_connections_per_login`.
    #[must_use]
    pub fn from_config(config: &GatewayConfig, retry_hint: Duration) -> Self {
        Connections::new(config.max_connections_per_login, retry_hint)
    }

    /// Counts a new connection for `login`, or refuses it. `max` is the record's own cap.
    ///
    /// # Errors
    /// [`Error::Overloaded`] when the login already holds its most.
    pub fn open(&mut self, login: &str, max: Option<u32>) -> Result<(), Error> {
        let max = max.unwrap_or(self.default_max);
        let held = self.open.get(login).copied().unwrap_or(0);
        if held >= max {
            return Err(Error::Overloaded {
                retry_after: self.retry_hint,
            });
        }
        *self.open.entry(login.to_owned()).or_insert(0) += 1;
        Ok(())
    }

    /// A connection of `login` ended. Closing one that was never counted does nothing.
    pub fn close(&mut self, login: &str) {
        if let Some(held) = self.open.get_mut(login) {
            *held -= 1;
            if *held == 0 {
                self.open.remove(login);
            }
        }
    }

    /// Connections `login` holds now.
    #[must_use]
    pub fn held(&self, login: &str) -> u32 {
        self.open.get(login).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_burst_passes_then_the_next_request_is_refused_with_a_hint() {
        let mut l = RateLimiter::new(200, 400);
        for _ in 0..400 {
            assert!(l.check("a", None, None, 0).is_ok());
        }
        // 200 a second is one token every 5 ms.
        assert_eq!(
            l.check("a", None, None, 0),
            Err(RateLimited { retry_after: ms(5) })
        );
    }

    #[test]
    fn the_hint_comes_true() {
        let mut l = RateLimiter::new(10, 1);
        assert!(l.check("a", None, None, 1_000).is_ok());
        let wait = l.check("a", None, None, 1_000).unwrap_err().retry_after;
        assert_eq!(wait, ms(100));
        // One millisecond early is still refused; on the hint it passes.
        assert!(l.check("a", None, None, 1_000 + 99).is_err());
        assert!(l.check("a", None, None, 1_000 + 100).is_ok());
    }

    #[test]
    fn the_bucket_refills_but_never_above_the_burst() {
        let mut l = RateLimiter::new(100, 3);
        for _ in 0..3 {
            assert!(l.check("a", None, None, 0).is_ok());
        }
        assert!(l.check("a", None, None, 0).is_err());
        // An hour later it holds 3 tokens, not 360,000.
        for _ in 0..3 {
            assert!(l.check("a", None, None, 3_600_000).is_ok());
        }
        assert!(l.check("a", None, None, 3_600_000).is_err());
    }

    #[test]
    fn logins_do_not_share_a_bucket() {
        let mut l = RateLimiter::new(1, 1);
        assert!(l.check("a", None, None, 0).is_ok());
        assert!(l.check("a", None, None, 0).is_err());
        assert!(l.check("b", None, None, 0).is_ok());
    }

    #[test]
    fn a_login_record_overrides_the_defaults_and_a_change_applies_at_once() {
        let mut l = RateLimiter::new(1, 1);
        // The record allows a burst of 5.
        for _ in 0..5 {
            assert!(l.check("a", Some(1), Some(5), 0).is_ok());
        }
        assert!(l.check("a", Some(1), Some(5), 0).is_err());
        // Cutting the burst to 1 also cuts what the bucket may hold.
        assert!(l.check("a", Some(1), Some(1), 10_000).is_ok());
        assert!(l.check("a", Some(1), Some(1), 10_000).is_err());
    }

    #[test]
    fn a_zero_limit_is_raised_to_one() {
        let mut l = RateLimiter::new(0, 0);
        assert!(l.check("a", None, None, 0).is_ok());
        assert_eq!(
            l.check("a", None, None, 0),
            Err(RateLimited {
                retry_after: ms(1_000)
            })
        );
        assert!(l.check("a", Some(0), Some(0), 1_000).is_ok());
    }

    #[test]
    fn a_clock_that_goes_backwards_gives_nothing_and_does_not_panic() {
        let mut l = RateLimiter::new(1_000, 1);
        assert!(l.check("a", None, None, 5_000).is_ok());
        assert!(l.check("a", None, None, 1_000).is_err());
        // Forward again from the latest time seen, not from the earlier one.
        assert!(l.check("a", None, None, 5_001).is_ok());
    }

    #[test]
    fn the_largest_limits_do_not_overflow() {
        let mut l = RateLimiter::new(u32::MAX, u32::MAX);
        assert!(l.check("a", None, None, 0).is_ok());
        assert!(l.check("a", None, None, u64::MAX).is_ok());
    }

    #[test]
    fn sweep_forgets_only_full_buckets() {
        let mut l = RateLimiter::new(1, 2);
        assert!(l.check("idle", None, None, 0).is_ok());
        assert!(l.check("busy", None, None, 0).is_ok());
        assert!(l.check("busy", None, None, 0).is_ok());
        // After 1 s `idle` (1 of 2 used, 1 token a second) is full; `busy` (both used) is not.
        l.sweep(1_000);
        assert_eq!(l.tracked_logins(), 1);
        // The forgotten one behaves as new; the kept one keeps its debt.
        assert!(l.check("idle", None, None, 1_000).is_ok());
        assert!(l.check("busy", None, None, 1_000).is_ok());
        assert!(l.check("busy", None, None, 1_000).is_err());
    }

    #[test]
    fn it_follows_the_config() {
        let g = GatewayConfig::default();
        let mut l = RateLimiter::from_config(&g);
        for _ in 0..g.rate_burst {
            assert!(l.check("a", None, None, 0).is_ok());
        }
        assert!(l.check("a", None, None, 0).is_err());
    }

    #[test]
    fn connections_stop_at_the_cap_and_free_a_place_on_close() {
        let mut c = Connections::new(2, ms(1_000));
        assert!(c.open("a", None).is_ok());
        assert!(c.open("a", None).is_ok());
        assert_eq!(
            c.open("a", None),
            Err(Error::Overloaded {
                retry_after: ms(1_000)
            })
        );
        assert_eq!(c.held("a"), 2);
        c.close("a");
        assert!(c.open("a", None).is_ok());
        // Another login has its own count.
        assert!(c.open("b", None).is_ok());
    }

    #[test]
    fn a_record_cap_overrides_the_default() {
        let mut c = Connections::new(32, ms(1));
        assert!(c.open("a", Some(1)).is_ok());
        assert!(c.open("a", Some(1)).is_err());
        assert!(c.open("a", Some(2)).is_ok());
        assert!(Connections::new(0, ms(1)).open("a", None).is_err());
    }

    #[test]
    fn closing_an_uncounted_connection_does_nothing_and_a_zero_count_is_forgotten() {
        let mut c = Connections::new(1, ms(1));
        c.close("never");
        assert_eq!(c.held("never"), 0);
        assert!(c.open("a", None).is_ok());
        c.close("a");
        c.close("a");
        assert_eq!(c.held("a"), 0);
        assert!(c.open.is_empty());
    }
}
