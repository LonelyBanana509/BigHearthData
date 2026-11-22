//! One private PostgreSQL instance: its folders, its settings, and its creation.
//!
//! Layout under `base_dir`:
//!
//! ```text
//! shards/<id>/data/        the PostgreSQL data folder (made by initdb)
//! shards/<id>/run/         the Unix socket (the instance has no network port)
//! shards/<id>/startup.log  what the server prints before its own log starts
//! ```
//!
//! The instance is closed by construction: it listens on no network address, its socket folder and
//! data folder are mode 0700, and `pg_hba.conf` lets in only the service user, vouched for by the
//! operating system ("peer"). How the restricted roles of M2 log in is decided at M2.
//!
//! The data layer manages three files in the data folder, rewritten at every start so a changed
//! config takes effect: `bhdata.conf` (included from `postgresql.conf`), `pg_hba.conf`, and the one
//! `include` line in `postgresql.conf`.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bhdata_types::{Durability, EngineConfig};

use crate::error::EngineError;
use crate::sys;

/// The only `PATH` the PostgreSQL programs are started with.
const TOOL_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
/// The longest socket path accepted. The operating system's limit is 107 bytes; failing early here
/// gives a clear message instead of a server that cannot create its socket.
const MAX_SOCKET_PATH: usize = 100;
const INCLUDE_LINE: &str = "include 'bhdata.conf'";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSpec {
    /// Names the instance's folder, e.g. `shard0`. Lowercase letters, digits, `-` and `_`.
    pub id: String,
    pub base_dir: PathBuf,
    /// The folder with `initdb`, `postgres`, `pg_ctl` and `pg_isready`.
    pub pg_bin_dir: PathBuf,
    /// The instance's superuser: the name of the user the server runs as.
    pub superuser: String,
    pub port: u16,
    pub durability: Durability,
    pub max_connections: u32,
    pub locale: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    Created,
    AlreadyInitialised,
}

/// The version a PostgreSQL program reports, e.g. `postgres (PostgreSQL) 17.2` is 17.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PgVersion {
    pub major: u32,
    /// `None` for pre-releases such as `18beta1`.
    pub minor: Option<u32>,
}

/// Reads the version out of the text `postgres --version` prints. Only majors from 10 up (the
/// single-number scheme) are understood.
pub fn parse_pg_version(text: &str) -> Option<PgVersion> {
    let word = text
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let (major, rest) = split_digits(word);
    let major: u32 = major.parse().ok()?;
    if major < 10 {
        return None;
    }
    let minor = rest.strip_prefix('.').and_then(|r| {
        let (digits, tail) = split_digits(r);
        // "17.2" has a minor; "17.2beta" or "17.x" does not.
        if digits.is_empty() || !tail.is_empty() {
            None
        } else {
            digits.parse().ok()
        }
    });
    Some(PgVersion { major, minor })
}

fn split_digits(s: &str) -> (&str, &str) {
    let n = s.bytes().take_while(u8::is_ascii_digit).count();
    s.split_at(n)
}

fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

fn valid_user(s: &str) -> bool {
    let mut bytes = s.bytes();
    s.len() <= 32
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn valid_locale(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'@' | b'-'))
}

/// A path that is safe to write inside a `postgresql.conf` string and to hand to a program.
fn check_path(what: &'static str, p: &Path) -> Result<(), EngineError> {
    let Some(text) = p.to_str() else {
        return Err(EngineError::invalid(what, "must be valid UTF-8"));
    };
    if !p.is_absolute() {
        return Err(EngineError::invalid(what, "must be an absolute path"));
    }
    if text
        .chars()
        .any(|c| c.is_control() || c == '\'' || c == '\\')
    {
        return Err(EngineError::invalid(
            what,
            "must not contain quotes, backslashes or control characters",
        ));
    }
    Ok(())
}

