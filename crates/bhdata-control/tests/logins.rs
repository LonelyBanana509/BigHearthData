//! Runs the logins and the audit log against a real PostgreSQL (wire-protocol spec, sections 6, 7
//! and 10): a login keeps a verifier and never the password, every change writes its audit row in
//! the same transaction, the gateway's `LoginStore` reads and reports through the admin pool, and
//! the database itself refuses what the rules refuse. Ignored by default; it needs the server
//! installed and an ordinary user (PostgreSQL refuses to run as root):
//!
//! ```text
//! BHDATA_PG_BIN=/usr/lib/postgresql/17/bin \
//!     cargo test -p bhdata-control --test logins -- --ignored --nocapture
//! ```

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use bhdata_api::login::{AuditEvent, LoginStore, Permissions, RecipeAllow};
use bhdata_control::audit::{self, AuditRow, MAX_NAME_BYTES};
use bhdata_control::logins::{blocking, Change, Limit, LoginError, Permission, PgLoginStore};
use bhdata_control::Target;
use bhdata_engine::instance::{self, InstanceSpec};
use bhdata_engine::supervise;
use bhdata_pg::bootstrap::bootstrap_blocking;
use bhdata_pg::pool::Pools;
use bhdata_proto::scram::Verifier;
use bhdata_types::config::PoolConfig;
use bhdata_types::{Durability, LoginId, Timestamp};

struct Instance {
    spec: InstanceSpec,
    base: PathBuf,
}

impl Instance {
    fn start() -> Instance {
        let bin = std::env::var("BHDATA_PG_BIN").expect("set BHDATA_PG_BIN");
        let base = std::env::temp_dir().join(format!("bhlogins-{}", std::process::id()));
        let spec = InstanceSpec {
            id: "shard0".into(),
            base_dir: base.clone(),
            pg_bin_dir: PathBuf::from(bin),
            superuser: instance::current_user().expect("user name"),
            port: 54_398,
            durability: Durability::Full,
            max_connections: 60,
            locale: "C.UTF-8".into(),
        };
        instance::init(&spec).expect("init");
        supervise::start(&spec).expect("start");
        Instance { spec, base }
    }

    fn target(&self) -> Target {
        Target {
            socket_dir: self.spec.run_dir(),
            port: self.spec.port,
        }
    }

    fn boot(&self) {
        bootstrap_blocking(&self.target(), &self.spec.superuser).expect("bootstrap");
    }

    /// Runs a script as the superuser in the `bhdata` database, first acting as `role` if given.
    fn psql(&self, role: Option<&str>, sql: &str) -> Result<String, String> {
        let mut child = Command::new(self.spec.bin("psql"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("-h")
            .arg(self.spec.run_dir())
            .args([
                "-p",
                &self.spec.port.to_string(),
                "-U",
                &self.spec.superuser,
            ])
            .args(["-d", "bhdata", "-X", "-q", "-tA", "-v", "ON_ERROR_STOP=1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run psql");
        let script = match role {
            Some(r) => format!("SET ROLE {r};\n{sql}\n"),
            None => format!("{sql}\n"),
        };
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(script.as_bytes())
            .expect("write");
        let out = child.wait_with_output().expect("psql output");
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).to_string())
        }
    }

