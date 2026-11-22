//! Running external commands: absolute program path, fixed argument list, no shell, fixed
//! environment, no stdin. Actions describe commands as [`Cmd`] values and hand them to a
//! [`Runner`], so tests can record the exact sequence instead of running anything.

use std::process::{Command, Stdio};

use crate::error::HelperError;

/// The only environment a child process gets. Nothing from the caller's environment is passed on
/// (`sudo` usually resets it, but the helper does not rely on that).
pub const FIXED_ENV: [(&str, &str); 6] = [
    ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
    ("LANG", "C"),
    ("LC_ALL", "C"),
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("APT_LISTCHANGES_FRONTEND", "none"),
    // Stops `needrestart` (Ubuntu) from prompting or restarting services on its own.
    ("NEEDRESTART_SUSPEND", "1"),
];

/// One command: a program given by absolute path and its arguments. Never passed to a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: &'static str,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new<I, S>(program: &'static str, args: I) -> Cmd
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Cmd {
            program,
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

pub trait Runner {
    fn run(&mut self, cmd: &Cmd) -> Result<(), HelperError>;
}

/// Runs commands for real. Output goes to the helper's own stdout and stderr (the journal).
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<(), HelperError> {
        eprintln!(
            "bhdata-helper: running {} {}",
            cmd.program,
            cmd.args.join(" ")
        );
        let status = Command::new(cmd.program)
            .args(&cmd.args)
            .env_clear()
            .envs(FIXED_ENV)
            .stdin(Stdio::null())
            .status()
            .map_err(|e| HelperError::io(format!("could not start {}", cmd.program), e))?;
        if status.success() {
            Ok(())
        } else {
            Err(HelperError::CommandFailed {
                program: cmd.program,
                code: status.code(),
            })
        }
    }
}
