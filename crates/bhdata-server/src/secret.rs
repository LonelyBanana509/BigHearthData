//! The gateway's secret for the decoy answers of unknown logins (wire-protocol spec, decision 7).
//!
//! It must be the same across restarts, or the salt an unknown name gets would change between two
//! runs and a client could tell a name that does not exist from one that does. It is made once, the
//! first time the gateway starts, and kept in `gateway-secret` inside the engine's `base_dir` (a
//! folder the service user owns), readable by that user only.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use bhdata_proto::scram::ServerSecret;

/// The file's name inside `base_dir`.
pub const FILE_NAME: &str = "gateway-secret";

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn read(path: &Path) -> Result<ServerSecret, String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("cannot read the gateway secret {}: {e}", path.display()))?;
    unhex(&text).map(ServerSecret::new).ok_or_else(|| {
        format!(
            "{} is not a gateway secret (64 hex digits); remove it to make a new one",
            path.display()
        )
    })
}

/// The installation's secret: read from `base_dir`, or made there on the first call.
///
/// # Errors
/// A sentence: the folder cannot be written, the file is not a secret, or there is no randomness.
pub fn load_or_create(base_dir: &Path) -> Result<ServerSecret, String> {
    let path: PathBuf = base_dir.join(FILE_NAME);
    match fs::read_to_string(&path) {
        Ok(_) => return read(&path),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => {
            return Err(format!(
                "cannot read the gateway secret {}: {e}",
                path.display()
            ))
        }
    }
    let secret = ServerSecret::generate()
        .map_err(|e| format!("no randomness for the gateway secret: {e}"))?;
    // `create_new` and mode 0600 from the first byte: the secret is never readable by anyone else,
    // not even for a moment, and two servers starting together cannot both write it.
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(hex(secret.as_bytes()).as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|e| format!("cannot write the gateway secret {}: {e}", path.display()))?;
            Ok(secret)
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => read(&path),
        Err(e) => Err(format!(
            "cannot create the gateway secret {}: {e}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bhdata-secret-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_first_call_makes_it_private_and_the_second_returns_the_same() {
        let d = dir("same");
        let one = load_or_create(&d).unwrap();
        let mode = fs::metadata(d.join(FILE_NAME))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let two = load_or_create(&d).unwrap();
        assert_eq!(one.as_bytes(), two.as_bytes());
    }

    #[test]
    fn two_installations_have_different_secrets() {
        let a = load_or_create(&dir("a")).unwrap();
        let b = load_or_create(&dir("b")).unwrap();
        assert_ne!(a.as_bytes(), b.as_bytes());
    }

    #[test]
    fn a_file_that_is_not_a_secret_is_refused_and_not_replaced() {
        let d = dir("junk");
        fs::write(d.join(FILE_NAME), "not a secret").unwrap();
        let e = load_or_create(&d).unwrap_err();
        assert!(e.contains("not a gateway secret"), "{e}");
        assert_eq!(
            fs::read_to_string(d.join(FILE_NAME)).unwrap(),
            "not a secret"
        );
    }

    #[test]
    fn a_missing_folder_is_a_sentence() {
        let e = load_or_create(Path::new("/nonexistent-bhdata-dir")).unwrap_err();
        assert!(e.contains("gateway secret"), "{e}");
    }
}
