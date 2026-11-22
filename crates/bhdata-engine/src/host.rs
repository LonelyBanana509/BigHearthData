//! What the service runs at M1, and the report behind `bhdatactl status`.
//!
//! At M1 the "server" is only the engine host: it reads the config, creates and starts the private
//! PostgreSQL, watches it, and applies minor-release updates in the maintenance window. The real
//! server (M5) grows around this; the pieces stay as they are.
//!
//! This lives in the engine crate so that `bhdata-server` and `bhdatactl` stay thin and need no
//! crate dependencies beyond this one (plan section 5).

use std::convert::Infallible;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bhdata_types::{Clock, Config, SystemClock};

use crate::default_pg_bin_dir;
use crate::error::EngineError;
use crate::install::{self, HELPER_PATH};
use crate::instance::{self, InitOutcome, InstanceSpec, PgVersion};
use crate::supervise::{self, Health, PgOps, StartOutcome, Status, SuperviseConfig, Supervisor};
use crate::update::{Report, SystemUpdateOps, Updater};

/// Where the installer puts the config file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/bhdata/config.toml";
/// Where the installer puts the server binary.
pub const SERVER_PATH: &str = "/usr/libexec/bhdata/bhdata-server";
/// The user the service runs as. It is also the instance's superuser.
pub const SERVICE_USER: &str = "bhdata";
/// The one instance M1 runs. More shards come with sharding.
pub const SHARD_ID: &str = "shard0";
/// How often the host looks at the watcher and the update window.
const LOOP_INTERVAL: Duration = Duration::from_secs(10);

fn log(msg: &str) {
    eprintln!("bhdata: {msg}");
}

/// The config path from the command line: nothing (the default path) or `--config <absolute path>`.
pub fn config_path_from_args(args: &[OsString]) -> Result<PathBuf, String> {
    match args {
        [] => Ok(PathBuf::from(DEFAULT_CONFIG_PATH)),
        [flag, path] if flag == "--config" => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err("--config needs an absolute path".to_string())
            }
        }
        _ => Err(format!(
            "usage: [--config <absolute path>] (default {DEFAULT_CONFIG_PATH})"
        )),
    }
}

/// Reads config text. `origin` names where it came from, for messages. Unknown keys and values out
/// of range are errors, so a typo never becomes a silently ignored setting.
pub fn parse_config(text: &str, origin: &str) -> Result<Config, EngineError> {
    let config: Config = toml::from_str(text)
        .map_err(|e| EngineError::invalid("config", format!("{origin}: {e}")))?;
    config
        .validate()
        .map_err(|e| EngineError::invalid("config", format!("{origin}: {e}")))?;
    Ok(config)
}

pub fn load_config(path: &Path) -> Result<Config, EngineError> {
    let text = fs::read_to_string(path)
        .map_err(|e| EngineError::io(format!("could not read {}", path.display()), e))?;
    parse_config(&text, &path.display().to_string())
}

/// The one instance M1 runs, from the config.
pub fn spec_from(config: &Config, superuser: String) -> Result<InstanceSpec, EngineError> {
    InstanceSpec::from_config(
        &config.engine,
        config.durability,
        SHARD_ID,
        default_pg_bin_dir(),
        superuser,
    )
}

fn describe(status: &Status) -> String {
    let mut text = format!(
        "instance {SHARD_ID}: {:?}, {} restart(s)",
        status.state, status.restarts
    );
    if let Some(error) = &status.last_error {
        text.push_str(&format!(", last restart failed: {error}"));
    }
    text
}

/// Creates (once) and starts the instance, then watches it and applies updates, for as long as the
/// process lives. Only returns on an error that stops it from starting.
pub fn run(config: &Config) -> Result<Infallible, EngineError> {
    let spec = spec_from(config, instance::current_user()?)?;
    if !install::is_installed(&spec.pg_bin_dir) {
        return Err(EngineError::invalid(
            "postgresql",
            format!(
                "the PostgreSQL programs are not in {}; run the installer",
                spec.pg_bin_dir.display()
            ),
        ));
    }
    match instance::init(&spec)? {
        InitOutcome::Created => log(&format!("created instance {SHARD_ID}")),
        InitOutcome::AlreadyInitialised => log(&format!("instance {SHARD_ID} found")),
    }
    match supervise::start(&spec)? {
        StartOutcome::Started => log(&format!("started instance {SHARD_ID}")),
        StartOutcome::AlreadyRunning => log(&format!("instance {SHARD_ID} was already running")),
    }

    let supervisor = Supervisor::spawn(Arc::new(PgOps(spec.clone())), SuperviseConfig::default());
    let ops = SystemUpdateOps {
        helper: PathBuf::from(HELPER_PATH),
        specs: vec![spec],
    };
    let mut updater = Updater::new(config.engine.maintenance_hour_utc);
    let clock = SystemClock::new();
    let mut last = supervisor.status();
    loop {
        thread::sleep(LOOP_INTERVAL);
        let now = supervisor.status();
        if now != last {
            log(&describe(&now));
            last = now;
        }
        match updater.tick(clock.now(), &ops) {
            Ok(Report::OutsideWindow | Report::AlreadyAttemptedToday) => {}
            Ok(Report::UpToDate) => log("minor-release update checked: nothing to restart"),
            Ok(Report::Restarted(ids)) => {
                log(&format!(
                    "minor-release update applied; restarted {}",
                    ids.join(", ")
                ));
            }
            Err(e) => log(&format!("minor-release update failed: {e}")),
        }
    }
}

