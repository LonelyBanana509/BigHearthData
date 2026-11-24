//! Runs `bhdata-pg` against a real PostgreSQL (spec section 9): the bootstrap, the lockdown of each
//! role, `claim_request`, and the real error codes. Ignored by default; it needs the server
//! installed and an ordinary user (PostgreSQL refuses to run as root):
//!
//! ```text
//! BHDATA_PG_BIN=/usr/lib/postgresql/17/bin \
//!     cargo test -p bhdata-engine --test pg_access -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use bhdata_engine::instance::{self, InstanceSpec};
use bhdata_engine::supervise;
use bhdata_pg::bootstrap::{bootstrap_blocking, BootstrapError};
use bhdata_pg::errors::{PgErrorKind, Retry};
use bhdata_pg::pool::{pg_error, PoolError, Pools, Target};
use bhdata_types::config::PoolConfig;
use bhdata_types::Durability;

struct Instance {
    spec: InstanceSpec,
    base: PathBuf,
}

impl Instance {
    fn start() -> Instance {
        let bin = std::env::var("BHDATA_PG_BIN").expect("set BHDATA_PG_BIN");
        let base = std::env::temp_dir().join(format!("bhpga-{}", std::process::id()));
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

    /// Runs SQL as the superuser, in `db`; returns the trimmed output or the error text.
    fn psql(&self, db: &str, sql: &str) -> Result<String, String> {
        let out = Command::new(self.spec.bin("psql"))
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
            .args(["-d", db, "-tA", "-v", "ON_ERROR_STOP=1", "-c", sql])
            .output()
            .expect("run psql");
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).to_string())
        }
    }

    fn boot(&self) -> Result<bhdata_pg::bootstrap::BootstrapReport, BootstrapError> {
        bootstrap_blocking(&self.target(), &self.spec.superuser)
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = supervise::stop(&self.spec);
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn sizes() -> PoolConfig {
    PoolConfig {
        recipe_writers: 2,
        readers: 1,
        sql_sessions: 1,
        admin: 1,
        reserved_backup: 0,
    }
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn bootstrap_is_idempotent_and_refuses_what_is_wrong() {
    let pg = Instance::start();

    let first = pg.boot().unwrap();
    assert_eq!(
        (
            first.schema_version,
            first.applied.clone(),
            first.created_database
        ),
        (4, vec![1, 2, 3, 4], true)
    );
    let second = pg.boot().unwrap();
    assert_eq!(
        (
            second.schema_version,
            second.applied,
            second.created_database
        ),
        (4, vec![], false)
    );

    // A role with a wrong attribute is named, not fixed; fixing it by hand lets start-up go on.
    pg.psql("postgres", "ALTER ROLE bhdata_reader CREATEDB")
        .unwrap();
    let text = pg.boot().unwrap_err().to_string();
    assert!(
        text.contains("bhdata_reader") && text.contains("createdb"),
        "{text}"
    );
    pg.psql("postgres", "ALTER ROLE bhdata_reader NOCREATEDB")
        .unwrap();
    assert!(pg.boot().is_ok());

    // A schema with another owner is named.
    pg.psql(
        "bhdata",
        &format!("ALTER SCHEMA app OWNER TO \"{}\"", pg.spec.superuser),
    )
    .unwrap();
    let text = pg.boot().unwrap_err().to_string();
    assert!(
        text.contains("app") && text.contains("bhdata_owner"),
        "{text}"
    );
    pg.psql("bhdata", "ALTER SCHEMA app OWNER TO bhdata_owner")
        .unwrap();

    // Data from a newer release is never touched.
    pg.psql(
        "bhdata",
        "UPDATE bhdata_internal.meta SET value = '99' WHERE key = 'schema_version'",
    )
    .unwrap();
    assert!(matches!(pg.boot(), Err(BootstrapError::TooNew(_))));
    pg.psql(
        "bhdata",
        "UPDATE bhdata_internal.meta SET value = '4' WHERE key = 'schema_version'",
    )
    .unwrap();
    assert!(pg.boot().unwrap().applied.is_empty());

    // The managed files are what the server runs with.
    assert_eq!(
        pg.psql("postgres", "show superuser_reserved_connections")
            .unwrap(),
        "3"
    );
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn each_role_logs_in_as_itself_and_the_recipe_role_is_locked_down() {
    let pg = Instance::start();
    pg.boot().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let pools = Pools::connect(&pg.target(), &sizes()).await.unwrap();
        for (name, client) in [
            ("bhdata_recipe", pools.recipe().await.unwrap()),
            ("bhdata_reader", pools.reader().await.unwrap()),
            ("bhdata_sql", pools.sql().await.unwrap()),
            ("bhdata_admin", pools.admin().await.unwrap()),
        ] {
            let user: String = client.query_one("SELECT current_user::text", &[]).await.unwrap().get(0);
            assert_eq!(user, name);
        }

        let c = pools.recipe().await.unwrap();
        for sql in [
            "SELECT * FROM bhdata_internal.requests",
            "SELECT * FROM bhdata_internal.change_log",
            "INSERT INTO bhdata_internal.requests(login_id,request_id,payload_hash,status) VALUES('a','b','\\x00','done')",
            "DELETE FROM bhdata_internal.requests",
            "CREATE TABLE app.x(i int)",
            "CREATE TABLE public.x(i int)",
            "CREATE TEMP TABLE t(i int)",
            "CREATE SCHEMA evil",
            "SELECT * FROM sql.anything",
            "SET ROLE bhdata_owner",
        ] {
            let e = c.batch_execute(sql).await.unwrap_err();
            assert_eq!(pg_error(&e).kind, PgErrorKind::Forbidden, "{sql}");
        }
        assert!(c.batch_execute("CREATE EXTENSION pgcrypto").await.is_err());
        assert!(c.batch_execute("CREATE EXTENSION pgcrypto SCHEMA app").await.is_err());
        // Its timeouts are the ones in the spec table, set in the database.
        for (name, want) in [("lock_timeout", "2s"), ("statement_timeout", "5s"), ("idle_in_transaction_session_timeout", "10s")] {
            let v: String = c.query_one(&format!("SHOW {name}"), &[]).await.unwrap().get(0);
            assert_eq!(v, want, "{name}");
        }
        // Reader and SQL roles cannot call the narrow functions either.
        let call = "SELECT * FROM bhdata_internal.claim_request('l','z','\\x01','r',1,'s',now(),false)";
        let r = pools.reader().await.unwrap();
        assert_eq!(pg_error(&r.query(call, &[]).await.unwrap_err()).kind, PgErrorKind::Forbidden);
        let s = pools.sql().await.unwrap();
        assert_eq!(pg_error(&s.query(call, &[]).await.unwrap_err()).kind, PgErrorKind::Forbidden);
        // SQL mode writes only in its own schema; the admin makes tables and the others get rights.
        s.batch_execute("CREATE TABLE sql.mine(i int); INSERT INTO sql.mine VALUES (1)").await.unwrap();
        assert!(s.batch_execute("CREATE TABLE app.nope(i int)").await.is_err());
    });
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn claim_request_returns_new_same_or_conflict_and_concurrent_retries_get_one_result() {
    let pg = Instance::start();
    pg.boot().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let pools = std::sync::Arc::new(Pools::connect(&pg.target(), &sizes()).await.unwrap());
        let c = pools.recipe().await.unwrap();
        let claim = |id: &str, hash: &str| {
            format!("SELECT * FROM bhdata_internal.claim_request('l','{id}','\\x{hash}','rec',1,'s',now(),false)")
        };

        let r = c.query_one(&claim("r1", "01"), &[]).await.unwrap();
        assert_eq!(r.get::<_, String>(0), "new");
        let r = c.query_one(&claim("r1", "01"), &[]).await.unwrap();
        assert_eq!(
            (r.get::<_, String>(0), r.get::<_, Option<String>>(1)),
            ("same".into(), Some("running".into()))
        );
        let r = c.query_one(&claim("r1", "02"), &[]).await.unwrap();
        assert_eq!(r.get::<_, String>(0), "conflict");

        c.query_one(
            "SELECT bhdata_internal.append_change('l','r1','s','t','insert','k')",
            &[],
        )
        .await
        .unwrap();
        c.batch_execute("SELECT bhdata_internal.finish_request('l','r1','done',NULL,'\\xaa')")
            .await
            .unwrap();
        let r = c.query_one(&claim("r1", "01"), &[]).await.unwrap();
        assert_eq!(
            (r.get::<_, String>(0), r.get::<_, Option<String>>(1)),
            ("same".into(), Some("done".into()))
        );
        assert_eq!(r.get::<_, Option<Vec<u8>>>(3), Some(vec![0xaa]));

        // A finished request cannot be finished again, nor given more changes; our own code says so.
        for sql in [
            "SELECT bhdata_internal.finish_request('l','r1','done',NULL,'\\xaa')",
            "SELECT bhdata_internal.append_change('l','r1','s','t','insert','k')",
            "SELECT bhdata_internal.finish_request('l','nope','done',NULL,NULL)",
        ] {
            let e = c.batch_execute(sql).await.unwrap_err();
            assert_eq!(pg_error(&e).kind, PgErrorKind::Refused, "{sql}");
        }
        // The id limits are enforced by the table.
        let e = c.query_one(&claim("", "01"), &[]).await.unwrap_err();
        assert_eq!(pg_error(&e).kind, PgErrorKind::Constraint);
        drop(c);

        // Two identical requests at once: the second waits for the first's commit, then both
        // see one result.
        let p = pools.clone();
        let a = tokio::spawn(async move {
            let mut c = p.recipe().await.unwrap();
            let tx = c.transaction().await.unwrap();
            let row = tx
                .query_one(
                    "SELECT * FROM bhdata_internal.claim_request('l','cc','\\x07','r',1,'s',now(),false)",
                    &[],
                )
                .await
                .unwrap();
            let outcome: String = row.get(0);
            tokio::time::sleep(Duration::from_millis(1_500)).await;
            tx.batch_execute("SELECT bhdata_internal.finish_request('l','cc','done',NULL,'\\xbb')")
                .await
                .unwrap();
            tx.commit().await.unwrap();
            outcome
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        let started = Instant::now();
        let c = pools.recipe().await.unwrap();
        let row = c
            .query_one(
                "SELECT * FROM bhdata_internal.claim_request('l','cc','\\x07','r',1,'s',now(),false)",
                &[],
            )
            .await
            .unwrap();
        let waited = started.elapsed();
        assert_eq!(a.await.unwrap(), "new");
        assert_eq!(
            (row.get::<_, String>(0), row.get::<_, Option<String>>(1)),
            ("same".into(), Some("done".into()))
        );
        assert!(
            waited >= Duration::from_millis(700),
            "waited only {waited:?}"
        );
    });
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn real_errors_map_to_the_kinds_in_the_table_and_a_busy_pool_answers_quickly() {
    let pg = Instance::start();
    pg.boot().unwrap();
    let rt = runtime();
    rt.block_on(async {
        let pools = std::sync::Arc::new(Pools::connect(&pg.target(), &sizes()).await.unwrap());
        let admin = pools.admin().await.unwrap();
        admin
            .batch_execute("CREATE TABLE app.t(id int PRIMARY KEY, n int)")
            .await
            .unwrap();
        admin
            .batch_execute("GRANT SELECT, INSERT, UPDATE ON app.t TO bhdata_recipe")
            .await
            .unwrap();
        drop(admin);

        let c = pools.recipe().await.unwrap();
        c.batch_execute("INSERT INTO app.t VALUES (1, 0), (2, 0)")
            .await
            .unwrap();
        let e = c
            .batch_execute("INSERT INTO app.t VALUES (1, 9)")
            .await
            .unwrap_err();
        assert_eq!(
            (pg_error(&e).kind, pg_error(&e).retry),
            (PgErrorKind::UniqueViolation, Retry::No)
        );
        let e = c
            .batch_execute("INSERT INTO app.t VALUES (NULL, 9)")
            .await
            .unwrap_err();
        assert_eq!(pg_error(&e).kind, PgErrorKind::Constraint);
        let e = c
            .batch_execute("BEGIN READ ONLY; UPDATE app.t SET n = 1")
            .await
            .unwrap_err();
        assert_eq!(pg_error(&e).kind, PgErrorKind::Forbidden);
        c.batch_execute("ROLLBACK").await.unwrap();

        // Statement timeout: the reader role's own limit, set in the database.
        let r = pools.reader().await.unwrap();
        let e = r.batch_execute("SELECT pg_sleep(8)").await.unwrap_err();
        assert_eq!(pg_error(&e).kind, PgErrorKind::StatementTimeout);
        drop(r);

        // Lock timeout: one recipe connection holds a row, the other waits longer than 2 s.
        c.batch_execute("BEGIN; UPDATE app.t SET n = n + 1 WHERE id = 1")
            .await
            .unwrap();
        let other = pools.recipe().await.unwrap();
        let e = other
            .batch_execute("UPDATE app.t SET n = 0 WHERE id = 1")
            .await
            .unwrap_err();
        assert_eq!(
            (pg_error(&e).kind, pg_error(&e).retry),
            (PgErrorKind::LockTimeout, Retry::No)
        );
        c.batch_execute("ROLLBACK").await.unwrap();

        // Pool exhausted: both recipe connections are out, so the third caller is told at once.
        let started = Instant::now();
        let busy = pools.recipe().await;
        assert!(
            matches!(busy, Err(PoolError::Busy { .. })),
            "{:?}",
            busy.err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        drop((c, other));

        // Deadlock: two connections lock the two rows in opposite order. One is chosen as the
        // victim and is told to retry (a limited number of times).
        let run = |p: std::sync::Arc<Pools>, first: i32, second: i32| {
            tokio::spawn(async move {
                let mut c = p.recipe().await.unwrap();
                let tx = c.transaction().await.unwrap();
                tx.batch_execute(&format!("UPDATE app.t SET n = n + 1 WHERE id = {first}"))
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(500)).await;
                let r = tx
                    .batch_execute(&format!("UPDATE app.t SET n = n + 1 WHERE id = {second}"))
                    .await;
                r.map_err(|e| pg_error(&e))
            })
        };
        let (x, y) = (run(pools.clone(), 1, 2), run(pools.clone(), 2, 1));
        let results = [x.await.unwrap(), y.await.unwrap()];
        let victims: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
        assert_eq!(victims.len(), 1, "{results:?}");
        assert_eq!(
            (victims[0].kind, victims[0].retry),
            (PgErrorKind::Deadlock, Retry::Limited)
        );

        // Closing the pools is noticed.
        pools.close();
        assert!(matches!(
            pools.recipe().await,
            Err(PoolError::Closed { .. })
        ));
    });
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn a_running_server_with_old_login_rules_gets_the_new_ones_and_status_reports_the_database() {
    use bhdata_engine::host;
    use bhdata_engine::supervise::StartOutcome;
    use bhdata_pg::bootstrap::verify_logins_blocking;

    let pg = Instance::start();
    pg.boot().unwrap();
    verify_logins_blocking(&pg.target(), &sizes()).unwrap();

    // An instance made by M1: only the service user may log in, and the server has read that.
    let hba = pg.spec.data_dir().join("pg_hba.conf");
    std::fs::write(&hba, format!("local all {} peer\n", pg.spec.superuser)).unwrap();
    let reload = Command::new(pg.spec.bin("pg_ctl"))
        .arg("reload")
        .arg("-D")
        .arg(pg.spec.data_dir())
        .status()
        .unwrap();
    assert!(reload.success());
    std::thread::sleep(Duration::from_millis(500));
    assert!(matches!(
        verify_logins_blocking(&pg.target(), &sizes()),
        Err(BootstrapError::Pool(_))
    ));

    // Starting again finds it running, writes the files, and tells the server to read them.
    assert_eq!(
        supervise::start(&pg.spec).unwrap(),
        StartOutcome::AlreadyRunning
    );
    std::thread::sleep(Duration::from_millis(500));
    verify_logins_blocking(&pg.target(), &sizes()).unwrap();

    let (lines, ok) = host::status(&pg.spec, &sizes());
    assert!(ok, "{lines:?}");
    assert!(
        lines.contains(&"database: schema version 4".to_string()),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("connections: pools use 5 + 3")),
        "{lines:?}"
    );
}