impl InstanceSpec {
    /// Builds a spec from the engine config. `superuser` is normally [`current_user`].
    pub fn from_config(
        cfg: &EngineConfig,
        durability: Durability,
        id: &str,
        pg_bin_dir: PathBuf,
        superuser: String,
    ) -> Result<InstanceSpec, EngineError> {
        let spec = InstanceSpec {
            id: id.to_string(),
            base_dir: cfg.base_dir.clone(),
            pg_bin_dir,
            superuser,
            port: cfg.port,
            durability,
            max_connections: cfg.max_connections,
            locale: cfg.locale.clone(),
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), EngineError> {
        if !valid_id(&self.id) {
            return Err(EngineError::invalid(
                "instance id",
                "use 1 to 32 of: lowercase letters, digits, - and _",
            ));
        }
        if !valid_user(&self.superuser) {
            return Err(EngineError::invalid(
                "superuser",
                "must be a plain lowercase user name",
            ));
        }
        if !valid_locale(&self.locale) {
            return Err(EngineError::invalid(
                "locale",
                "contains unexpected characters",
            ));
        }
        if self.port < 1024 {
            return Err(EngineError::invalid("port", "must be 1024 or higher"));
        }
        if self.max_connections < 10 {
            return Err(EngineError::invalid(
                "max_connections",
                "must be at least 10",
            ));
        }
        check_path("base_dir", &self.base_dir)?;
        check_path("pg_bin_dir", &self.pg_bin_dir)?;
        let socket = self.socket_path();
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(EngineError::invalid(
                "base_dir",
                format!(
                    "the socket path {} would be longer than {MAX_SOCKET_PATH} bytes; use a shorter folder",
                    socket.display()
                ),
            ));
        }
        Ok(())
    }

    pub fn dir(&self) -> PathBuf {
        self.base_dir.join("shards").join(&self.id)
    }
    pub fn data_dir(&self) -> PathBuf {
        self.dir().join("data")
    }
    pub fn run_dir(&self) -> PathBuf {
        self.dir().join("run")
    }
    pub fn startup_log(&self) -> PathBuf {
        self.dir().join("startup.log")
    }
    pub fn socket_path(&self) -> PathBuf {
        self.run_dir().join(format!(".s.PGSQL.{}", self.port))
    }
    pub fn bin(&self, program: &str) -> PathBuf {
        self.pg_bin_dir.join(program)
    }
}

/// The name of the user this process runs as, for use as [`InstanceSpec::superuser`].
pub fn current_user() -> Result<String, EngineError> {
    sys::current_user_name()
}

/// A command for one of PostgreSQL's programs: closed environment, no stdin.
pub(crate) fn tool_command(program: &Path) -> Command {
    let mut c = Command::new(program);
    c.env_clear()
        .env("PATH", TOOL_PATH)
        .env("LC_MESSAGES", "C")
        .stdin(Stdio::null());
    c
}

/// Runs a PostgreSQL program to the end. Returns what it printed on stdout.
pub(crate) fn run_tool(
    name: &'static str,
    program: &Path,
    args: &[&OsStr],
) -> Result<String, EngineError> {
    let out = tool_command(program)
        .args(args)
        .output()
        .map_err(|e| EngineError::io(format!("could not start {}", program.display()), e))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let mut detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if detail.is_empty() {
        detail = format!("exited with {}", out.status);
    }
    detail.truncate(400);
    Err(EngineError::Tool {
        program: name,
        detail,
    })
}

/// The first line `postgres --version` prints, e.g. `postgres (PostgreSQL) 17.2 (Debian ...)`.
pub fn server_version_text(spec: &InstanceSpec) -> Result<String, EngineError> {
    let out = run_tool(
        "postgres",
        &spec.bin("postgres"),
        &[OsStr::new("--version")],
    )?;
    Ok(out.lines().next().unwrap_or("").trim().to_string())
}

pub fn server_version(spec: &InstanceSpec) -> Result<PgVersion, EngineError> {
    let text = server_version_text(spec)?;
    parse_pg_version(&text).ok_or_else(|| EngineError::Tool {
        program: "postgres",
        detail: format!("could not read a version from {text:?}"),
    })
}

