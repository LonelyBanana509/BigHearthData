//! The Rust client, the gateway, the thin handler and the real write path against a real PostgreSQL
//! (wire-protocol spec, section 9: W1, W4 and the stored-replay half of W6 with nothing faked).
//! Ignored by default; it needs the server installed and an ordinary user (PostgreSQL refuses to
//! run as root), as the other real-server suites do:
//!
//! ```text
//! bash claude/pg-tests.sh -p bhdata-server pg_wire
//! ```

#![allow(clippy::unwrap_used)]
// The handler is the server binary's own module; the test builds the same handler the server does.
#[path = "../src/handler.rs"]
#[allow(dead_code)]
mod handler;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bhdata_api::types::config::PoolConfig;
use bhdata_api::types::Durability;
use bhdata_api::types::{Clock, Config, SystemClock};
use bhdata_client::client::{Call, Client, ClientConfig, ClientError, Security};
use bhdata_control::logins::{self, Change, Permission, PgLoginStore};
use bhdata_control::{blocking, Target};
use bhdata_engine::instance::{self, InstanceSpec};
use bhdata_engine::supervise;
use bhdata_gateway::listener::Gateway;
use bhdata_gateway::session::Options;
use bhdata_pg::bootstrap::bootstrap_blocking;
use bhdata_pg::pool::Pools;
use bhdata_proto::json::Value;
use bhdata_proto::scram::ServerSecret;
use bhdata_proto::types::{ErrorKind, RequestId, SubjectId};
use bhdata_recipe::Limits;
use bhdata_writer::exec::{ExecConfig, Executor};
use bhdata_writer::queue::WriteQueue;

use handler::{PgPlans, ThinHandler};

struct Instance {
    spec: InstanceSpec,
    base: PathBuf,
}

impl Instance {
    fn start() -> Instance {
        let bin = std::env::var("BHDATA_PG_BIN").expect("set BHDATA_PG_BIN");
        let base = std::env::temp_dir().join(format!("bhwire-{}", std::process::id()));
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

fn call(recipe: &str, subject: &str, arg: (&str, i64), id: Option<&str>) -> Call {
    Call {
        recipe: recipe.to_string(),
        version: None,
        subject: SubjectId::new(subject).unwrap(),
        counterparty: None,
        args: BTreeMap::from([(arg.0.to_string(), Value::Int(arg.1))]),
        request_id: id.map(|i| RequestId::new(i).unwrap()),
    }
}

fn client_config(addr: std::net::SocketAddr, login: &str) -> ClientConfig {
    let mut c = ClientConfig::new(addr, login, "pw", Security::PlainLoopback);
    c.max_retry = Duration::from_secs(10);
    c
}

#[test]
#[ignore = "needs a real PostgreSQL and a non-root user"]
fn a_client_runs_registered_recipes_through_the_gateway_and_the_real_write_path() {
    let pg = Instance::start();
    let target = pg.target();
    bootstrap_blocking(&target, &pg.spec.superuser).expect("bootstrap");

    // The registry: a schema, two recipes, and two logins.
    blocking::apply_schema(&target, &data("schema.toml"), "test").expect("schema");
    for name in ["daily", "spend"] {
        blocking::register_recipe(
            &target,
            &data(&format!("{name}.toml")),
            &Limits::default(),
            "test",
        )
        .expect("register");
    }
    let by = "test";
    for login in ["alice", "limited"] {
        logins::blocking::create(&target, login, b"pw", 4, by).expect("login");
        logins::blocking::change(
            &target,
            login,
            &Change::Grant(Permission::parse("execute").unwrap()),
            by,
        )
        .expect("execute");
    }
    logins::blocking::change(&target, "alice", &Change::GrantAllRecipes, by).expect("all recipes");
    logins::blocking::change(
        &target,
        "limited",
        &Change::GrantRecipes(vec!["daily".into()]),
        by,
    )
    .expect("one recipe");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let config = Config::default();
        let sizes = PoolConfig {
            recipe_writers: 8,
            readers: 1,
            sql_sessions: 1,
            admin: 4,
            reserved_backup: 0,
        };
        let pools = Arc::new(Pools::connect(&target, &sizes).await.expect("pools"));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
        let executor = Executor::new(
            pools.clone(),
            clock.clone(),
            ExecConfig::from_config(&config),
        );
        let queue = WriteQueue::start(executor, clock.clone(), &config.writer, 8);
        let handler = ThinHandler::new(PgPlans::new(pools.clone(), clock.clone()), queue.clone());
        let mut gateway_config = config.gateway.clone();
        gateway_config.listen = "127.0.0.1:0".parse().unwrap();
        gateway_config.insecure_local = true;
        gateway_config.scram_iterations = 4;
        let gateway = Gateway::start(
            gateway_config,
            Arc::new(handler),
            Arc::new(PgLoginStore::new(pools.clone())),
            ServerSecret::new([5; 32]),
            Options::default(),
        )
        .await
        .expect("gateway");
        let addr = gateway.local_addr();

        let alice = Client::connect(client_config(addr, "alice"))
            .await
            .expect("login");

        // A new subject claims its daily reward.
        let first = alice
            .execute(call("daily", "u1", ("day", 1), Some("claim-day-1")))
            .await
            .expect("daily");
        assert!(!first.replayed);
        let first_text = format!("{:?}", first.result);
        assert!(
            first_text.contains("100"),
            "balance 100 expected: {first_text}"
        );

        // The same id again, as a retry after a lost reply would send it: the stored answer comes
        // back, marked as a replay, and nothing is applied twice.
        let again = alice
            .execute(call("daily", "u1", ("day", 1), Some("claim-day-1")))
            .await
            .expect("replay");
        assert!(again.replayed);
        assert_eq!(again.result, first.result);

        // A second claim on the same day, under a new id, is the recipe's own named error.
        let e = alice
            .execute(call("daily", "u1", ("day", 1), None))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), Some(ErrorKind::RecipeDefined));
        assert_eq!(e.wire().unwrap().name, "already_claimed");

