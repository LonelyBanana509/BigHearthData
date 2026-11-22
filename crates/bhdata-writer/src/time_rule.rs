//! The clock rule for a request's creation time (spec `write-path.md`, section 4).
//!
//! The answer depends on the clock and the request alone. It never looks at a record, so a retry
//! just after the retry window gets the same answer whether or not compaction has already run.

use std::time::Duration;

use bhdata_api::types::{Error, Timestamp};

/// Decides whether a request created at `requested_at` may go on at `now`.
///
/// - older than `now - retry_window`: `request_expired`;
/// - more than `tolerance` ahead of `now`: `bad_arguments`;
/// - otherwise: allowed.
///
/// # Errors
/// The two named errors above.
pub fn check_request_time(
    now: Timestamp,
    requested_at: Timestamp,
    retry_window: Duration,
    tolerance: Duration,
) -> Result<(), Error> {
    if now.saturating_duration_since(requested_at) > retry_window {
        return Err(Error::RequestExpired);
    }
    if requested_at.saturating_duration_since(now) > tolerance {
        return Err(Error::BadArguments("request time is in the future".into()));
    }
    Ok(())
}

/// The cutoff for compaction: a record whose `requested_at` is before this is moved to history.
/// It is `now - retry_window - grace`, stopping at the epoch.
#[must_use]
pub fn compaction_cutoff(now: Timestamp, retry_window: Duration, grace: Duration) -> Timestamp {
    let keep = retry_window.saturating_add(grace);
    let ms = now
        .as_unix_millis()
        .saturating_sub(u64::try_from(keep.as_millis()).unwrap_or(u64::MAX));
    Timestamp::from_unix_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 3600);
    const TOL: Duration = Duration::from_secs(300);

    fn at(ms: u64) -> Timestamp {
        Timestamp::from_unix_millis(ms)
    }

    #[test]
    fn a_request_inside_the_window_goes_on() {
        let now = at(10 * 24 * 3600 * 1000);
        assert_eq!(check_request_time(now, now, DAY, TOL), Ok(()));
        assert_eq!(
            check_request_time(now, at(now.as_unix_millis() - 1), DAY, TOL),
            Ok(())
        );
    }

    #[test]
    fn the_window_edge_is_inclusive_and_one_millisecond_past_it_expires() {
        let now = at(10 * 24 * 3600 * 1000);
        let edge = at(now.as_unix_millis() - 24 * 3600 * 1000);
        assert_eq!(check_request_time(now, edge, DAY, TOL), Ok(()));
        assert_eq!(
            check_request_time(now, at(edge.as_unix_millis() - 1), DAY, TOL),
            Err(Error::RequestExpired)
        );
    }

    #[test]
    fn the_future_has_a_small_tolerance() {
        let now = at(1_000_000);
        let ahead = |ms: u64| at(now.as_unix_millis() + ms);
        assert_eq!(check_request_time(now, ahead(300_000), DAY, TOL), Ok(()));
        assert!(matches!(
            check_request_time(now, ahead(300_001), DAY, TOL),
            Err(Error::BadArguments(m)) if m.contains("future")
        ));
    }

    #[test]
    fn near_the_epoch_nothing_wraps() {
        assert_eq!(
            check_request_time(Timestamp::EPOCH, Timestamp::EPOCH, DAY, TOL),
            Ok(())
        );
        assert_eq!(
            check_request_time(at(5), Timestamp::from_unix_millis(u64::MAX), DAY, TOL),
            Err(Error::BadArguments("request time is in the future".into()))
        );
    }

    #[test]
    fn the_cutoff_is_window_plus_grace_before_now() {
        let now = at(100 * 3600 * 1000);
        let grace = Duration::from_secs(3600);
        assert_eq!(
            compaction_cutoff(now, DAY, grace),
            at((100 - 25) * 3600 * 1000)
        );
        assert_eq!(compaction_cutoff(at(1), DAY, grace), Timestamp::EPOCH);
    }

    /// A record is compacted only once the clock rule has already refused every retry of it, so
    /// compaction can never turn an answer of "stored result" into "expired" for a request the
    /// rule still lets through.
    #[test]
    fn compaction_never_touches_a_request_the_rule_still_allows() {
        let grace = Duration::from_secs(3600);
        let now = at(100 * 3600 * 1000);
        let cutoff = compaction_cutoff(now, DAY, grace);
        for back in [
            0u64,
            1,
            3_600_000,
            24 * 3_600_000,
            24 * 3_600_000 + 1,
            25 * 3_600_000,
        ] {
            let requested = at(now.as_unix_millis() - back);
            let compacted = requested < cutoff;
            let allowed = check_request_time(now, requested, DAY, TOL).is_ok();
            assert!(!(compacted && allowed), "back {back}");
        }
    }
}
