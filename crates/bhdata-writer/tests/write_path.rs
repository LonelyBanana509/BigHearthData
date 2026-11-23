//! The write-path suite against a real PostgreSQL (spec `write-path.md`, section 10): tests 1 to
//! 13, 17 and 18, each on a private instance. Ignored by default; it needs the server installed
//! and an ordinary user (PostgreSQL refuses to run as root):
//!
//! ```text
//! BHDATA_PG_BIN=/usr/lib/postgresql/17/bin \
//!     cargo test -p bhdata-writer --test write_path -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as Proc, Stdio};
use std::sync::Arc;
use std::time::Duration;

use bhdata_api::recipe::eval::Value;
use bhdata_api::recipe::{compile, parse_recipe, Limits, Plan};
use bhdata_api::shard::{Command, Retention, Status};
use bhdata_api::types::config::PoolConfig;
use bhdata_api::types::{
    Clock, Config, Error, LoginId, ManualClock, RecipeVersion, RequestId, SubjectId, Timestamp,
};
use bhdata_changelog::{read_after, Op, Position};
use bhdata_engine::instance::{self, InstanceSpec};
use bhdata_engine::supervise;
use bhdata_pg::bootstrap::bootstrap_blocking;
use bhdata_pg::pool::{pg_error, Pools, Target};
use bhdata_pg::PgErrorKind;
use bhdata_schema::ddl::create_schema;
use bhdata_schema::parse_schema;
use bhdata_writer::exec::{ExecConfig, Executor, Finished};
use bhdata_writer::history;
use bhdata_writer::time_rule::compaction_cutoff;

const HOUR: Duration = Duration::from_secs(3_600);
const DAY: Duration = Duration::from_secs(24 * 3_600);
/// The fake clock starts here (2027), far from any real record.
const T0: u64 = 1_800_000_000_000;

/// A time well after every record the tests write (the database's own `now()` is real, 2026).
fn far_future() -> Timestamp {
    Timestamp::from_unix_millis(T0 + 365 * 24 * 3_600 * 1_000)
}

