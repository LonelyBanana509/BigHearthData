//! `bhdatactl login`: the logins of the gateway (wire-protocol spec, sections 6 and 7). Lane C.
//!
//! ```text
//! bhdatactl login create <name>                 [--password-file <file>] [--config <path>]
//! bhdatactl login set-password <name>           [--password-file <file>] [--config <path>]
//! bhdatactl login grant <name> <permission>                              [--config <path>]
//! bhdatactl login grant <name> recipes <recipe>... | '*'                 [--config <path>]
//! bhdatactl login revoke <name> <permission>                             [--config <path>]
//! bhdatactl login revoke <name> recipes <recipe>... | '*'                [--config <path>]
//! bhdatactl login limit <name> rate|burst|connections <number>|default   [--config <path>]
//! bhdatactl login disable <name>   |   bhdatactl login enable <name>     [--config <path>]
//! bhdatactl login list                                                   [--config <path>]
//! bhdatactl login audit [<name>] [--limit <rows>]                        [--config <path>]
//! ```
//!
//! Permissions are `execute`, `register`, `admin` and `sql`. A new login may do nothing until it is
//! granted something. **A password is never taken from the command line**, where `ps` and the shell
//! history would show it: it is typed at a prompt (twice, without echo), read from one line of
//! standard input when that is not a terminal, or read from `--password-file`, which must not be
//! readable by anyone else. Every change is written to the audit log in the same transaction.

use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};

use bhdata_api::login::LoginRecord;
use bhdata_control::audit::AuditRow;
use bhdata_control::logins::{self, blocking, Change, Limit, Permission};

use crate::commands;

/// The rows `login audit` shows when `--limit` is not given.
const DEFAULT_AUDIT_ROWS: i64 = 50;
/// The most rows `login audit` shows.
const MAX_AUDIT_ROWS: i64 = 10_000;

/// What the command does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `create <name>`.
    Create(String),
    /// `set-password <name>`.
    SetPassword(String),
    /// `grant`, `revoke`, `limit`, `disable`, `enable`: one change to one login.
    Change(String, Change),
    /// `list`.
    List,
    /// `audit [<name>]`.
    Audit(Option<String>, i64),
}

/// A `login` command line, understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginArgs {
    /// What to do.
    pub action: Action,
    /// `--password-file`, for `create` and `set-password`.
    pub password_file: Option<PathBuf>,
    /// `--config <path>` or nothing, for the engine's config reader.
    pub rest: Vec<OsString>,
}

/// Reads what follows `login`.
///
/// # Errors
/// A sentence saying what is wrong.
pub fn parse(args: &[OsString]) -> Result<LoginArgs, String> {
    let mut words: Vec<String> = Vec::new();
    let mut password_file = None;
    let mut limit: Option<i64> = None;
    let mut rest: Vec<OsString> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let text = arg.to_string_lossy();
        let mut value = |what: &str| {
            it.next()
                .ok_or_else(|| format!("{what} needs a value"))
                .cloned()
        };
        match text.as_ref() {
            "--config" => {
                rest.push(arg.clone());
                rest.push(value("--config")?);
            }
            "--password-file" => password_file = Some(PathBuf::from(value("--password-file")?)),
            "--limit" => {
                let v = value("--limit")?.to_string_lossy().into_owned();
                limit = Some(
                    v.parse::<i64>()
                        .ok()
                        .filter(|n| (1..=MAX_AUDIT_ROWS).contains(n))
                        .ok_or_else(|| {
                            format!("--limit must be a number from 1 to {MAX_AUDIT_ROWS}")
                        })?,
                );
            }
            "--password" => {
                return Err(
                    "a password is never taken from the command line; leave it out to be \
                            asked, or use --password-file"
                        .to_string(),
                )
            }
            o if o.starts_with("--") => return Err(format!("unknown option {o}")),
            _ => words.push(text.into_owned()),
        }
    }
    let action = action(&words)?;
    if password_file.is_some() && !matches!(action, Action::Create(_) | Action::SetPassword(_)) {
        return Err("--password-file belongs to `create` and `set-password`".to_string());
    }
    if limit.is_some() && !matches!(action, Action::Audit(..)) {
        return Err("--limit belongs to `audit`".to_string());
    }
    let action = match action {
        Action::Audit(name, _) => Action::Audit(name, limit.unwrap_or(DEFAULT_AUDIT_ROWS)),
        other => other,
    };
    Ok(LoginArgs {
        action,
        password_file,
        rest,
    })
}

