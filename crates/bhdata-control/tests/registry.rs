//! Runs the registry against a real PostgreSQL (spec `recipe-compiler.md`, sections 5 and 6):
//! a schema becomes tables, registered versions survive a restart, an identical registration is
//! idempotent, and the recipe role can do exactly what the plans' grants say and nothing more.
//! Ignored by default; it needs the server installed and an ordinary user (PostgreSQL refuses to
//! run as root):
//!
//! ```text
//! BHDATA_PG_BIN=/usr/lib/postgresql/17/bin \
//!     cargo test -p bhdata-control --test registry -- --ignored --nocapture
//! ```

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use bhdata_control::blocking;
use bhdata_control::registry::{RegisterOutcome, SchemaOutcome};
use bhdata_control::{ControlError, Target};
use bhdata_engine::instance::{self, InstanceSpec};
use bhdata_engine::supervise;
use bhdata_pg::bootstrap::bootstrap_blocking;
use bhdata_recipe::canon::sha256_hex;
use bhdata_recipe::plan::StepPlan;
use bhdata_recipe::{compile, parse_recipe, Code, Limits};
use bhdata_schema::parse_schema;
use bhdata_types::Durability;

struct Instance {
    spec: InstanceSpec,
    base: PathBuf,
}

impl Instance {
    fn start() -> Instance {
        let bin = std::env::var("BHDATA_PG_BIN").expect("set BHDATA_PG_BIN");
        let base = std::env::temp_dir().join(format!("bhctl-{}", std::process::id()));
        let spec = InstanceSpec {
            id: "shard0".into(),
            base_dir: base.clone(),
            pg_bin_dir: PathBuf::from(bin),
            superuser: instance::current_user().expect("user name"),
            port: 54_397,
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

    fn boot(&self) -> bhdata_pg::bootstrap::BootstrapReport {
        bootstrap_blocking(&self.target(), &self.spec.superuser).expect("bootstrap")
    }

    /// Runs a script as the superuser in the `bhdata` database, first acting as `role` if given.
    /// Returns the output, or the error text.
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

fn data(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../bhdata-recipe/tests/data")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn denied(r: Result<String, String>) -> bool {
    r.is_err_and(|e| e.contains("permission denied"))
}

const BY: &str = "test";

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn a_schema_becomes_tables_and_recipes_are_versioned_idempotent_and_exactly_granted() {
    let pg = Instance::start();
    let target = pg.target();
    let limits = Limits::default();
    let report = pg.boot();
    assert_eq!(
        (report.schema_version, report.applied.as_slice()),
        (2, &[1, 2][..])
    );

    // ---- no schema yet ----
    let spend = data("spend.toml");
    let err = blocking::register_recipe(&target, &spend, &limits, BY).unwrap_err();
    assert!(matches!(err, ControlError::NoSchema), "{err}");
    assert_eq!(blocking::list(&target).expect("list"), []);

    // ---- schema apply: tables, unchanged, additive, destructive ----
    let schema = data("schema.toml");
    let out = blocking::apply_schema(&target, &schema, BY).expect("apply");
    assert!(
        matches!(out, SchemaOutcome::Applied { number: 1, .. }),
        "{out:?}"
    );
    assert_eq!(
        pg.psql(None, "SELECT count(*) FROM app.users, app.entries")
            .as_deref(),
        Ok("0")
    );
    assert_eq!(
        blocking::apply_schema(&target, &schema, BY).expect("again"),
        SchemaOutcome::Unchanged { number: 1 }
    );
    let wider = format!(
        "{schema}\n[[table]]\nname = \"teams\"\nkind = \"subject\"\ncolumns = [{{ name = \"size\", type = \"i64\" }}]\n"
    );
    let out = blocking::apply_schema(&target, &wider, BY).expect("additive");
    assert_eq!(
        out,
        SchemaOutcome::Applied {
            number: 2,
            changes: vec!["add table teams".to_string()]
        }
    );
    assert_eq!(
        pg.psql(None, "SELECT count(*) FROM app.teams").as_deref(),
        Ok("0")
    );
    // Dropping `entries` is destructive: refused whole, nothing changes.
    let narrower = schema.replace(
        "[[table]]\nname = \"entries\"",
        "[[table]]\nname = \"entries_gone\"",
    );
    let err = blocking::apply_schema(&target, &narrower, BY).unwrap_err();
    assert!(matches!(err, ControlError::Destructive(_)), "{err}");
    assert_eq!(
        pg.psql(None, "SELECT count(*) FROM bhdata_control.schema_versions")
            .as_deref(),
        Ok("2")
    );
    assert_eq!(
        pg.psql(None, "SELECT count(*) FROM app.entries").as_deref(),
        Ok("0")
    );
    let err = blocking::apply_schema(&target, "not a schema [", BY).unwrap_err();
    assert!(matches!(err, ControlError::Schema(_)), "{err}");
    // The reader and SQL roles can read the new tables; the recipe role has nothing yet.
    assert!(pg
        .psql(Some("bhdata_reader"), "SELECT count(*) FROM app.users")
        .is_ok());
    assert!(denied(
        pg.psql(Some("bhdata_recipe"), "SELECT count(*) FROM app.users")
    ));

    // ---- register: versions, repeats, rejections ----
    let out = blocking::register_recipe(&target, &spend, &limits, BY).expect("register");
    let RegisterOutcome::Registered {
        version: 1,
        content_hash,
        ..
    } = out
    else {
        panic!("{out:?}");
    };
    let again = blocking::register_recipe(&target, &spend, &limits, BY).expect("repeat");
    assert_eq!(again.describe(), "already registered as spend@1, unchanged");
    // Comments and spacing do not make a new version.
    let noisy = format!("# deployed by hand\n\n{}", spend.replace(" = ", "   =   "));
    assert!(matches!(
        blocking::register_recipe(&target, &noisy, &limits, BY).expect("noisy"),
        RegisterOutcome::Unchanged { version: 1, .. }
    ));
    let changed = spend.replace("max = 1_000_000", "max = 2_000_000");
    let out = blocking::register_recipe(&target, &changed, &limits, BY).expect("v2");
    assert!(
        matches!(out, RegisterOutcome::Registered { version: 2, .. }),
        "{out:?}"
    );
    // A recipe the compiler refuses is not registered, and says why.
    let bad = spend.replace("below  = \"insufficient_funds\"\n", "");
    let err = blocking::register_recipe(&target, &bad, &limits, BY).unwrap_err();
    assert!(
        matches!(&err, ControlError::Rejected(r) if r.code == Code::Bounds),
        "{err}"
    );
    assert!(err.to_string().contains("[bounds]"), "{err}");
    // A limit is checked at registration.
    let tight = Limits {
        max_steps: 2,
        ..Limits::default()
    };
    let err = blocking::register_recipe(&target, &data("give.toml"), &tight, BY).unwrap_err();
    assert!(
        matches!(&err, ControlError::Rejected(r) if r.code == Code::TooBig),
        "{err}"
    );
    for name in ["give", "daily", "add_interest", "history"] {
        blocking::register_recipe(&target, &data(&format!("{name}.toml")), &limits, BY)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }

    // ---- reading back ----
    let rows = blocking::list(&target).expect("list");
    let names: Vec<String> = rows
        .iter()
        .map(|r| format!("{}@{}", r.name, r.version))
        .collect();
    assert_eq!(
        names,
        [
            "add_interest@1",
            "daily@1",
            "give@1",
            "history@1",
            "spend@1",
            "spend@2"
        ]
    );
    assert!(rows
        .iter()
        .all(|r| r.status == "active" && r.schema_version == 2 && r.registered_by == BY));
    let v1 = blocking::get(&target, "spend", 1).expect("get");
    assert_eq!(v1.row.content_hash, content_hash);
    assert_eq!(
        sha256_hex(&v1.plan_json),
        content_hash,
        "the plan is stored as the hashed text"
    );
    assert_eq!(v1.source, spend);
    assert_eq!(
        blocking::latest(&target, "spend")
            .expect("latest")
            .row
            .version,
        2
    );
    for (name, version) in [("spend", Some(9)), ("nope", Some(1)), ("nope", None)] {
        let err = match version {
            Some(v) => blocking::get(&target, name, v).unwrap_err(),
            None => blocking::latest(&target, name).unwrap_err(),
        };
        assert!(matches!(err, ControlError::UnknownRecipe { .. }), "{err}");
    }

    // ---- two registrations of the same new recipe at once make one version ----
    let twin = spend.replace("name    = \"spend\"", "name    = \"twin\"");
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (t, text) = (target.clone(), twin.clone());
            std::thread::spawn(move || blocking::register_recipe(&t, &text, &Limits::default(), BY))
        })
        .collect();
    let outcomes: Vec<RegisterOutcome> = handles
        .into_iter()
        .map(|h| h.join().expect("thread").expect("register"))
        .collect();
    let fresh = outcomes
        .iter()
        .filter(|o| matches!(o, RegisterOutcome::Registered { .. }))
        .count();
    assert_eq!(fresh, 1, "{outcomes:?}");
    assert_eq!(
        pg.psql(
            None,
            "SELECT count(*) FROM bhdata_control.recipes WHERE name = 'twin'"
        )
        .as_deref(),
        Ok("1")
    );

    // ---- the recipe role can do what the grants say, and nothing more ----
    let compiled = {
        let limits = Limits::default();
        let schema = parse_schema(&wider).expect("schema");
        compile(
            &schema,
            &parse_recipe(&spend, &limits).expect("parse"),
            &[],
            &limits,
        )
        .expect("compile")
    };
    let sql_of = |n: usize| match &compiled.steps[n - 1].plan {
        StepPlan::Ensure { sql } | StepPlan::Add { sql, .. } | StepPlan::Insert { sql, .. } => {
            sql.clone()
        }
        other => panic!("{other:?}"),
    };
    let StepPlan::Insert {
        lock_sql: Some(lock),
        ..
    } = &compiled.steps[2].plan
    else {
        panic!("the insert takes a lock");
    };
    let flow = format!(
        "BEGIN;\nPREPARE ens AS {};\nPREPARE add AS {};\nPREPARE lck AS {lock};\nPREPARE ins AS {};\n\
         EXECUTE ens('u1');\nEXECUTE add('u1', 100);\nEXECUTE lck('u1');\nEXECUTE ins('u1', 1700000000000000, -30);\n\
         COMMIT;\nSELECT balance FROM app.users WHERE id = 'u1';",
        sql_of(1),
        sql_of(2),
        sql_of(3)
    );
    let shown = pg
        .psql(Some("bhdata_recipe"), &flow)
        .expect("the plan's statements run");
    assert_eq!(shown.lines().last(), Some("100"), "{shown}");
    for what in [
        "SELECT name FROM app.users",
        "UPDATE app.users SET name = 'x' WHERE id = 'u1'",
        "DELETE FROM app.users",
        "UPDATE app.entries SET delta = 0",
        "INSERT INTO app.teams (id, size) VALUES ('t', 1)",
        "SELECT * FROM bhdata_control.recipes",
        "SELECT * FROM bhdata_control.schema_versions",
    ] {
        assert!(denied(pg.psql(Some("bhdata_recipe"), what)), "{what}");
    }

    // ---- a right nobody registered does not survive; a repeat repairs a lost one ----
    pg.psql(None, "GRANT UPDATE (name) ON app.users TO bhdata_recipe")
        .expect("grant");
    assert!(pg
        .psql(
            Some("bhdata_recipe"),
            "UPDATE app.users SET name = 'x' WHERE false"
        )
        .is_ok());
    blocking::reapply_grants(&target).expect("reapply");
    assert!(denied(pg.psql(
        Some("bhdata_recipe"),
        "UPDATE app.users SET name = 'x' WHERE false"
    )));
    pg.psql(
        None,
        "REVOKE SELECT (balance) ON app.users FROM bhdata_recipe",
    )
    .expect("revoke");
    assert!(denied(
        pg.psql(Some("bhdata_recipe"), "SELECT balance FROM app.users")
    ));
    // Registering the latest content again adds no version, and repairs the lost right.
    assert!(matches!(
        blocking::register_recipe(&target, &changed, &limits, BY).expect("repeat"),
        RegisterOutcome::Unchanged { version: 2, .. }
    ));
    assert!(pg
        .psql(Some("bhdata_recipe"), "SELECT balance FROM app.users")
        .is_ok());
    // Going back to the content of an older version is a new version: the latest is what callers
    // get.
    let back = blocking::register_recipe(&target, &spend, &limits, BY).expect("back");
    assert!(
        matches!(back, RegisterOutcome::Registered { version: 3, .. }),
        "{back:?}"
    );

    // ---- a registered row is never changed, except its status ----
    for what in [
        "UPDATE bhdata_control.recipes SET plan = 'x'",
        "UPDATE bhdata_control.recipes SET source = 'x'",
        "UPDATE bhdata_control.schema_versions SET source = 'x'",
        "DELETE FROM bhdata_control.recipes",
        "DELETE FROM bhdata_control.schema_versions",
        "TRUNCATE bhdata_control.recipes",
    ] {
        assert!(denied(pg.psql(Some("bhdata_admin"), what)), "{what}");
    }
    for role in ["bhdata_reader", "bhdata_sql"] {
        assert!(
            denied(pg.psql(Some(role), "SELECT * FROM bhdata_control.recipes")),
            "{role}"
        );
    }
    // A retired latest version is not reused for identical content.
    pg.psql(
        Some("bhdata_admin"),
        "UPDATE bhdata_control.recipes SET status = 'retired' WHERE name = 'spend' AND version = 3",
    )
    .expect("retire");
    assert_eq!(
        blocking::latest(&target, "spend")
            .expect("latest")
            .row
            .version,
        2
    );
    let out = blocking::register_recipe(&target, &spend, &limits, BY).expect("v4");
    assert!(
        matches!(out, RegisterOutcome::Registered { version: 4, .. }),
        "{out:?}"
    );

    // ---- restart: everything is still there, and the bootstrap changes nothing ----
    let before = blocking::list(&target).expect("list");
    pg.restart();
    let report = pg.boot();
    assert_eq!(
        (report.schema_version, report.applied.is_empty()),
        (2, true)
    );
    assert_eq!(blocking::list(&target).expect("list"), before);
    assert_eq!(
        blocking::get(&target, "spend", 1)
            .expect("get")
            .row
            .content_hash,
        content_hash
    );
    assert!(blocking::reapply_grants(&target).is_ok());
    assert!(pg
        .psql(Some("bhdata_recipe"), "SELECT balance FROM app.users")
        .is_ok());

    // ---- a stored plan that no longer matches its source stops the grants ----
    pg.psql(None, "UPDATE bhdata_control.recipes SET source = replace(source, '1_000_000', '3_000_000') WHERE name = 'spend' AND version = 1")
        .expect("tamper");
    let err = blocking::reapply_grants(&target).unwrap_err();
    assert!(matches!(err, ControlError::Inconsistent(_)), "{err}");
    assert!(err.to_string().contains("spend@1"), "{err}");
}
