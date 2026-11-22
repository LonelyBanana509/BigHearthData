//! The three actions the helper can run, one function each. There are no arguments beyond the
//! action name: everything else (package names, paths, the service unit) is fixed in this file.
//!
//! * `install-pinned`: add the PostgreSQL apt repository, make sure no default cluster is created,
//!   install the pinned PostgreSQL major version. Run by the installer as root.
//! * `update`: install the newest minor release of the pinned major version. It does not restart
//!   anything: the engine manager restarts its own instances in the maintenance window.
//! * `restart`: restart the data layer's systemd service.
//!
//! `install-pinned` and `update` refuse to run when a Debian-managed cluster of the pinned major
//! already exists on the machine, because a package install could restart it. The data layer's
//! own instances are made with `initdb` and are not Debian-managed clusters, so they never
//! trigger this.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::HelperError;
use crate::exec::{Cmd, Runner};

/// The PostgreSQL major version this release of the data layer is pinned to (design section 3,
/// "PostgreSQL upgrades"). Changing it is a deliberate release decision.
pub const PG_MAJOR: &str = "17";
/// The systemd unit the `restart` action restarts. The unit file itself is written by `packaging/`.
pub const SERVICE_UNIT: &str = "bhdata-server.service";

const APT_GET: &str = "/usr/bin/apt-get";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
/// Ships in the `postgresql-common` package: adds the official PostgreSQL apt repository.
const PGDG_SCRIPT: &str = "/usr/share/postgresql-common/pgdg/apt.postgresql.org.sh";

/// The line that stops the Debian package from creating a default `main` cluster.
const NO_MAIN_CLUSTER: &str = "create_main_cluster = false";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    InstallPinned,
    Update,
    Restart,
}

impl Action {
    pub const ALL: [Action; 3] = [Action::InstallPinned, Action::Update, Action::Restart];

    pub fn name(self) -> &'static str {
        match self {
            Action::InstallPinned => "install-pinned",
            Action::Update => "update",
            Action::Restart => "restart",
        }
    }

    /// Exactly one argument, equal to one action name. No prefixes, no trimming, no options.
    pub fn parse(args: &[OsString]) -> Result<Action, HelperError> {
        let [arg] = args else {
            return Err(HelperError::usage("expected exactly one argument"));
        };
        let text = arg.to_str().unwrap_or("");
        match Action::ALL.iter().copied().find(|a| a.name() == text) {
            Some(action) => Ok(action),
            None => {
                let shown: String = arg.to_string_lossy().chars().take(40).collect();
                Err(HelperError::usage(&format!("unknown action {shown:?}")))
            }
        }
    }
}

/// The few file locations the actions touch. Tests point them into a temp directory.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub os_release: PathBuf,
    /// Debian-managed clusters live in `<etc_postgresql>/<major>/<name>`.
    pub etc_postgresql: PathBuf,
    pub createcluster_conf: PathBuf,
    pub pg_bin_dir: PathBuf,
}

impl Ctx {
    pub fn system() -> Ctx {
        Ctx {
            os_release: PathBuf::from("/etc/os-release"),
            etc_postgresql: PathBuf::from("/etc/postgresql"),
            createcluster_conf: PathBuf::from("/etc/postgresql-common/createcluster.conf"),
            pg_bin_dir: PathBuf::from(format!("/usr/lib/postgresql/{PG_MAJOR}/bin")),
        }
    }
}

pub fn run(action: Action, ctx: &Ctx, runner: &mut dyn Runner) -> Result<(), HelperError> {
    match action {
        Action::InstallPinned => install_pinned(ctx, runner),
        Action::Update => update(ctx, runner),
        Action::Restart => restart(runner),
    }
}

fn server_package() -> String {
    format!("postgresql-{PG_MAJOR}")
}

fn client_package() -> String {
    format!("postgresql-client-{PG_MAJOR}")
}

