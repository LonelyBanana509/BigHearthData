//! Server binary: wires every stage-0 crate into one process.
//!
//! At M1 it runs the engine host only (`bhdata_engine::host`): it starts and watches the private
//! PostgreSQL. The real server arrives with milestone M5.

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    bhdata_engine::host::server_main(&args)
}
