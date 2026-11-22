//! Checks made before any action runs: the helper must be root, must live in a location that only
//! root can change, and must be on a supported platform. Each check is a pure function over
//! text or numbers, with a thin wrapper that reads the real system, so tests need neither root
//! nor a particular distro.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::actions::Ctx;
use crate::error::HelperError;

/// Debian and Ubuntu only (design, supported platforms). Derivatives are not accepted: the
/// PostgreSQL apt repository setup is only supported on these two.
const SUPPORTED_IDS: [&str; 2] = ["debian", "ubuntu"];
/// The architectures the official PostgreSQL packages cover (design, engine manager).
const SUPPORTED_ARCH: [&str; 2] = ["x86_64", "aarch64"];

/// Reads the four user ids (real, effective, saved, filesystem) from the text of `/proc/self/status`.
pub fn parse_uids(status: &str) -> Option<[u32; 4]> {
    let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    let mut ids = line.split_whitespace().map(|n| n.parse::<u32>());
    let parsed = [
        ids.next()?.ok()?,
        ids.next()?.ok()?,
        ids.next()?.ok()?,
        ids.next()?.ok()?,
    ];
    if ids.next().is_some() {
        return None;
    }
    Some(parsed)
}

/// True only when all four ids are root. (The owner of `/proc/self` is not used: it reads as root
/// for a process that is not dumpable.)
pub fn require_root(status: &str) -> Result<(), HelperError> {
    match parse_uids(status) {
        Some([0, 0, 0, 0]) => Ok(()),
        Some(_) => Err(HelperError::NotAllowed("must run as root".into())),
        None => Err(HelperError::NotAllowed(
            "could not read the user ids of this process".into(),
        )),
    }
}

/// Owned by root and not writable by group or others.
pub fn owner_and_mode_ok(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o022 == 0
}

/// The file and every directory above it must pass [`owner_and_mode_ok`], otherwise another user
/// could swap the helper for something else.
pub fn check_location(path: &Path) -> Result<(), HelperError> {
    for p in path.ancestors() {
        let meta = fs::metadata(p)
            .map_err(|e| HelperError::io(format!("could not inspect {}", p.display()), e))?;
        if !owner_and_mode_ok(meta.uid(), meta.mode()) {
            return Err(HelperError::NotAllowed(format!(
                "{} is not root-owned and closed to group and other writes; the helper must run from a root-owned location",
                p.display()
            )));
        }
    }
    Ok(())
}

/// Reads one `KEY=value` from the text of `/etc/os-release` (quotes removed).
fn os_release_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == key).then(|| v.trim().trim_matches(|c| c == '"' || c == '\''))
    })
}

pub fn check_platform(os_release: &str, arch: &str) -> Result<(), HelperError> {
    let id = os_release_value(os_release, "ID").unwrap_or("");
    if !SUPPORTED_IDS.contains(&id) {
        return Err(HelperError::Refused(
            "unsupported system: only Debian and Ubuntu (with apt) are supported".into(),
        ));
    }
    if !SUPPORTED_ARCH.contains(&arch) {
        return Err(HelperError::Refused(
            "unsupported architecture: only amd64 and arm64 are supported".into(),
        ));
    }
    Ok(())
}