/// `apt-get` with the options every call needs. The two `Dpkg::Options` keep the existing version
/// of a config file instead of prompting, which matters because this helper edits
/// `createcluster.conf` (a config file of `postgresql-common`) and must not lose that edit.
fn apt(args: &[&str]) -> Cmd {
    let mut all = vec![
        "-y".to_string(),
        "-o".to_string(),
        "Dpkg::Options::=--force-confold".to_string(),
        "-o".to_string(),
        "Dpkg::Options::=--force-confdef".to_string(),
    ];
    all.extend(args.iter().map(|a| a.to_string()));
    Cmd {
        program: APT_GET,
        args: all,
    }
}

pub fn install_pinned(ctx: &Ctx, run: &mut dyn Runner) -> Result<(), HelperError> {
    refuse_if_clusters_exist(ctx)?;
    run.run(&apt(&["update"]))?;
    run.run(&apt(&[
        "install",
        "--no-install-recommends",
        "postgresql-common",
    ]))?;
    // Must happen after postgresql-common is installed (the file belongs to it) and before the
    // server package is installed (its install is what would create the default cluster).
    set_create_main_cluster_false(&ctx.createcluster_conf)?;
    run.run(&Cmd::new(PGDG_SCRIPT, ["-y"]))?;
    run.run(&apt(&["update"]))?;
    run.run(&apt(&[
        "install",
        "--no-install-recommends",
        &server_package(),
    ]))?;
    verify_install(ctx)
}

pub fn update(ctx: &Ctx, run: &mut dyn Runner) -> Result<(), HelperError> {
    refuse_if_clusters_exist(ctx)?;
    run.run(&apt(&["update"]))?;
    run.run(&apt(&[
        "install",
        "--only-upgrade",
        "--no-install-recommends",
        &server_package(),
        &client_package(),
    ]))?;
    verify_install(ctx)
}

pub fn restart(run: &mut dyn Runner) -> Result<(), HelperError> {
    run.run(&Cmd::new(SYSTEMCTL, ["restart", SERVICE_UNIT]))
}

/// Names of Debian-managed clusters of the pinned major (`/etc/postgresql/<major>/<name>`).
fn existing_clusters(ctx: &Ctx) -> Result<Vec<String>, HelperError> {
    let dir = ctx.etc_postgresql.join(PG_MAJOR);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(HelperError::io(
                format!("could not read {}", dir.display()),
                e,
            ))
        }
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| HelperError::io(format!("could not read {}", dir.display()), e))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

fn refuse_if_clusters_exist(ctx: &Ctx) -> Result<(), HelperError> {
    let found = existing_clusters(ctx)?;
    if found.is_empty() {
        Ok(())
    } else {
        Err(HelperError::Refused(format!(
            "PostgreSQL {PG_MAJOR} already has cluster(s) on this machine ({}); refusing to run a package install that could restart them",
            found.join(", ")
        )))
    }
}

/// After an install or update: the programs the engine manager needs are there, and the package
/// did not create a default cluster. Nothing is ever deleted here.
fn verify_install(ctx: &Ctx) -> Result<(), HelperError> {
    for program in ["postgres", "initdb", "pg_ctl"] {
        let path = ctx.pg_bin_dir.join(program);
        if !path.is_file() {
            return Err(HelperError::PostCheck(format!(
                "{} is missing after the package step; if PostgreSQL {PG_MAJOR} was never installed, run install-pinned first",
                path.display()
            )));
        }
    }
    let found = existing_clusters(ctx)?;
    if !found.is_empty() {
        return Err(HelperError::PostCheck(format!(
            "a default cluster appeared ({}) although {NO_MAIN_CLUSTER} was set; it was left in place",
            found.join(", ")
        )));
    }
    Ok(())
}

/// Returns `input` with every active `create_main_cluster` line replaced by
/// [`NO_MAIN_CLUSTER`], or the line appended if there was none. Commented lines and all other
/// lines are kept exactly as they were.
pub fn with_no_main_cluster(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + NO_MAIN_CLUSTER.len() + 1);
    let mut found = false;
    for line in input.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        let active = !body.trim_start().starts_with('#');
        let key = body.split_once('=').map(|(k, _)| k.trim());
        if active && key == Some("create_main_cluster") {
            found = true;
            out.push_str(NO_MAIN_CLUSTER);
            out.push_str(ending);
        } else {
            out.push_str(line);
        }
    }
    if !found {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(NO_MAIN_CLUSTER);
        out.push('\n');
    }
    out
}

