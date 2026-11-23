//! Server binary: wires every stage-0 crate into one process.
//!
//! It runs the engine host (`bhdata_engine::host`), which starts and watches the private
//! PostgreSQL, and after the bootstrap it starts the write path ([`write_path`]): the write queue
//! and the compaction job. The gateway and the rest arrive with milestone M5.

mod write_path;

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    bhdata_engine::host::server_main_with(&args, write_path::start)
}