/// The settings file the data layer manages.
pub fn render_settings(spec: &InstanceSpec) -> String {
    // Relaxed durability only stops waiting for the sync at commit. `fsync` stays on in every
    // mode, so a crash can lose the last few acknowledged commits but never corrupt the data.
    let synchronous_commit = match spec.durability {
        Durability::Full => "on",
        Durability::Relaxed => "off",
    };
    format!(
        "# Managed by bhdata-engine. Rewritten at every start; change the data layer's config instead.\n\
listen_addresses = ''\n\
port = {port}\n\
unix_socket_directories = '{run}'\n\
unix_socket_permissions = 0700\n\
max_connections = {max_connections}\n\
superuser_reserved_connections = {reserved}\n\
fsync = on\n\
full_page_writes = on\n\
synchronous_commit = {synchronous_commit}\n\
password_encryption = scram-sha-256\n\
logging_collector = on\n\
log_directory = 'log'\n\
log_filename = 'postgresql-%a.log'\n\
log_truncate_on_rotation = on\n\
log_rotation_age = 1d\n\
log_rotation_size = 0\n",
        port = spec.port,
        run = spec.run_dir().display(),
        max_connections = spec.max_connections,
        reserved = bhdata_pg::conn_budget::SUPERUSER_RESERVED_CONNECTIONS,
    )
}

/// `pg_hba.conf`: the local socket only, checked by the operating system, with the `bhdata` map
/// deciding which database roles the service user may log in as (see [`render_ident`]).
/// Everything else is rejected, because nothing else matches.
pub fn render_hba() -> String {
    format!(
        "# Managed by bhdata-engine. Local socket only; pg_ident.conf says which roles may log in.\n{}\n",
        bhdata_pg::roles::hba_line()
    )
}

/// `pg_ident.conf`: the service user as itself (for the bootstrap) and as each of the four login
/// roles of `bhdata-pg`. They all map from the one operating-system user: this limits what a bug
/// in a pool can do, and does not isolate processes.
pub fn render_ident(spec: &InstanceSpec) -> Result<String, EngineError> {
    bhdata_pg::roles::ident_map(&spec.superuser)
        .map_err(|e| EngineError::invalid("service user name", e.to_string()))
}

/// `text` with the `include` line for `bhdata.conf` at its end, unless it is already there. It is
/// last so that it overrides the defaults `initdb` wrote.
pub fn with_include(text: &str) -> String {
    if text.lines().any(|l| l.trim() == INCLUDE_LINE) {
        return text.to_string();
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(INCLUDE_LINE);
    out.push('\n');
    out
}

/// Writes `content` to `path` through a temporary file and a rename, mode 0600.
pub(crate) fn write_atomic(path: &Path, content: &str) -> Result<(), EngineError> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.bhdata-tmp"));
    let err = |what: &str, e: io::Error| EngineError::io(format!("{what} {}", tmp.display()), e);
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(err("could not clear", e)),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| err("could not create", e))?;
    file.write_all(content.as_bytes())
        .map_err(|e| err("could not write", e))?;
    file.sync_all().map_err(|e| err("could not sync", e))?;
    fs::rename(&tmp, path)
        .map_err(|e| EngineError::io(format!("could not replace {}", path.display()), e))
}

/// Like [`write_atomic`], but leaves the file alone when it already has this content.
fn write_if_changed(path: &Path, content: &str) -> Result<(), EngineError> {
    if fs::read_to_string(path).is_ok_and(|old| old == content) {
        return Ok(());
    }
    write_atomic(path, content)
}

fn read_text(path: &Path) -> Result<String, EngineError> {
    fs::read_to_string(path)
        .map_err(|e| EngineError::io(format!("could not read {}", path.display()), e))
}

fn make_private_dir(path: &Path) -> Result<(), EngineError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| EngineError::io(format!("could not create {}", path.display()), e))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|e| EngineError::io(format!("could not close {}", path.display()), e))
}

/// Whether the data folder has been made by `initdb`.
pub fn is_initialised(spec: &InstanceSpec) -> bool {
    spec.data_dir().join("PG_VERSION").is_file()
}