fn data(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../bhdata-recipe/tests/data")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

struct Rig {
    spec: InstanceSpec,
    base: PathBuf,
    rt: tokio::runtime::Runtime,
    pools: Arc<Pools>,
    clock: Arc<ManualClock>,
    exec: Arc<Executor>,
    plans: BTreeMap<&'static str, Arc<Plan>>,
}

impl Rig {
    fn new(port: u16) -> Rig {
        Rig::with_owner_check(port, None)
    }

    fn with_owner_check(port: u16, owns: Option<bhdata_writer::exec::OwnsFn>) -> Rig {
        let bin = std::env::var("BHDATA_PG_BIN").expect("set BHDATA_PG_BIN");
        let base = std::env::temp_dir().join(format!("bhwp-{}-{port}", std::process::id()));
        let spec = InstanceSpec {
            id: "shard0".into(),
            base_dir: base.clone(),
            pg_bin_dir: PathBuf::from(bin),
            superuser: instance::current_user().expect("user name"),
            port,
            durability: bhdata_api::types::Durability::Full,
            max_connections: 60,
            locale: "C.UTF-8".into(),
        };
        instance::init(&spec).expect("init");
        supervise::start(&spec).expect("start");
        let target = Target {
            socket_dir: spec.run_dir(),
            port: spec.port,
        };
        bootstrap_blocking(&target, &spec.superuser).expect("bootstrap");

        // The schema and the starter recipes, as the registry would apply them.
        let schema = parse_schema(&data("schema.toml")).expect("schema");
        let mut script = create_schema(&schema).join(";\n");
        script.push(';');
        let mut plans = BTreeMap::new();
        for name in ["spend", "give", "daily", "add_interest", "history"] {
            let recipe =
                parse_recipe(&data(&format!("{name}.toml")), &Limits::default()).expect("recipe");
            let plan = compile(&schema, &recipe, &[], &Limits::default()).expect("compiles");
            for g in plan.grants.statements("bhdata_recipe") {
                script.push_str(&format!("\n{g};"));
            }
            plans.insert(name, Arc::new(plan));
        }
        let rig_psql = |role: Option<&str>, sql: &str| psql(&spec, role, sql);
        rig_psql(Some("bhdata_admin"), &script).expect("tables and grants");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("runtime");
        let sizes = PoolConfig {
            recipe_writers: 16,
            readers: 1,
            sql_sessions: 1,
            admin: 2,
            reserved_backup: 0,
        };
        let pools = Arc::new(rt.block_on(Pools::connect(&target, &sizes)).expect("pools"));
        let clock = Arc::new(ManualClock::new(Timestamp::from_unix_millis(T0)));
        let cfg = ExecConfig::from_config(&Config::default());
        let mut exec = Executor::new(pools.clone(), clock.clone(), cfg);
        if let Some(owns) = owns {
            exec = exec.with_owner_check(owns);
        }
        Rig {
            spec,
            base,
            rt,
            pools,
            clock,
            exec: Arc::new(exec),
            plans,
        }
    }

    /// A script as the superuser in `bhdata`, first acting as `role` if given.
    fn sql(&self, q: &str) -> String {
        psql(&self.spec, None, q).unwrap_or_else(|e| panic!("{q}: {e}"))
    }

    fn count(&self, q: &str) -> i64 {
        self.sql(q)
            .parse()
            .unwrap_or_else(|_| panic!("not a number: {q}"))
    }

    fn fund(&self, id: &str, balance: i64) {
        self.sql(&format!(
            "INSERT INTO app.users (id, balance, interest, last_claim) VALUES ('{id}', {balance}, 0, 0)"
        ));
    }

    fn balance(&self, id: &str) -> i64 {
        self.count(&format!("SELECT balance FROM app.users WHERE id = '{id}'"))
    }

    fn cmd(
        &self,
        recipe: &str,
        request: &str,
        subject: &str,
        counterparty: Option<&str>,
        args: &[(&str, Value)],
    ) -> Command {
        self.cmd_for("l", recipe, request, subject, counterparty, args)
    }

    fn cmd_for(
        &self,
        login: &str,
        recipe: &str,
        request: &str,
        subject: &str,
        counterparty: Option<&str>,
        args: &[(&str, Value)],
    ) -> Command {
        Command {
            login_id: LoginId::new(login).expect("login id"),
            request_id: RequestId::new(request).expect("request id"),
            requested_at: self.clock.now(),
            plan: self.plans[recipe].clone(),
            version: RecipeVersion::new(1),
            version_named: false,
            subject: SubjectId::new(subject).expect("subject id"),
            counterparty: counterparty.map(|c| SubjectId::new(c).expect("counterparty id")),
            args: args
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            retention: Retention::Window,
        }
    }

    fn spend(&self, request: &str, subject: &str, amount: i64) -> Command {
        self.cmd(
            "spend",
            request,
            subject,
            None,
            &[("amount", Value::I64(amount))],
        )
    }

    /// Spawns a command on the runtime.
    fn spawn(&self, c: Command) -> tokio::task::JoinHandle<Result<Finished, Error>> {
        let exec = self.exec.clone();
        self.rt.spawn(async move { exec.run(&c).await })
    }

    /// Spawns a command that is retried, as a client would, while the answer is `overloaded`
    /// (nothing is stored then, so the same request id is safe to send again).
    fn spawn_retrying(&self, c: Command) -> tokio::task::JoinHandle<Result<Finished, Error>> {
        let exec = self.exec.clone();
        self.rt.spawn(async move {
            let mut tries = 0;
            loop {
                match exec.run(&c).await {
                    Err(Error::Overloaded { retry_after }) if tries < 60 => {
                        tries += 1;
                        tokio::time::sleep(retry_after).await;
                    }
                    other => return other,
                }
            }
        })
    }

    fn run(&self, c: &Command) -> Result<Finished, Error> {
        self.rt.block_on(self.exec.run(c))
    }

    fn kill_postgres(&self) {
        let ok = Proc::new(self.spec.bin("pg_ctl"))
            .args(["stop", "-m", "immediate", "-w", "-D"])
            .arg(self.spec.data_dir())
            .stdout(Stdio::null())
            .status()
            .expect("pg_ctl");
        assert!(ok.success(), "pg_ctl stop -m immediate");
    }

    fn start_postgres(&self) {
        supervise::start(&self.spec).expect("start again");
    }

    /// Every change-log entry below the horizon, from the start, through the admin pool.
    fn changes(&self) -> Vec<(String, String, Op)> {
        self.rt.block_on(async {
            let c = self.pools.admin().await.expect("admin");
            read_after(&c, Position::START, 1_000)
                .await
                .expect("read")
                .into_iter()
                .map(|l| (l.entry.table, l.entry.row_key, l.entry.op))
                .collect()
        })
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.pools.close();
        let _ = supervise::stop(&self.spec);
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn psql(spec: &InstanceSpec, role: Option<&str>, sql: &str) -> Result<String, String> {
    let mut child = Proc::new(spec.bin("psql"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .arg("-h")
        .arg(spec.run_dir())
        .args(["-p", &spec.port.to_string(), "-U", &spec.superuser])
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

fn done(f: &Finished) -> &str {
    assert_eq!(f.outcome.status, Status::Done, "{:?}", f.outcome);
    &f.outcome.result
}

fn failed(f: &Finished) -> &str {
    match &f.outcome.status {
        Status::Failed { error } => error,
        Status::Done => panic!("expected a named error, got {:?}", f.outcome),
    }
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn one_command_applies_once_and_leaves_one_entry_per_changed_row() {
    // Tests 8 (and the happy path of 1).
    let rig = Rig::new(54_411);
    rig.fund("alice", 500);

    let f = rig.run(&rig.spend("r1", "alice", 30)).expect("runs");
    assert_eq!(done(&f), r#"{"values":{"balance":470}}"#);
    assert!(!f.outcome.replayed && f.commit_time.is_some());
    assert_eq!(rig.balance("alice"), 470);
    assert_eq!(
        rig.sql("SELECT count(*) || ',' || min(delta) FROM app.entries WHERE owner_id = 'alice'"),
        "1,-30"
    );
    assert_eq!(
        rig.sql("SELECT status || ' ' || recipe || ' ' || subject FROM bhdata_internal.requests"),
        "done spend alice"
    );
    // One entry per changed row, in the order the rows were first touched; none for `ensure` on a
    // row that already existed.
    assert_eq!(
        rig.changes(),
        vec![
            ("users".into(), "alice".into(), Op::Update),
            ("entries".into(), "alice/1".into(), Op::Insert),
        ]
    );

    // A failed command and a replay add no entries.
    let f = rig.run(&rig.spend("r2", "alice", 10_000)).expect("runs");
    assert_eq!(failed(&f), "insufficient_funds");
    let f = rig.run(&rig.spend("r1", "alice", 30)).expect("replays");
    assert!(f.outcome.replayed);
    assert_eq!(rig.changes().len(), 2);

    // A counterparty command: a new row is an insert, the others updates, in step order.
    rig.fund("carol", 100);
    let give = rig.cmd(
        "give",
        "g1",
        "carol",
        Some("dave"),
        &[("amount", Value::I64(40))],
    );
    let f = rig.run(&give).expect("runs");
    done(&f);
    let after: Vec<_> = rig.changes().into_iter().skip(2).collect();
    assert_eq!(
        after,
        vec![
            ("users".into(), "dave".into(), Op::Insert),
            ("users".into(), "carol".into(), Op::Update),
            ("entries".into(), "carol/1".into(), Op::Insert),
        ]
    );
    assert_eq!((rig.balance("carol"), rig.balance("dave")), (60, 40));
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn two_concurrent_commands_with_one_key_apply_once_and_both_get_the_result() {
    // Test 1.
    let rig = Rig::new(54_412);
    rig.fund("alice", 500);
    let a = rig.spawn(rig.spend("same", "alice", 7));
    let b = rig.spawn(rig.spend("same", "alice", 7));
    let (a, b) = rig
        .rt
        .block_on(async { (a.await.unwrap(), b.await.unwrap()) });
    let (a, b) = (a.expect("a"), b.expect("b"));
    assert_eq!(a.outcome.result, b.outcome.result);
    assert_eq!(done(&a), r#"{"values":{"balance":493}}"#);
    // Exactly one of them did the work.
    assert_eq!(
        [a.outcome.replayed, b.outcome.replayed]
            .iter()
            .filter(|r| **r)
            .count(),
        1
    );
    assert_eq!(rig.balance("alice"), 493);
    assert_eq!(rig.count("SELECT count(*) FROM app.entries"), 1);
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests"),
        1
    );
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn the_same_key_with_another_payload_is_a_conflict_and_changes_nothing() {
    // Test 2.
    let rig = Rig::new(54_413);
    rig.fund("alice", 500);
    done(&rig.run(&rig.spend("k", "alice", 5)).expect("first"));
    let err = rig.run(&rig.spend("k", "alice", 6)).unwrap_err();
    assert_eq!(err, Error::RequestIdConflict);
    // Another subject under the same key is also another payload.
    let err = rig.run(&rig.spend("k", "bob", 5)).unwrap_err();
    assert_eq!(err, Error::RequestIdConflict);
    assert_eq!(rig.balance("alice"), 495);
    assert_eq!(rig.count("SELECT count(*) FROM app.users"), 1);
    assert_eq!(rig.count("SELECT count(*) FROM app.entries"), 1);
    // The same key under another login is another request.
    let f = rig
        .run(&rig.cmd_for(
            "other",
            "spend",
            "k",
            "alice",
            None,
            &[("amount", Value::I64(5))],
        ))
        .expect("other login");
    assert!(!f.outcome.replayed);
    assert_eq!(rig.balance("alice"), 490);
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn a_client_that_goes_away_at_any_moment_gets_the_single_result_on_retry() {
    // Test 3: the task is dropped at many different moments (before, in the middle of and after the
    // commit); whichever it was, the retry finds the command applied exactly once.
    let rig = Rig::new(54_414);
    rig.fund("alice", 500);
    for i in 0..40u64 {
        let c = rig.spend(&format!("c{i}"), "alice", 1);
        let h = rig.spawn(c.clone());
        rig.rt.block_on(async {
            tokio::time::sleep(Duration::from_micros(i * 400)).await;
        });
        h.abort();
        let _ = rig.rt.block_on(h);
        let f = rig.run(&c).expect("retry");
        done(&f);
    }
    assert_eq!(rig.balance("alice"), 460);
    assert_eq!(rig.count("SELECT count(*) FROM app.entries"), 40);
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests WHERE status = 'done'"),
        40
    );
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests WHERE status = 'running'"),
        0
    );
    assert_eq!(rig.changes().len(), 80);
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn after_postgres_is_killed_a_retry_applies_exactly_once() {
    // Test 4: `pg_ctl -m immediate` while commands are running. Whatever each command's reply was
    // (an answer, or `overloaded`), the retry with the same id settles it: applied once, and
    // anything that was answered before the crash is still there.
    let rig = Rig::new(54_415);
    rig.fund("alice", 1_000);
    let commands: Vec<Command> = (0..30)
        .map(|i| rig.spend(&format!("k{i}"), "alice", 1))
        .collect();
    let handles: Vec<_> = commands.iter().map(|c| rig.spawn(c.clone())).collect();
    std::thread::sleep(Duration::from_millis(25));
    rig.kill_postgres();
    let replies: Vec<Result<Finished, Error>> =
        rig.rt.block_on(async { futures_join(handles).await });
    rig.start_postgres();

    for (c, first) in commands.iter().zip(&replies) {
        let mut tries = 0;
        let second = loop {
            match rig.run(c) {
                Ok(f) => break f,
                Err(Error::Overloaded { .. }) if tries < 40 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("retry of {}: {e}", c.request_id.as_str()),
            }
        };
        done(&second);
        if first.is_ok() {
            assert!(
                second.outcome.replayed,
                "{} was answered before the crash, so it must still be there",
                c.request_id.as_str()
            );
        }
    }
    assert_eq!(rig.balance("alice"), 970);
    assert_eq!(rig.count("SELECT count(*) FROM app.entries"), 30);
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests"),
        30
    );
    assert_eq!(rig.changes().len(), 60);
}

/// Awaits every handle; a task that died counts as an `internal` reply.
async fn futures_join(
    handles: Vec<tokio::task::JoinHandle<Result<Finished, Error>>>,
) -> Vec<Result<Finished, Error>> {
    let mut out = Vec::new();
    for h in handles {
        out.push(
            h.await
                .unwrap_or_else(|e| Err(Error::Internal(e.to_string()))),
        );
    }
    out
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn a_failed_check_is_a_stored_named_error_that_a_retry_repeats() {
    // Test 5, with a recipe-defined error name.
    let rig = Rig::new(54_416);
    let claim = |req: &str| rig.cmd("daily", req, "alice", None, &[("day", Value::I64(5))]);
    let f = rig.run(&claim("d1")).expect("first");
    assert_eq!(done(&f), r#"{"values":{"balance":100}}"#);
    let entries_before = rig.changes().len();

    let f = rig.run(&claim("d2")).expect("second day-5 claim");
    assert_eq!(failed(&f), "already_claimed");
    assert_eq!(f.outcome.result, r#"{"error":"already_claimed"}"#);
    assert_eq!(
        Error::from_recipe_name(failed(&f)),
        Error::RecipeDefined("already_claimed".into())
    );
    // The retry returns the same named error, replayed.
    let again = rig.run(&claim("d2")).expect("retry");
    assert_eq!(failed(&again), "already_claimed");
    assert!(again.outcome.replayed);
    assert_eq!(again.outcome.result, f.outcome.result);
    // Data unchanged, no entry, and the record says failed.
    assert_eq!(rig.balance("alice"), 100);
    assert_eq!(rig.changes().len(), entries_before);
    assert_eq!(
        rig.sql("SELECT status || ' ' || error_kind FROM bhdata_internal.requests WHERE request_id = 'd2'"),
        "failed already_claimed"
    );
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn a_retry_after_the_window_is_expired_with_the_record_present_and_with_it_gone() {
    // Test 6.
    let rig = Rig::new(54_417);
    rig.fund("alice", 500);
    let c = rig.spend("old", "alice", 5);
    done(&rig.run(&c).expect("first"));

    rig.clock.advance(DAY + Duration::from_secs(1));
    assert_eq!(rig.run(&c).unwrap_err(), Error::RequestExpired);
    // Still inside the window, a retry is a replay.
    let fresh = rig.spend("new", "alice", 5);
    done(&rig.run(&fresh).expect("fresh"));
    rig.clock.advance(HOUR);
    done(&rig.run(&fresh).expect("replay inside the window"));

    // The same answer once the record is gone.
    rig.sql("DELETE FROM bhdata_internal.requests WHERE request_id = 'old'");
    assert_eq!(rig.run(&c).unwrap_err(), Error::RequestExpired);
    assert_eq!(rig.balance("alice"), 490);

    // A creation time too far ahead of the clock is a bad argument; a little ahead is fine.
    let mut ahead = rig.spend("ahead", "alice", 1);
    ahead.requested_at = rig.clock.now().saturating_add(Duration::from_secs(600));
    assert!(matches!(
        rig.run(&ahead).unwrap_err(),
        Error::BadArguments(_)
    ));
    ahead.requested_at = rig.clock.now().saturating_add(Duration::from_secs(60));
    done(&rig.run(&ahead).expect("within the tolerance"));
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn after_compaction_a_retry_is_expired_and_the_history_is_never_read() {
    // Test 7.
    let rig = Rig::new(54_418);
    rig.fund("alice", 500);
    let c = rig.spend("gone", "alice", 5);
    done(&rig.run(&c).expect("first"));
    rig.clock.advance(DAY + HOUR + Duration::from_secs(5));
    let cutoff = compaction_cutoff(rig.clock.now(), DAY, HOUR);
    let moved = rig.rt.block_on(async {
        let a = rig.pools.admin().await.unwrap();
        history::compact(&a, cutoff, 100).await.unwrap()
    });
    assert_eq!(moved, 1);
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests"),
        0
    );
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.request_history"),
        1
    );
    assert_eq!(rig.run(&c).unwrap_err(), Error::RequestExpired);
    // A new request with the old id and a current time is a new request, as the spec says: the
    // expiry rule is the clock's, and the history has no say.
    let again = rig.spend("gone", "alice", 5);
    assert!(!rig.run(&again).expect("a new request").outcome.replayed);
    assert_eq!(rig.balance("alice"), 490);
    // The history never holds a hash or a result.
    assert!(psql(
        &rig.spec,
        None,
        "SELECT payload_hash FROM bhdata_internal.request_history"
    )
    .is_err());
    assert!(psql(
        &rig.spec,
        None,
        "SELECT result FROM bhdata_internal.request_history"
    )
    .is_err());
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn a_failed_check_after_an_ensure_leaves_no_subject_row() {
    // Test 9.
    let rig = Rig::new(54_419);
    let f = rig.run(&rig.spend("ghost", "nobody", 5)).expect("runs");
    assert_eq!(failed(&f), "insufficient_funds");
    assert_eq!(rig.count("SELECT count(*) FROM app.users"), 0);
    assert_eq!(rig.count("SELECT count(*) FROM app.entries"), 0);
    assert!(rig.changes().is_empty());
    assert_eq!(
        rig.sql("SELECT status FROM bhdata_internal.requests WHERE request_id = 'ghost'"),
        "failed"
    );
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn give_both_ways_at_once_finishes_and_deadlocks_are_retried() {
    // Test 10.
    let rig = Rig::new(54_420);
    rig.fund("a", 1_000);
    rig.fund("b", 1_000);
    for round in 0..60 {
        let ab = rig.cmd(
            "give",
            &format!("ab{round}"),
            "a",
            Some("b"),
            &[("amount", Value::I64(1))],
        );
        let ba = rig.cmd(
            "give",
            &format!("ba{round}"),
            "b",
            Some("a"),
            &[("amount", Value::I64(1))],
        );
        let (x, y) = (rig.spawn(ab), rig.spawn(ba));
        let (x, y) = rig
            .rt
            .block_on(async { (x.await.unwrap(), y.await.unwrap()) });
        done(&x.expect("a to b"));
        done(&y.expect("b to a"));
    }
    assert_eq!((rig.balance("a"), rig.balance("b")), (1_000, 1_000));
    assert_eq!(rig.count("SELECT count(*) FROM app.entries"), 120);
    let deadlocks = rig.count("SELECT deadlocks FROM pg_stat_database WHERE datname = 'bhdata'");
    println!("deadlocks found and retried: {deadlocks}");
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn the_reader_never_loses_an_entry_and_stops_at_the_horizon() {
    // Test 11: A takes the lower transaction id and commits last; B commits first.
    let rig = Rig::new(54_421);
    rig.fund("alice", 500);
    rig.rt.block_on(async {
        let mut a = rig.pools.recipe().await.unwrap();
        let tx = a.transaction().await.unwrap();
        let claim = "SELECT * FROM bhdata_internal.claim_request('l','held','\\x01','spend',1,'alice',now(),false)";
        tx.query_one(claim, &[]).await.unwrap();
        tx.execute(
            "SELECT bhdata_internal.append_change('l','held','alice','users','update','alice')",
            &[],
        )
        .await
        .unwrap();

        // B commits while A is open: both entries of B are above the horizon.
        let f = rig.exec.run(&rig.spend("b", "alice", 3)).await.expect("b");
        done(&f);
        let c = rig.pools.admin().await.unwrap();
        let seen = read_after(&c, Position::START, 100).await.unwrap();
        assert!(seen.is_empty(), "nothing is visible above the horizon: {seen:?}");

        tx.execute(
            "SELECT bhdata_internal.finish_request('l','held','done',NULL,'\\x7b7d')",
            &[],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // Now A's entry comes first although B committed first, and nothing is missing.
        let seen = read_after(&c, Position::START, 100).await.unwrap();
        let rows: Vec<(&str, &str)> = seen
            .iter()
            .map(|l| (l.entry.table.as_str(), l.entry.row_key.as_str()))
            .collect();
        assert_eq!(rows, [("users", "alice"), ("users", "alice"), ("entries", "alice/1")]);
        assert!(seen.windows(2).all(|w| w[0].position < w[1].position));
        assert!(seen[0].position.xid < seen[1].position.xid);

        // Resuming from a position gives the rest, and a batch limit is respected.
        let rest = read_after(&c, seen[0].position, 100).await.unwrap();
        assert_eq!(rest.len(), 2);
        let one = read_after(&c, Position::START, 1).await.unwrap();
        assert_eq!(one.len(), 1);

        // Trim: only what every reader has passed, and only what is old enough.
        let before_all = Timestamp::from_unix_millis(1);
        assert_eq!(
            bhdata_changelog::trim(&c, seen[2].position, before_all, 100).await.unwrap(),
            0
        );
        let far = far_future();
        assert_eq!(bhdata_changelog::trim(&c, seen[1].position, far, 100).await.unwrap(), 2);
        let left = read_after(&c, Position::START, 100).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].position, seen[2].position);
    });
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn commits_in_the_opposite_order_of_their_ids_are_read_by_id_and_the_row_converges() {
    // Test 12: B takes the lower id, A updates the row and commits first, B updates it after.
    let rig = Rig::new(54_422);
    rig.fund("x", 0);
    rig.rt.block_on(async {
        let claim = |id: &str| {
            format!("SELECT * FROM bhdata_internal.claim_request('l','{id}','\\x01','spend',1,'x',now(),false)")
        };
        let mut b = rig.pools.recipe().await.unwrap();
        let mut a = rig.pools.recipe().await.unwrap();
        let tb = b.transaction().await.unwrap();
        tb.query_one(&claim("b"), &[]).await.unwrap(); // B's id is the lower one
        let ta = a.transaction().await.unwrap();
        ta.query_one(&claim("a"), &[]).await.unwrap();
        ta.execute("UPDATE app.users SET balance = balance + 1 WHERE id = 'x'", &[]).await.unwrap();
        ta.execute("SELECT bhdata_internal.append_change('l','a','x','users','update','x')", &[]).await.unwrap();
        ta.execute("SELECT bhdata_internal.finish_request('l','a','done',NULL,'\\x7b7d')", &[]).await.unwrap();
        ta.commit().await.unwrap(); // A commits first

        tb.execute("UPDATE app.users SET balance = balance + 10 WHERE id = 'x'", &[]).await.unwrap();
        tb.execute("SELECT bhdata_internal.append_change('l','b','x','users','update','x')", &[]).await.unwrap();
        tb.execute("SELECT bhdata_internal.finish_request('l','b','done',NULL,'\\x7b7d')", &[]).await.unwrap();
        tb.commit().await.unwrap();

        let c = rig.pools.admin().await.unwrap();
        let seen = read_after(&c, Position::START, 100).await.unwrap();
        // Order is by transaction id: B's entry first although A committed first.
        assert_eq!(seen.len(), 2);
        assert!(seen[0].position.xid < seen[1].position.xid);
    });
    // A consumer that re-reads the row converges, whatever the order of the entries.
    assert_eq!(rig.balance("x"), 11);
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn the_recipe_role_cannot_delete_rewrite_forge_or_call_the_admin_functions() {
    // Test 13.
    let rig = Rig::new(54_423);
    rig.fund("alice", 500);
    done(
        &rig.run(&rig.spend("fin", "alice", 1))
            .expect("a finished request"),
    );
    rig.rt.block_on(async {
        let c = rig.pools.recipe().await.unwrap();
        let refused = |what: &str, kind: PgErrorKind, e: tokio_postgres::Error| {
            assert_eq!(pg_error(&e).kind, kind, "{what}: {e}");
        };
        for sql in [
            "DELETE FROM bhdata_internal.requests",
            "UPDATE bhdata_internal.requests SET status = 'running'",
            "SELECT * FROM bhdata_internal.requests",
            "DELETE FROM bhdata_internal.change_log",
            "SELECT * FROM bhdata_internal.request_history",
            "DELETE FROM app.users",
            "UPDATE app.users SET name = 'x'",
            "SELECT bhdata_internal.compact_requests(now(), 10)",
            "SELECT bhdata_internal.purge_history(now(), 10, false)",
            "SELECT * FROM bhdata_internal.read_changes('0', 0, 10)",
            "SELECT bhdata_internal.trim_change_log('0', 0, now(), 10)",
        ] {
            let Err(e) = c.batch_execute(sql).await else {
                panic!("the recipe role was allowed: {sql}");
            };
            refused(sql, PgErrorKind::Forbidden, e);
        }
        // A forged change for a finished request, and a second finish, are refused by our own code.
        for sql in [
            "SELECT bhdata_internal.append_change('l','fin','alice','users','update','alice')",
            "SELECT bhdata_internal.finish_request('l','fin','done',NULL,'\\x00')",
        ] {
            let e = c.batch_execute(sql).await.unwrap_err();
            refused(sql, PgErrorKind::Refused, e);
        }
        drop(c);
        // The reader and the SQL role cannot call the admin functions; the admin can, and cannot
        // read the tables.
        let r = rig.pools.reader().await.unwrap();
        let e = r
            .batch_execute("SELECT bhdata_internal.compact_requests(now(), 10)")
            .await
            .unwrap_err();
        refused("reader", PgErrorKind::Forbidden, e);
        let a = rig.pools.admin().await.unwrap();
        a.batch_execute("SELECT bhdata_internal.compact_requests(now() - interval '1 year', 10)")
            .await
            .expect("the admin may compact");
        let e = a
            .batch_execute("SELECT * FROM bhdata_internal.change_log")
            .await
            .unwrap_err();
        refused("admin on the table", PgErrorKind::Forbidden, e);
    });
    // Nothing above changed anything.
    assert_eq!(rig.balance("alice"), 499);
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests"),
        1
    );
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn compaction_under_load_loses_and_duplicates_no_record_and_never_moves_a_running_one() {
    // Test 17, part one.
    let rig = Rig::new(54_424);
    rig.fund("alice", 10_000);
    for i in 0..40 {
        done(
            &rig.run(&rig.spend(&format!("old{i}"), "alice", 1))
                .expect("old"),
        );
    }
    let mut permanent = rig.spend("forever", "alice", 1);
    permanent.retention = Retention::Permanent;
    done(&rig.run(&permanent).expect("permanent"));
    // A committed `running` record cannot come from the writer; plant one the way a bug would.
    rig.sql(
        "INSERT INTO bhdata_internal.requests (login_id, request_id, payload_hash, status, subject, requested_at, permanent) \
         VALUES ('l', 'stuck', '\\x00', 'running', 's', 'epoch'::timestamptz + 1000 * interval '1 millisecond', false)",
    );
    rig.clock.advance(DAY + HOUR + Duration::from_secs(60));

    let cutoff = compaction_cutoff(rig.clock.now(), DAY, HOUR);
    let new: Vec<_> = (0..30)
        .map(|i| rig.spawn_retrying(rig.spend(&format!("new{i}"), "alice", 1)))
        .collect();
    let moved = rig.rt.block_on(async {
        let a = rig.pools.admin().await.unwrap();
        let mut moved = 0;
        loop {
            let n = history::compact(&a, cutoff, 7).await.unwrap();
            moved += n;
            if n < 7 {
                break moved;
            }
        }
    });
    for h in new {
        done(&rig.rt.block_on(h).unwrap().expect("new"));
    }
    assert_eq!(moved, 40);
    // Nothing lost, nothing twice: every finished old record is in the history exactly once.
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.request_history"),
        40
    );
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests WHERE request_id LIKE 'old%'"),
        0
    );
    // The running one and the permanent one stayed; so did the 30 new ones.
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests"),
        32
    );
    assert_eq!(
        rig.sql("SELECT string_agg(request_id, ',' ORDER BY request_id) FROM bhdata_internal.requests WHERE NOT request_id LIKE 'new%'"),
        "forever,stuck"
    );
    // History rows keep the finish time and status of the originals.
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.request_history WHERE status = 'done' AND finished_at IS NOT NULL"),
        40
    );
    // Purging the history removes it, in batches.
    let gone = rig.rt.block_on(async {
        let a = rig.pools.admin().await.unwrap();
        let far = far_future();
        let mut gone = history::purge(&a, far, 25, false).await.unwrap();
        gone += history::purge(&a, far, 25, false).await.unwrap();
        gone
    });
    assert_eq!(gone, 40);
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn compaction_killed_in_the_middle_loses_and_duplicates_nothing() {
    // Test 17, part two.
    let rig = Rig::new(54_425);
    rig.fund("alice", 10_000);
    for i in 0..60 {
        done(
            &rig.run(&rig.spend(&format!("c{i}"), "alice", 1))
                .expect("old"),
        );
    }
    rig.clock.advance(DAY + HOUR + Duration::from_secs(60));
    let cutoff = compaction_cutoff(rig.clock.now(), DAY, HOUR);
    let job = rig.rt.spawn({
        let pools = rig.pools.clone();
        async move {
            loop {
                let a = pools.admin().await.map_err(|e| e.to_string())?;
                let n = history::compact(&a, cutoff, 3)
                    .await
                    .map_err(|e| e.to_string())?;
                if n == 0 {
                    return Ok::<(), String>(());
                }
            }
        }
    });
    std::thread::sleep(Duration::from_millis(40));
    rig.kill_postgres();
    let _ = rig.rt.block_on(job); // it fails, or it had already finished
    rig.start_postgres();

    // Whatever the moment, every record is in exactly one of the two tables.
    let requests = rig.count("SELECT count(*) FROM bhdata_internal.requests");
    let history_rows = rig.count("SELECT count(*) FROM bhdata_internal.request_history");
    assert_eq!(requests + history_rows, 60, "{requests} + {history_rows}");
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests r JOIN bhdata_internal.request_history h USING (login_id, request_id)"),
        0
    );
    // The job can simply be run again.
    let finished = rig.rt.block_on(async {
        let a = rig.pools.admin().await.unwrap();
        loop {
            if history::compact(&a, cutoff, 3).await.unwrap() == 0 {
                break;
            }
        }
        true
    });
    assert!(finished);
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.request_history"),
        60
    );
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests"),
        0
    );
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn bad_commands_are_refused_before_anything_is_stored() {
    // Test 18.
    let owns: bhdata_writer::exec::OwnsFn =
        Arc::new(|s: &SubjectId| !s.as_str().starts_with("far"));
    let rig = Rig::with_owner_check(54_426, Some(owns));
    rig.fund("alice", 500);

    // Out of bounds, missing, extra and wrongly typed arguments.
    for amount in [0, -1, 1_000_001] {
        let e = rig.run(&rig.spend("b", "alice", amount)).unwrap_err();
        assert!(
            matches!(&e, Error::BadArguments(m) if m.contains("amount")),
            "{amount}: {e}"
        );
    }
    let none = rig.cmd("spend", "b", "alice", None, &[]);
    assert!(matches!(
        rig.run(&none).unwrap_err(),
        Error::BadArguments(_)
    ));
    let extra = rig.cmd(
        "spend",
        "b",
        "alice",
        None,
        &[("amount", Value::I64(1)), ("more", Value::I64(1))],
    );
    assert!(matches!(
        rig.run(&extra).unwrap_err(),
        Error::BadArguments(_)
    ));
    let wrong = rig.cmd(
        "spend",
        "b",
        "alice",
        None,
        &[("amount", Value::Text("1".into()))],
    );
    assert!(matches!(
        rig.run(&wrong).unwrap_err(),
        Error::BadArguments(_)
    ));
    // A counterparty where the recipe has none, and none where it needs one.
    let cp = rig.cmd(
        "spend",
        "b",
        "alice",
        Some("bob"),
        &[("amount", Value::I64(1))],
    );
    assert!(matches!(rig.run(&cp).unwrap_err(), Error::BadArguments(_)));
    let no_cp = rig.cmd("give", "b", "alice", None, &[("amount", Value::I64(1))]);
    assert!(matches!(
        rig.run(&no_cp).unwrap_err(),
        Error::BadArguments(_)
    ));

    // A read is not a command.
    let read = rig.cmd("history", "b", "alice", None, &[]);
    assert!(matches!(rig.run(&read).unwrap_err(), Error::Internal(_)));

    // A subject, or a counterparty, on another shard.
    let far = rig.spend("b", "far-away", 1);
    assert_eq!(rig.run(&far).unwrap_err(), Error::WrongShard);
    let far_cp = rig.cmd(
        "give",
        "b",
        "alice",
        Some("far-bob"),
        &[("amount", Value::I64(1))],
    );
    assert_eq!(rig.run(&far_cp).unwrap_err(), Error::WrongShard);

    // None of it stored anything.
    assert_eq!(
        rig.count("SELECT count(*) FROM bhdata_internal.requests"),
        0
    );
    assert_eq!(rig.count("SELECT count(*) FROM app.entries"), 0);
    assert_eq!(rig.balance("alice"), 500);
    assert!(rig.changes().is_empty());
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn decimals_and_timestamps_travel_exactly() {
    // The `decimal` path of the plan: a rate with six digits, half-even rounding to two.
    let rig = Rig::new(54_427);
    rig.sql(
        "INSERT INTO app.users (id, balance, interest, last_claim) VALUES ('alice', 0, 100.00, 0)",
    );
    let rate = |v: i128| Value::Dec { value: v, scale: 6 };
    // 100.00 * 0.015000 = 1.50
    let c = rig.cmd(
        "add_interest",
        "i1",
        "alice",
        None,
        &[("rate", rate(15_000))],
    );
    let f = rig.run(&c).expect("runs");
    assert!(done(&f).contains("interest"));
    assert_eq!(
        rig.sql("SELECT interest FROM app.users WHERE id = 'alice'"),
        "101.50"
    );
    // The same rate written differently is the same request (the hash is of the typed value).
    let again = rig.cmd(
        "add_interest",
        "i1",
        "alice",
        None,
        &[("rate", rate(15_000))],
    );
    assert!(rig.run(&again).expect("replay").outcome.replayed);
    // 101.50 * 0.000050 = 0.005075 -> 0.01 (half-even, not a tie)
    let c = rig.cmd("add_interest", "i2", "alice", None, &[("rate", rate(50))]);
    done(&rig.run(&c).expect("runs"));
    assert_eq!(
        rig.sql("SELECT interest FROM app.users WHERE id = 'alice'"),
        "101.51"
    );
    // The timestamp of an entry is the clock's, to the microsecond.
    rig.sql("UPDATE app.users SET balance = 50 WHERE id = 'alice'");
    done(&rig.run(&rig.spend("s1", "alice", 1)).expect("spend"));
    assert_eq!(
        rig.count("SELECT (extract(epoch FROM at) * 1000000)::bigint FROM app.entries"),
        i64::try_from(T0).unwrap() * 1_000
    );
}
