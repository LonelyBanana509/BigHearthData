//! bhdatactl: install, start, status, register, import, backup, restore, stress.
//!
//! At M3: `status`, `schema apply`, `recipe register` and `recipe list` (see [`commands`]); the
//! rest arrives with milestone M8.

mod commands;

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    commands::main(&args)
}
