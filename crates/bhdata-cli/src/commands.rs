//! The commands of `bhdatactl` (spec `docs/specs/recipe-compiler.md`, section 5). At M3:
//!
//! ```text
//! bhdatactl status                         [--config <absolute path>]
//! bhdatactl schema apply <file>            [--config <absolute path>]
//! bhdatactl recipe register <file>         [--config <absolute path>]
//! bhdatactl recipe list                    [--config <absolute path>]
//! ```
//!
//! `status` is the engine's. The others run as the local admin only (permission checks arrive with
//! logins, M5): like `status` they must be run as the service user, because the database login
//! map accepts only that operating-system user. Exit codes: 0 done, 1 failed, 2 bad command line.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use bhdata_control::blocking;
use bhdata_control::registry::{RecipeRow, SchemaOutcome};
use bhdata_engine::host::{self, SERVICE_USER};
use bhdata_engine::instance;
use bhdata_recipe::Limits;

const USAGE: &str = "usage: bhdatactl status [--config <absolute path>]\n\
       bhdatactl schema apply <file> [--config <absolute path>]\n\
       bhdatactl recipe register <file> [--config <absolute path>]\n\
       bhdatactl recipe list [--config <absolute path>]";

/// A command line, understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `status`, with all its arguments for the engine to read.
    Status(Vec<OsString>),
    /// `schema apply <file>`.
    SchemaApply {
        /// The schema file.
        file: PathBuf,
        /// What follows the file (`--config <path>` or nothing).
        rest: Vec<OsString>,
    },
    /// `recipe register <file>`.
    RecipeRegister {
        /// The recipe file.
        file: PathBuf,
        /// What follows the file.
        rest: Vec<OsString>,
    },
    /// `recipe list`.
    RecipeList {
        /// What follows.
        rest: Vec<OsString>,
    },
}

/// Reads the command line (without the program name).
///
/// # Errors
/// A sentence saying what is wrong; the caller adds the usage text.
pub fn parse(args: &[OsString]) -> Result<Command, String> {
    let word = |i: usize| args.get(i).and_then(|a| a.to_str());
    let file_and_rest = |at: usize| -> Result<(PathBuf, Vec<OsString>), String> {
        match args.get(at) {
            Some(f) if !f.to_string_lossy().starts_with("--") => {
                Ok((PathBuf::from(f), args[at + 1..].to_vec()))
            }
            _ => Err("a file is missing".to_string()),
        }
    };
    match (word(0), word(1)) {
        (Some("status"), _) => Ok(Command::Status(args.to_vec())),
        (Some("schema"), Some("apply")) => {
            let (file, rest) = file_and_rest(2)?;
            Ok(Command::SchemaApply { file, rest })
        }
        (Some("recipe"), Some("register")) => {
            let (file, rest) = file_and_rest(2)?;
            Ok(Command::RecipeRegister { file, rest })
        }
        (Some("recipe"), Some("list")) => Ok(Command::RecipeList {
            rest: args[2..].to_vec(),
        }),
        (None, _) => Err("no command given".to_string()),
        _ => Err("unknown command".to_string()),
    }
}

/// One line per registered version.
#[must_use]
pub fn render_list(rows: &[RecipeRow]) -> Vec<String> {
    if rows.is_empty() {
        return vec!["no recipes registered".to_string()];
    }
    rows.iter()
        .map(|r| {
            format!(
                "{}@{}  {}  schema {}  {}  {}  by {}",
                r.name,
                r.version,
                r.status,
                r.schema_version,
                &r.content_hash[..r.content_hash.len().min(12)],
                r.registered_at,
                r.registered_by
            )
        })
        .collect()
}

/// The words for a schema outcome.
#[must_use]
pub fn render_schema(outcome: &SchemaOutcome) -> String {
    match outcome {
        SchemaOutcome::Unchanged { number } => {
            format!("schema unchanged, still at version {number}")
        }
        SchemaOutcome::Applied { number, changes } if changes.is_empty() => {
            format!("schema version {number} recorded (no changes)")
        }
        SchemaOutcome::Applied { number, changes } => {
            format!("schema version {number} applied: {}", changes.join("; "))
        }
    }
}

struct Setup {
    target: bhdata_control::Target,
    limits: Limits,
    by: String,
}

