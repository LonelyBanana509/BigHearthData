//! What the writer does after a failed attempt (spec `write-path.md`, section 5).
//!
//! Only a deadlock or a serialization failure is retried, because nothing was committed and the
//! database says another try can succeed. A broken connection is never retried by the writer: the
//! outcome of a commit may be unknown, and the client's retry settles it through the request id.

use std::time::Duration;

use bhdata_pg::{PgError, Retry};

/// The shortest pause before another attempt.
pub const PAUSE_MIN: Duration = Duration::from_millis(5);
/// The longest pause before another attempt.
pub const PAUSE_MAX: Duration = Duration::from_millis(25);

/// What to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Wait this long, then run the whole command again from `BEGIN`.
    Retry {
        /// The pause.
        pause: Duration,
    },
    /// Stop; the reply is `PgError::client_error(true)` (`overloaded` for a deadlock).
    GiveUp,
}

/// The decision after attempt number `attempt` (counting from 1) failed with `error`.
/// `jitter` is any random number; it only spreads the pause between [`PAUSE_MIN`] and [`PAUSE_MAX`],
/// so a test passes a fixed one.
#[must_use]
pub fn after_failure(error: &PgError, attempt: u32, max_attempts: u32, jitter: u64) -> Next {
    if error.retry == Retry::Limited && attempt < max_attempts {
        Next::Retry {
            pause: pause(jitter),
        }
    } else {
        Next::GiveUp
    }
}

/// A pause from `jitter`: 5 to 25 ms in whole milliseconds.
#[must_use]
pub fn pause(jitter: u64) -> Duration {
    let span = u64::try_from((PAUSE_MAX - PAUSE_MIN).as_millis()).unwrap_or(0) + 1;
    PAUSE_MIN + Duration::from_millis(jitter % span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bhdata_pg::classify;

    fn err(code: Option<&str>) -> PgError {
        classify(code)
    }

    #[test]
    fn deadlock_and_serialization_are_retried_until_the_last_attempt() {
        for code in ["40P01", "40001"] {
            let e = err(Some(code));
            assert!(matches!(after_failure(&e, 1, 3, 0), Next::Retry { .. }));
            assert!(matches!(after_failure(&e, 2, 3, 0), Next::Retry { .. }));
            assert_eq!(after_failure(&e, 3, 3, 0), Next::GiveUp, "{code}");
        }
    }

    #[test]
    fn one_attempt_in_all_means_no_retry() {
        assert_eq!(after_failure(&err(Some("40P01")), 1, 1, 0), Next::GiveUp);
    }

    #[test]
    fn nothing_else_is_retried_by_the_writer() {
        for code in [
            Some("23505"),
            Some("55P03"),
            Some("57014"),
            Some("53300"),
            Some("42501"),
            Some("BH001"),
            Some("XX000"),
            Some("57P01"),
            Some("08006"),
            None,
        ] {
            assert_eq!(after_failure(&err(code), 1, 3, 0), Next::GiveUp, "{code:?}");
        }
    }

    #[test]
    fn the_pause_stays_between_5_and_25_ms_for_any_jitter() {
        assert_eq!(pause(0), Duration::from_millis(5));
        assert_eq!(pause(20), Duration::from_millis(25));
        assert_eq!(pause(21), Duration::from_millis(5));
        for j in [1, 7, 1_000, u64::MAX] {
            let p = pause(j);
            assert!(p >= PAUSE_MIN && p <= PAUSE_MAX, "{j}");
        }
    }

    #[test]
    fn giving_up_after_a_deadlock_is_overloaded() {
        use bhdata_api::types::Error;
        let e = err(Some("40P01"));
        assert!(matches!(e.client_error(true), Error::Overloaded { .. }));
    }
}
