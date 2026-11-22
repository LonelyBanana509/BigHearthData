//! Small facts about the running process, read from `/proc` and `/etc/passwd` (no libc, no
//! `unsafe`). Each reader has a pure parser next to it so tests need no particular user.

use std::fs;

use crate::error::EngineError;

/// The four user ids (real, effective, saved, filesystem) from the text of `/proc/self/status`.
pub fn parse_uids(status: &str) -> Option<[u32; 4]> {
    let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    let mut ids = line.split_whitespace().map(|n| n.parse::<u32>());
    let parsed = [
        ids.next()?.ok()?,
        ids.next()?.ok()?,
        ids.next()?.ok()?,
        ids.next()?.ok()?,
    ];
    ids.next().is_none().then_some(parsed)
}

/// The name that goes with `uid` in the text of `/etc/passwd`.
pub fn user_name_for_uid(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        let _password = f.next()?;
        let id: u32 = f.next()?.parse().ok()?;
        (id == uid && !name.is_empty()).then(|| name.to_string())
    })
}

fn effective_uid() -> Result<u32, EngineError> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|e| EngineError::io("could not read /proc/self/status", e))?;
    parse_uids(&status)
        .map(|ids| ids[1])
        .ok_or_else(|| EngineError::invalid("process", "could not read this process's user id"))
}

/// PostgreSQL refuses to start when the effective user is root.
pub fn running_as_root() -> Result<bool, EngineError> {
    Ok(effective_uid()? == 0)
}

/// The name of the user this process runs as. It is also the name of the instance's superuser,
/// because the instance only accepts connections that the operating system vouches for ("peer").
pub fn current_user_name() -> Result<String, EngineError> {
    let uid = effective_uid()?;
    let passwd = fs::read_to_string("/etc/passwd")
        .map_err(|e| EngineError::io("could not read /etc/passwd", e))?;
    user_name_for_uid(&passwd, uid)
        .ok_or_else(|| EngineError::invalid("process", format!("no user name for id {uid}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uid_lines() {
        assert_eq!(
            parse_uids("Name:\tx\nUid:\t1000\t1001\t1002\t1003\n"),
            Some([1000, 1001, 1002, 1003])
        );
        assert_eq!(parse_uids("Uid:\t0\t0\t0\n"), None);
        assert_eq!(parse_uids("Uid:\t0\t0\t0\t0\t0\n"), None);
        assert_eq!(parse_uids("Uid:\t0\tx\t0\t0\n"), None);
        assert_eq!(parse_uids(""), None);
    }

    #[test]
    fn finds_user_names_in_passwd_text() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nbhdata:x:998:998::/var/lib/bhdata:/usr/sbin/nologin\nbroken line\n";
        assert_eq!(user_name_for_uid(passwd, 0).as_deref(), Some("root"));
        assert_eq!(user_name_for_uid(passwd, 998).as_deref(), Some("bhdata"));
        assert_eq!(user_name_for_uid(passwd, 5), None);
        assert_eq!(user_name_for_uid(":x:7:7:::\n", 7), None);
    }

    #[test]
    fn this_process_has_a_user_name() {
        // Works for whoever runs the tests, root or not.
        assert!(!current_user_name().unwrap().is_empty());
        let _ = running_as_root().unwrap();
    }
}
