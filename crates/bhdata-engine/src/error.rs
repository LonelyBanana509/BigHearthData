//! The engine manager's errors. These are internal errors: the gateway turns anything that reaches
//! a client into `overloaded` or `internal`.

use std::io;

use thiserror::Error;

use crate::install::{HelperAction, HelperFailure};

#[derive(Debug, Error)]
pub enum EngineError {
    /// A setting or name that is not allowed. Nothing was changed.
    #[error("invalid {what}: {why}")]
    Invalid { what: &'static str, why: String },

    /// PostgreSQL refuses to run as root, so the engine does not try.
    #[error("PostgreSQL cannot run as root; run this as the service user")]
    RunningAsRoot,

    #[error("{what}: {source}")]
    Io {
        what: String,
        #[source]
        source: io::Error,
    },

    /// A PostgreSQL program (`initdb`, `postgres`, `pg_ctl`) failed.
    #[error("{program} failed: {detail}")]
    Tool {
        program: &'static str,
        detail: String,
    },

    /// The data folder belongs to another major version than the installed server. Moving between
    /// majors is an explicit admin action, never automatic (design section 3).
    #[error(
        "the data folder {dir} is PostgreSQL {found} but the installed server is {installed}; a major upgrade is needed"
    )]
    VersionMismatch {
        dir: String,
        found: String,
        installed: String,
    },

    /// The folder where the instance should be exists and is not empty, but is not a cluster.
    #[error(
        "{0} exists and is not an empty or initialised PostgreSQL folder; refusing to touch it"
    )]
    NotACluster(String),

    #[error("the instance has not been created yet; run init first")]
    NotInitialised,

    #[error("helper action {action:?}: {failure}")]
    Helper {
        action: HelperAction,
        failure: HelperFailure,
    },
}

impl EngineError {
    pub(crate) fn io(what: impl Into<String>, source: io::Error) -> EngineError {
        EngineError::Io {
            what: what.into(),
            source,
        }
    }

    pub(crate) fn invalid(what: &'static str, why: impl Into<String>) -> EngineError {
        EngineError::Invalid {
            what,
            why: why.into(),
        }
    }
}