        // Spend: a success, then more than the balance (a stored failure, final and replayed).
        let spent = alice
            .execute(call("spend", "u1", ("amount", 30), None))
            .await
            .expect("spend");
        let spent_text = format!("{:?}", spent.result);
        assert!(
            spent_text.contains("70"),
            "balance 70 expected: {spent_text}"
        );
        let broke = alice
            .execute(call("spend", "u1", ("amount", 1_000), Some("too-much")))
            .await
            .unwrap_err();
        assert_eq!(broke.kind(), Some(ErrorKind::InsufficientFunds));
        let broke_again = alice
            .execute(call("spend", "u1", ("amount", 1_000), Some("too-much")))
            .await
            .unwrap_err();
        assert_eq!(broke_again.kind(), Some(ErrorKind::InsufficientFunds));

        // W4 with the real registry: bad arguments by the plan's types, an unknown recipe for a login
        // with `*`, and an unregistered name for a restricted login.
        let bad = alice
            .execute(Call {
                args: BTreeMap::from([("amount".to_string(), Value::str("30"))]),
                ..call("spend", "u1", ("amount", 0), None)
            })
            .await
            .unwrap_err();
        assert_eq!(bad.kind(), Some(ErrorKind::BadArguments));
        assert!(bad.wire().unwrap().message.contains("`amount`"));
        let nope = alice
            .execute(call("no_such_recipe", "u1", ("n", 1), None))
            .await
            .unwrap_err();
        assert_eq!(nope.kind(), Some(ErrorKind::UnknownRecipe));

        let limited = Client::connect(client_config(addr, "limited"))
            .await
            .expect("login");
        limited
            .execute(call("daily", "u2", ("day", 1), None))
            .await
            .expect("allowed recipe");
        let denied = limited
            .execute(call("spend", "u2", ("amount", 1), None))
            .await
            .unwrap_err();
        assert_eq!(denied.kind(), Some(ErrorKind::Unauthorized));

        // A wrong password is a login error, not retried.
        let wrong = Client::connect(ClientConfig {
            password: b"nope".to_vec(),
            ..client_config(addr, "alice")
        })
        .await
        .unwrap_err();
        assert!(matches!(wrong, ClientError::Login(_)), "{wrong:?}");

        gateway.shutdown().await;
        queue.shutdown(Duration::from_secs(5)).await;
        pools.close();
    });
}
