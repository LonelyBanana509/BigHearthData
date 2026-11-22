//! Tiny privileged helper binary that runs a fixed list of actions only.
//!
//! Usage: `bhdata-helper <action>` with `action` one of `install-pinned`, `update`, `restart`.
//! Anything else is refused. There are no options, no free-form values and no shell strings;
//! every package name, path and service name is fixed in `actions.rs`.
//!
//! It must run as root from a root-owned location, on Debian or Ubuntu. It has no dependencies
//! beyond the standard library, on purpose: it is the one piece of the project that runs with
//! root rights.
//!
//! Exit codes: 0 ok, 2 usage, 3 not allowed to run here, 4 refused, 5 a step failed, 6 internal.

mod actions;
mod error;
mod exec;
mod guard;

#[cfg(test)]
mod testutil;

use std::ffi::OsString;
use std::process::ExitCode;

use actions::{Action, Ctx};
use error::HelperError;
use exec::SystemRunner;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match real_main(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bhdata-helper: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

fn real_main(args: &[OsString]) -> Result<(), HelperError> {
    // A bad argument is reported before anything else is looked at, root or not.
    let action = Action::parse(args)?;
    let ctx = Ctx::system();
    guard::preflight(&ctx)?;
    eprintln!("bhdata-helper: action {}", action.name());
    actions::run(action, &ctx, &mut SystemRunner)?;
    eprintln!("bhdata-helper: action {} finished", action.name());
    Ok(())
}