/// Everything checked before an action runs, against the real system.
pub fn preflight(ctx: &Ctx) -> Result<(), HelperError> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|e| HelperError::io("could not read /proc/self/status", e))?;
    require_root(&status)?;

    let exe = std::env::current_exe()
        .map_err(|e| HelperError::io("could not find the helper's own path", e))?;
    check_location(&exe)?;

    let os_release = fs::read_to_string(&ctx.os_release)
        .map_err(|e| HelperError::io(format!("could not read {}", ctx.os_release.display()), e))?;
    check_platform(&os_release, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const STATUS_ROOT: &str = "Name:\thelper\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n";
    const UBUNTU: &str = "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nID=ubuntu\nID_LIKE=debian\n";
    const DEBIAN: &str = "PRETTY_NAME=\"Debian GNU/Linux 12\"\nID=debian\n";
    const MINT: &str = "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n";

    fn my_uid() -> u32 {
        let status = fs::read_to_string("/proc/self/status").unwrap();
        parse_uids(&status).unwrap()[1]
    }

    #[test]
    fn parses_uid_line() {
        assert_eq!(parse_uids(STATUS_ROOT), Some([0, 0, 0, 0]));
        assert_eq!(parse_uids("Uid:\t1000\t0\t0\t0\n"), Some([1000, 0, 0, 0]));
    }

    #[test]
    fn rejects_malformed_uid_lines() {
        assert_eq!(parse_uids(""), None);
        assert_eq!(parse_uids("Uid:\t0\t0\t0\n"), None);
        assert_eq!(parse_uids("Uid:\t0\t0\t0\t0\t0\n"), None);
        assert_eq!(parse_uids("Uid:\t0\tx\t0\t0\n"), None);
    }

    #[test]
    fn root_needs_all_four_ids_zero() {
        assert!(require_root(STATUS_ROOT).is_ok());
        for line in [
            "Uid:\t1000\t1000\t1000\t1000",
            "Uid:\t1000\t0\t0\t0",
            "Uid:\t0\t1000\t0\t0",
            "Uid:\t0\t0\t1000\t0",
            "Uid:\t0\t0\t0\t1000",
        ] {
            assert!(require_root(line).is_err(), "{line}");
        }
        assert!(require_root("no uid here").is_err());
    }

    #[test]
    fn owner_and_mode_rules() {
        assert!(owner_and_mode_ok(0, 0o100755));
        assert!(owner_and_mode_ok(0, 0o100700));
        assert!(!owner_and_mode_ok(0, 0o100775), "group-writable");
        assert!(!owner_and_mode_ok(0, 0o100757), "world-writable");
        assert!(!owner_and_mode_ok(1000, 0o100755), "not root");
    }

    #[test]
    fn world_writable_directory_fails_the_location_check() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("loc-open");
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let file = dir.path().join("helper");
        fs::write(&file, b"x").unwrap();
        // Fails whoever runs the test: the directory is open to everyone.
        assert!(matches!(
            check_location(&file),
            Err(HelperError::NotAllowed(_))
        ));
    }

    #[test]
    fn location_check_follows_ownership() {
        // Only root can pass for a file it owns; for any other user the owner check must fail.
        let dir = TempDir::new("loc-closed");
        let file = dir.path().join("helper");
        fs::write(&file, b"x").unwrap();
        let result = check_location(&file);
        // The temp dir's own ancestors (e.g. a sticky /tmp, mode 1777) are world-writable, so this
        // is an error everywhere except where the temp dir sits in a closed root-owned tree.
        if my_uid() != 0 {
            assert!(result.is_err());
        }
    }

    #[test]
    fn missing_path_is_an_io_error() {
        let err = check_location(Path::new("/definitely/not/here")).unwrap_err();
        assert!(matches!(err, HelperError::Io { .. }));
    }

    #[test]
    fn accepts_debian_and_ubuntu_on_both_architectures() {
        for text in [UBUNTU, DEBIAN] {
            for arch in ["x86_64", "aarch64"] {
                assert!(check_platform(text, arch).is_ok());
            }
        }
    }

    #[test]
    fn refuses_other_systems_and_architectures() {
        assert!(matches!(
            check_platform(MINT, "x86_64"),
            Err(HelperError::Refused(_))
        ));
        assert!(matches!(
            check_platform("ID=fedora\n", "x86_64"),
            Err(HelperError::Refused(_))
        ));
        assert!(matches!(
            check_platform("", "x86_64"),
            Err(HelperError::Refused(_))
        ));
        assert!(matches!(
            check_platform(UBUNTU, "riscv64"),
            Err(HelperError::Refused(_))
        ));
    }

    #[test]
    fn os_release_values_are_unquoted() {
        assert_eq!(os_release_value("ID=\"ubuntu\"\n", "ID"), Some("ubuntu"));
        assert_eq!(os_release_value("ID='debian'\n", "ID"), Some("debian"));
        assert_eq!(os_release_value("ID_LIKE=debian\n", "ID"), None);
    }
}