    fn restart(&self) {
        supervise::stop(&self.spec).expect("stop");
        supervise::start(&self.spec).expect("start");
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = supervise::stop(&self.spec);
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn denied(r: Result<String, String>) -> bool {
    r.is_err_and(|e| e.contains("permission denied"))
}

const BY: &str = "test";
const PASSWORD: &[u8] = b"correct horse battery staple";
const ITERATIONS: u32 = 4096;
const SOURCE: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

fn events_of(rows: &[AuditRow]) -> Vec<&str> {
    rows.iter().rev().map(|r| r.event.as_str()).collect()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

/// Two admin connections, made and used on `rt` (its tasks only run inside `block_on`).
fn admin_pool(rt: &tokio::runtime::Runtime, target: &Target) -> Arc<Pools> {
    let sizes = PoolConfig {
        recipe_writers: 0,
        readers: 0,
        sql_sessions: 0,
        admin: 2,
        reserved_backup: 0,
    };
    Arc::new(
        rt.block_on(Pools::connect(target, &sizes))
            .expect("admin pool"),
    )
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn logins_are_made_changed_read_by_the_gateway_and_audited() {
    let pg = Instance::start();
    pg.boot();
    let target = pg.target();

    // ---- create: a verifier, never the password; nothing allowed yet ----
    blocking::create(&target, "alice", PASSWORD, ITERATIONS, BY).expect("create");
    let dup = blocking::create(&target, "alice", b"other", ITERATIONS, BY).unwrap_err();
    assert!(
        matches!(dup, LoginError::Exists(ref n) if n == "alice"),
        "{dup}"
    );
    for bad in ["has space", "", "a=b", "x,y"] {
        let e = blocking::create(&target, bad, PASSWORD, ITERATIONS, BY).unwrap_err();
        assert!(matches!(e, LoginError::Invalid(_)), "{bad}: {e}");
    }
    let e = blocking::create(&target, "bob", b"", ITERATIONS, BY).unwrap_err();
    assert!(matches!(e, LoginError::Invalid(_)), "{e}");

    let listed = blocking::list(&target).expect("list");
    assert_eq!(
        listed.len(),
        1,
        "a refused create leaves no row: {listed:?}"
    );
    let alice = &listed[0];
    assert_eq!(alice.login_id.as_str(), "alice");
    assert_eq!(alice.permissions, Permissions::default());
    assert_eq!(alice.recipes, RecipeAllow::None);
    assert_eq!(
        (
            alice.rate_per_sec,
            alice.rate_burst,
            alice.max_connections,
            alice.disabled
        ),
        (None, None, None, false)
    );
    // The stored verifier is the one the password derives with the stored salt and count.
    let expected = Verifier::derive(PASSWORD, alice.verifier.salt.clone(), ITERATIONS).unwrap();
    assert_eq!(alice.verifier.iterations, ITERATIONS);
    assert_eq!(alice.verifier.stored_key, expected.stored_key);
    assert_eq!(alice.verifier.server_key, expected.server_key);
    let dump = pg
        .psql(None, "SELECT * FROM bhdata_control.logins")
        .expect("dump");
    assert!(
        !dump.contains("correct horse"),
        "the password is stored nowhere"
    );

    // ---- changes ----
    for change in [
        Change::Grant(Permission::Execute),
        Change::GrantRecipes(names(&["spend", "star"])),
        Change::GrantRecipes(names(&["narrow"])),
        Change::RevokeRecipes(names(&["star"])),
        Change::SetLimit(Limit::Rate, 50),
        Change::SetLimit(Limit::Connections, 4),
        Change::ClearLimit(Limit::Connections),
    ] {
        blocking::change(&target, "alice", &change, BY).expect("change");
    }
    let alice = blocking::list(&target).expect("list").remove(0);
    assert!(alice.permissions.execute && !alice.permissions.admin);
    assert!(alice.recipes.permits("spend") && alice.recipes.permits("narrow"));
    assert!(!alice.recipes.permits("star"));
    assert_eq!(
        (alice.rate_per_sec, alice.max_connections),
        (Some(50), None)
    );

    // `*` replaces the names; names and `*` do not mix; a refusal changes nothing and audits nothing.
    blocking::change(&target, "alice", &Change::GrantAllRecipes, BY).expect("star");
    let before = blocking::audit_list(&target, Some("alice"), 1000)
        .unwrap()
        .len();
    let mixed =
        blocking::change(&target, "alice", &Change::GrantRecipes(names(&["x"])), BY).unwrap_err();
    assert!(matches!(mixed, LoginError::Invalid(_)), "{mixed}");
    let zero =
        blocking::change(&target, "alice", &Change::SetLimit(Limit::Rate, 0), BY).unwrap_err();
    assert!(matches!(zero, LoginError::Invalid(_)), "{zero}");
    assert_eq!(
        blocking::audit_list(&target, Some("alice"), 1000)
            .unwrap()
            .len(),
        before
    );
    assert_eq!(
        blocking::list(&target).unwrap()[0].recipes,
        RecipeAllow::All
    );
    blocking::change(&target, "alice", &Change::RevokeAllRecipes, BY).expect("unstar");

    // An unknown login is `Unknown` for every command.
    let e = blocking::change(&target, "nobody", &Change::Disable, BY).unwrap_err();
    assert!(
        matches!(e, LoginError::Unknown(ref n) if n == "nobody"),
        "{e}"
    );
    let e = blocking::set_password(&target, "nobody", b"pw", ITERATIONS, BY).unwrap_err();
    assert!(matches!(e, LoginError::Unknown(_)), "{e}");

    // ---- a new password replaces the verifier; disable and enable ----
    blocking::set_password(&target, "alice", b"a new password", ITERATIONS, BY).expect("set");
    let alice = blocking::list(&target).unwrap().remove(0);
    let new = Verifier::derive(b"a new password", alice.verifier.salt.clone(), ITERATIONS).unwrap();
    assert_eq!(alice.verifier.stored_key, new.stored_key);
    assert_ne!(alice.verifier.stored_key, expected.stored_key);
    blocking::change(&target, "alice", &Change::Disable, BY).expect("disable");
    assert!(blocking::list(&target).unwrap()[0].disabled);
    blocking::change(&target, "alice", &Change::Enable, BY).expect("enable");

    // ---- the audit log: every change, in order, by whom, and no secret ----
    let rows = blocking::audit_list(&target, Some("alice"), 1000).expect("audit");
    assert_eq!(
        events_of(&rows),
        [
            "login_created",
            "permissions_changed",
            "recipes_changed",
            "recipes_changed",
            "recipes_changed",
            "limits_changed",
            "limits_changed",
            "limits_changed",
            "recipes_changed",
            "recipes_changed",
            "password_changed",
            "login_disabled",
            "login_enabled",
        ]
    );
    assert!(rows.iter().all(|r| r.actor.as_deref() == Some(BY)));
    let text = format!("{rows:?}");
    assert!(
        !text.contains("correct horse") && !text.contains("a new password"),
        "{text}"
    );
    assert_eq!(rows.first().unwrap().detail.as_deref(), Some("enabled"));

    // ---- the gateway's view: LoginStore through the admin pool ----
    let rt = runtime();
    let store = PgLoginStore::new(admin_pool(&rt, &target));
    let found = rt
        .block_on(store.find("alice"))
        .expect("find")
        .expect("exists");
    assert_eq!(found.login_id.as_str(), "alice");
    assert!(found.permissions.execute && !found.disabled);
    assert!(rt.block_on(store.find("ghost")).expect("find").is_none());
    // Whatever a client sent can be asked for: it is answered, never an error and never a SQL problem.
    for odd in [
        "",
        "a b",
        "x'; DROP TABLE logins; --",
        "é",
        &"x".repeat(5000),
    ] {
        assert!(
            rt.block_on(store.find(odd)).expect("find").is_none(),
            "{odd:?}"
        );
    }
    assert_eq!(blocking::list(&target).unwrap().len(), 1);

    let at = Timestamp::from_unix_millis(1_700_000_000_123);
    let alice_id = LoginId::new("alice").unwrap();
    for event in [
        AuditEvent::LoginSucceeded {
            at,
            login_id: alice_id,
            source: SOURCE,
        },
        AuditEvent::LoginFailed {
            at,
            name: "ghost".into(),
            name_exists: false,
            source: SOURCE,
        },
        AuditEvent::LoginFailed {
            at,
            name: "not a valid id!".into(),
            name_exists: false,
            source: SOURCE,
        },
        AuditEvent::LoginFailed {
            at,
            name: "y".repeat(5000),
            name_exists: false,
            source: SOURCE,
        },
        AuditEvent::LockedOut {
            at,
            name: "ghost".into(),
        },
        AuditEvent::LockoutReleased {
            at,
            name: "ghost".into(),
        },
    ] {
        rt.block_on(store.audit(event)).expect("audit");
    }
    let ghost = blocking::audit_list(&target, Some("ghost"), 100).unwrap();
    assert_eq!(
        events_of(&ghost),
        ["login_failed", "locked_out", "lockout_released"]
    );
    assert_eq!(ghost[2].name_exists, Some(false));
    assert_eq!(ghost[2].source.as_deref(), Some("10.0.0.7"));
    assert_eq!(ghost[2].at, "2023-11-14T22:13:20.123Z");
    assert_eq!(
        (ghost[0].name_exists, ghost[0].source.clone()),
        (None, None)
    );
    let odd = blocking::audit_list(&target, Some("not a valid id!"), 10).unwrap();
    assert_eq!(odd.len(), 1);
    let long = blocking::audit_list(&target, Some(&"y".repeat(MAX_NAME_BYTES)), 10).unwrap();
    assert_eq!(long.len(), 1, "a long name is cut, not refused");
    let success = blocking::audit_list(&target, Some("alice"), 1).unwrap();
    assert_eq!(success[0].event, "login_succeeded");
    assert_eq!(success[0].name_exists, Some(true));

    // ---- what the database refuses by itself ----
    // Only the admin role reads or writes these tables, and it never deletes or edits an audit row.
    for role in ["bhdata_recipe", "bhdata_reader", "bhdata_sql"] {
        assert!(
            denied(pg.psql(Some(role), "SELECT * FROM bhdata_control.logins")),
            "{role}"
        );
        assert!(
            denied(pg.psql(Some(role), "SELECT * FROM bhdata_control.audit")),
            "{role}"
        );
    }
    assert!(pg
        .psql(
            Some("bhdata_admin"),
            "SELECT count(*) FROM bhdata_control.logins"
        )
        .is_ok());
    for sql in [
        "DELETE FROM bhdata_control.audit",
        "TRUNCATE bhdata_control.audit",
        "UPDATE bhdata_control.audit SET detail = 'x'",
        "DELETE FROM bhdata_control.logins",
        "UPDATE bhdata_control.logins SET login_id = 'zed'",
        "UPDATE bhdata_control.logins SET created_by = 'zed'",
    ] {
        assert!(denied(pg.psql(Some("bhdata_admin"), sql)), "{sql}");
    }
    // The checks hold even for the superuser.
    for (what, sql) in [
        ("a login id with a space", "INSERT INTO bhdata_control.logins (login_id, salt, iterations, stored_key, server_key, created_by) VALUES ('a b', '\\x01', 1, decode(repeat('00',32),'hex'), decode(repeat('00',32),'hex'), 'x')"),
        ("a short key", "INSERT INTO bhdata_control.logins (login_id, salt, iterations, stored_key, server_key, created_by) VALUES ('k', '\\x01', 1, '\\x00', decode(repeat('00',32),'hex'), 'x')"),
        ("`*` together with names", "UPDATE bhdata_control.logins SET all_recipes = true, recipes = '{a}' WHERE login_id = 'alice'"),
        ("a bad recipe name", "UPDATE bhdata_control.logins SET recipes = '{Bad}' WHERE login_id = 'alice'"),
        ("a null recipe", "UPDATE bhdata_control.logins SET recipes = ARRAY['a', NULL] WHERE login_id = 'alice'"),
        ("a zero rate", "UPDATE bhdata_control.logins SET rate_per_sec = 0 WHERE login_id = 'alice'"),
        ("an unknown audit event", "INSERT INTO bhdata_control.audit (at, event, name) VALUES (now(), 'made_up', 'x')"),
    ] {
        let r = pg.psql(None, sql);
        assert!(r.as_ref().is_err_and(|e| e.contains("violates")), "{what}: {r:?}");
    }

    // ---- logins and audit survive a restart ----
    let count = blocking::audit_list(&target, None, 10_000).unwrap().len();
    pg.restart();
    assert_eq!(blocking::list(&target).unwrap().len(), 1);
    assert_eq!(
        blocking::audit_list(&target, None, 10_000).unwrap().len(),
        count
    );
    // Bootstrapping again changes nothing about the logins.
    pg.boot();
    assert_eq!(blocking::list(&target).unwrap().len(), 1);
    let _ = audit::clip_name("");
}
