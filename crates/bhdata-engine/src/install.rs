//! Calling the privileged helper (`bhdata-helper`) for package actions.
//!
//! The helper is a separate root-run program with three fixed actions. This module only builds the
//! call and reads the exit code; the helper's own rules (root, root-owned location, platform) are
//! enforced by the helper itself.
//!
//! * `install-pinned` is for the installer, which runs as root. It is never called through `sudo`,
//!   because it is not in the `sudoers` file (spec, decision 3).
//! * `update` and `restart` are for the running server. As a non-root user they go through
//!   `sudo -n`, which fails at once instead of asking for a password.
//!
//! The helper's exit codes are part of its contract; [`HelperFailure::from_code`] must match
//! `bhdata-helper/src/error.rs` (a test compares the two files).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::EngineError;
use crate::sys;

/// The service unit's name; it must equal `SERVICE_UNIT` in `bhdata-helper/src/actions.rs`.
pub const SERVICE_UNIT_NAME: &str = "bhdata-server.service";
/// Where the installer puts the helper. The `sudoers` file and the service unit use these paths
/// too; tests compare them (see `host.rs`).
pub const HELPER_PATH: &str = "/usr/libexec/bhdata/bhdata-helper";
const SUDO: &str = "/usr/bin/sudo";
const TOOL_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperAction {
    InstallPinned,
    Update,
    Restart,
}

impl HelperAction {
    /// The exact word the helper accepts.
    pub fn name(self) -> &'static str {
        match self {
            HelperAction::InstallPinned => "install-pinned",
            HelperAction::Update => "update",
            HelperAction::Restart => "restart",
        }
    }
}

/// Why a helper call failed, from the helper's exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperFailure {
    /// 2: the call itself was wrong. A bug in this crate.
    Usage,
    /// 3: not root, or the helper sits somewhere others could change it.
    NotAllowed,
    /// 4: a safety rule said no (unsupported system, or a cluster of the pinned major exists).
    Refused,
    /// 5: a step failed (apt, systemd, or the check afterwards).
    StepFailed,
    /// 6: a file error inside the helper.
    Internal,
    /// Any other exit code, or `None` when `sudo` or the helper was killed.
    Other(Option<i32>),
}

impl HelperFailure {
    pub fn from_code(code: Option<i32>) -> HelperFailure {
        match code {
            Some(2) => HelperFailure::Usage,
            Some(3) => HelperFailure::NotAllowed,
            Some(4) => HelperFailure::Refused,
            Some(5) => HelperFailure::StepFailed,
            Some(6) => HelperFailure::Internal,
            other => HelperFailure::Other(other),
        }
    }
}

impl fmt::Display for HelperFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HelperFailure::Usage => f.write_str("the helper did not understand the call (bug)"),
            HelperFailure::NotAllowed => f.write_str(
                "the helper is not allowed to run here (not root, or not in a root-owned location)",
            ),
            HelperFailure::Refused => f.write_str("the helper refused (see its message above)"),
            HelperFailure::StepFailed => {
                f.write_str("a step of the helper failed (see its output above)")
            }
            HelperFailure::Internal => f.write_str("the helper hit a file error"),
            HelperFailure::Other(Some(c)) => {
                write!(f, "exited with status {c} (sudo may have refused)")
            }
            HelperFailure::Other(None) => f.write_str("was stopped by a signal"),
        }
    }
}

/// The program and arguments for one helper call. Root calls the helper directly; anyone else
/// goes through `sudo -n`.
pub fn helper_command(
    helper: &Path,
    action: HelperAction,
    as_root: bool,
) -> (PathBuf, Vec<OsString>) {
    if as_root {
        (helper.to_path_buf(), vec![OsString::from(action.name())])
    } else {
        (
            PathBuf::from(SUDO),
            vec![
                OsString::from("-n"),
                helper.as_os_str().to_os_string(),
                OsString::from(action.name()),
            ],
        )
    }
}

/// Runs one helper action and waits for it. The helper's output goes to this process's output
/// (the journal when run as a service).
pub fn run_helper(helper: &Path, action: HelperAction) -> Result<(), EngineError> {
    run_helper_as(helper, action, sys::running_as_root()?)
}

pub(crate) fn run_helper_as(
    helper: &Path,
    action: HelperAction,
    as_root: bool,
) -> Result<(), EngineError> {
    if !helper.is_absolute() {
        return Err(EngineError::invalid(
            "helper path",
            "must be an absolute path",
        ));
    }
    if action == HelperAction::InstallPinned && !as_root {
        return Err(EngineError::invalid(
            "install",
            "install-pinned must be run as root by the installer; it is not available through sudo",
        ));
    }
    let (program, args) = helper_command(helper, action, as_root);
    let status = Command::new(&program)
        .args(&args)
        .env_clear()
        .env("PATH", TOOL_PATH)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| EngineError::io(format!("could not start {}", program.display()), e))?;
    if status.success() {
        Ok(())
    } else {
        Err(EngineError::Helper {
            action,
            failure: HelperFailure::from_code(status.code()),
        })
    }
}

