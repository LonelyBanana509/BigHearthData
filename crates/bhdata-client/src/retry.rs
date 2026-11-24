//! The retry table of wire-protocol spec section 5. Lane K.
//!
//! Pure: no sockets, no clock, no randomness. The caller says what it saw ([`Seen`]), how long the
//! call has been retrying (`elapsed`) and a random number in `[0, 1)`; [`Retry::decide`] says what
//! to do ([`Decision`]). That keeps the table testable without a connection.
//!
//! The rule behind every row is the same: a retry always carries the **same request id and the
//! same `created_at_ms`** (the caller keeps them), so the write path settles a request whose
//! outcome is unknown (applied once, or applied now) and refuses one whose window is over.

use std::time::Duration;

use bhdata_proto::types::ErrorKind;

/// The longest the SDK may keep retrying, whatever the settings say: the retry window of the write
/// path (`requests.retry_window_secs`, 24 hours by default). Past it a request is `request_expired`.
pub const RETRY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// The default for `max_retry_secs` (spec section 5).
pub const DEFAULT_MAX_RETRY: Duration = Duration::from_secs(60);

/// How long `internal` waits before its one retry (spec section 5).
pub const INTERNAL_WAIT: Duration = Duration::from_secs(1);

/// The shortest wait for a hint, so a server that sends none (or zero) is not retried in a tight loop.
pub const MIN_HINT: Duration = Duration::from_millis(50);

/// The pause before the first reconnect; it doubles up to [`RECONNECT_BACKOFF_MAX`].
pub const RECONNECT_BACKOFF_FIRST: Duration = Duration::from_millis(100);

/// The longest pause between two reconnects.
pub const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// What one attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seen {
    /// A `reply` with `ok: true`: `done`, or a stored `failed` replayed. Never retried.
    Ok,
    /// A `reply` with `ok: false`, by its kind and its `retry_after_ms` if it had one.
    Error {
        /// The error's kind.
        kind: ErrorKind,
        /// The server's hint, for `overloaded` and `rate_limited`.
        retry_after_ms: Option<u64>,
    },
    /// The connection was lost or could not be made before the reply came.
    ConnectionLost,
    /// No reply within the call's own timeout.
    Timeout,
    /// The server sent `shutdown` (or closed the connection while draining).
    Shutdown,
}

/// What the SDK does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Hand this result to the application; do not retry.
    Return,
    /// Wait, then send the same request again on the same connection. Nothing was stored.
    WaitThenSend(Duration),
    /// Open a new connection (after the pause) and send the same request again. The outcome of
    /// the last attempt is unknown.
    ReconnectThenSend(Duration),
}

/// Why the SDK gave up, which decides what the application is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GaveUp {
    /// The last attempt got an answer (an error kind). The application gets that error.
    Answered,
    /// The last attempt got no answer: the outcome is unknown. The application must read the state
    /// or send the same request id again later.
    OutcomeUnknown,
}

/// What [`Retry::decide`] returns: the step, or that the time is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Do this.
    Do(Step),
    /// Stop retrying; tell the application what [`GaveUp`] says.
    GiveUp(GaveUp),
}

/// One call's retry state. Make one per call (not per connection).
#[derive(Debug, Clone)]
pub struct Retry {
    max: Duration,
    internal_retried: bool,
    reconnects: u32,
}

impl Retry {
    /// A call that retries for at most `max_retry`, and never more than [`RETRY_WINDOW`].
    #[must_use]
    pub fn new(max_retry: Duration) -> Self {
        Retry {
            max: max_retry.min(RETRY_WINDOW),
            internal_retried: false,
            reconnects: 0,
        }
    }

    /// The pause that waits out a hint: the hint times a random factor from 1 to 1.5, so clients
    /// that were refused together do not come back together. `unit` is in `[0, 1)`.
    #[must_use]
    pub fn jittered(hint: Duration, unit: f64) -> Duration {
        let unit = if unit.is_finite() {
            unit.clamp(0.0, 1.0)
        } else {
            0.0
        };
        hint.mul_f64(1.0 + 0.5 * unit)
    }

