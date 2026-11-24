//! bhdatactl: install, start, status, register, import, backup, restore, stress.
//!
//! At M4: `status`, `schema apply`, `recipe register`, `recipe list` (see [`commands`]) and the dev
//! command `call` (see [`call`]); the rest arrives with milestone M8.

mod call;
mod commands;
mod gateway;
mod login;

use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    commands::main(&args)
}