fn setup(rest: &[OsString]) -> Result<Setup, String> {
    let path = host::config_path_from_args(rest)?;
    let config = host::load_config(&path).map_err(|e| e.to_string())?;
    let spec = host::spec_from(&config, SERVICE_USER.to_string()).map_err(|e| e.to_string())?;
    let by = instance::current_user().map_err(|e| e.to_string())?;
    Ok(Setup {
        target: host::target_of(&spec),
        limits: Limits::from(&config.recipes),
        by,
    })
}

fn read_file(path: &std::path::Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn run(command: &Command) -> Result<Vec<String>, String> {
    match command {
        Command::Status(_) => Err("status is handled by the engine".to_string()),
        Command::SchemaApply { file, rest } => {
            let s = setup(rest)?;
            let text = read_file(file)?;
            let outcome =
                blocking::apply_schema(&s.target, &text, &s.by).map_err(|e| e.to_string())?;
            Ok(vec![render_schema(&outcome)])
        }
        Command::RecipeRegister { file, rest } => {
            let s = setup(rest)?;
            let text = read_file(file)?;
            let outcome = blocking::register_recipe(&s.target, &text, &s.limits, &s.by)
                .map_err(|e| e.to_string())?;
            Ok(vec![outcome.describe()])
        }
        Command::RecipeList { rest } => {
            let s = setup(rest)?;
            let rows = blocking::list(&s.target).map_err(|e| e.to_string())?;
            Ok(render_list(&rows))
        }
    }
}

/// The whole of `bhdatactl`.
pub fn main(args: &[OsString]) -> ExitCode {
    let command = match parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bhdatactl: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Command::Status(all) = &command {
        return host::ctl_main(all);
    }
    match run(&command) {
        Ok(lines) => {
            for line in lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("bhdatactl: {e}");
            ExitCode::from(1)
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
    fn each_command_is_read_with_its_file_and_its_options() {
        assert_eq!(
            parse(&args(&["schema", "apply", "s.toml"])),
            Ok(Command::SchemaApply {
                file: PathBuf::from("s.toml"),
                rest: vec![]
            })
        );
        assert_eq!(
            parse(&args(&[
                "recipe",
                "register",
                "r.toml",
                "--config",
                "/etc/x.toml"
            ])),
            Ok(Command::RecipeRegister {
                file: PathBuf::from("r.toml"),
                rest: args(&["--config", "/etc/x.toml"])
            })
        );
        assert_eq!(
            parse(&args(&["recipe", "list"])),
            Ok(Command::RecipeList { rest: vec![] })
        );
        assert_eq!(
            parse(&args(&["status", "--config", "/a.toml"])),
            Ok(Command::Status(args(&["status", "--config", "/a.toml"])))
        );
    }

    #[test]
    fn a_missing_file_or_an_unknown_command_is_a_bad_command_line() {
        for bad in [
            args(&[]),
            args(&["schema"]),
            args(&["schema", "apply"]),
            args(&["schema", "apply", "--config", "/a.toml"]),
            args(&["recipe", "register"]),
            args(&["recipe", "retire", "x"]),
            args(&["restart"]),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(main(&args(&["restart"])), ExitCode::from(2));
        assert_eq!(main(&args(&["recipe", "register"])), ExitCode::from(2));
    }

    #[test]
    fn a_bad_option_after_the_file_is_an_error_not_a_default() {
        let cmd = parse(&args(&["recipe", "list", "extra"])).expect("parses");
        let Command::RecipeList { rest } = cmd else {
            panic!()
        };
        assert!(setup(&rest).is_err());
    }

    #[test]
    fn the_list_has_one_line_per_version_and_says_so_when_empty() {
        assert_eq!(render_list(&[]), ["no recipes registered"]);
        let row = RecipeRow {
            name: "spend".into(),
            version: 3,
            content_hash: "0123456789abcdef0123".into(),
            schema_version: 2,
            status: "active".into(),
            registered_at: "2026-11-23T10:00:00Z".into(),
            registered_by: "bhdata".into(),
        };
        assert_eq!(
            render_list(&[row]),
            ["spend@3  active  schema 2  0123456789ab  2026-11-23T10:00:00Z  by bhdata"]
        );
    }

    #[test]
    fn schema_outcomes_are_one_sentence() {
        assert_eq!(
            render_schema(&SchemaOutcome::Unchanged { number: 2 }),
            "schema unchanged, still at version 2"
        );
        assert_eq!(
            render_schema(&SchemaOutcome::Applied {
                number: 3,
                changes: vec!["add table a".into(), "add column b.c".into()]
            }),
            "schema version 3 applied: add table a; add column b.c"
        );
    }
}
