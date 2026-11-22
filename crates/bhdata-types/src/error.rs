//! The named errors (plan section 3), defined once for every crate.
//!
//! Every client-visible error is one of these. `bhdata-proto` maps [`ErrorKind`] to wire codes
//! once; nothing else invents an error name.

use std::fmt;
use std::time::Duration;

use thiserror::Error as ThisError;

use crate::ids::TransferId;

/// Result type used across the workspace.
pub type Result<T> = std::result::Result<T, Error>;

/// A named error, with whatever data that error carries.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
pub enum Error {
    /// The system is falling behind; try again after the hint.
    #[error("overloaded; retry after {retry_after:?}")]
    Overloaded {
        /// How long the caller should wait before retrying.
        retry_after: Duration,
    },
    /// A spend or transfer the balance cannot cover.
    #[error("insufficient funds")]
    InsufficientFunds,
    /// An amount that would overflow.
    #[error("overflow")]
    Overflow,
    /// A create with an id that already exists under a different request id.
    #[error("already exists")]
    AlreadyExists,
    /// An import file whose hash is already recorded as finished.
    #[error("already imported")]
    AlreadyImported,
    /// The same request id was reused with a different payload.
    #[error("request id conflict")]
    RequestIdConflict,
    /// A retry that arrived after the stored result expired (or was compacted into history).
    #[error("request expired")]
    RequestExpired,
    /// Missing or wrong login, or a permission the login does not have.
    #[error("unauthorized")]
    Unauthorized,
    /// A per-login rate limit was hit.
    #[error("rate limited")]
    RateLimited,
    /// The login is locked out after repeated failures.
    #[error("locked out")]
    LockedOut,
    /// The named recipe (or version) is not registered.
    #[error("unknown recipe")]
    UnknownRecipe,
    /// The arguments failed validation. The text says which and why.
    #[error("bad arguments: {0}")]
    BadArguments(String),
    /// Internal: the request reached a shard that does not own the slot at that epoch.
    /// The planner refreshes its map and retries; it is never shown to a client.
    #[error("wrong shard")]
    WrongShard,
    /// A cross-shard command is still running; the client can poll with the transfer id.
    #[error("pending (transfer {transfer})")]
    Pending {
        /// The transaction to poll.
        transfer: TransferId,
    },
    /// Something broke on our side. The text is for logs; clients get no detail.
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Which named error this is, without its data.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Overloaded { .. } => ErrorKind::Overloaded,
            Error::InsufficientFunds => ErrorKind::InsufficientFunds,
            Error::Overflow => ErrorKind::Overflow,
            Error::AlreadyExists => ErrorKind::AlreadyExists,
            Error::AlreadyImported => ErrorKind::AlreadyImported,
            Error::RequestIdConflict => ErrorKind::RequestIdConflict,
            Error::RequestExpired => ErrorKind::RequestExpired,
            Error::Unauthorized => ErrorKind::Unauthorized,
            Error::RateLimited => ErrorKind::RateLimited,
            Error::LockedOut => ErrorKind::LockedOut,
            Error::UnknownRecipe => ErrorKind::UnknownRecipe,
            Error::BadArguments(_) => ErrorKind::BadArguments,
            Error::WrongShard => ErrorKind::WrongShard,
            Error::Pending { .. } => ErrorKind::Pending,
            Error::Internal(_) => ErrorKind::Internal,
        }
    }
}

/// The name of a named error, with no data attached. Cheap to copy and compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrorKind {
    /// `overloaded`
    Overloaded,
    /// `insufficient_funds`
    InsufficientFunds,
    /// `overflow`
    Overflow,
    /// `already_exists`
    AlreadyExists,
    /// `already_imported`
    AlreadyImported,
    /// `request_id_conflict`
    RequestIdConflict,
    /// `request_expired`
    RequestExpired,
    /// `unauthorized`
    Unauthorized,
    /// `rate_limited`
    RateLimited,
    /// `locked_out`
    LockedOut,
    /// `unknown_recipe`
    UnknownRecipe,
    /// `bad_arguments`
    BadArguments,
    /// `wrong_shard`
    WrongShard,
    /// `pending`
    Pending,
    /// `internal`
    Internal,
}