/// The whole of `bhdata-server` at M1: exit code 2 for a bad command line, 1 for anything that
/// stops it from starting.
pub fn server_main(args: &[OsString]) -> ExitCode {
    let path = match config_path_from_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bhdata-server: {e}");
            return ExitCode::from(2);
        }
    };
    let result = load_config(&path).and_then(|config| run(&config));
    match result {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("bhdata-server: {e}");
            ExitCode::from(1)
        }
    }
}

/// What `bhdatactl status` found out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusFacts {
    pub bin_dir: PathBuf,
    pub installed: bool,
    /// False when the instance's folder cannot be read by this user.
    pub accessible: bool,
    pub running: bool,
    pub health: Health,
    pub started_with: Option<PgVersion>,
}

/// The report lines, and whether everything is fine (the instance accepts connections).
pub fn render_status(facts: &StatusFacts) -> (Vec<String>, bool) {
    if !facts.installed {
        return (
            vec![format!(
                "PostgreSQL programs: not found in {}",
                facts.bin_dir.display()
            )],
            false,
        );
    }
    if !facts.accessible {
        return (
            vec![format!(
                "cannot read the instance folder as this user; run: sudo -u {SERVICE_USER} bhdatactl status"
            )],
            false,
        );
    }
    let health = match facts.health {
        Health::Ready => "accepting connections",
        Health::Starting => "starting up",
        Health::Down => "not answering",
    };
    let mut lines = vec![
        format!(
            "instance {SHARD_ID}: {}",
            if facts.running {
                "running"
            } else {
                "not running"
            }
        ),
        format!("health: {health}"),
    ];
    if let Some(v) = facts.started_with {
        let minor = v.minor.map_or(String::new(), |m| format!(".{m}"));
        lines.push(format!("started with PostgreSQL {}{minor}", v.major));
    }
    (lines, facts.health == Health::Ready)
}

pub fn gather_status(spec: &InstanceSpec) -> StatusFacts {
    let accessible = match fs::metadata(spec.run_dir()) {
        Ok(_) => true,
        Err(e) => e.kind() != io::ErrorKind::PermissionDenied,
    };
    StatusFacts {
        bin_dir: spec.pg_bin_dir.clone(),
        installed: install::is_installed(&spec.pg_bin_dir),
        accessible,
        running: accessible && supervise::is_running(spec),
        health: if accessible {
            supervise::health(spec)
        } else {
            Health::Down
        },
        started_with: supervise::recorded_version(spec),
    }
}

/// Gathers and renders in one step.
pub fn status(spec: &InstanceSpec) -> (Vec<String>, bool) {
    render_status(&gather_status(spec))
}