fn action(words: &[String]) -> Result<Action, String> {
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let name = |n: &str| n.to_string();
    let bad = |e: logins::LoginError| e.to_string();
    match w.as_slice() {
        [] => Err("a login command is missing".to_string()),
        ["create", n] => Ok(Action::Create(name(n))),
        ["set-password", n] => Ok(Action::SetPassword(name(n))),
        ["disable", n] => Ok(Action::Change(name(n), Change::Disable)),
        ["enable", n] => Ok(Action::Change(name(n), Change::Enable)),
        ["list"] => Ok(Action::List),
        ["audit"] => Ok(Action::Audit(None, 0)),
        ["audit", n] => Ok(Action::Audit(Some(name(n)), 0)),
        [verb @ ("grant" | "revoke"), n, "recipes", names @ ..] if !names.is_empty() => {
            let grant = *verb == "grant";
            let change = if names.contains(&"*") {
                if names.len() != 1 {
                    return Err("`*` cannot be mixed with recipe names".to_string());
                }
                if grant {
                    Change::GrantAllRecipes
                } else {
                    Change::RevokeAllRecipes
                }
            } else {
                let list = names.iter().map(|s| (*s).to_string()).collect();
                if grant {
                    Change::GrantRecipes(list)
                } else {
                    Change::RevokeRecipes(list)
                }
            };
            Ok(Action::Change(name(n), change))
        }
        ["grant", n, p] => Ok(Action::Change(
            name(n),
            Change::Grant(Permission::parse(p).map_err(bad)?),
        )),
        ["revoke", n, p] => Ok(Action::Change(
            name(n),
            Change::Revoke(Permission::parse(p).map_err(bad)?),
        )),
        ["limit", n, l, v] => {
            let limit = Limit::parse(l).map_err(bad)?;
            let change = if *v == "default" {
                Change::ClearLimit(limit)
            } else {
                let number: u32 = v
                    .parse()
                    .map_err(|_| format!("`{v}` is not a number or `default`"))?;
                Change::SetLimit(limit, number)
            };
            Ok(Action::Change(name(n), change))
        }
        _ => Err("not a login command".to_string()),
    }
}

// ---- passwords ----

/// `bytes` without one trailing line end (`\n` or `\r\n`) and nothing else, so a password may
/// itself hold spaces.
#[must_use]
pub fn without_line_end(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    bytes
}

fn read_password_file(path: &Path) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} can be read by others; run `chmod 600` on it first",
                path.display()
            ));
        }
    }
    std::fs::read(path)
        .map(without_line_end)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Asks on the terminal with echo off (through `stty`), and puts echo back whatever happens.
fn prompt_hidden(prompt: &str) -> Result<Vec<u8>, String> {
    let stty = |arg: &str| {
        Process::new("stty")
            .arg(arg)
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let hidden = stty("-echo");
    let mut line = Vec::new();
    let read = std::io::stdin().lock().read_until(b'\n', &mut line);
    if hidden {
        stty("echo");
    }
    eprintln!();
    read.map_err(|e| format!("cannot read the password: {e}"))?;
    Ok(without_line_end(line))
}

fn read_password(file: Option<&Path>) -> Result<Vec<u8>, String> {
    let password = if let Some(path) = file {
        read_password_file(path)?
    } else if std::io::stdin().is_terminal() {
        let first = prompt_hidden("password: ")?;
        let second = prompt_hidden("password again: ")?;
        if first != second {
            return Err("the two passwords differ; nothing was changed".to_string());
        }
        first
    } else {
        let mut line = Vec::new();
        std::io::stdin()
            .lock()
            .take(4096)
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("cannot read the password: {e}"))?;
        without_line_end(line)
    };
    if password.is_empty() {
        return Err("the password is empty".to_string());
    }
    Ok(password)
}

