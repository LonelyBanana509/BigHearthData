//! Starting, stopping and watching one private instance.
//!
//! Start and stop go through `pg_ctl`, which knows how to find a running server from its pid file,
//! so a server left over from an earlier run of the data layer is found and reused, never started
//! twice. Health is `pg_isready` on the instance's own socket, because this crate opens no
//! database connections.
//!
//! The watcher ([`Supervisor`]) is one thread per instance. It restarts the server when
//!
//! * the process is gone, at once, or
//! * the process is there but has not answered for `misses_before_restart` checks in a row.
//!
//! A server that says it is starting up (crash recovery can take a while) is left alone and does
//! not count as a miss. Restarts back off: 1 s, 2 s, 4 s, ... up to a cap, and the count resets
//! as soon as the server answers again, so a crash loop does not spin.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::error::EngineError;
use crate::instance::{
    self, check_data_version, is_initialised, parse_pg_version, run_tool, tool_command,
    write_atomic, InstanceSpec, PgVersion,
};
use crate::sys;

/// How long `pg_ctl` waits for the server to start or stop, in seconds.
const PG_CTL_WAIT_SECS: &str = "60";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Accepting connections.
    Ready,
    /// Up but not accepting yet (starting, or in crash recovery).
    Starting,
    /// Not answering.
    Down,
}

/// Reads `pg_isready`'s exit code: 0 accepting, 1 rejecting (starting up), anything else is down.
pub fn health_from_exit_code(code: Option<i32>) -> Health {
    match code {
        Some(0) => Health::Ready,
        Some(1) => Health::Starting,
        _ => Health::Down,
    }
}