/// Writes the managed settings into an initialised data folder. Safe to call at every start.
pub fn apply_settings(spec: &InstanceSpec) -> Result<(), EngineError> {
    spec.validate()?;
    if !is_initialised(spec) {
        return Err(EngineError::NotInitialised);
    }
    let data = spec.data_dir();
    make_private_dir(&spec.run_dir())?;
    let conf = data.join("postgresql.conf");
    write_if_changed(&conf, &with_include(&read_text(&conf)?))?;
    write_if_changed(&data.join("bhdata.conf"), &render_settings(spec))?;
    write_if_changed(&data.join("pg_ident.conf"), &render_ident(spec)?)?;
    write_if_changed(&data.join("pg_hba.conf"), &render_hba())
}

/// Fails with [`EngineError::VersionMismatch`] when the data folder was made by another major
/// version than `installed`. Only a minor-release difference is allowed.
pub(crate) fn check_data_version(
    spec: &InstanceSpec,
    installed: PgVersion,
) -> Result<(), EngineError> {
    let data = spec.data_dir();
    let found = read_text(&data.join("PG_VERSION"))?.trim().to_string();
    if found.parse::<u32>().ok() == Some(installed.major) {
        Ok(())
    } else {
        Err(EngineError::VersionMismatch {
            dir: data.display().to_string(),
            found,
            installed: installed.major.to_string(),
        })
    }
}

/// Creates the instance if it does not exist, and applies the managed settings either way.
/// Refuses to run as root (PostgreSQL would refuse anyway), to touch a folder that is not a
/// cluster, and to reuse a data folder of another major version.
pub fn init(spec: &InstanceSpec) -> Result<InitOutcome, EngineError> {
    init_with(spec, sys::running_as_root()?)
}

