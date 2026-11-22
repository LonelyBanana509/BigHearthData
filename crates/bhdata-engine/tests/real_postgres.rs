//! Runs the engine against a real PostgreSQL. Ignored by default, because it needs the server
//! installed and must run as an ordinary user (PostgreSQL refuses to run as root):
//!
//! ```text
//! BHDATA_PG_BIN=/usr/lib/postgresql/17/bin \
//!     cargo test -p bhdata-engine --test real_postgres -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bhdata_engine::instance::{self, InitOutcome, InstanceSpec};
use bhdata_engine::supervise::{self, Health, PgOps, StartOutcome, State, SuperviseConfig};
use bhdata_types::Durability;

fn spec(base: PathBuf) -> InstanceSpec {
    let bin =
        std::env::var("BHDATA_PG_BIN").expect("set BHDATA_PG_BIN to the PostgreSQL bin folder");
    InstanceSpec {
        id: "shard0".into(),
        base_dir: base,
        pg_bin_dir: PathBuf::from(bin),
        superuser: instance::current_user().expect("user name"),
        port: 54_399,
        durability: Durability::Full,
        max_connections: 50,
        locale: "C.UTF-8".into(),
    }
}

fn psql(spec: &InstanceSpec, sql: &str) -> String {
    let out = Command::new(spec.bin("psql"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .arg("-h")
        .arg(spec.run_dir())
        .args(["-p", &spec.port.to_string(), "-U", &spec.superuser])
        .args(["-d", "postgres", "-tA", "-c", sql])
        .output()
        .expect("run psql");
    assert!(
        out.status.success(),
        "psql: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn a_real_instance_is_private_durable_and_comes_back_after_a_crash() {
    let base = std::env::temp_dir().join(format!("bhreal-{}", std::process::id()));
    let spec = spec(base.clone());

    assert_eq!(instance::init(&spec).unwrap(), InitOutcome::Created);
    assert_eq!(
        instance::init(&spec).unwrap(),
        InitOutcome::AlreadyInitialised
    );
    assert_eq!(supervise::start(&spec).unwrap(), StartOutcome::Started);
    assert_eq!(
        supervise::start(&spec).unwrap(),
        StartOutcome::AlreadyRunning
    );
    assert_eq!(supervise::health(&spec), Health::Ready);

    // The managed settings are the ones the server actually runs with.
    assert_eq!(psql(&spec, "show listen_addresses"), "");
    assert_eq!(psql(&spec, "show synchronous_commit"), "on");
    assert_eq!(psql(&spec, "show fsync"), "on");
    assert_eq!(psql(&spec, "show max_connections"), "50");
    assert_eq!(psql(&spec, "show data_checksums"), "on");
    assert_eq!(psql(&spec, "show unix_socket_permissions"), "0700");

    // A crash is noticed and repaired by the watcher.
    let cfg = SuperviseConfig {
        interval: Duration::from_millis(200),
        misses_before_restart: 3,
        backoff_base: Duration::from_millis(100),
        backoff_max: Duration::from_secs(2),
    };
    let sup = supervise::Supervisor::spawn(Arc::new(PgOps(spec.clone())), cfg);
    let pid = std::fs::read_to_string(spec.data_dir().join("postmaster.pid"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .trim()
        .to_string();
    assert!(Command::new("/bin/kill")
        .args(["-9", &pid])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let s = sup.status();
        if s.restarts >= 1 && s.state == State::Running {
            break;
        }
        assert!(Instant::now() < deadline, "not restarted: {s:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(psql(&spec, "select 1"), "1");
    sup.stop();

    supervise::restart(&spec).unwrap();
    assert_eq!(supervise::health(&spec), Health::Ready);
    supervise::stop(&spec).unwrap();
    assert_eq!(supervise::health(&spec), Health::Down);
    let _ = std::fs::remove_dir_all(base);
}
