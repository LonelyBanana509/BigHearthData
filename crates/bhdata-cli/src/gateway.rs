//! `bhdatactl gateway reload`: makes the running server read its certificate and key again
//! (wire-protocol spec, decision 2 and test W12).
//!
//! ```text
//! bhdatactl gateway reload
//! ```
//!
//! The server reloads on `SIGHUP`, without dropping an open connection; a reload that fails (a
//! missing file, a key that does not match) leaves the old pair in place and is logged by the server.
//! This command finds the server's process through systemd and sends it the signal. It needs no
//! root: the service runs as the service user and so does `bhdatactl` (see `call.rs`), and
//! `systemctl show` may be read by anyone.

use std::ffi::OsString;
use std::process::Command as Process;

/// The systemd unit of the server (`packaging/bhdata-server.service`).
const UNIT: &str = "bhdata-server";

/// A `gateway` command line, understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayArgs {
    /// `gateway reload`.
    Reload,
}

/// Reads what follows `gateway`.
///
/// # Errors
/// A sentence saying what is wrong.
pub fn parse(args: &[OsString]) -> Result<GatewayArgs, String> {
    match args.first().and_then(|a| a.to_str()) {
        Some("reload") if args.len() == 1 => Ok(GatewayArgs::Reload),
        Some("reload") => Err("`gateway reload` takes no options".to_string()),
        Some(other) => Err(format!("unknown gateway command `{other}`")),
        None => Err("gateway needs a command (reload)".to_string()),
    }
}

/// The process id in the output of `systemctl show -p MainPID --value <unit>`.
///
/// # Errors
/// A sentence: the output is not a number, or the number is 0 (the unit is not running).
pub fn parse_main_pid(output: &str) -> Result<u32, String> {
    let text = output.trim();
    match text.parse::<u32>() {
        Ok(0) => Err(format!("{UNIT} is not running")),
        Ok(pid) => Ok(pid),
        Err(_) => Err(format!("systemctl did not give a process id: `{text}`")),
    }
}

fn main_pid() -> Result<u32, String> {
    let out = Process::new("systemctl")
        .args(["show", "-p", "MainPID", "--value", UNIT])
        .output()
        .map_err(|e| format!("cannot run systemctl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "systemctl could not show {UNIT}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_main_pid(&String::from_utf8_lossy(&out.stdout))
}

/// Runs the command; the lines to print, or why it failed.
///
/// # Errors
/// A sentence: no systemd, the server is not running, or the signal could not be sent.
pub fn run(args: &GatewayArgs) -> Result<Vec<String>, String> {
    match args {
        GatewayArgs::Reload => {
            let pid = main_pid()?;
            let out = Process::new("kill")
                .args(["-HUP", &pid.to_string()])
                .output()
                .map_err(|e| format!("cannot run kill: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "could not signal the server (process {pid}); run this as the service user: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(vec![format!(
                "asked the server (process {pid}) to reload its certificate; the result is in its log"
            )])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn reload_is_read_and_anything_else_is_refused() {
        assert_eq!(parse(&args(&["reload"])), Ok(GatewayArgs::Reload));
        for (bad, word) in [
            (args(&[]), "needs a command"),
            (args(&["restart"]), "restart"),
            (args(&["reload", "--now"]), "no options"),
        ] {
            let e = parse(&bad).unwrap_err();
            assert!(e.contains(word), "{bad:?}: {e}");
        }
    }

    #[test]
    fn the_process_id_is_read_from_systemctl() {
        assert_eq!(parse_main_pid("4242\n"), Ok(4242));
        assert!(parse_main_pid("0\n").unwrap_err().contains("not running"));
        for junk in ["", "abc", "-5", "1 2"] {
            assert!(parse_main_pid(junk).is_err(), "{junk:?}");
        }
    }
}