pub(crate) fn init_with(spec: &InstanceSpec, as_root: bool) -> Result<InitOutcome, EngineError> {
    spec.validate()?;
    if as_root {
        return Err(EngineError::RunningAsRoot);
    }
    let installed = server_version(spec)?;
    make_private_dir(&spec.dir())?;
    let data = spec.data_dir();

    let outcome = if is_initialised(spec) {
        check_data_version(spec, installed)?;
        InitOutcome::AlreadyInitialised
    } else {
        let occupied = fs::read_dir(&data).is_ok_and(|mut entries| entries.next().is_some());
        if occupied {
            return Err(EngineError::NotACluster(data.display().to_string()));
        }
        let username = format!("--username={}", spec.superuser);
        let locale = format!("--locale={}", spec.locale);
        run_tool(
            "initdb",
            &spec.bin("initdb"),
            &[
                OsStr::new("-D"),
                data.as_os_str(),
                OsStr::new(&username),
                OsStr::new("--encoding=UTF8"),
                OsStr::new(&locale),
                OsStr::new("--auth-local=peer"),
                OsStr::new("--auth-host=reject"),
                OsStr::new("--data-checksums"),
            ],
        )?;
        InitOutcome::Created
    };
    apply_settings(spec)?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{script, TempDir};

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

    /// Stand-ins for `postgres` and `initdb` that record how they were called and make the files
    /// a real `initdb` would. `major` is the version the stub server reports.
    fn stub_bin(dir: &Path, major: u32) {
        let bin = dir.join("bin");
        fs::create_dir_all(&bin).unwrap();
        script(
            &bin.join("postgres"),
            &format!("echo 'postgres (PostgreSQL) {major}.2 (stub)'"),
        );
        let log = dir.join("initdb-args.txt");
        script(
            &bin.join("initdb"),
            &format!(
                "echo \"$@\" > {log}\nmkdir -p \"$2\"\necho {major} > \"$2/PG_VERSION\"\necho '#listen_addresses = x' > \"$2/postgresql.conf\"\necho '# initdb hba' > \"$2/pg_hba.conf\"",
                log = log.display()
            ),
        );
    }

    #[test]
    fn parses_server_versions() {
        let v = parse_pg_version;
        assert_eq!(
            v("postgres (PostgreSQL) 16.15 (Ubuntu 16.15-0ubuntu0.24.04.1)"),
            Some(PgVersion {
                major: 16,
                minor: Some(15)
            })
        );
        assert_eq!(
            v("postgres (PostgreSQL) 17.2"),
            Some(PgVersion {
                major: 17,
                minor: Some(2)
            })
        );
        assert_eq!(
            v("postgres (PostgreSQL) 18beta1"),
            Some(PgVersion {
                major: 18,
                minor: None
            })
        );
        assert_eq!(
            v("postgres (PostgreSQL) 9.6.24"),
            None,
            "pre-10 numbering is not supported"
        );
        assert_eq!(v("nothing here"), None);
        assert_eq!(v(""), None);
    }

    #[test]
    fn a_good_spec_is_valid_and_paths_hang_off_the_base() {
        let spec = spec_in(Path::new("/var/lib/bhdata"));
        spec.validate().unwrap();
        assert_eq!(
            spec.data_dir(),
            Path::new("/var/lib/bhdata/shards/shard0/data")
        );
        assert_eq!(
            spec.socket_path(),
            Path::new("/var/lib/bhdata/shards/shard0/run/.s.PGSQL.54320")
        );
    }

    #[test]
    fn bad_names_and_paths_are_refused() {
        let base = Path::new("/var/lib/bhdata");
        let mut cases: Vec<InstanceSpec> = Vec::new();
        for id in ["", "Shard", "a/b", "..", "a b", &"x".repeat(33)] {
            cases.push(InstanceSpec {
                id: id.into(),
                ..spec_in(base)
            });
        }
        for user in ["", "Root", "1abc", "a;b", "a b"] {
            cases.push(InstanceSpec {
                superuser: user.into(),
                ..spec_in(base)
            });
        }
        for locale in ["", "C UTF-8", "C;x", "a'b"] {
            cases.push(InstanceSpec {
                locale: locale.into(),
                ..spec_in(base)
            });
        }
        for path in ["relative", "/with'quote", "/with\\slash", "/with\nnewline"] {
            cases.push(InstanceSpec {
                base_dir: PathBuf::from(path),
                ..spec_in(base)
            });
            cases.push(InstanceSpec {
                pg_bin_dir: PathBuf::from(path),
                ..spec_in(base)
            });
        }
        cases.push(InstanceSpec {
            port: 80,
            ..spec_in(base)
        });
        cases.push(InstanceSpec {
            max_connections: 2,
            ..spec_in(base)
        });
        for spec in cases {
            assert!(
                matches!(spec.validate(), Err(EngineError::Invalid { .. })),
                "{spec:?}"
            );
        }
    }

    #[test]
    fn a_socket_path_that_is_too_long_is_refused_early() {
        let long = PathBuf::from(format!("/{}", "d".repeat(90)));
        let err = spec_in(&long).validate().unwrap_err();
        assert!(err.to_string().contains("socket path"), "{err}");
    }

    #[test]
    fn settings_close_the_instance_and_keep_fsync_on() {
        let text = render_settings(&spec_in(Path::new("/var/lib/bhdata")));
        assert!(text.contains("listen_addresses = ''\n"));
        assert!(text.contains("unix_socket_directories = '/var/lib/bhdata/shards/shard0/run'\n"));
        assert!(text.contains("unix_socket_permissions = 0700\n"));
        assert!(text.contains("fsync = on\n"));
        assert!(text.contains("synchronous_commit = on\n"));
        assert!(text.contains("port = 54320\n"));
        assert!(!text.contains("listen_addresses = '*'"));
    }

    #[test]
    fn relaxed_durability_skips_the_commit_wait_but_never_fsync() {
        let spec = InstanceSpec {
            durability: Durability::Relaxed,
            ..spec_in(Path::new("/var/lib/bhdata"))
        };
        let text = render_settings(&spec);
        assert!(text.contains("synchronous_commit = off\n"));
        assert!(text.contains("fsync = on\n"));
        assert!(!text.contains("fsync = off"));
    }

    #[test]
    fn hba_has_one_rule_and_it_is_peer_through_the_bhdata_map() {
        let text = render_hba();
        let rules: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].split_whitespace().collect::<Vec<_>>(),
            ["local", "all", "all", "peer", "map=bhdata"]
        );
        assert!(!text.contains("trust"));
        assert!(!text.contains("host"));
    }

    #[test]
    fn ident_maps_the_service_user_to_itself_and_the_four_login_roles_only() {
        let text = render_ident(&spec_in(Path::new("/var/lib/bhdata"))).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(lines.len(), 5);
        assert!(lines.iter().all(|l| l.starts_with("bhdata bhdata ")));
        for role in [
            "bhdata",
            "bhdata_recipe",
            "bhdata_sql",
            "bhdata_reader",
            "bhdata_admin",
        ] {
            assert!(
                lines.iter().any(|l| l.ends_with(&format!(" {role}"))),
                "{role}"
            );
        }
        assert!(!text.contains("bhdata_owner"));
    }

    #[test]
    fn a_service_user_name_that_needs_quoting_is_refused() {
        let mut spec = spec_in(Path::new("/var/lib/bhdata"));
        spec.superuser = "bad user".into();
        assert!(matches!(
            render_ident(&spec),
            Err(EngineError::Invalid {
                what: "service user name",
                ..
            })
        ));
    }

    #[test]
    fn the_reserved_superuser_connections_come_from_the_one_constant() {
        let text = render_settings(&spec_in(Path::new("/var/lib/bhdata")));
        assert!(text.contains(&format!(
            "superuser_reserved_connections = {}\n",
            bhdata_pg::conn_budget::SUPERUSER_RESERVED_CONNECTIONS
        )));
    }

    #[test]
    fn include_line_is_added_once_at_the_end() {
        let once = with_include("a = 1\n#x\n");
        assert_eq!(once, "a = 1\n#x\ninclude 'bhdata.conf'\n");
        assert_eq!(with_include(&once), once);
        assert_eq!(with_include("a = 1"), "a = 1\ninclude 'bhdata.conf'\n");
        assert_eq!(with_include(""), "include 'bhdata.conf'\n");
    }

    #[test]
    fn init_makes_a_private_instance_with_the_exact_initdb_call() {
        let dir = TempDir::new("init-new");
        stub_bin(dir.path(), 17);
        let spec = spec_in(dir.path());
        assert_eq!(init_with(&spec, false).unwrap(), InitOutcome::Created);

        let args = fs::read_to_string(dir.path().join("initdb-args.txt")).unwrap();
        assert_eq!(
            args.trim(),
            format!(
                "-D {} --username=bhdata --encoding=UTF8 --locale=C.UTF-8 --auth-local=peer --auth-host=reject --data-checksums",
                spec.data_dir().display()
            )
        );
        let mode = |p: PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(spec.dir()), 0o700);
        assert_eq!(mode(spec.run_dir()), 0o700);
        let data = spec.data_dir();
        assert!(fs::read_to_string(data.join("postgresql.conf"))
            .unwrap()
            .ends_with("include 'bhdata.conf'\n"));
        assert_eq!(
            fs::read_to_string(data.join("pg_hba.conf")).unwrap(),
            render_hba()
        );
        assert_eq!(
            fs::read_to_string(data.join("bhdata.conf")).unwrap(),
            render_settings(&spec)
        );
        assert_eq!(mode(data.join("bhdata.conf")), 0o600);
    }

    #[test]
    fn init_a_second_time_changes_nothing_and_reports_it() {
        let dir = TempDir::new("init-twice");
        stub_bin(dir.path(), 17);
        let spec = spec_in(dir.path());
        init_with(&spec, false).unwrap();
        let conf = spec.data_dir().join("bhdata.conf");
        let before = fs::metadata(&conf).unwrap().modified().unwrap();
        assert_eq!(
            init_with(&spec, false).unwrap(),
            InitOutcome::AlreadyInitialised
        );
        assert_eq!(fs::metadata(&conf).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn a_changed_setting_is_rewritten_on_the_next_apply() {
        let dir = TempDir::new("apply-change");
        stub_bin(dir.path(), 17);
        let spec = spec_in(dir.path());
        init_with(&spec, false).unwrap();
        let relaxed = InstanceSpec {
            durability: Durability::Relaxed,
            ..spec
        };
        apply_settings(&relaxed).unwrap();
        let text = fs::read_to_string(relaxed.data_dir().join("bhdata.conf")).unwrap();
        assert!(text.contains("synchronous_commit = off"));
    }

    #[test]
    fn init_refuses_root_before_running_anything() {
        let dir = TempDir::new("init-root");
        stub_bin(dir.path(), 17);
        let spec = spec_in(dir.path());
        assert!(matches!(
            init_with(&spec, true),
            Err(EngineError::RunningAsRoot)
        ));
        assert!(!dir.path().join("initdb-args.txt").exists());
        assert!(!spec.dir().exists());
    }

    #[test]
    fn init_refuses_a_data_folder_of_another_major_version() {
        let dir = TempDir::new("init-mismatch");
        stub_bin(dir.path(), 17);
        let spec = spec_in(dir.path());
        fs::create_dir_all(spec.data_dir()).unwrap();
        fs::write(spec.data_dir().join("PG_VERSION"), "16\n").unwrap();
        let err = init_with(&spec, false).unwrap_err();
        assert!(matches!(err, EngineError::VersionMismatch { .. }), "{err}");
        assert!(!dir.path().join("initdb-args.txt").exists());
    }

    #[test]
    fn init_never_touches_a_folder_that_is_not_a_cluster() {
        let dir = TempDir::new("init-foreign");
        stub_bin(dir.path(), 17);
        let spec = spec_in(dir.path());
        fs::create_dir_all(spec.data_dir()).unwrap();
        fs::write(spec.data_dir().join("precious.txt"), "keep").unwrap();
        let err = init_with(&spec, false).unwrap_err();
        assert!(matches!(err, EngineError::NotACluster(_)), "{err}");
        assert_eq!(
            fs::read_to_string(spec.data_dir().join("precious.txt")).unwrap(),
            "keep"
        );
        assert!(!dir.path().join("initdb-args.txt").exists());
    }

    #[test]
    fn an_empty_data_folder_is_fine() {
        let dir = TempDir::new("init-empty");
        stub_bin(dir.path(), 17);
        let spec = spec_in(dir.path());
        fs::create_dir_all(spec.data_dir()).unwrap();
        assert_eq!(init_with(&spec, false).unwrap(), InitOutcome::Created);
    }

    #[test]
    fn a_failing_initdb_is_reported_with_its_message() {
        let dir = TempDir::new("init-fail");
        stub_bin(dir.path(), 17);
        script(
            &dir.path().join("bin/initdb"),
            "echo 'locale not found' >&2; exit 1",
        );
        let err = init_with(&spec_in(dir.path()), false).unwrap_err();
        match err {
            EngineError::Tool { program, detail } => {
                assert_eq!(program, "initdb");
                assert!(detail.contains("locale not found"));
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn apply_needs_an_initialised_folder() {
        let dir = TempDir::new("apply-early");
        let spec = spec_in(dir.path());
        assert!(matches!(
            apply_settings(&spec),
            Err(EngineError::NotInitialised)
        ));
    }

    #[test]
    fn tools_start_with_a_closed_environment() {
        let dir = TempDir::new("tool-env");
        let probe = dir.path().join("probe");
        // cargo sets CARGO_MANIFEST_DIR for the test process; the tool must not inherit it.
        script(
            &probe,
            "echo \"PATH=$PATH\"; echo \"MANIFEST=${CARGO_MANIFEST_DIR-unset}\"",
        );
        assert!(std::env::var_os("CARGO_MANIFEST_DIR").is_some());
        let out = run_tool("probe", &probe, &[]).unwrap();
        assert!(out.contains("PATH=/usr/sbin:/usr/bin:/sbin:/bin"));
        assert!(out.contains("MANIFEST=unset"));
    }
}
