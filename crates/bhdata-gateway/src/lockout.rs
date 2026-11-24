//! Failed-login counters per name and per source address, on a fake clock in tests (spec, decision 8).
//! Lane S.
//!
//! Pure state; `now_ms` is milliseconds on a monotonic clock, passed in by the caller. The counters are
//! per gateway copy at stage 1, as the design says. The session code keeps one [`Lockout`] behind a
//! mutex, asks it before it looks at a password, and tells it what happened afterwards.
//!
//! Names are whatever the client sent, existing or not, so a login that does not exist locks exactly
//! like one that does and the client learns nothing from it (decision 7).

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;

use bhdata_api::types::config::GatewayConfig;

/// The window for the per-source count: "twenty in a minute".
const SOURCE_WINDOW_MS: u64 = 60_000;

/// Most names (and most source addresses) tracked at once. A flood of made-up names cannot grow memory
/// without bound: past this, a new entry replaces one that is not locked, and if every entry is locked
/// the new one is not tracked.
const MAX_TRACKED: usize = 100_000;

/// The limits, from `[gateway]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockoutConfig {
    /// Consecutive failures for one name that lock it.
    pub failures: u32,
    /// How long a locked name stays locked.
    pub lock_ms: u64,
    /// Failures from one source in a minute that close it out.
    pub source_failures_per_minute: u32,
    /// How long that source is closed out.
    pub source_block_ms: u64,
}

impl LockoutConfig {
    /// `lockout_failures`, `lockout_secs`, `source_failures_per_minute` and `source_block_secs`.
    #[must_use]
    pub fn from_config(config: &GatewayConfig) -> Self {
        LockoutConfig {
            failures: config.lockout_failures,
            lock_ms: config.lockout_secs.saturating_mul(1_000),
            source_failures_per_minute: config.source_failures_per_minute,
            source_block_ms: config.source_block_secs.saturating_mul(1_000),
        }
    }
}

/// The answer for a name that may not try now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Locked {
    /// How long until the lock ends, in milliseconds (for logs; the client is only told `locked_out`).
    pub remaining_ms: u64,
}

/// The answer for a source address that may not try now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blocked {
    /// How long until the source may try again, in milliseconds.
    pub remaining_ms: u64,
}

/// What recording a failure did to the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureEffect {
    /// This failure locked the name. The caller stores a `LockedOut` audit row, waits for it, and only
    /// then answers `locked_out` (spec, section 7).
    pub name_locked: bool,
    /// This failure closed the source out.
    pub source_blocked: bool,
}

#[derive(Debug, Default)]
struct NameState {
    consecutive: u32,
    locked_until: Option<u64>,
    /// When the last failure happened, to forget a counter nobody is feeding.
    last_failure: u64,
}

#[derive(Debug, Default)]
struct SourceState {
    failures: VecDeque<u64>,
    blocked_until: Option<u64>,
}

/// The two counters.
#[derive(Debug)]
pub struct Lockout {
    config: LockoutConfig,
    names: HashMap<String, NameState>,
    sources: HashMap<IpAddr, SourceState>,
}

impl Lockout {
    /// A pair of empty counters.
    #[must_use]
    pub fn new(config: LockoutConfig) -> Self {
        Lockout {
            config,
            names: HashMap::new(),
            sources: HashMap::new(),
        }
    }

