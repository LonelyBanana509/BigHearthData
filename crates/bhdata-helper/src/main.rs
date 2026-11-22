//! Tiny privileged helper binary that runs a fixed list of actions only.
//!
//! Empty at M0: the real code arrives with milestone M1.

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("bhdata-helper: not implemented yet (milestone M1)");
    ExitCode::from(2)
}
