//! bhdatactl: install, start, status, register, import, backup, restore, stress.
//!
//! At M1 only `status` exists (`bhdata_engine::host::ctl_main`); the rest arrives with milestone M8.

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    bhdata_engine::host::ctl_main(&args)
}
