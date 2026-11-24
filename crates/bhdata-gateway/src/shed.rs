//! The gateway-wide in-flight count and the `overloaded` rule (spec, decision 9). Lane S.
//!
//! One [`Shed`] is shared by every connection task (it is `Clone`, and the copies share one count).
//! A request that passed the permission and rate-limit checks asks [`Shed::admit`] for a [`Permit`];
//! the permit gives its place back when it is dropped, however the request ends. A refused request did
//! no work, so a retry with the same request id is safe.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bhdata_api::types::config::GatewayConfig;
use bhdata_api::types::Error;

/// The hint sent with `overloaded` when the gateway is over its in-flight cap. A placeholder until M11.
pub const DEFAULT_RETRY_HINT: Duration = Duration::from_millis(100);

#[derive(Debug)]
struct Inner {
    max_total: u64,
    retry_hint: Duration,
    drain_hint: Duration,
    in_flight: AtomicU64,
    draining: AtomicBool,
}

/// The shared in-flight counter.
#[derive(Debug, Clone)]
pub struct Shed {
    inner: Arc<Inner>,
}

/// One admitted request. Dropping it frees its place.
#[derive(Debug)]
#[must_use = "a permit holds a place until it is dropped"]
pub struct Permit {
    inner: Arc<Inner>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.inner.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Shed {
    /// A counter that admits at most `max_total` requests at once. While draining (see
    /// [`Shed::start_draining`]) the hint is `drain_hint`.
    #[must_use]
    pub fn new(max_total: u32, retry_hint: Duration, drain_hint: Duration) -> Self {
        Shed {
            inner: Arc::new(Inner {
                max_total: u64::from(max_total),
                retry_hint,
                drain_hint,
                in_flight: AtomicU64::new(0),
                draining: AtomicBool::new(false),
            }),
        }
    }

    /// From `[gateway]`: `max_inflight_total`, the default hint, and `drain_ms` for the drain hint.
    #[must_use]
    pub fn from_config(config: &GatewayConfig) -> Self {
        Shed::new(
            config.max_inflight_total,
            DEFAULT_RETRY_HINT,
            Duration::from_millis(config.drain_ms),
        )
    }

    /// Takes one place, or answers `overloaded` with the hint.
    ///
    /// # Errors
    /// [`Error::Overloaded`] when the gateway already has `max_inflight_total` requests open, or is
    /// draining for shutdown (spec, section 8).
    pub fn admit(&self) -> Result<Permit, Error> {
        let inner = &self.inner;
        if inner.draining.load(Ordering::Acquire) {
            return Err(Error::Overloaded {
                retry_after: inner.drain_hint,
            });
        }
        // Take a place first, then give it back if it was over the cap: no lock, and the count never
        // lets more than `max_total` permits live at once.
        let before = inner.in_flight.fetch_add(1, Ordering::AcqRel);
        if before >= inner.max_total {
            inner.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(Error::Overloaded {
                retry_after: inner.retry_hint,
            });
        }
        Ok(Permit {
            inner: Arc::clone(inner),
        })
    }

    /// Requests open right now.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.inner.in_flight.load(Ordering::Acquire)
    }

    /// Shutdown has begun: new requests are answered `overloaded`; open ones keep their permits.
    pub fn start_draining(&self) {
        self.inner.draining.store(true, Ordering::Release);
    }

    /// `true` once [`Shed::start_draining`] was called.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perm::check_execute;
    use crate::ratelimit::RateLimiter;
    use bhdata_api::login::{Permissions, RecipeAllow};

    fn shed(max: u32) -> Shed {
        Shed::new(
            max,
            Duration::from_millis(100),
            Duration::from_millis(5_000),
        )
    }

    #[test]
    fn it_admits_up_to_the_cap_and_then_says_overloaded_with_a_hint() {
        let s = shed(2);
        let a = s.admit().unwrap();
        let _b = s.admit().unwrap();
        assert_eq!(s.in_flight(), 2);
        assert_eq!(
            s.admit().unwrap_err(),
            Error::Overloaded {
                retry_after: Duration::from_millis(100)
            }
        );
        // The refused request took nothing.
        assert_eq!(s.in_flight(), 2);
        drop(a);
        assert_eq!(s.in_flight(), 1);
        assert!(s.admit().is_ok());
    }

    #[test]
    fn copies_share_one_count() {
        let s = shed(1);
        let other = s.clone();
        let _p = s.admit().unwrap();
        assert!(other.admit().is_err());
    }

    #[test]
    fn a_dropped_permit_always_frees_its_place() {
        let s = shed(3);
        for _ in 0..1_000 {
            let _p = s.admit().unwrap();
        }
        assert_eq!(s.in_flight(), 0);
    }

    #[test]
    fn draining_refuses_new_work_with_the_drain_hint_but_keeps_open_permits() {
        let s = shed(10);
        let open = s.admit().unwrap();
        s.start_draining();
        assert!(s.is_draining());
        assert_eq!(
            s.admit().unwrap_err(),
            Error::Overloaded {
                retry_after: Duration::from_millis(5_000)
            }
        );
        assert_eq!(s.in_flight(), 1);
        drop(open);
        assert_eq!(s.in_flight(), 0);
    }

    #[test]
    fn a_cap_of_zero_admits_nothing() {
        assert!(shed(0).admit().is_err());
    }

    #[test]
    fn it_follows_the_config() {
        let config = GatewayConfig {
            max_inflight_total: 1,
            drain_ms: 2_000,
            ..GatewayConfig::default()
        };
        let s = Shed::from_config(&config);
        let _p = s.admit().unwrap();
        assert!(s.admit().is_err());
        assert_eq!(GatewayConfig::default().max_inflight_total, 1_024);
    }

    #[test]
    fn decision_9_order_permission_then_rate_limit_then_shed() {
        // A request refused at one step never reaches, or uses up, a later one.
        let allow = RecipeAllow::All;
        let can = Permissions {
            execute: true,
            ..Permissions::default()
        };
        let mut limiter = RateLimiter::new(1, 1);
        let s = shed(0);

        // 1. No permission: unauthorized, and the rate limiter is not touched.
        assert!(check_execute(Permissions::default(), &allow, "r").is_err());
        assert_eq!(limiter.tracked_logins(), 0);

        // 2. Permission passes, the bucket takes a token, then shed refuses: `overloaded`.
        assert!(check_execute(can, &allow, "r").is_ok());
        assert!(limiter.check("alice", None, None, 0).is_ok());
        assert!(matches!(s.admit(), Err(Error::Overloaded { .. })));

        // 3. The next request from the same login is refused by the limiter before shed is asked.
        assert!(limiter.check("alice", None, None, 0).is_err());
    }
}