    /// The pause before the next reconnect: 100 ms, doubling, at most 5 s.
    fn backoff(&self) -> Duration {
        let shift = self.reconnects.min(16);
        RECONNECT_BACKOFF_FIRST
            .saturating_mul(1u32 << shift)
            .min(RECONNECT_BACKOFF_MAX)
    }

    /// Decides after one attempt. `elapsed` is the time since the call started; `unit` is a random
    /// number in `[0, 1)`.
    ///
    /// A pause never runs past the deadline: if waiting would end after `max_retry`, the SDK gives
    /// up now rather than sleep and then give up.
    pub fn decide(&mut self, seen: Seen, elapsed: Duration, unit: f64) -> Decision {
        let step = match seen {
            // Final answers, whatever their kind: a business error is final and the stored result
            // replays; the rest are the application's to handle (spec section 5, "never retries").
            Seen::Ok => Step::Return,
            Seen::Error {
                kind,
                retry_after_ms,
            } => match kind {
                ErrorKind::Overloaded | ErrorKind::RateLimited => {
                    let hint = Duration::from_millis(retry_after_ms.unwrap_or(0)).max(MIN_HINT);
                    Step::WaitThenSend(Self::jittered(hint, unit))
                }
                ErrorKind::Internal if !self.internal_retried => {
                    self.internal_retried = true;
                    Step::WaitThenSend(INTERNAL_WAIT)
                }
                // `request_expired` ends the window; the rest never retry. `pending` does not occur
                // until M15 and is returned as it comes.
                _ => Step::Return,
            },
            Seen::ConnectionLost | Seen::Timeout | Seen::Shutdown => {
                let pause = self.backoff();
                self.reconnects = self.reconnects.saturating_add(1);
                Step::ReconnectThenSend(pause)
            }
        };
        match step {
            Step::Return => Decision::Do(Step::Return),
            Step::WaitThenSend(pause) | Step::ReconnectThenSend(pause) => {
                if elapsed.saturating_add(pause) >= self.max {
                    Decision::GiveUp(match seen {
                        Seen::ConnectionLost | Seen::Timeout | Seen::Shutdown => {
                            GaveUp::OutcomeUnknown
                        }
                        _ => GaveUp::Answered,
                    })
                } else {
                    Decision::Do(step)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn err(kind: ErrorKind, hint: Option<u64>) -> Seen {
        Seen::Error {
            kind,
            retry_after_ms: hint,
        }
    }

    fn retry() -> Retry {
        Retry::new(DEFAULT_MAX_RETRY)
    }

    #[test]
    fn an_answer_is_returned_and_never_retried() {
        for kind in [
            ErrorKind::InsufficientFunds,
            ErrorKind::Overflow,
            ErrorKind::RecipeDefined,
            ErrorKind::RequestIdConflict,
            ErrorKind::BadArguments,
            ErrorKind::UnknownRecipe,
            ErrorKind::Unauthorized,
            ErrorKind::LockedOut,
            ErrorKind::AlreadyExists,
            ErrorKind::AlreadyImported,
            ErrorKind::RequestExpired,
            ErrorKind::Pending,
            ErrorKind::WrongShard,
        ] {
            assert_eq!(
                retry().decide(err(kind, None), Duration::ZERO, 0.0),
                Decision::Do(Step::Return),
                "{kind:?}"
            );
        }
        assert_eq!(
            retry().decide(Seen::Ok, Duration::ZERO, 0.0),
            Decision::Do(Step::Return)
        );
    }

    #[test]
    fn a_hint_is_waited_out_between_one_and_one_and_a_half_times() {
        for kind in [ErrorKind::Overloaded, ErrorKind::RateLimited] {
            let low = retry().decide(err(kind, Some(1000)), Duration::ZERO, 0.0);
            let high = retry().decide(err(kind, Some(1000)), Duration::ZERO, 0.999_999);
            assert_eq!(low, Decision::Do(Step::WaitThenSend(S)));
            let Decision::Do(Step::WaitThenSend(d)) = high else {
                panic!("{high:?}")
            };
            assert!(d > S && d < Duration::from_millis(1500), "{d:?}");
        }
    }

    #[test]
    fn a_missing_or_zero_hint_is_still_waited_out() {
        for hint in [None, Some(0)] {
            let Decision::Do(Step::WaitThenSend(d)) =
                retry().decide(err(ErrorKind::Overloaded, hint), Duration::ZERO, 0.0)
            else {
                panic!()
            };
            assert!(d >= MIN_HINT, "{d:?}");
        }
    }

    #[test]
    fn jitter_stays_inside_its_range_for_any_input() {
        for unit in [f64::NAN, f64::INFINITY, -3.0, 7.0, 0.5] {
            let d = Retry::jittered(S, unit);
            assert!(d >= S && d <= Duration::from_millis(1500), "{unit}: {d:?}");
        }
    }

    #[test]
    fn internal_is_retried_once_after_a_second_then_returned() {
        let mut r = retry();
        assert_eq!(
            r.decide(err(ErrorKind::Internal, None), Duration::ZERO, 0.0),
            Decision::Do(Step::WaitThenSend(INTERNAL_WAIT))
        );
        assert_eq!(
            r.decide(err(ErrorKind::Internal, None), S, 0.0),
            Decision::Do(Step::Return)
        );
    }

    #[test]
    fn a_lost_connection_reconnects_with_a_growing_pause() {
        let mut r = retry();
        let mut pauses = Vec::new();
        for lost in [Seen::ConnectionLost, Seen::Timeout, Seen::Shutdown] {
            let Decision::Do(Step::ReconnectThenSend(d)) = r.decide(lost, Duration::ZERO, 0.0)
            else {
                panic!("{lost:?}")
            };
            pauses.push(d);
        }
        assert_eq!(
            pauses,
            [
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400)
            ]
        );
        for _ in 0..40 {
            r.decide(Seen::ConnectionLost, Duration::ZERO, 0.0);
        }
        let Decision::Do(Step::ReconnectThenSend(d)) =
            r.decide(Seen::ConnectionLost, Duration::ZERO, 0.0)
        else {
            panic!()
        };
        assert_eq!(d, RECONNECT_BACKOFF_MAX);
    }

    #[test]
    fn it_gives_up_at_the_deadline_and_says_whether_the_outcome_is_known() {
        let mut r = Retry::new(Duration::from_secs(10));
        assert_eq!(
            r.decide(
                err(ErrorKind::Overloaded, Some(2000)),
                Duration::from_secs(9),
                0.0
            ),
            Decision::GiveUp(GaveUp::Answered)
        );
        assert_eq!(
            r.decide(Seen::ConnectionLost, Duration::from_secs(10), 0.0),
            Decision::GiveUp(GaveUp::OutcomeUnknown)
        );
        assert!(matches!(
            r.decide(
                err(ErrorKind::Overloaded, Some(500)),
                Duration::from_secs(1),
                0.0
            ),
            Decision::Do(Step::WaitThenSend(_))
        ));
        assert_eq!(
            r.decide(
                err(ErrorKind::InsufficientFunds, None),
                Duration::from_secs(99),
                0.0
            ),
            Decision::Do(Step::Return)
        );
    }

    #[test]
    fn the_deadline_never_passes_the_retry_window() {
        let r = Retry::new(Duration::from_secs(10 * 24 * 3600));
        assert_eq!(r.max, RETRY_WINDOW);
        assert_eq!(Retry::new(Duration::ZERO).max, Duration::ZERO);
    }
}