pub fn health(spec: &InstanceSpec) -> Health {
    let port = spec.port.to_string();
    let status = tool_command(&spec.bin("pg_isready"))
        .args(["-h"])
        .arg(spec.run_dir())
        .args(["-p", &port, "-t", "2"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match status {
        Ok(s) => health_from_exit_code(s.code()),
        Err(_) => Health::Down,
    }
}

/// Whether a server process for this data folder exists (`pg_ctl status` exits 0).
pub fn is_running(spec: &InstanceSpec) -> bool {
    tool_command(&spec.bin("pg_ctl"))
        .arg("status")
        .arg("-D")
        .arg(spec.data_dir())
        .output()
        .is_ok_and(|o| o.status.success())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    Started,
    AlreadyRunning,
}

fn started_version_path(spec: &InstanceSpec) -> PathBuf {
    spec.dir().join("started-version")
}

/// The server version this instance was last started with, if known. A minor-release update is due
/// when the installed programs are newer than this.
pub fn recorded_version(spec: &InstanceSpec) -> Option<PgVersion> {
    parse_pg_version(&fs::read_to_string(started_version_path(spec)).ok()?)
}

/// Starts the instance and waits until it accepts connections. Re-applies the managed settings
/// first, so a changed config takes effect. Does nothing if it is already running.
pub fn start(spec: &InstanceSpec) -> Result<StartOutcome, EngineError> {
    start_with(spec, sys::running_as_root()?)
}

pub(crate) fn start_with(spec: &InstanceSpec, as_root: bool) -> Result<StartOutcome, EngineError> {
    spec.validate()?;
    if as_root {
        return Err(EngineError::RunningAsRoot);
    }
    if !is_initialised(spec) {
        return Err(EngineError::NotInitialised);
    }
    let version_text = instance::server_version_text(spec)?;
    let installed = parse_pg_version(&version_text).ok_or_else(|| EngineError::Tool {
        program: "postgres",
        detail: format!("could not read a version from {version_text:?}"),
    })?;
    check_data_version(spec, installed)?;
    instance::apply_settings(spec)?;
    if is_running(spec) {
        // A server that was already up still runs with the old `pg_hba.conf` and `pg_ident.conf`
        // (an instance made by an older release, say); a reload makes it read the files above.
        run_tool(
            "pg_ctl",
            &spec.bin("pg_ctl"),
            &[
                OsStr::new("reload"),
                OsStr::new("-D"),
                spec.data_dir().as_os_str(),
            ],
        )?;
        return Ok(StartOutcome::AlreadyRunning);
    }
    // Create the startup log private before the server writes to it.
    let log = spec.startup_log();
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .map_err(|e| EngineError::io(format!("could not open {}", log.display()), e))?;
    run_tool(
        "pg_ctl",
        &spec.bin("pg_ctl"),
        &[
            OsStr::new("start"),
            OsStr::new("-D"),
            spec.data_dir().as_os_str(),
            OsStr::new("-w"),
            OsStr::new("-t"),
            OsStr::new(PG_CTL_WAIT_SECS),
            OsStr::new("-l"),
            log.as_os_str(),
        ],
    )?;
    write_atomic(&started_version_path(spec), &format!("{version_text}\n"))?;
    Ok(StartOutcome::Started)
}

/// Stops the instance with a fast shutdown (open sessions are ended, committed data is kept).
/// Does nothing if it is not running.
pub fn stop(spec: &InstanceSpec) -> Result<(), EngineError> {
    if !is_running(spec) {
        return Ok(());
    }
    run_tool(
        "pg_ctl",
        &spec.bin("pg_ctl"),
        &[
            OsStr::new("stop"),
            OsStr::new("-D"),
            spec.data_dir().as_os_str(),
            OsStr::new("-m"),
            OsStr::new("fast"),
            OsStr::new("-w"),
            OsStr::new("-t"),
            OsStr::new(PG_CTL_WAIT_SECS),
        ],
    )
    .map(|_| ())
}

pub fn restart(spec: &InstanceSpec) -> Result<(), EngineError> {
    stop(spec)?;
    start(spec).map(|_| ())
}

/// How long to wait before restart number `attempt` (0 for the first): `base`, then double each
/// time, never more than `max`.
pub fn backoff_delay(base: Duration, max: Duration, attempt: u32) -> Duration {
    base.saturating_mul(1u32 << attempt.min(16)).min(max)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuperviseConfig {
    /// Time between health checks.
    pub interval: Duration,
    /// Failed checks in a row, with the process still there, before it is restarted.
    pub misses_before_restart: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for SuperviseConfig {
    fn default() -> Self {
        SuperviseConfig {
            interval: Duration::from_secs(2),
            misses_before_restart: 3,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
        }
    }
}

/// What the watcher needs from an instance. [`PgOps`] is the real one; tests use a fake.
pub trait Ops: Send + Sync {
    fn health(&self) -> Health;
    fn is_running(&self) -> bool;
    fn restart(&self) -> Result<(), EngineError>;
}

/// The real instance, through `pg_ctl` and `pg_isready`.
pub struct PgOps(pub InstanceSpec);

impl Ops for PgOps {
    fn health(&self) -> Health {
        health(&self.0)
    }
    fn is_running(&self) -> bool {
        is_running(&self.0)
    }
    fn restart(&self) -> Result<(), EngineError> {
        restart(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Before the first check.
    Starting,
    Running,
    /// Not answering; `misses` checks in a row so far.
    Unhealthy {
        misses: u32,
    },
    /// Waiting to restart, or restarting; `attempt` counts since the last time it answered.
    Restarting {
        attempt: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    /// Restarts tried since the watcher began, successful or not.
    pub restarts: u32,
    /// The error of the last failed restart, cleared when the server answers again.
    pub last_error: Option<String>,
}

struct Shared {
    status: Mutex<Status>,
    stop: Mutex<bool>,
    wake: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic in the watcher must not make the status unreadable.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    /// Sleeps up to `d`. Returns true if the watcher was asked to stop.
    fn sleep(&self, d: Duration) -> bool {
        let guard = lock(&self.stop);
        let (guard, _) = self
            .wake
            .wait_timeout_while(guard, d, |stopped| !*stopped)
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }

    fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut lock(&self.status));
    }
}

/// The watcher thread for one instance. Dropping it stops the thread (not the server).
pub struct Supervisor {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Supervisor {
    pub fn spawn(ops: Arc<dyn Ops>, cfg: SuperviseConfig) -> Supervisor {
        let shared = Arc::new(Shared {
            status: Mutex::new(Status {
                state: State::Starting,
                restarts: 0,
                last_error: None,
            }),
            stop: Mutex::new(false),
            wake: Condvar::new(),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            thread::spawn(move || watch(&*ops, cfg, &shared))
        };
        Supervisor {
            shared,
            thread: Some(thread),
        }
    }

    pub fn status(&self) -> Status {
        lock(&self.shared.status).clone()
    }

    /// Stops the watcher and waits for it. The server keeps running.
    pub fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        *lock(&self.shared.stop) = true;
        self.shared.wake.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shut_down();
    }
}

fn watch(ops: &dyn Ops, cfg: SuperviseConfig, shared: &Shared) {
    let mut misses = 0u32;
    let mut attempts = 0u32;
    loop {
        if shared.sleep(cfg.interval) {
            return;
        }
        match ops.health() {
            Health::Ready => {
                misses = 0;
                attempts = 0;
                shared.update(|s| {
                    s.state = State::Running;
                    s.last_error = None;
                });
                continue;
            }
            Health::Starting if ops.is_running() => {
                shared.update(|s| s.state = State::Starting);
                continue;
            }
            _ => {}
        }
        if ops.is_running() {
            misses += 1;
            shared.update(|s| s.state = State::Unhealthy { misses });
            if misses < cfg.misses_before_restart {
                continue;
            }
        }
        shared.update(|s| {
            s.state = State::Restarting {
                attempt: attempts + 1,
            }
        });
        if shared.sleep(backoff_delay(cfg.backoff_base, cfg.backoff_max, attempts)) {
            return;
        }
        let result = ops.restart();
        attempts += 1;
        misses = 0;
        shared.update(|s| {
            s.restarts += 1;
            s.last_error = result.err().map(|e| e.to_string());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{script, TempDir};
    use bhdata_types::Durability;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Instant;

    // ---- the watcher, against a fake instance ----

    struct Fake {
        health: Mutex<Health>,
        running: AtomicBool,
        restarts: AtomicU32,
        fail_restarts: AtomicU32,
    }

    impl Fake {
        fn new(health: Health, running: bool) -> Arc<Fake> {
            Arc::new(Fake {
                health: Mutex::new(health),
                running: AtomicBool::new(running),
                restarts: AtomicU32::new(0),
                fail_restarts: AtomicU32::new(0),
            })
        }
        fn restarts(&self) -> u32 {
            self.restarts.load(Ordering::SeqCst)
        }
    }

    impl Ops for Fake {
        fn health(&self) -> Health {
            *lock(&self.health)
        }
        fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }
        fn restart(&self) -> Result<(), EngineError> {
            self.restarts.fetch_add(1, Ordering::SeqCst);
            if self.fail_restarts.load(Ordering::SeqCst) > 0 {
                self.fail_restarts.fetch_sub(1, Ordering::SeqCst);
                return Err(EngineError::Tool {
                    program: "pg_ctl",
                    detail: "boom".into(),
                });
            }
            *lock(&self.health) = Health::Ready;
            self.running.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn fast() -> SuperviseConfig {
        SuperviseConfig {
            interval: Duration::from_millis(5),
            misses_before_restart: 3,
            backoff_base: Duration::from_millis(5),
            backoff_max: Duration::from_millis(20),
        }
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn backoff_doubles_and_stops_at_the_cap() {
        let (base, max) = (Duration::from_secs(1), Duration::from_secs(60));
        let secs = |n| backoff_delay(base, max, n).as_secs();
        assert_eq!([secs(0), secs(1), secs(2), secs(3)], [1, 2, 4, 8]);
        assert_eq!(secs(6), 60);
        assert_eq!(secs(40), 60, "a huge attempt count must not overflow");
    }

    #[test]
    fn isready_exit_codes_map_to_health() {
        assert_eq!(health_from_exit_code(Some(0)), Health::Ready);
        assert_eq!(health_from_exit_code(Some(1)), Health::Starting);
        assert_eq!(health_from_exit_code(Some(2)), Health::Down);
        assert_eq!(health_from_exit_code(Some(3)), Health::Down);
        assert_eq!(health_from_exit_code(None), Health::Down);
    }

    #[test]
    fn a_healthy_server_is_left_alone() {
        let fake = Fake::new(Health::Ready, true);
        let sup = Supervisor::spawn(fake.clone(), fast());
        wait_until("running", || sup.status().state == State::Running);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(fake.restarts(), 0);
        sup.stop();
    }

    #[test]
    fn a_dead_server_is_restarted_without_waiting_for_misses() {
        let fake = Fake::new(Health::Down, false);
        let cfg = SuperviseConfig {
            misses_before_restart: 1_000_000,
            ..fast()
        };
        let sup = Supervisor::spawn(fake.clone(), cfg);
        wait_until("restarted and answering", || {
            sup.status().state == State::Running
        });
        assert_eq!(fake.restarts(), 1);
        assert_eq!(sup.status().restarts, 1);
    }

    #[test]
    fn a_hung_server_is_restarted_only_after_enough_misses() {
        let fake = Fake::new(Health::Down, true);
        let cfg = SuperviseConfig {
            interval: Duration::from_millis(20),
            misses_before_restart: 3,
            ..fast()
        };
        let sup = Supervisor::spawn(fake.clone(), cfg);
        wait_until(
            "two misses seen",
            || matches!(sup.status().state, State::Unhealthy { misses } if misses >= 2),
        );
        assert_eq!(fake.restarts(), 0, "two misses is not enough");
        wait_until("restarted", || fake.restarts() == 1);
    }

    #[test]
    fn a_server_that_is_starting_up_is_never_restarted() {
        let fake = Fake::new(Health::Starting, true);
        let sup = Supervisor::spawn(fake.clone(), fast());
        wait_until("starting seen", || sup.status().state == State::Starting);
        thread::sleep(Duration::from_millis(80));
        assert_eq!(fake.restarts(), 0);
        assert_eq!(sup.status().state, State::Starting);
    }

    #[test]
    fn failed_restarts_are_recorded_retried_and_cleared_on_recovery() {
        let fake = Fake::new(Health::Down, false);
        fake.fail_restarts.store(2, Ordering::SeqCst);
        let sup = Supervisor::spawn(fake.clone(), fast());
        wait_until("two failures seen", || fake.restarts() >= 2);
        wait_until("recovered", || {
            let s = sup.status();
            s.state == State::Running && s.last_error.is_none()
        });
        assert_eq!(fake.restarts(), 3);
        assert_eq!(sup.status().restarts, 3);
    }

    #[test]
    fn stopping_is_prompt_even_with_a_long_interval() {
        let fake = Fake::new(Health::Ready, true);
        let cfg = SuperviseConfig {
            interval: Duration::from_secs(3600),
            ..fast()
        };
        let sup = Supervisor::spawn(fake, cfg);
        let t = Instant::now();
        sup.stop();
        assert!(t.elapsed() < Duration::from_secs(2));
    }

    // ---- start, stop and health, against stub programs ----

    fn spec_in(dir: &Path) -> InstanceSpec {
        InstanceSpec {
            id: "shard0".into(),
            base_dir: dir.to_path_buf(),
            pg_bin_dir: dir.join("bin"),
            superuser: "bhdata".into(),
            port: 54_320,
            durability: Durability::Full,
            max_connections: 100,
            locale: "C.UTF-8".into(),
        }
    }

    /// `postgres`, `initdb`, `pg_ctl` and `pg_isready` stand-ins. "Running" is a file; `pg_ctl`
    /// appends every call to `calls.txt`.
    fn stub_bin(dir: &Path, major: u32) {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let flag = dir.join("running");
        let calls = dir.join("calls.txt");
        script(
            &bin.join("postgres"),
            &format!("echo 'postgres (PostgreSQL) {major}.2 (stub)'"),
        );
        script(
            &bin.join("initdb"),
            &format!(
                "mkdir -p \"$2\"\necho {major} > \"$2/PG_VERSION\"\necho '#x' > \"$2/postgresql.conf\"\necho '#h' > \"$2/pg_hba.conf\""
            ),
        );
        script(
            &bin.join("pg_ctl"),
            &format!(
                "echo \"$@\" >> {calls}\ncase \"$1\" in\n status) [ -f {flag} ] && exit 0; exit 3;;\n start) touch {flag};;\n stop) rm -f {flag};;\nesac",
                calls = calls.display(),
                flag = flag.display()
            ),
        );
        script(
            &bin.join("pg_isready"),
            &format!("[ -f {} ] && exit 0; exit 2", flag.display()),
        );
    }

    fn calls(dir: &Path) -> Vec<String> {
        fs::read_to_string(dir.join("calls.txt"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn made_instance(label: &str) -> (TempDir, InstanceSpec) {
        let dir = TempDir::new(label);
        stub_bin(dir.path(), 17);
        let spec = spec_in(dir.path());
        instance::init_with(&spec, false).unwrap();
        (dir, spec)
    }

    #[test]
    fn start_runs_pg_ctl_with_the_exact_arguments_and_records_the_version() {
        let (dir, spec) = made_instance("start-new");
        assert_eq!(start_with(&spec, false).unwrap(), StartOutcome::Started);
        assert_eq!(
            calls(dir.path()),
            vec![
                format!("status -D {}", spec.data_dir().display()),
                format!(
                    "start -D {} -w -t 60 -l {}",
                    spec.data_dir().display(),
                    spec.startup_log().display()
                )
            ]
        );
        let mode = fs::metadata(spec.startup_log())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            recorded_version(&spec),
            Some(PgVersion {
                major: 17,
                minor: Some(2)
            })
        );
        assert_eq!(health(&spec), Health::Ready);
        assert!(is_running(&spec));
    }

    #[test]
    fn start_is_a_no_op_when_the_server_is_already_up() {
        let (dir, spec) = made_instance("start-twice");
        start_with(&spec, false).unwrap();
        assert_eq!(
            start_with(&spec, false).unwrap(),
            StartOutcome::AlreadyRunning
        );
        let starts = calls(dir.path())
            .iter()
            .filter(|c| c.starts_with("start"))
            .count();
        assert_eq!(starts, 1);
        // The files were written again, so the running server is told to read them.
        assert_eq!(
            calls(dir.path()).last().unwrap(),
            &format!("reload -D {}", spec.data_dir().display())
        );
    }

    #[test]
    fn start_reapplies_a_changed_setting() {
        let (_dir, spec) = made_instance("start-apply");
        let relaxed = InstanceSpec {
            durability: Durability::Relaxed,
            ..spec
        };
        start_with(&relaxed, false).unwrap();
        let text = fs::read_to_string(relaxed.data_dir().join("bhdata.conf")).unwrap();
        assert!(text.contains("synchronous_commit = off"));
    }

    #[test]
    fn start_refuses_root_before_running_anything() {
        let (dir, spec) = made_instance("start-root");
        assert!(matches!(
            start_with(&spec, true),
            Err(EngineError::RunningAsRoot)
        ));
        assert!(calls(dir.path()).is_empty());
    }

    #[test]
    fn start_needs_an_initialised_instance() {
        let dir = TempDir::new("start-early");
        stub_bin(dir.path(), 17);
        assert!(matches!(
            start_with(&spec_in(dir.path()), false),
            Err(EngineError::NotInitialised)
        ));
    }

    #[test]
    fn start_refuses_a_data_folder_of_another_major_version() {
        let (dir, spec) = made_instance("start-mismatch");
        fs::write(spec.data_dir().join("PG_VERSION"), "16\n").unwrap();
        let err = start_with(&spec, false).unwrap_err();
        assert!(matches!(err, EngineError::VersionMismatch { .. }), "{err}");
        assert!(calls(dir.path()).is_empty());
    }

    #[test]
    fn a_failing_pg_ctl_start_is_reported_and_records_no_version() {
        let (dir, spec) = made_instance("start-fail");
        script(
            &dir.path().join("bin/pg_ctl"),
            "case \"$1\" in status) exit 3;; esac\necho 'could not bind' >&2; exit 1",
        );
        match start_with(&spec, false).unwrap_err() {
            EngineError::Tool { program, detail } => {
                assert_eq!(program, "pg_ctl");
                assert!(detail.contains("could not bind"));
            }
            other => panic!("{other}"),
        }
        assert_eq!(recorded_version(&spec), None);
    }

    #[test]
    fn stop_does_nothing_when_not_running_and_stops_fast_when_running() {
        let (dir, spec) = made_instance("stop");
        stop(&spec).unwrap();
        assert!(calls(dir.path()).iter().all(|c| !c.starts_with("stop")));
        start_with(&spec, false).unwrap();
        stop(&spec).unwrap();
        assert!(calls(dir.path()).contains(&format!(
            "stop -D {} -m fast -w -t 60",
            spec.data_dir().display()
        )));
        assert!(!is_running(&spec));
        assert_eq!(health(&spec), Health::Down);
    }

    #[test]
    fn missing_programs_read_as_down_and_not_running() {
        let dir = TempDir::new("no-bin");
        let spec = spec_in(dir.path());
        assert_eq!(health(&spec), Health::Down);
        assert!(!is_running(&spec));
    }
}
