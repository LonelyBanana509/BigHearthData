//! The helper's errors and the process exit codes they map to.
//!
//! Exit codes are part of the helper's contract (the installer and the server read them):
//! 0 ok, 2 usage, 3 not allowed to run here, 4 refused, 5 a step failed, 6 internal.

use std::fmt;
use std::io;

use crate::actions::Action;

pub const EXIT_USAGE: u8 = 2;
pub const EXIT_NOT_ALLOWED: u8 = 3;
pub const EXIT_REFUSED: u8 = 4;
pub const EXIT_STEP_FAILED: u8 = 5;
pub const EXIT_INTERNAL: u8 = 6;

#[derive(Debug)]
pub enum HelperError {
    /// Wrong or missing argument. The text never repeats the raw argument unescaped.
    Usage(String),
    /// Not running as root, or running from a location someone else could change.
    NotAllowed(String),
    /// A safety rule says no: unsupported platform, or an existing cluster of the pinned major.
    Refused(String),
    /// An external command exited non-zero (or was killed by a signal: `code` is `None`).
    CommandFailed {
        program: &'static str,
        code: Option<i32>,
    },
    /// The commands all succeeded but the result is not what the action promises.
    PostCheck(String),
    Io {
        what: String,
        source: io::Error,
    },
}

impl HelperError {
    pub fn exit_code(&self) -> u8 {
        match self {
            HelperError::Usage(_) => EXIT_USAGE,
            HelperError::NotAllowed(_) => EXIT_NOT_ALLOWED,
            HelperError::Refused(_) => EXIT_REFUSED,
            HelperError::CommandFailed { .. } | HelperError::PostCheck(_) => EXIT_STEP_FAILED,
            HelperError::Io { .. } => EXIT_INTERNAL,
        }
    }

    pub fn usage(problem: &str) -> HelperError {
        let names: Vec<&str> = Action::ALL.iter().map(|a| a.name()).collect();
        HelperError::Usage(format!(
            "{problem}; usage: bhdata-helper <action>, where action is one of: {}",
            names.join(", ")
        ))
    }

    pub fn io(what: impl Into<String>, source: io::Error) -> HelperError {
        HelperError::Io {
            what: what.into(),
            source,
        }
    }
}

impl fmt::Display for HelperError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HelperError::Usage(m)
            | HelperError::NotAllowed(m)
            | HelperError::Refused(m)
            | HelperError::PostCheck(m) => f.write_str(m),
            HelperError::CommandFailed {
                program,
                code: Some(c),
            } => write!(f, "{program} exited with status {c}"),
            HelperError::CommandFailed {
                program,
                code: None,
            } => write!(f, "{program} was stopped by a signal"),
            HelperError::Io { what, source } => write!(f, "{what}: {source}"),
        }
    }
}

impl std::error::Error for HelperError {}