/// Applies [`with_no_main_cluster`] to the file. Does nothing if the file already says so.
/// Written to a temporary file next to the original, then renamed over it, keeping its mode.
fn set_create_main_cluster_false(path: &Path) -> Result<(), HelperError> {
    let old = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(HelperError::io(
                format!("could not read {}", path.display()),
                e,
            ))
        }
    };
    let new = with_no_main_cluster(&old);
    if new == old {
        return Ok(());
    }
    let mode = fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(0o644);
    let tmp = path.with_file_name("createcluster.conf.bhdata-tmp");
    let write = |what: &str, r: io::Result<()>| {
        r.map_err(|e| HelperError::io(format!("{what} {}", tmp.display()), e))
    };
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return write("could not clear", Err(e)),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| HelperError::io(format!("could not create {}", tmp.display()), e))?;
    write(
        "could not write",
        io::Write::write_all(&mut file, new.as_bytes()),
    )?;
    write("could not sync", file.sync_all())?;
    write(
        "could not set the mode of",
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)),
    )?;
    fs::rename(&tmp, path)
        .map_err(|e| HelperError::io(format!("could not replace {}", path.display()), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    type Hook = Box<dyn FnMut(&Cmd) -> Result<(), HelperError>>;

    /// Records every command and runs nothing.
    struct Fake {
        cmds: Vec<Cmd>,
        hook: Hook,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                cmds: Vec::new(),
                hook: Box::new(|_| Ok(())),
            }
        }
        fn with_hook(hook: Hook) -> Fake {
            Fake {
                cmds: Vec::new(),
                hook,
            }
        }
    }

    impl Runner for Fake {
        fn run(&mut self, cmd: &Cmd) -> Result<(), HelperError> {
            self.cmds.push(cmd.clone());
            (self.hook)(cmd)
        }
    }

    fn ctx_in(dir: &Path) -> Ctx {
        Ctx {
            os_release: dir.join("os-release"),
            etc_postgresql: dir.join("etc-postgresql"),
            createcluster_conf: dir.join("createcluster.conf"),
            pg_bin_dir: dir.join("pgbin"),
        }
    }

    fn put_binaries(ctx: &Ctx) {
        fs::create_dir_all(&ctx.pg_bin_dir).unwrap();
        for p in ["postgres", "initdb", "pg_ctl"] {
            fs::write(ctx.pg_bin_dir.join(p), b"").unwrap();
        }
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    // ---- argument parsing ----

    #[test]
    fn parses_each_action_name() {
        assert_eq!(
            Action::parse(&os(&["install-pinned"])).unwrap(),
            Action::InstallPinned
        );
        assert_eq!(Action::parse(&os(&["update"])).unwrap(), Action::Update);
        assert_eq!(Action::parse(&os(&["restart"])).unwrap(), Action::Restart);
    }

    #[test]
    fn rejects_everything_that_is_not_exactly_one_action_name() {
        let bad: [&[&str]; 14] = [
            &[],
            &["install-pinned", "extra"],
            &["restart", "--"],
            &["update", "update"],
            &[""],
            &["INSTALL-PINNED"],
            &["Restart"],
            &[" restart"],
            &["restart "],
            &["restart\n"],
            &["install"],
            &["--help"],
            &["--version"],
            &["restart;id"],
        ];
        for args in bad {
            let err = Action::parse(&os(args)).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{args:?}");
        }
    }

    #[test]
    fn rejects_a_non_utf8_argument() {
        use std::os::unix::ffi::OsStringExt;
        let arg = OsString::from_vec(vec![0x72, 0xff, 0x73]);
        assert_eq!(Action::parse(&[arg]).unwrap_err().exit_code(), 2);
    }

    #[test]
    fn usage_error_escapes_the_argument_and_lists_the_actions() {
        let text = Action::parse(&os(&["bad\u{1b}[31m\n"]))
            .unwrap_err()
            .to_string();
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains("bad\u{1b}"));
        for a in Action::ALL {
            assert!(text.contains(a.name()));
        }
    }

    #[test]
    fn long_arguments_are_cut_in_the_message() {
        let long = "x".repeat(10_000);
        let text = Action::parse(&os(&[&long])).unwrap_err().to_string();
        assert!(text.len() < 400);
    }

    // ---- create_main_cluster edit ----

    #[test]
    fn appends_the_line_to_an_empty_or_missing_file() {
        assert_eq!(with_no_main_cluster(""), "create_main_cluster = false\n");
    }

    #[test]
    fn appends_after_a_file_without_trailing_newline() {
        assert_eq!(
            with_no_main_cluster("a = 1"),
            "a = 1\ncreate_main_cluster = false\n"
        );
    }

    #[test]
    fn replaces_an_active_line_and_keeps_everything_else() {
        let input = "# header\n#create_main_cluster = true\nowner = x\ncreate_main_cluster=true\nlast = 2\n";
        let want = "# header\n#create_main_cluster = true\nowner = x\ncreate_main_cluster = false\nlast = 2\n";
        assert_eq!(with_no_main_cluster(input), want);
    }

    #[test]
    fn a_commented_line_alone_gets_an_active_line_appended() {
        assert_eq!(
            with_no_main_cluster("#create_main_cluster = true\n"),
            "#create_main_cluster = true\ncreate_main_cluster = false\n"
        );
    }

    #[test]
    fn edit_is_idempotent_and_replaces_every_active_duplicate() {
        let once = with_no_main_cluster("create_main_cluster = true\ncreate_main_cluster = true\n");
        assert_eq!(
            once,
            "create_main_cluster = false\ncreate_main_cluster = false\n"
        );
        assert_eq!(with_no_main_cluster(&once), once);
    }

    #[test]
    fn similar_keys_are_not_touched() {
        let input = "create_main_cluster_x = true\nxcreate_main_cluster = true\n";
        let out = with_no_main_cluster(input);
        assert!(out.starts_with(input));
        assert!(out.ends_with("create_main_cluster = false\n"));
    }

    #[test]
    fn keeps_crlf_line_endings_on_the_replaced_line() {
        assert_eq!(
            with_no_main_cluster("create_main_cluster = true\r\nx = 1\r\n"),
            "create_main_cluster = false\r\nx = 1\r\n"
        );
    }

    #[test]
    fn file_edit_keeps_mode_and_leaves_no_temp_file() {
        let dir = TempDir::new("conf-edit");
        let ctx = ctx_in(dir.path());
        fs::write(&ctx.createcluster_conf, "create_main_cluster = true\n").unwrap();
        fs::set_permissions(&ctx.createcluster_conf, fs::Permissions::from_mode(0o640)).unwrap();
        set_create_main_cluster_false(&ctx.createcluster_conf).unwrap();
        assert_eq!(
            fs::read_to_string(&ctx.createcluster_conf).unwrap(),
            "create_main_cluster = false\n"
        );
        let mode = fs::metadata(&ctx.createcluster_conf)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(mode, 0o640);
        assert!(!dir.path().join("createcluster.conf.bhdata-tmp").exists());
    }

    #[test]
    fn file_edit_creates_a_missing_file_as_0644() {
        let dir = TempDir::new("conf-new");
        let ctx = ctx_in(dir.path());
        set_create_main_cluster_false(&ctx.createcluster_conf).unwrap();
        let meta = fs::metadata(&ctx.createcluster_conf).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o644);
    }

    #[test]
    fn file_edit_survives_a_stale_temp_file_and_skips_an_unneeded_write() {
        let dir = TempDir::new("conf-stale");
        let ctx = ctx_in(dir.path());
        fs::write(dir.path().join("createcluster.conf.bhdata-tmp"), "junk").unwrap();
        set_create_main_cluster_false(&ctx.createcluster_conf).unwrap();
        assert!(!dir.path().join("createcluster.conf.bhdata-tmp").exists());
        // Already correct: no rewrite, so a leftover temp name is not even created.
        let before = fs::metadata(&ctx.createcluster_conf)
            .unwrap()
            .modified()
            .unwrap();
        set_create_main_cluster_false(&ctx.createcluster_conf).unwrap();
        let after = fs::metadata(&ctx.createcluster_conf)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after);
    }

    // ---- install-pinned ----

    fn apt_cmd(args: &[&str]) -> Cmd {
        apt(args)
    }

    #[test]
    fn install_pinned_runs_the_exact_sequence() {
        let dir = TempDir::new("install-seq");
        let ctx = ctx_in(dir.path());
        let bins = ctx.clone();
        let mut fake = Fake::with_hook(Box::new(move |cmd| {
            if cmd.args.last().map(String::as_str) == Some("postgresql-17") {
                put_binaries(&bins);
            }
            Ok(())
        }));
        install_pinned(&ctx, &mut fake).unwrap();
        assert_eq!(
            fake.cmds,
            vec![
                apt_cmd(&["update"]),
                apt_cmd(&["install", "--no-install-recommends", "postgresql-common"]),
                Cmd::new(PGDG_SCRIPT, ["-y"]),
                apt_cmd(&["update"]),
                apt_cmd(&["install", "--no-install-recommends", "postgresql-17"]),
            ]
        );
    }

    #[test]
    fn install_pinned_sets_the_no_main_cluster_rule_before_the_server_package() {
        let dir = TempDir::new("install-order");
        let ctx = ctx_in(dir.path());
        let conf = ctx.createcluster_conf.clone();
        let bins = ctx.clone();
        let mut fake = Fake::with_hook(Box::new(move |cmd| {
            if cmd.args.last().map(String::as_str) == Some("postgresql-17") {
                let text = fs::read_to_string(&conf).unwrap();
                assert!(
                    text.contains(NO_MAIN_CLUSTER),
                    "rule missing when the server package was installed"
                );
                put_binaries(&bins);
            }
            Ok(())
        }));
        install_pinned(&ctx, &mut fake).unwrap();
    }

    #[test]
    fn install_pinned_refuses_when_a_cluster_of_the_pinned_major_exists() {
        let dir = TempDir::new("install-refuse");
        let ctx = ctx_in(dir.path());
        fs::create_dir_all(ctx.etc_postgresql.join(PG_MAJOR).join("main")).unwrap();
        let mut fake = Fake::new();
        let err = install_pinned(&ctx, &mut fake).unwrap_err();
        assert_eq!(err.exit_code(), 4);
        assert!(fake.cmds.is_empty(), "nothing may run before the refusal");
        assert!(!ctx.createcluster_conf.exists());
    }

    #[test]
    fn other_majors_and_an_empty_major_dir_do_not_block_install() {
        let dir = TempDir::new("install-others");
        let ctx = ctx_in(dir.path());
        fs::create_dir_all(ctx.etc_postgresql.join("16").join("main")).unwrap();
        fs::create_dir_all(ctx.etc_postgresql.join(PG_MAJOR)).unwrap();
        assert!(refuse_if_clusters_exist(&ctx).is_ok());
    }

    #[test]
    fn install_pinned_fails_when_the_binaries_do_not_appear() {
        let dir = TempDir::new("install-nobins");
        let ctx = ctx_in(dir.path());
        let err = install_pinned(&ctx, &mut Fake::new()).unwrap_err();
        assert_eq!(err.exit_code(), 5);
    }

    #[test]
    fn install_pinned_fails_and_keeps_the_cluster_when_a_default_cluster_appears() {
        let dir = TempDir::new("install-cluster");
        let ctx = ctx_in(dir.path());
        let made = ctx.clone();
        let mut fake = Fake::with_hook(Box::new(move |cmd| {
            if cmd.args.last().map(String::as_str) == Some("postgresql-17") {
                put_binaries(&made);
                fs::create_dir_all(made.etc_postgresql.join(PG_MAJOR).join("main")).unwrap();
            }
            Ok(())
        }));
        let err = install_pinned(&ctx, &mut fake).unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert!(ctx.etc_postgresql.join(PG_MAJOR).join("main").exists());
    }

    #[test]
    fn a_failing_step_stops_the_sequence() {
        let dir = TempDir::new("install-stop");
        let ctx = ctx_in(dir.path());
        let mut fake = Fake::with_hook(Box::new(|cmd| {
            if cmd.program == PGDG_SCRIPT {
                Err(HelperError::CommandFailed {
                    program: PGDG_SCRIPT,
                    code: Some(1),
                })
            } else {
                Ok(())
            }
        }));
        let err = install_pinned(&ctx, &mut fake).unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert_eq!(fake.cmds.len(), 3, "update, install common, repo script");
    }

    // ---- update ----

    #[test]
    fn update_upgrades_only_the_pinned_packages() {
        let dir = TempDir::new("update-seq");
        let ctx = ctx_in(dir.path());
        put_binaries(&ctx);
        let mut fake = Fake::new();
        update(&ctx, &mut fake).unwrap();
        assert_eq!(
            fake.cmds,
            vec![
                apt_cmd(&["update"]),
                apt_cmd(&[
                    "install",
                    "--only-upgrade",
                    "--no-install-recommends",
                    "postgresql-17",
                    "postgresql-client-17",
                ]),
            ]
        );
    }

    #[test]
    fn update_refuses_when_a_cluster_of_the_pinned_major_exists() {
        let dir = TempDir::new("update-refuse");
        let ctx = ctx_in(dir.path());
        put_binaries(&ctx);
        fs::create_dir_all(ctx.etc_postgresql.join(PG_MAJOR).join("main")).unwrap();
        let mut fake = Fake::new();
        assert_eq!(update(&ctx, &mut fake).unwrap_err().exit_code(), 4);
        assert!(fake.cmds.is_empty());
    }

    #[test]
    fn update_without_a_prior_install_says_so() {
        let dir = TempDir::new("update-noinstall");
        let ctx = ctx_in(dir.path());
        let err = update(&ctx, &mut Fake::new()).unwrap_err();
        assert_eq!(err.exit_code(), 5);
        assert!(err.to_string().contains("install-pinned"));
    }

    // ---- restart ----

    #[test]
    fn restart_runs_one_fixed_systemctl_command() {
        let mut fake = Fake::new();
        restart(&mut fake).unwrap();
        assert_eq!(
            fake.cmds,
            vec![Cmd::new(SYSTEMCTL, ["restart", "bhdata-server.service"])]
        );
    }

    // ---- every command, every action ----

    #[test]
    fn every_command_is_an_absolute_path_with_plain_arguments() {
        let dir = TempDir::new("hygiene");
        let ctx = ctx_in(dir.path());
        let mut all = Vec::new();
        for action in Action::ALL {
            // A fresh directory state for each action; binaries are present so verify passes.
            put_binaries(&ctx);
            let _ = fs::remove_file(&ctx.createcluster_conf);
            let mut fake = Fake::new();
            run(action, &ctx, &mut fake).unwrap();
            all.extend(fake.cmds);
        }
        assert!(!all.is_empty());
        for cmd in &all {
            assert!(cmd.program.starts_with('/'), "{}", cmd.program);
            for arg in &cmd.args {
                assert!(
                    !arg.is_empty()
                        && arg
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || "-_.:=+/".contains(c)),
                    "unexpected character in argument {arg:?}"
                );
            }
        }
    }

    #[test]
    fn fixed_environment_is_closed_and_non_interactive() {
        use crate::exec::FIXED_ENV;
        let get = |k: &str| FIXED_ENV.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
        assert_eq!(get("PATH"), Some("/usr/sbin:/usr/bin:/sbin:/bin"));
        assert_eq!(get("DEBIAN_FRONTEND"), Some("noninteractive"));
        assert_eq!(get("NEEDRESTART_SUSPEND"), Some("1"));
        assert!(FIXED_ENV.iter().all(|(n, _)| !n.starts_with("LD_")));
    }

    #[test]
    fn error_exit_codes_are_stable() {
        assert_eq!(HelperError::usage("x").exit_code(), 2);
        assert_eq!(HelperError::NotAllowed(String::new()).exit_code(), 3);
        assert_eq!(HelperError::Refused(String::new()).exit_code(), 4);
        assert_eq!(HelperError::PostCheck(String::new()).exit_code(), 5);
        assert_eq!(
            HelperError::CommandFailed {
                program: "/x",
                code: None
            }
            .exit_code(),
            5
        );
        assert_eq!(HelperError::io("x", io::Error::other("y")).exit_code(), 6);
    }
}