/// Whether the pinned server programs are in `pg_bin_dir`.
pub fn is_installed(pg_bin_dir: &Path) -> bool {
    ["postgres", "initdb", "pg_ctl", "pg_isready"]
        .iter()
        .all(|p| pg_bin_dir.join(p).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{script, TempDir};
    use std::fs;

    const HELPER: &str = "/usr/libexec/bhdata/bhdata-helper";

    #[test]
    fn action_names_are_the_helpers_words() {
        assert_eq!(HelperAction::InstallPinned.name(), "install-pinned");
        assert_eq!(HelperAction::Update.name(), "update");
        assert_eq!(HelperAction::Restart.name(), "restart");
    }

    #[test]
    fn root_calls_the_helper_directly() {
        let (prog, args) = helper_command(Path::new(HELPER), HelperAction::Update, true);
        assert_eq!(prog, Path::new(HELPER));
        assert_eq!(args, vec![OsString::from("update")]);
    }

    #[test]
    fn others_go_through_sudo_without_a_prompt() {
        let (prog, args) = helper_command(Path::new(HELPER), HelperAction::Restart, false);
        assert_eq!(prog, Path::new("/usr/bin/sudo"));
        assert_eq!(
            args,
            vec![
                OsString::from("-n"),
                OsString::from(HELPER),
                OsString::from("restart"),
            ]
        );
    }

    #[test]
    fn install_is_refused_for_non_root_before_anything_runs() {
        let dir = TempDir::new("install-nonroot");
        let marker = dir.path().join("ran");
        let helper = dir.path().join("helper");
        script(&helper, &format!("touch {}", marker.display()));
        let err = run_helper_as(&helper, HelperAction::InstallPinned, false).unwrap_err();
        assert!(matches!(err, EngineError::Invalid { .. }), "{err}");
        assert!(!marker.exists());
    }

    #[test]
    fn a_relative_helper_path_is_refused() {
        let err =
            run_helper_as(Path::new("bhdata-helper"), HelperAction::Update, true).unwrap_err();
        assert!(matches!(err, EngineError::Invalid { .. }));
    }

    #[test]
    fn exit_codes_map_to_failures() {
        let expect = [
            (2, HelperFailure::Usage),
            (3, HelperFailure::NotAllowed),
            (4, HelperFailure::Refused),
            (5, HelperFailure::StepFailed),
            (6, HelperFailure::Internal),
            (1, HelperFailure::Other(Some(1))),
            (127, HelperFailure::Other(Some(127))),
        ];
        for (code, want) in expect {
            assert_eq!(HelperFailure::from_code(Some(code)), want);
        }
        assert_eq!(HelperFailure::from_code(None), HelperFailure::Other(None));
    }

    #[test]
    fn a_root_call_reports_success_and_the_exact_failure_code() {
        let dir = TempDir::new("install-run");
        let helper = dir.path().join("helper");
        let seen = dir.path().join("seen");
        script(&helper, &format!("echo \"$@\" > {}", seen.display()));
        run_helper_as(&helper, HelperAction::Update, true).unwrap();
        assert_eq!(fs::read_to_string(&seen).unwrap().trim(), "update");

        script(&helper, "exit 4");
        match run_helper_as(&helper, HelperAction::Update, true).unwrap_err() {
            EngineError::Helper { action, failure } => {
                assert_eq!(action, HelperAction::Update);
                assert_eq!(failure, HelperFailure::Refused);
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn the_helper_gets_a_closed_environment() {
        let dir = TempDir::new("install-env");
        let helper = dir.path().join("helper");
        let seen = dir.path().join("seen");
        // cargo sets CARGO_MANIFEST_DIR for the test process; the helper must not inherit it.
        script(
            &helper,
            &format!(
                "echo \"${{CARGO_MANIFEST_DIR-unset}}\" > {}",
                seen.display()
            ),
        );
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        run_helper_as(&helper, HelperAction::Update, true).unwrap();
        assert_eq!(fs::read_to_string(&seen).unwrap().trim(), "unset");
    }

    #[test]
    fn installed_means_all_four_programs_are_there() {
        let dir = TempDir::new("is-installed");
        assert!(!is_installed(dir.path()));
        for p in ["postgres", "initdb", "pg_ctl"] {
            fs::write(dir.path().join(p), b"").unwrap();
        }
        assert!(!is_installed(dir.path()));
        fs::write(dir.path().join("pg_isready"), b"").unwrap();
        assert!(is_installed(dir.path()));
    }

    // The helper has no dependencies, so it cannot share these values; this keeps the copies equal.
    fn helper_source(file: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../bhdata-helper/src")
            .join(file);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn constant(text: &str, name: &str) -> String {
        let line = text
            .lines()
            .find(|l| l.contains(name) && l.contains('='))
            .unwrap_or_else(|| panic!("{name} not found"));
        line.split_once('=')
            .map(|(_, v)| v.trim().trim_end_matches(';').trim_matches('"').to_string())
            .unwrap_or_default()
    }

    #[test]
    fn exit_codes_match_the_helpers_source() {
        let src = helper_source("error.rs");
        for (name, failure) in [
            ("EXIT_USAGE", HelperFailure::Usage),
            ("EXIT_NOT_ALLOWED", HelperFailure::NotAllowed),
            ("EXIT_REFUSED", HelperFailure::Refused),
            ("EXIT_STEP_FAILED", HelperFailure::StepFailed),
            ("EXIT_INTERNAL", HelperFailure::Internal),
        ] {
            let code: i32 = constant(&src, name).parse().unwrap();
            assert_eq!(HelperFailure::from_code(Some(code)), failure, "{name}");
        }
    }

    #[test]
    fn action_names_and_pinned_major_match_the_helpers_source() {
        let actions = helper_source("actions.rs");
        for a in [
            HelperAction::InstallPinned,
            HelperAction::Update,
            HelperAction::Restart,
        ] {
            assert!(
                actions.contains(&format!("\"{}\"", a.name())),
                "{}",
                a.name()
            );
        }
        assert_eq!(
            constant(&actions, "pub const PG_MAJOR"),
            crate::PG_MAJOR.to_string()
        );
        assert_eq!(
            constant(&actions, "pub const SERVICE_UNIT"),
            SERVICE_UNIT_NAME
        );
    }
}