/// The whole of `bhdatactl` at M1: `status [--config <absolute path>]`. The other commands arrive
/// with M8. Exit codes: 0 healthy, 1 not healthy or unreadable config, 2 bad command line.
pub fn ctl_main(args: &[OsString]) -> ExitCode {
    let usage = "usage: bhdatactl status [--config <absolute path>]";
    let rest = match args.split_first() {
        Some((cmd, rest)) if cmd == "status" => rest,
        _ => {
            eprintln!("bhdatactl: {usage}");
            return ExitCode::from(2);
        }
    };
    let path = match config_path_from_args(rest) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bhdatactl: {e}");
            return ExitCode::from(2);
        }
    };
    // The instance's superuser is always the service user, whoever asks.
    let spec = load_config(&path).and_then(|c| spec_from(&c, SERVICE_USER.to_string()));
    match spec {
        Ok(spec) => {
            let (lines, ok) = status(&spec);
            for line in lines {
                println!("{line}");
            }
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
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
    use bhdata_types::Durability;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_config_path_defaults_and_takes_one_absolute_override() {
        assert_eq!(
            config_path_from_args(&[]).unwrap(),
            Path::new(DEFAULT_CONFIG_PATH)
        );
        assert_eq!(
            config_path_from_args(&args(&["--config", "/tmp/x.toml"])).unwrap(),
            Path::new("/tmp/x.toml")
        );
        for bad in [
            args(&["--config"]),
            args(&["--config", "relative.toml"]),
            args(&["/etc/x.toml"]),
            args(&["--config", "/a", "extra"]),
            args(&["--conf", "/a"]),
        ] {
            assert!(config_path_from_args(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn config_text_is_read_with_defaults_and_checked() {
        assert_eq!(parse_config("", "x").unwrap(), Config::default());
        let c = parse_config("durability = \"relaxed\"\n[engine]\nport = 6000\n", "x").unwrap();
        assert_eq!(c.durability, Durability::Relaxed);
        assert_eq!(c.engine.port, 6000);
        assert_eq!(c.engine.max_connections, 100);
    }

    #[test]
    fn config_errors_name_the_file_and_the_problem() {
        let typo = parse_config("[engine]\nprot = 1\n", "/etc/bhdata/config.toml").unwrap_err();
        assert!(
            typo.to_string().contains("/etc/bhdata/config.toml"),
            "{typo}"
        );
        assert!(typo.to_string().contains("prot"), "{typo}");
        let range = parse_config("[engine]\nmaintenance_hour_utc = 30\n", "f").unwrap_err();
        assert!(
            range.to_string().contains("maintenance_hour_utc"),
            "{range}"
        );
        assert!(parse_config("not toml at all [", "f").is_err());
    }

    #[test]
    fn a_missing_config_file_is_an_io_error_naming_the_path() {
        let err = load_config(Path::new("/nonexistent/bhdata/config.toml")).unwrap_err();
        assert!(matches!(err, EngineError::Io { .. }), "{err}");
        assert!(err.to_string().contains("/nonexistent/bhdata/config.toml"));
    }

    #[test]
    fn the_spec_is_shard0_under_the_configured_folder_with_the_pinned_programs() {
        let spec = spec_from(&Config::default(), "bhdata".into()).unwrap();
        assert_eq!(spec.id, "shard0");
        assert_eq!(spec.base_dir, Path::new("/var/lib/bhdata"));
        assert_eq!(spec.pg_bin_dir, default_pg_bin_dir());
        assert_eq!(spec.superuser, "bhdata");
        assert!(spec_from(&Config::default(), "Not A User".into()).is_err());
    }

    fn facts() -> StatusFacts {
        StatusFacts {
            bin_dir: PathBuf::from("/usr/lib/postgresql/17/bin"),
            installed: true,
            accessible: true,
            running: true,
            health: Health::Ready,
            started_with: Some(PgVersion {
                major: 17,
                minor: Some(2),
            }),
        }
    }

    #[test]
    fn a_healthy_instance_reports_ok() {
        let (lines, ok) = render_status(&facts());
        assert!(ok);
        assert_eq!(
            lines,
            [
                "instance shard0: running",
                "health: accepting connections",
                "started with PostgreSQL 17.2"
            ]
        );
    }

    #[test]
    fn problems_are_named_and_not_ok() {
        let down = StatusFacts {
            running: false,
            health: Health::Down,
            ..facts()
        };
        let (lines, ok) = render_status(&down);
        assert!(!ok);
        assert!(lines[0].contains("not running") && lines[1].contains("not answering"));

        let starting = StatusFacts {
            health: Health::Starting,
            ..facts()
        };
        assert!(!render_status(&starting).1);

        let missing = StatusFacts {
            installed: false,
            ..facts()
        };
        let (lines, ok) = render_status(&missing);
        assert!(!ok && lines[0].contains("not found in /usr/lib/postgresql/17/bin"));

        let blocked = StatusFacts {
            accessible: false,
            ..facts()
        };
        let (lines, ok) = render_status(&blocked);
        assert!(!ok && lines[0].contains("sudo -u bhdata bhdatactl status"));
    }

    #[test]
    fn a_status_line_for_the_watcher_shows_restarts_and_the_last_error() {
        use crate::supervise::State;
        let s = Status {
            state: State::Running,
            restarts: 2,
            last_error: None,
        };
        assert_eq!(describe(&s), "instance shard0: Running, 2 restart(s)");
        let s = Status {
            last_error: Some("boom".into()),
            ..s
        };
        assert!(describe(&s).ends_with("last restart failed: boom"));
    }

    #[test]
    fn status_of_an_instance_that_was_never_made_is_not_ok() {
        let spec = spec_from(&Config::default(), "bhdata".into()).unwrap();
        // No PostgreSQL programs where the pinned version would be, in a plain test machine.
        let (lines, ok) = status(&InstanceSpec {
            pg_bin_dir: PathBuf::from("/nonexistent/pg/bin"),
            ..spec
        });
        assert!(!ok);
        assert!(lines[0].contains("not found"));
    }

    #[test]
    fn the_server_and_the_ctl_refuse_bad_command_lines() {
        assert_eq!(
            server_main(&args(&["--bogus"])),
            ExitCode::from(2),
            "server"
        );
        assert_eq!(ctl_main(&args(&[])), ExitCode::from(2));
        assert_eq!(ctl_main(&args(&["restart"])), ExitCode::from(2));
        assert_eq!(
            ctl_main(&args(&["status", "--config", "rel.toml"])),
            ExitCode::from(2)
        );
        // A config that cannot be read is "not healthy", not a usage error.
        assert_eq!(
            ctl_main(&args(&["status", "--config", "/nonexistent/c.toml"])),
            ExitCode::from(1)
        );
    }

    // ---- the packaging files must agree with the constants above ----

    fn packaging(file: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging")
            .join(file);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn the_shipped_config_file_is_valid_and_equals_the_defaults() {
        let text = packaging("config.toml");
        assert_eq!(
            parse_config(&text, "packaging/config.toml").unwrap(),
            Config::default()
        );
    }

    #[test]
    fn sudoers_grants_update_and_restart_to_the_service_user_and_nothing_else() {
        let text = packaging("bhdata.sudoers");
        let rules: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(rules.len(), 1, "{rules:?}");
        let rule = rules[0];
        assert!(
            rule.starts_with(&format!("{SERVICE_USER} ALL=(root) NOPASSWD:")),
            "{rule}"
        );
        let commands: Vec<&str> = rule
            .split_once("NOPASSWD:")
            .map(|(_, c)| c.split(',').map(str::trim).collect())
            .unwrap_or_default();
        assert_eq!(
            commands,
            [
                format!("{HELPER_PATH} update"),
                format!("{HELPER_PATH} restart")
            ]
        );
        assert!(!rule.contains("install-pinned"));
        assert!(!rule.contains("ALL, ") && !rule.contains("(ALL)"));
    }

    #[test]
    fn the_unit_runs_the_server_as_the_service_user_and_keeps_sudo_working() {
        let text = packaging("bhdata-server.service");
        let line = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(key))
                .map(str::to_string)
        };
        assert_eq!(line("User=").as_deref(), Some(SERVICE_USER));
        assert_eq!(
            line("ExecStart=").as_deref(),
            Some(format!("{SERVER_PATH} --config {DEFAULT_CONFIG_PATH}").as_str())
        );
        assert_eq!(line("Restart=").as_deref(), Some("always"));
        assert!(text.contains("WantedBy=multi-user.target"));
        // These would stop the helper from working through sudo (see the comment in the file).
        let settings: Vec<&str> = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();
        assert!(settings
            .iter()
            .all(|l| !l.contains("NoNewPrivileges") && !l.contains("ProtectSystem")));
        assert!(text.contains(install::SERVICE_UNIT_NAME));
    }

    #[test]
    fn the_scripts_use_the_same_paths_names_and_service_as_the_code() {
        let text = packaging("install.sh");
        for needle in [
            format!("USER_NAME={SERVICE_USER}"),
            format!(
                "LIBEXEC={}",
                Path::new(HELPER_PATH).parent().unwrap().display()
            ),
            format!(
                "CONF_DIR={}",
                Path::new(DEFAULT_CONFIG_PATH).parent().unwrap().display()
            ),
            format!("DATA_DIR={}", Config::default().engine.base_dir.display()),
            format!("UNIT_NAME={}", install::SERVICE_UNIT_NAME),
        ] {
            assert!(text.contains(&needle), "install.sh lacks {needle}");
        }
        let un = packaging("uninstall.sh");
        for needle in [
            format!("USER_NAME={SERVICE_USER}"),
            format!("DATA_DIR={}", Config::default().engine.base_dir.display()),
            format!("UNIT_NAME={}", install::SERVICE_UNIT_NAME),
        ] {
            assert!(un.contains(&needle), "uninstall.sh lacks {needle}");
        }
    }
}