impl ErrorKind {
    /// Every named error, in the order of plan section 3.
    pub const ALL: [ErrorKind; 15] = [
        ErrorKind::Overloaded,
        ErrorKind::InsufficientFunds,
        ErrorKind::Overflow,
        ErrorKind::AlreadyExists,
        ErrorKind::AlreadyImported,
        ErrorKind::RequestIdConflict,
        ErrorKind::RequestExpired,
        ErrorKind::Unauthorized,
        ErrorKind::RateLimited,
        ErrorKind::LockedOut,
        ErrorKind::UnknownRecipe,
        ErrorKind::BadArguments,
        ErrorKind::WrongShard,
        ErrorKind::Pending,
        ErrorKind::Internal,
    ];

    /// The name used in the plan, the docs and on the wire (snake case).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            ErrorKind::Overloaded => "overloaded",
            ErrorKind::InsufficientFunds => "insufficient_funds",
            ErrorKind::Overflow => "overflow",
            ErrorKind::AlreadyExists => "already_exists",
            ErrorKind::AlreadyImported => "already_imported",
            ErrorKind::RequestIdConflict => "request_id_conflict",
            ErrorKind::RequestExpired => "request_expired",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::LockedOut => "locked_out",
            ErrorKind::UnknownRecipe => "unknown_recipe",
            ErrorKind::BadArguments => "bad_arguments",
            ErrorKind::WrongShard => "wrong_shard",
            ErrorKind::Pending => "pending",
            ErrorKind::Internal => "internal",
        }
    }

    /// Looks a kind up by its name; `None` for anything that is not a named error.
    #[must_use]
    pub fn from_name(name: &str) -> Option<ErrorKind> {
        ErrorKind::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// `false` only for `wrong_shard`, which the planner handles itself and a client never sees.
    #[must_use]
    pub const fn is_client_visible(self) -> bool {
        !matches!(self, ErrorKind::WrongShard)
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact list from plan section 3. If this test fails, the plan and the code disagree.
    const PLAN_SECTION_3: [&str; 15] = [
        "overloaded",
        "insufficient_funds",
        "overflow",
        "already_exists",
        "already_imported",
        "request_id_conflict",
        "request_expired",
        "unauthorized",
        "rate_limited",
        "locked_out",
        "unknown_recipe",
        "bad_arguments",
        "wrong_shard",
        "pending",
        "internal",
    ];

    #[test]
    fn names_match_the_plan_in_order() {
        let names: Vec<&str> = ErrorKind::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(names, PLAN_SECTION_3);
    }

    #[test]
    fn names_round_trip_and_are_unique() {
        for kind in ErrorKind::ALL {
            assert_eq!(ErrorKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(ErrorKind::from_name("nope"), None);
        assert_eq!(ErrorKind::from_name("Overloaded"), None);
    }

    #[test]
    fn every_error_maps_to_its_kind() {
        let transfer = TransferId::new("t1").unwrap();
        let errors = [
            Error::Overloaded {
                retry_after: Duration::from_millis(250),
            },
            Error::InsufficientFunds,
            Error::Overflow,
            Error::AlreadyExists,
            Error::AlreadyImported,
            Error::RequestIdConflict,
            Error::RequestExpired,
            Error::Unauthorized,
            Error::RateLimited,
            Error::LockedOut,
            Error::UnknownRecipe,
            Error::BadArguments("amount".into()),
            Error::WrongShard,
            Error::Pending { transfer },
            Error::Internal("boom".into()),
        ];
        let kinds: Vec<ErrorKind> = errors.iter().map(Error::kind).collect();
        assert_eq!(kinds, ErrorKind::ALL);
    }

    #[test]
    fn only_wrong_shard_is_hidden_from_clients() {
        for kind in ErrorKind::ALL {
            assert_eq!(kind.is_client_visible(), kind != ErrorKind::WrongShard);
        }
    }

    #[test]
    fn display_carries_the_data_that_matters() {
        let err = Error::Pending {
            transfer: TransferId::new("abc").unwrap(),
        };
        assert_eq!(err.to_string(), "pending (transfer abc)");
        assert_eq!(
            ErrorKind::RequestIdConflict.to_string(),
            "request_id_conflict"
        );
    }
}
