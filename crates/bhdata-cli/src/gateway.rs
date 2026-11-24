//! `bhdatactl gateway reload`: makes the running server read its certificate and key again
//! (wire-protocol spec, decision 2 and test W12).
//!
//! ```text
//! bhdatactl gateway reload
//! bhdatactl gateway pin [--config <absolute path>]
//! ```
//!
//! `pin` prints the key pin of the certificate the config names: the SHA-256 of its public key, 64
//! hex digits, which is what a client is given to trust (spec, decision 2). A renewed certificate
//! with the same key has the same pin.
//!
//! The server reloads on `SIGHUP`, without dropping an open connection; a reload that fails (a
//! missing file, a key that does not match) leaves the old pair in place and is logged by the server.
//! This command finds the server's process through systemd and sends it the signal. It needs no
//! root: the service runs as the service user and so does `bhdatactl` (see `call.rs`), and
//! `systemctl show` may be read by anyone.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command as Process;

use bhdata_client::client::KeyPin;
use bhdata_engine::host;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::CertificateDer;

/// The systemd unit of the server (`packaging/bhdata-server.service`).
const UNIT: &str = "bhdata-server";

/// A `gateway` command line, understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayArgs {
    /// `gateway reload`.
    Reload,
    /// `gateway pin`, with what follows it (`--config <path>` or nothing).
    Pin {
        /// What follows `pin`.
        rest: Vec<OsString>,
    },
}

/// Reads what follows `gateway`.
///
/// # Errors
/// A sentence saying what is wrong.
pub fn parse(args: &[OsString]) -> Result<GatewayArgs, String> {
    match args.first().and_then(|a| a.to_str()) {
        Some("reload") if args.len() == 1 => Ok(GatewayArgs::Reload),
        Some("reload") => Err("`gateway reload` takes no options".to_string()),
        Some("pin") => Ok(GatewayArgs::Pin {
            rest: args[1..].to_vec(),
        }),
        Some(other) => Err(format!("unknown gateway command `{other}`")),
        None => Err("gateway needs a command (reload or pin)".to_string()),
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

/// The pin of the first (leaf) certificate in a PEM file.
///
/// # Errors
/// A sentence: the file cannot be read, holds no certificate, or the certificate cannot be read.
pub fn pin_of_pem_file(path: &Path) -> Result<KeyPin, String> {
    let cert = CertificateDer::pem_file_iter(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?
        .next()
        .ok_or_else(|| format!("{} holds no certificate", path.display()))?
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    KeyPin::of_certificate(&cert).ok_or_else(|| {
        format!(
            "{} does not hold a certificate this client can read",
            path.display()
        )
    })
}

/// Runs the command; the lines to print, or why it failed.
///
/// # Errors
/// A sentence: no systemd, the server is not running, or the signal could not be sent.
pub fn run(args: &GatewayArgs) -> Result<Vec<String>, String> {
    match args {
        GatewayArgs::Pin { rest } => {
            let path = host::config_path_from_args(rest)?;
            let config = host::load_config(&path).map_err(|e| e.to_string())?;
            let cert = config.gateway.cert_path.ok_or_else(|| {
                "[gateway] has no cert_path (a plain loopback gateway has no key to pin)"
                    .to_string()
            })?;
            Ok(vec![pin_of_pem_file(&cert)?.to_hex()])
        }
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
    fn pin_takes_the_config_option_and_nothing_else_is_assumed() {
        assert_eq!(
            parse(&args(&["pin"])),
            Ok(GatewayArgs::Pin { rest: vec![] })
        );
        assert_eq!(
            parse(&args(&["pin", "--config", "/etc/x.toml"])),
            Ok(GatewayArgs::Pin {
                rest: args(&["--config", "/etc/x.toml"])
            })
        );
    }

    /// A certificate made by openssl, and the pin openssl computes for it
    /// (`x509 -pubkey | pkey -pubin -outform der | sha256sum`), so the check does not use this code.
    const CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBkjCCATmgAwIBAgIURraCR7EB9PdTvJcaPTcp2mK9apEwCgYIKoZIzj0EAwIw\n\
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MTEyNDA5MTUzMloXDTM2MTEyMTA5\n\
MTUzMlowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D\n\
AQcDQgAE9zXPXHxaWbpvNe6v8GvTIKgzKEki295kc1Xcbi08Llr1fT5oert+k1UV\n\
fLird9X9rTZ+XPPlqXJkKWcL+BzVhaNpMGcwHQYDVR0OBBYEFK2hjzhzY+rNnYbW\n\
OiX/XmdBb1ppMB8GA1UdIwQYMBaAFK2hjzhzY+rNnYbWOiX/XmdBb1ppMA8GA1Ud\n\
EwEB/wQFMAMBAf8wFAYDVR0RBA0wC4IJbG9jYWxob3N0MAoGCCqGSM49BAMCA0cA\n\
MEQCIE5I8JL+vfiIJRN2uh8Px3fNgaHO8ErBM5uqWJPWu6WZAiASjKUfumT6fL0S\n\
0M7iDoEyobvX+fj0HEWHSIQP3SPJ7g==\n\
-----END CERTIFICATE-----\n\
";
    const EXPECTED: &str = "77c37db606aa0fa4e4423acff4007215acba7a211b9f5211322c297f86099d92";

    #[test]
    fn the_pin_of_a_pem_file_is_what_openssl_computes() {
        let path = std::env::temp_dir().join(format!("bhdata-pin-{}.crt", std::process::id()));
        std::fs::write(&path, CERT).unwrap();
        assert_eq!(pin_of_pem_file(&path).unwrap().to_hex(), EXPECTED);
        std::fs::write(&path, "not a certificate").unwrap();
        assert!(pin_of_pem_file(&path)
            .unwrap_err()
            .contains("no certificate"));
        std::fs::remove_file(&path).unwrap();
        assert!(pin_of_pem_file(&path).unwrap_err().contains("cannot read"));
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
