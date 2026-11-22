//! Server binary: wires every stage-0 crate into one process.
//!
//! Empty at M0: the real code arrives with milestone M5.

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("bhdata-server: not implemented yet (milestone M5)");
    ExitCode::from(2)
}