    /// May this name try now? While it is locked even the right password is refused, so ask before the
    /// password is looked at. A lock that has run out is cleared here, together with its counter.
    ///
    /// # Errors
    /// [`Locked`] with the time left.
    pub fn check_name(&mut self, name: &str, now_ms: u64) -> Result<(), Locked> {
        let Some(state) = self.names.get_mut(name) else {
            return Ok(());
        };
        match state.locked_until {
            Some(until) if now_ms < until => Err(Locked {
                remaining_ms: until - now_ms,
            }),
            Some(_) => {
                // The lock ran out: a fresh start.
                self.names.remove(name);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// May this source address try now?
    ///
    /// # Errors
    /// [`Blocked`] with the time left.
    pub fn check_source(&mut self, source: IpAddr, now_ms: u64) -> Result<(), Blocked> {
        let Some(state) = self.sources.get_mut(&source) else {
            return Ok(());
        };
        match state.blocked_until {
            Some(until) if now_ms < until => Err(Blocked {
                remaining_ms: until - now_ms,
            }),
            Some(_) => {
                self.sources.remove(&source);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// A login failed (wrong proof, a name that does not exist, a disabled login). Counts it against
    /// the name and the source. Call it only for an attempt that was allowed to try: an attempt refused
    /// by [`Lockout::check_name`] or [`Lockout::check_source`] changes nothing, so a locked name does
    /// not stay locked for ever by being tried.
    pub fn record_failure(&mut self, name: &str, source: IpAddr, now_ms: u64) -> FailureEffect {
        FailureEffect {
            name_locked: self.count_name_failure(name, now_ms),
            source_blocked: self.count_source_failure(source, now_ms),
        }
    }

    /// A login succeeded: the name's counter goes back to zero (the source's window is left alone).
    pub fn record_success(&mut self, name: &str) {
        self.names.remove(name);
    }

    /// Ends locks that have run out and forgets counters nobody has fed for a lock's length. The
    /// session calls it every few seconds. It returns the names whose lock ended, once each, so the
    /// caller can write a `LockoutReleased` audit row.
    ///
    /// A counter below the threshold that sat idle for as long as a lock lasts is forgotten, so the map
    /// holds only names that failed recently: the spec's "consecutive" is read as "consecutive in a
    /// period no longer than the lock".
    pub fn sweep(&mut self, now_ms: u64) -> Vec<String> {
        let lock_ms = self.config.lock_ms;
        let mut released = Vec::new();
        self.names.retain(|name, s| match s.locked_until {
            Some(until) if now_ms >= until => {
                released.push(name.clone());
                false
            }
            Some(_) => true,
            None => now_ms.saturating_sub(s.last_failure) < lock_ms,
        });
        self.sources.retain(|_, s| {
            while s
                .failures
                .front()
                .is_some_and(|t| now_ms.saturating_sub(*t) >= SOURCE_WINDOW_MS)
            {
                s.failures.pop_front();
            }
            match s.blocked_until {
                Some(until) => now_ms < until,
                None => !s.failures.is_empty(),
            }
        });
        released.sort();
        released
    }

    /// Names with a counter or a lock right now.
    #[must_use]
    pub fn tracked_names(&self) -> usize {
        self.names.len()
    }

    /// Source addresses with a window or a block right now.
    #[must_use]
    pub fn tracked_sources(&self) -> usize {
        self.sources.len()
    }

    fn count_name_failure(&mut self, name: &str, now_ms: u64) -> bool {
        if !self.names.contains_key(name) && self.names.len() >= MAX_TRACKED {
            // Make room by dropping one name that is not locked; if all are locked, track nothing new.
            let victim = self
                .names
                .iter()
                .find(|(_, s)| s.locked_until.is_none())
                .map(|(k, _)| k.clone());
            match victim {
                Some(key) => {
                    self.names.remove(&key);
                }
                None => return false,
            }
        }
        let state = self.names.entry(name.to_owned()).or_default();
        if state.locked_until.is_some() {
            // Already locked (an attempt that should have been refused first): nothing more to do.
            return false;
        }
        state.consecutive = state.consecutive.saturating_add(1);
        state.last_failure = now_ms;
        if state.consecutive >= self.config.failures {
            state.locked_until = Some(now_ms.saturating_add(self.config.lock_ms));
            return true;
        }
        false
    }

    fn count_source_failure(&mut self, source: IpAddr, now_ms: u64) -> bool {
        if !self.sources.contains_key(&source) && self.sources.len() >= MAX_TRACKED {
            let victim = self
                .sources
                .iter()
                .find(|(_, s)| s.blocked_until.is_none())
                .map(|(k, _)| *k);
            match victim {
                Some(key) => {
                    self.sources.remove(&key);
                }
                None => return false,
            }
        }
        let state = self.sources.entry(source).or_default();
        if state.blocked_until.is_some() {
            return false;
        }
        while state
            .failures
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= SOURCE_WINDOW_MS)
        {
            state.failures.pop_front();
        }
        state.failures.push_back(now_ms);
        if state.failures.len() >= self.config.source_failures_per_minute as usize {
            state.failures.clear();
            state.blocked_until = Some(now_ms.saturating_add(self.config.source_block_ms));
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, last])
    }

    fn lockout() -> Lockout {
        Lockout::new(LockoutConfig::from_config(&GatewayConfig::default()))
    }

    #[test]
    fn the_defaults_are_the_specs() {
        let c = LockoutConfig::from_config(&GatewayConfig::default());
        assert_eq!(c.failures, 5);
        assert_eq!(c.lock_ms, 15 * MIN);
        assert_eq!(c.source_failures_per_minute, 20);
        assert_eq!(c.source_block_ms, MIN);
    }

    #[test]
    fn five_failures_lock_the_name_for_fifteen_minutes_and_the_right_password_is_refused() {
        let mut l = lockout();
        // Different sources so only the name counter is in play.
        for i in 0..4u8 {
            let e = l.record_failure("alice", ip(i), 1_000);
            assert!(!e.name_locked);
            assert!(l.check_name("alice", 1_000).is_ok());
        }
        assert!(l.record_failure("alice", ip(9), 1_000).name_locked);
        // Locked: the caller is told before it looks at any password.
        assert_eq!(
            l.check_name("alice", 1_000 + 1),
            Err(Locked {
                remaining_ms: 15 * MIN - 1
            })
        );
        assert!(l.check_name("alice", 1_000 + 15 * MIN - 1).is_err());
        // Wait out the lock: accepted again, with a clean counter.
        assert!(l.check_name("alice", 1_000 + 15 * MIN).is_ok());
        assert!(
            !l.record_failure("alice", ip(1), 1_000 + 15 * MIN)
                .name_locked
        );
    }

    #[test]
    fn a_success_resets_the_counter() {
        let mut l = lockout();
        for i in 0..4u8 {
            l.record_failure("alice", ip(i), 0);
        }
        l.record_success("alice");
        for i in 0..4u8 {
            assert!(!l.record_failure("alice", ip(i), 0).name_locked);
        }
        assert!(l.record_failure("alice", ip(5), 0).name_locked);
    }

    #[test]
    fn a_name_that_does_not_exist_locks_like_one_that_does() {
        let mut l = lockout();
        let mut locked = false;
        for i in 0..5u8 {
            locked = l.record_failure("nobody", ip(i), 0).name_locked;
        }
        assert!(locked);
        assert!(l.check_name("nobody", 1).is_err());
    }

    #[test]
    fn names_are_counted_apart_and_exactly() {
        let mut l = lockout();
        for i in 0..5u8 {
            l.record_failure("alice", ip(i), 0);
        }
        assert!(l.check_name("alice", 1).is_err());
        assert!(l.check_name("bob", 1).is_ok());
        assert!(l.check_name("Alice", 1).is_ok());
    }

    #[test]
    fn an_attempt_on_a_locked_name_does_not_extend_the_lock() {
        let mut l = lockout();
        for i in 0..5u8 {
            l.record_failure("alice", ip(i), 0);
        }
        // Someone keeps trying; the caller does not record refused attempts, but even if it did:
        assert!(!l.record_failure("alice", ip(7), 10 * MIN).name_locked);
        assert!(l.check_name("alice", 15 * MIN).is_ok());
    }

    #[test]
    fn twenty_failures_from_one_source_in_a_minute_close_it_for_a_minute() {
        let mut l = lockout();
        let mut last = FailureEffect {
            name_locked: false,
            source_blocked: false,
        };
        // Twenty different names, so no name locks.
        for i in 0..20 {
            assert!(l.check_source(ip(1), i * 1_000).is_ok());
            last = l.record_failure(&format!("n{i}"), ip(1), i * 1_000);
        }
        assert!(last.source_blocked && !last.name_locked);
        assert_eq!(
            l.check_source(ip(1), 19_000 + 1),
            Err(Blocked {
                remaining_ms: MIN - 1
            })
        );
        assert!(l.check_source(ip(1), 19_000 + MIN).is_ok());
        // Another address is not affected.
        assert!(l.check_source(ip(2), 19_001).is_ok());
    }

    #[test]
    fn failures_spread_over_more_than_a_minute_do_not_block() {
        let mut l = lockout();
        for i in 0..100 {
            // One failure every 4 s: at most 15 in any minute.
            let e = l.record_failure(&format!("n{i}"), ip(1), i * 4_000);
            assert!(!e.source_blocked, "blocked at {i}");
        }
    }

    #[test]
    fn a_success_leaves_the_source_window_alone() {
        let mut l = lockout();
        for i in 0..19 {
            l.record_failure(&format!("n{i}"), ip(1), 0);
        }
        l.record_success("n0");
        assert!(l.record_failure("x", ip(1), 1).source_blocked);
    }

    #[test]
    fn sweep_reports_each_ended_lock_once_and_forgets_idle_counters() {
        let mut l = lockout();
        for i in 0..5u8 {
            l.record_failure("alice", ip(i), 0);
        }
        l.record_failure("bob", ip(1), 0);
        assert_eq!(l.tracked_names(), 2);
        // Mid-lock: nothing released; bob's counter is still fresh.
        assert!(l.sweep(14 * MIN).is_empty());
        assert_eq!(l.tracked_names(), 2);
        // Lock over: alice is released (for the audit log) and bob's idle counter is forgotten.
        assert_eq!(l.sweep(15 * MIN), vec!["alice".to_string()]);
        assert_eq!(l.tracked_names(), 0);
        assert!(l.sweep(16 * MIN).is_empty());
    }

    #[test]
    fn sweep_drops_quiet_sources() {
        let mut l = lockout();
        l.record_failure("a", ip(1), 0);
        assert_eq!(l.tracked_sources(), 1);
        l.sweep(MIN);
        assert_eq!(l.tracked_sources(), 0);
    }

    #[test]
    fn a_flood_of_made_up_names_cannot_grow_the_map_without_bound() {
        let mut l = lockout();
        for i in 0..(MAX_TRACKED as u64 + 50) {
            l.record_failure(&format!("n{i}"), ip(1), 0);
        }
        assert!(l.tracked_names() <= MAX_TRACKED);
    }

    #[test]
    fn locked_names_are_never_evicted_to_make_room() {
        let mut l = Lockout::new(LockoutConfig {
            failures: 1,
            lock_ms: 1_000_000,
            source_failures_per_minute: u32::MAX,
            source_block_ms: 1,
        });
        for i in 0..MAX_TRACKED as u64 {
            assert!(l.record_failure(&format!("n{i}"), ip(1), 0).name_locked);
        }
        // Full of locks: a new name is not tracked, and no lock is lost.
        assert!(!l.record_failure("new", ip(1), 0).name_locked);
        assert_eq!(l.tracked_names(), MAX_TRACKED);
        assert!(l.check_name("n0", 1).is_err());
    }

    #[test]
    fn the_largest_times_do_not_overflow() {
        let mut l = lockout();
        for i in 0..5u8 {
            l.record_failure("a", ip(i), u64::MAX - 1);
        }
        assert!(l.check_name("a", u64::MAX - 1).is_err());
        l.sweep(u64::MAX);
    }
}
