//! Maps a PostgreSQL error to what the data layer does about it (spec section 4).
//!
//! The input is the SQLSTATE code (five characters, for example `40P01`), never the message text,
//! which changes between versions and locales. An error that carries no code at all (a closed or
//! reset connection) is [`PgErrorKind::Unavailable`].

use std::time::Duration;

use bhdata_types::error::Error;

/// The code our own narrow functions raise when a request is not in the state the call needs.
/// `BH` is not a class PostgreSQL uses.
pub const SQLSTATE_BHDATA: &str = "BH001";

/// How long a client is told to wait after a database-side overload.
pub const OVERLOAD_RETRY_AFTER: Duration = Duration::from_millis(250);

/// What kind of database failure this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PgErrorKind {
    /// `40P01`: the database picked this transaction to break a deadlock.
    Deadlock,
    /// `40001`: the transaction could not be serialized.
    Serialization,
    /// `23505`.
    UniqueViolation,
    /// `23503`, `23502`, `23514`: foreign key, not null, check.
    Constraint,
    /// `55P03`: a lock was not granted within `lock_timeout`.
    LockTimeout,
    /// `57014`: cancelled, in practice by `statement_timeout`.
    StatementTimeout,
    /// `53300`, `53400`.
    TooManyConnections,
    /// `53100`, `53200`: disk full or out of memory.
    Resources,
    /// The server is going away or the connection is gone (`57P01` to `57P03`, class `08`, no code).
    Unavailable,
    /// `25006`, `42501`: read-only transaction, or a right the role does not have.
    Forbidden,
    /// [`SQLSTATE_BHDATA`]: one of our functions refused the call.
    Refused,
    /// Anything else.
    Other,
}

/// May the command be run again?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// No: the same command would fail the same way.
    No,
    /// Yes, a limited number of times (design section 9).
    Limited,
    /// Yes, once a new connection is made.
    AfterReconnect,
}

/// A classified database error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgError {
    /// What happened.
    pub kind: PgErrorKind,
    /// Whether to run the command again.
    pub retry: Retry,
    /// The SQLSTATE as received, kept for the log. Empty when the error had none.
    pub code: String,
}

impl PgError {
    /// The error a client sees if this one reaches it.
    ///
    /// `retries_exhausted` says that a [`Retry::Limited`] error has already been retried as often
    /// as allowed. Unique and constraint violations become `Internal` here: the caller that knows
    /// where they came from (a create, a request id) turns them into `already_exists`,
    /// `request_id_conflict` or `bad_arguments` before it gets here.
    #[must_use]
    pub fn client_error(&self, retries_exhausted: bool) -> Error {
        use PgErrorKind::{
            Constraint, Deadlock, Forbidden, LockTimeout, Other, Refused, Resources, Serialization,
            StatementTimeout, TooManyConnections, Unavailable, UniqueViolation,
        };
        let overloaded = Error::Overloaded {
            retry_after: OVERLOAD_RETRY_AFTER,
        };
        match self.kind {
            Deadlock | Serialization if retries_exhausted => overloaded,
            LockTimeout | StatementTimeout | TooManyConnections | Resources | Unavailable => {
                overloaded
            }
            Deadlock | Serialization | UniqueViolation | Constraint | Forbidden | Refused
            | Other => Error::Internal(format!("postgres {:?} ({})", self.kind, self.shown_code())),
        }
    }

    /// True when an operator should be told: the machine is out of disk or memory.
    #[must_use]
    pub fn raises_alarm(&self) -> bool {
        self.kind == PgErrorKind::Resources
    }

    fn shown_code(&self) -> &str {
        if self.code.is_empty() {
            "no code"
        } else {
            &self.code
        }
    }
}

/// Classifies an error by its SQLSTATE. `None` means the driver reported no code, which is what a
/// closed or reset connection looks like.
#[must_use]
pub fn classify(sqlstate: Option<&str>) -> PgError {
    let Some(code) = sqlstate else {
        return make(PgErrorKind::Unavailable, Retry::AfterReconnect, "");
    };
    let (kind, retry) = match code {
        "40P01" => (PgErrorKind::Deadlock, Retry::Limited),
        "40001" => (PgErrorKind::Serialization, Retry::Limited),
        "23505" => (PgErrorKind::UniqueViolation, Retry::No),
        "23503" | "23502" | "23514" => (PgErrorKind::Constraint, Retry::No),
        "55P03" => (PgErrorKind::LockTimeout, Retry::No),
        "57014" => (PgErrorKind::StatementTimeout, Retry::No),
        "53300" | "53400" => (PgErrorKind::TooManyConnections, Retry::No),
        "53100" | "53200" => (PgErrorKind::Resources, Retry::No),
        "57P01" | "57P02" | "57P03" => (PgErrorKind::Unavailable, Retry::AfterReconnect),
        "25006" | "42501" => (PgErrorKind::Forbidden, Retry::No),
        SQLSTATE_BHDATA => (PgErrorKind::Refused, Retry::No),
        c if c.len() == 5 && c.starts_with("08") => {
            (PgErrorKind::Unavailable, Retry::AfterReconnect)
        }
        _ => (PgErrorKind::Other, Retry::No),
    };
    make(kind, retry, code)
}