// ---- the words of the answers ----

/// One line per audit row.
#[must_use]
pub fn render_audit(rows: &[AuditRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["no audit rows".to_string()];
    }
    rows.iter()
        .map(|r| {
            let mut line = format!("{}  {}  {}  {}", r.id, r.at, r.event, r.name);
            if let Some(exists) = r.name_exists {
                line.push_str(if exists {
                    "  (login exists)"
                } else {
                    "  (no such login)"
                });
            }
            if let Some(source) = &r.source {
                line.push_str(&format!("  from {source}"));
            }
            if let Some(actor) = &r.actor {
                line.push_str(&format!("  by {actor}"));
            }
            if let Some(detail) = &r.detail {
                line.push_str(&format!("  {detail}"));
            }
            line
        })
        .collect()
}

/// One line per login.
#[must_use]
pub fn render_logins(records: &[LoginRecord]) -> Vec<String> {
    if records.is_empty() {
        return vec!["no logins".to_string()];
    }
    records.iter().map(logins::describe_login).collect()
}

/// Runs the command.
///
/// # Errors
/// A sentence saying what failed.
pub fn run(args: &LoginArgs) -> Result<Vec<String>, String> {
    let s = commands::setup(&args.rest)?;
    let e = |e: logins::LoginError| e.to_string();
    match &args.action {
        Action::Create(name) => {
            let password = read_password(args.password_file.as_deref())?;
            blocking::create(&s.target, name, &password, s.scram_iterations, &s.by).map_err(e)?;
            Ok(vec![format!(
                "login `{name}` created; it may do nothing until it is granted something"
            )])
        }
        Action::SetPassword(name) => {
            let password = read_password(args.password_file.as_deref())?;
            blocking::set_password(&s.target, name, &password, s.scram_iterations, &s.by)
                .map_err(e)?;
            Ok(vec![format!("password of `{name}` replaced")])
        }
        Action::Change(name, change) => {
            let applied = blocking::change(&s.target, name, change, &s.by).map_err(e)?;
            Ok(vec![format!("{name}: {}", applied.detail)])
        }
        Action::List => Ok(render_logins(&blocking::list(&s.target).map_err(e)?)),
        Action::Audit(name, limit) => Ok(render_audit(
            &blocking::audit_list(&s.target, name.as_deref(), *limit).map_err(e)?,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn action_of(list: &[&str]) -> Action {
        parse(&args(list)).expect("parses").action
    }

    #[test]
    fn each_command_is_read() {
        assert_eq!(
            action_of(&["create", "alice"]),
            Action::Create("alice".into())
        );
        assert_eq!(
            action_of(&["set-password", "alice"]),
            Action::SetPassword("alice".into())
        );
        assert_eq!(action_of(&["list"]), Action::List);
        assert_eq!(
            action_of(&["disable", "alice"]),
            Action::Change("alice".into(), Change::Disable)
        );
        assert_eq!(
            action_of(&["grant", "alice", "execute"]),
            Action::Change("alice".into(), Change::Grant(Permission::Execute))
        );
        assert_eq!(
            action_of(&["revoke", "alice", "sql"]),
            Action::Change("alice".into(), Change::Revoke(Permission::Sql))
        );
        assert_eq!(
            action_of(&["limit", "alice", "rate", "50"]),
            Action::Change("alice".into(), Change::SetLimit(Limit::Rate, 50))
        );
        assert_eq!(
            action_of(&["limit", "alice", "burst", "default"]),
            Action::Change("alice".into(), Change::ClearLimit(Limit::Burst))
        );
    }

    #[test]
    fn recipes_are_names_or_a_lone_star() {
        assert_eq!(
            action_of(&["grant", "alice", "recipes", "spend", "star"]),
            Action::Change(
                "alice".into(),
                Change::GrantRecipes(vec!["spend".into(), "star".into()])
            )
        );
        assert_eq!(
            action_of(&["grant", "alice", "recipes", "*"]),
            Action::Change("alice".into(), Change::GrantAllRecipes)
        );
        assert_eq!(
            action_of(&["revoke", "alice", "recipes", "*"]),
            Action::Change("alice".into(), Change::RevokeAllRecipes)
        );
        assert!(parse(&args(&["grant", "alice", "recipes", "*", "spend"])).is_err());
        assert!(parse(&args(&["grant", "alice", "recipes"])).is_err());
    }

    #[test]
    fn audit_takes_an_optional_name_and_a_row_limit() {
        assert_eq!(
            action_of(&["audit"]),
            Action::Audit(None, DEFAULT_AUDIT_ROWS)
        );
        assert_eq!(
            action_of(&["audit", "alice", "--limit", "5"]),
            Action::Audit(Some("alice".into()), 5)
        );
        for bad in ["0", "-1", "x", "10001"] {
            assert!(parse(&args(&["audit", "--limit", bad])).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_config_option_is_kept_for_the_config_reader() {
        let a = parse(&args(&["list", "--config", "/etc/x.toml"])).unwrap();
        assert_eq!(a.rest, args(&["--config", "/etc/x.toml"]));
    }

    #[test]
    fn a_password_on_the_command_line_is_refused_with_the_reason() {
        let e = parse(&args(&["create", "alice", "--password", "hunter2"])).unwrap_err();
        assert!(e.contains("never taken from the command line"), "{e}");
        assert!(!e.contains("hunter2"));
    }

    #[test]
    fn options_only_go_where_they_belong() {
        assert!(parse(&args(&["create", "alice", "--password-file", "/p"])).is_ok());
        assert!(parse(&args(&[
            "grant",
            "alice",
            "execute",
            "--password-file",
            "/p"
        ]))
        .is_err());
        assert!(parse(&args(&["list", "--limit", "3"])).is_err());
        assert!(parse(&args(&["list", "--bogus"])).is_err());
        assert!(parse(&args(&["create", "alice", "--password-file"])).is_err());
    }

    #[test]
    fn a_bad_command_line_is_an_error_not_a_default() {
        for bad in [
            args(&[]),
            args(&["create"]),
            args(&["create", "a", "b"]),
            args(&["grant", "alice"]),
            args(&["grant", "alice", "root"]),
            args(&["limit", "alice", "speed", "3"]),
            args(&["limit", "alice", "rate", "fast"]),
            args(&["delete", "alice"]),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_one_line_end_comes_off_a_password() {
        assert_eq!(without_line_end(b"pw\n".to_vec()), b"pw");
        assert_eq!(without_line_end(b"pw\r\n".to_vec()), b"pw");
        assert_eq!(
            without_line_end(b" pw with spaces \n".to_vec()),
            b" pw with spaces "
        );
        assert_eq!(without_line_end(b"pw\n\n".to_vec()), b"pw\n");
        assert_eq!(without_line_end(b"pw".to_vec()), b"pw");
    }

    #[cfg(unix)]
    #[test]
    fn a_password_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("bhdata-pw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pw");
        std::fs::write(&file, b"secret\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = read_password_file(&file).unwrap_err();
        assert!(e.contains("chmod 600") && !e.contains("secret"), "{e}");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_password_file(&file).unwrap(), b"secret");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_audit_listing_shows_what_each_row_has() {
        assert_eq!(render_audit(&[]), ["no audit rows"]);
        let failed = AuditRow {
            id: 7,
            at: "2026-11-24T10:00:00.123Z".into(),
            event: "login_failed".into(),
            name: "ghost".into(),
            name_exists: Some(false),
            source: Some("10.0.0.7".into()),
            actor: None,
            detail: None,
        };
        let changed = AuditRow {
            id: 8,
            at: "2026-11-24T10:01:00.000Z".into(),
            event: "permissions_changed".into(),
            name: "alice".into(),
            name_exists: None,
            source: None,
            actor: Some("bhdata".into()),
            detail: Some("granted execute".into()),
        };
        assert_eq!(
            render_audit(&[failed, changed]),
            [
                "7  2026-11-24T10:00:00.123Z  login_failed  ghost  (no such login)  from 10.0.0.7",
                "8  2026-11-24T10:01:00.000Z  permissions_changed  alice  by bhdata  granted execute"
            ]
        );
        assert_eq!(render_logins(&[]), ["no logins"]);
    }
}