fn make(kind: PgErrorKind, retry: Retry, code: &str) -> PgError {
    PgError {
        kind,
        retry,
        code: code.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bhdata_types::error::ErrorKind;
    use proptest::prelude::*;

    fn is(code: &str, kind: PgErrorKind, retry: Retry) {
        let e = classify(Some(code));
        assert_eq!((e.kind, e.retry), (kind, retry), "code {code}");
        assert_eq!(e.code, code);
    }

    #[test]
    fn deadlock_and_serialization_retry_a_limited_number_of_times() {
        is("40P01", PgErrorKind::Deadlock, Retry::Limited);
        is("40001", PgErrorKind::Serialization, Retry::Limited);
    }

    #[test]
    fn unique_violation_is_not_retried() {
        is("23505", PgErrorKind::UniqueViolation, Retry::No);
    }

    #[test]
    fn foreign_key_not_null_and_check_are_constraints() {
        for c in ["23503", "23502", "23514"] {
            is(c, PgErrorKind::Constraint, Retry::No);
        }
    }

    #[test]
    fn lock_and_statement_timeouts() {
        is("55P03", PgErrorKind::LockTimeout, Retry::No);
        is("57014", PgErrorKind::StatementTimeout, Retry::No);
    }

    #[test]
    fn connection_limits_and_resources() {
        is("53300", PgErrorKind::TooManyConnections, Retry::No);
        is("53400", PgErrorKind::TooManyConnections, Retry::No);
        is("53100", PgErrorKind::Resources, Retry::No);
        is("53200", PgErrorKind::Resources, Retry::No);
    }

    #[test]
    fn shutdowns_and_connection_class_wait_for_a_reconnect() {
        for c in [
            "57P01", "57P02", "57P03", "08000", "08003", "08006", "08P01",
        ] {
            is(c, PgErrorKind::Unavailable, Retry::AfterReconnect);
        }
    }

    #[test]
    fn no_code_means_the_connection_is_gone() {
        let e = classify(None);
        assert_eq!(e.kind, PgErrorKind::Unavailable);
        assert_eq!(e.retry, Retry::AfterReconnect);
        assert_eq!(e.code, "");
    }

    #[test]
    fn read_only_and_privilege_errors_are_forbidden() {
        is("25006", PgErrorKind::Forbidden, Retry::No);
        is("42501", PgErrorKind::Forbidden, Retry::No);
    }

    #[test]
    fn our_own_code_is_refused() {
        is(SQLSTATE_BHDATA, PgErrorKind::Refused, Retry::No);
    }

    #[test]
    fn anything_else_is_other_and_keeps_its_code() {
        is("42P01", PgErrorKind::Other, Retry::No);
        is("XX000", PgErrorKind::Other, Retry::No);
        is("", PgErrorKind::Other, Retry::No);
        // A code that merely starts like a listed one is not that code.
        is("40P011", PgErrorKind::Other, Retry::No);
        is("0800", PgErrorKind::Other, Retry::No);
        is("080000", PgErrorKind::Other, Retry::No);
    }

    #[test]
    fn retryable_errors_become_overloaded_only_when_retries_run_out() {
        let d = classify(Some("40P01"));
        assert_eq!(d.client_error(false).kind(), ErrorKind::Internal);
        assert_eq!(d.client_error(true).kind(), ErrorKind::Overloaded);
        let s = classify(Some("40001"));
        assert_eq!(s.client_error(false).kind(), ErrorKind::Internal);
        assert_eq!(s.client_error(true).kind(), ErrorKind::Overloaded);
    }

    #[test]
    fn pressure_errors_become_overloaded_with_a_hint() {
        for c in [
            "55P03", "57014", "53300", "53400", "53100", "53200", "57P01", "08006",
        ] {
            assert_eq!(
                classify(Some(c)).client_error(false),
                Error::Overloaded {
                    retry_after: OVERLOAD_RETRY_AFTER
                },
                "code {c}"
            );
        }
        assert_eq!(
            classify(None).client_error(false).kind(),
            ErrorKind::Overloaded
        );
    }

    #[test]
    fn bugs_become_internal_with_the_code_for_the_log() {
        for c in ["23505", "23503", "25006", "42501", SQLSTATE_BHDATA, "42P01"] {
            match classify(Some(c)).client_error(true) {
                Error::Internal(text) => assert!(text.contains(c), "{text}"),
                other => panic!("code {c} became {other:?}"),
            }
        }
    }

    #[test]
    fn only_resource_exhaustion_raises_an_alarm() {
        assert!(classify(Some("53100")).raises_alarm());
        assert!(classify(Some("53200")).raises_alarm());
        assert!(!classify(Some("53300")).raises_alarm());
        assert!(!classify(None).raises_alarm());
    }

    proptest! {
        #[test]
        fn no_input_panics(code in proptest::option::of(any::<String>()), exhausted in any::<bool>()) {
            let e = classify(code.as_deref());
            let _ = e.client_error(exhausted);
            let _ = e.raises_alarm();
        }

        #[test]
        fn the_code_is_kept_as_received(code in "[0-9A-Z]{5}") {
            prop_assert_eq!(classify(Some(&code)).code, code);
        }
    }
}
