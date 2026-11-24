//! The Rust client against the real gateway on real sockets (wire-protocol spec, section 9: W1, W4,
//! W6, W7, W11 and W14, and the client's rule that it never retries what it must not). The handler
//! is a fake that records what it was sent, so no PostgreSQL is needed; the same tests with the real
//! write path are in `tests/pg_wire.rs` (`--ignored`).

// The helpers below are test code outside `#[test]` functions, where `allow-unwrap-in-tests` does not reach.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bhdata_api::handler::{Call, Caller, Reply, RequestHandler};
use bhdata_api::login::{
    AuditEvent, LoginRecord, LoginStore, Permissions, RecipeAllow, ScramVerifier,
};
use bhdata_api::recipe::canon::Json;
use bhdata_api::shard::{Outcome, Status};
use bhdata_api::types::config::GatewayConfig;
use bhdata_api::types::{Clock, Error, ErrorKind, LoginId, ManualClock, SubjectId, Timestamp};
use bhdata_client::client::{
    Call as ClientCall, Client, ClientConfig, ClientError, KeyPin, Security,
};
use bhdata_gateway::listener::Gateway;
use bhdata_gateway::session::Options;
use bhdata_proto::json::Value;
use bhdata_proto::scram::{ServerSecret, Verifier};
use tokio::io::copy;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

// ------------------------------------------------------------------------------------------------
// Fakes
// ------------------------------------------------------------------------------------------------

#[derive(Default)]
struct MemStore {
    logins: Mutex<HashMap<String, LoginRecord>>,
    audit: Mutex<Vec<AuditEvent>>,
}

impl LoginStore for MemStore {
    async fn find(&self, name: &str) -> Result<Option<LoginRecord>, Error> {
        Ok(self.logins.lock().unwrap().get(name).cloned())
    }

    async fn audit(&self, event: AuditEvent) -> Result<(), Error> {
        self.audit.lock().unwrap().push(event);
        Ok(())
    }
}

fn record(name: &str, password: &[u8], execute: bool, recipes: RecipeAllow) -> LoginRecord {
    let v = Verifier::derive(password, vec![7; 16], 4).unwrap();
    LoginRecord {
        login_id: LoginId::new(name).unwrap(),
        verifier: ScramVerifier {
            salt: v.salt.clone(),
            iterations: v.iterations,
            stored_key: v.stored_key,
            server_key: v.server_key,
        },
        permissions: Permissions {
            execute,
            ..Permissions::default()
        },
        recipes,
        rate_per_sec: None,
        rate_burst: None,
        max_connections: None,
        disabled: false,
    }
}

/// One attempt as the handler saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    recipe: String,
    request_id: String,
    created_ms: u64,
}

/// `echo` returns `args.n`; `flaky` answers `overloaded` while `flaky_left` is above zero; `once`
/// applies a request at most once per `(login, request id)`, tells the proxy to cut the connection,
/// and answers late; `hang` never answers in time; `broke` is a stored failure.
#[derive(Default)]
struct Handler {
    seen: Mutex<Vec<Seen>>,
    applied: Mutex<HashMap<(String, String), ()>>,
    applications: AtomicUsize,
    flaky_left: AtomicUsize,
    cut: Arc<Notify>,
}

fn done(result: String, replayed: bool) -> Result<Reply, Error> {
    Ok(Outcome {
        status: Status::Done,
        result,
        replayed,
    })
}

impl RequestHandler for Handler {
    async fn handle(&self, caller: Caller, call: Call) -> Result<Reply, Error> {
        self.seen.lock().unwrap().push(Seen {
            recipe: call.recipe.clone(),
            request_id: call.request_id.as_str().to_string(),
            created_ms: call.created_at.as_unix_millis(),
        });
        let n = match &call.args {
            Json::Obj(map) => match map.get("n") {
                Some(Json::Int(n)) => *n,
                _ => 0,
            },
            _ => 0,
        };
        match call.recipe.as_str() {
            "flaky" => {
                let left = self.flaky_left.load(Ordering::SeqCst);
                if left > 0 {
                    self.flaky_left.store(left - 1, Ordering::SeqCst);
                    return Err(Error::Overloaded {
                        retry_after: Duration::from_millis(100),
                    });
                }
                done("{\"ok\":true}".to_string(), false)
            }
            "once" => {
                let key = (
                    caller.login_id.as_str().to_string(),
                    call.request_id.as_str().to_string(),
                );
                let first = self.applied.lock().unwrap().insert(key, ()).is_none();
                if first {
                    self.applications.fetch_add(1, Ordering::SeqCst);
                    self.cut.notify_one();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                done("{\"applied\":1}".to_string(), !first)
            }
            "hang" => {
                tokio::time::sleep(Duration::from_secs(10)).await;
                done("null".to_string(), false)
            }
            "broke" => Ok(Outcome {
                status: Status::Failed {
                    error: "insufficient_funds".to_string(),
                },
                result: "null".to_string(),
                replayed: false,
            }),
            "echo" => {
                tokio::time::sleep(Duration::from_millis(u64::try_from(n % 7).unwrap_or(0))).await;
                done(format!("{{\"n\":{n}}}"), false)
            }
            _ => Err(Error::UnknownRecipe),
        }
    }
}

// ------------------------------------------------------------------------------------------------
// The environment
// ------------------------------------------------------------------------------------------------

struct Env {
    _gateway: Gateway<Handler, MemStore>,
    handler: Arc<Handler>,
    store: Arc<MemStore>,
    addr: SocketAddr,
}

fn gateway_config() -> GatewayConfig {
    GatewayConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        insecure_local: true,
        scram_iterations: 4,
        handshake_timeout_secs: 5,
        ..GatewayConfig::default()
    }
}

fn real_now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// The gateway with a login `alice`/`pw` that may run everything, and one `limited`/`pw` that may
/// run only `echo`, and one `nobody`/`pw` that may not execute at all.
async fn start(config: GatewayConfig, clock: Arc<dyn Clock>) -> Env {
    let handler = Arc::new(Handler::default());
    let store = Arc::new(MemStore::default());
    {
        let mut logins = store.logins.lock().unwrap();
        for r in [
            record("alice", b"pw", true, RecipeAllow::All),
            record(
                "limited",
                b"pw",
                true,
                RecipeAllow::Names(["echo".to_string()].into_iter().collect()),
            ),
            record("nobody", b"pw", false, RecipeAllow::None),
        ] {
            logins.insert(r.login_id.as_str().to_string(), r);
        }
    }
    let options = Options {
        clock,
        server_name: "test-gateway".to_string(),
        on_incident: None,
    };
    let gateway = Gateway::start(
        config,
        Arc::clone(&handler),
        Arc::clone(&store),
        ServerSecret::new([9; 32]),
        options,
    )
    .await
    .unwrap();
    let addr = gateway.local_addr();
    Env {
        _gateway: gateway,
        handler,
        store,
        addr,
    }
}

async fn plain() -> Env {
    start(gateway_config(), real_clock()).await
}

fn real_clock() -> Arc<dyn Clock> {
    Arc::new(bhdata_api::types::SystemClock::new())
}

fn cfg(addr: SocketAddr, login: &str, password: &str) -> ClientConfig {
    let mut c = ClientConfig::new(addr, login, password, Security::PlainLoopback);
    c.max_retry = Duration::from_secs(10);
    c.call_timeout = Duration::from_secs(5);
    c
}

fn call(recipe: &str, n: i64) -> ClientCall {
    ClientCall {
        recipe: recipe.to_string(),
        version: None,
        subject: SubjectId::new("subject-1").unwrap(),
        counterparty: None,
        args: BTreeMap::from([("n".to_string(), Value::Int(n))]),
        request_id: None,
    }
}

// ------------------------------------------------------------------------------------------------
// TLS
// ------------------------------------------------------------------------------------------------

struct Pair {
    cert_der: Vec<u8>,
    cert_pem: String,
    key_pem: String,
}

fn make_pair() -> Pair {
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    Pair {
        cert_der: made.cert.der().to_vec(),
        cert_pem: made.cert.pem(),
        key_pem: made.key_pair.serialize_pem(),
    }
}

fn pin_of(pair: &Pair) -> KeyPin {
    KeyPin::of_certificate(&pair.cert_der).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bhdata-client-wire-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_tls(pair: &Pair, tag: &str) -> Env {
    let dir = temp_dir(tag);
    let (cert, key) = (dir.join("server.crt"), dir.join("server.key"));
    std::fs::write(&cert, &pair.cert_pem).unwrap();
    std::fs::write(&key, &pair.key_pem).unwrap();
    let config = GatewayConfig {
        insecure_local: false,
        cert_path: Some(cert),
        key_path: Some(key),
        ..gateway_config()
    };
    start(config, real_clock()).await
}

fn tls_cfg(env: &Env, pins: Vec<KeyPin>) -> ClientConfig {
    let mut c = cfg(env.addr, "alice", "pw");
    c.security = Security::Pinned(pins);
    c
}

#[tokio::test]
async fn w1_a_recipe_runs_over_tls_with_the_right_pin() {
    let pair = make_pair();
    let env = start_tls(&pair, "right").await;
    let client = Client::connect(tls_cfg(&env, vec![pin_of(&pair)]))
        .await
        .unwrap();
    let reply = client.execute(call("echo", 41)).await.unwrap();
    assert_eq!(
        reply.result,
        Value::obj([("n", Value::Int(41))]),
        "the result is the one the handler stored"
    );
    assert!(!reply.replayed);
}

#[tokio::test]
async fn w1_a_wrong_pin_is_refused_by_the_client_before_any_login() {
    let (pair, other) = (make_pair(), make_pair());
    let env = start_tls(&pair, "wrong").await;
    let e = Client::connect(tls_cfg(&env, vec![pin_of(&other)]))
        .await
        .unwrap_err();
    assert_eq!(e, ClientError::PinMismatch);
    // Not even a login attempt reached the server: nothing in the audit log, nothing for the handler.
    assert!(env.store.audit.lock().unwrap().is_empty());
    assert!(env.handler.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn two_pins_let_a_key_rotation_overlap() {
    let (pair, next) = (make_pair(), make_pair());
    let env = start_tls(&pair, "overlap").await;
    // The client already trusts the next key; the server still shows the old one.
    let client = Client::connect(tls_cfg(&env, vec![pin_of(&next), pin_of(&pair)]))
        .await
        .unwrap();
    client.execute(call("echo", 1)).await.unwrap();
}

#[tokio::test]
async fn a_client_with_no_pins_refuses_to_connect() {
    let pair = make_pair();
    let env = start_tls(&pair, "nopins").await;
    let e = Client::connect(tls_cfg(&env, Vec::new()))
        .await
        .unwrap_err();
    assert!(matches!(e, ClientError::Insecure(_)), "{e:?}");
}

// ------------------------------------------------------------------------------------------------
// Logins and permissions (W4)
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_wrong_password_and_an_unknown_login_are_the_same_refusal_and_are_not_retried() {
    let env = plain().await;
    let wrong = Client::connect(cfg(env.addr, "alice", "nope"))
        .await
        .unwrap_err();
    let unknown = Client::connect(cfg(env.addr, "ghost", "pw"))
        .await
        .unwrap_err();
    for e in [&wrong, &unknown] {
        assert!(matches!(e, ClientError::Login(_)), "{e:?}");
        assert_eq!(e.kind(), Some(ErrorKind::Unauthorized));
    }
    assert_eq!(
        wrong.wire().unwrap().message,
        unknown.wire().unwrap().message
    );
}

#[tokio::test]
async fn w4_permission_allow_list_and_unknown_recipe_answer_as_the_spec_says() {
    let env = plain().await;

    let nobody = Client::connect(cfg(env.addr, "nobody", "pw"))
        .await
        .unwrap();
    let e = nobody.execute(call("echo", 1)).await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::Unauthorized));

    let limited = Client::connect(cfg(env.addr, "limited", "pw"))
        .await
        .unwrap();
    limited.execute(call("echo", 1)).await.unwrap();
    // Outside the allow-list, and unknown: the same answer, so the list cannot be probed.
    for recipe in ["flaky", "no-such-recipe"] {
        let e = limited.execute(call(recipe, 1)).await.unwrap_err();
        assert_eq!(e.kind(), Some(ErrorKind::Unauthorized), "{recipe}");
    }

    let alice = Client::connect(cfg(env.addr, "alice", "pw")).await.unwrap();
    let e = alice.execute(call("no-such-recipe", 1)).await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::UnknownRecipe));

    // None of these were retried: each reached the handler at most once.
    let seen = env.handler.seen.lock().unwrap();
    let count = |r: &str| seen.iter().filter(|s| s.recipe == r).count();
    assert_eq!(count("no-such-recipe"), 1);
    assert_eq!(count("flaky"), 0, "refused at the permission check");
}

#[tokio::test]
async fn a_stored_failure_is_final_and_is_returned_with_the_recipes_own_name() {
    let env = plain().await;
    let client = Client::connect(cfg(env.addr, "alice", "pw")).await.unwrap();
    let e = client.execute(call("broke", 1)).await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::InsufficientFunds));
    assert_eq!(e.wire().unwrap().name, "insufficient_funds");
    assert_eq!(env.handler.seen.lock().unwrap().len(), 1, "never retried");
}

// ------------------------------------------------------------------------------------------------
// Retries (W11, W6)
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn w11_overloaded_is_waited_out_and_retried_with_the_same_id_and_time() {
    let env = plain().await;
    env.handler.flaky_left.store(2, Ordering::SeqCst);
    let client = Client::connect(cfg(env.addr, "alice", "pw")).await.unwrap();
    let started = std::time::Instant::now();
    let reply = client.execute(call("flaky", 1)).await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "two hints of 100 ms were waited: {:?}",
        started.elapsed()
    );
    let seen = env.handler.seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "two refusals and the answer");
    assert!(
        seen.iter().all(|s| s == &seen[0]),
        "the same request id and created_at_ms every time: {seen:?}"
    );
    assert_eq!(seen[0].request_id, reply.request_id.as_str());
}

#[tokio::test]
async fn the_client_gives_up_at_max_retry_and_returns_the_last_error() {
    let env = plain().await;
    env.handler.flaky_left.store(1_000, Ordering::SeqCst);
    let mut c = cfg(env.addr, "alice", "pw");
    c.max_retry = Duration::from_millis(600);
    let client = Client::connect(c).await.unwrap();
    let e = client.execute(call("flaky", 1)).await.unwrap_err();
    assert_eq!(e.kind(), Some(ErrorKind::Overloaded));
}

/// A TCP proxy to `upstream` whose first connection is cut when `cut` is notified: a reply lost on
/// the way back, after the server did the work.
async fn proxy(upstream: SocketAddr, cut: Arc<Notify>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut first = true;
        while let Ok((client, _)) = listener.accept().await {
            let cut = Arc::clone(&cut);
            let watch = first;
            first = false;
            tokio::spawn(async move {
                let Ok(server) = TcpStream::connect(upstream).await else {
                    return;
                };
                let (mut cr, mut cw) = client.into_split();
                let (mut sr, mut sw) = server.into_split();
                let up = copy(&mut cr, &mut sw);
                let down = copy(&mut sr, &mut cw);
                if watch {
                    tokio::select! {
                        _ = up => {},
                        _ = down => {},
                        () = cut.notified() => {},
                    }
                } else {
                    tokio::select! { _ = up => {}, _ = down => {} }
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn w6_a_connection_cut_after_the_work_is_retried_and_applies_once() {
    let env = plain().await;
    let via = proxy(env.addr, Arc::clone(&env.handler.cut)).await;
    let client = Client::connect(cfg(via, "alice", "pw")).await.unwrap();
    let reply = client.execute(call("once", 1)).await.unwrap();
    assert!(
        reply.replayed,
        "the second attempt found the first one's work: {reply:?}"
    );
    assert_eq!(
        env.handler.applications.load(Ordering::SeqCst),
        1,
        "applied exactly once"
    );
    let seen = env.handler.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], seen[1], "same id and created_at_ms on the retry");
}

#[tokio::test]
async fn a_request_with_no_answer_ends_as_outcome_unknown_with_the_id_to_send_again() {
    let env = plain().await;
    let mut c = cfg(env.addr, "alice", "pw");
    c.call_timeout = Duration::from_millis(150);
    c.max_retry = Duration::from_millis(900);
    let client = Client::connect(c).await.unwrap();
    let mut hang = call("hang", 1);
    hang.request_id = Some(bhdata_proto::types::RequestId::new("my-own-id").unwrap());
    let e = client.execute(hang).await.unwrap_err();
    assert_eq!(
        e,
        ClientError::OutcomeUnknown {
            request_id: bhdata_proto::types::RequestId::new("my-own-id").unwrap()
        }
    );
    assert!(env.handler.seen.lock().unwrap().len() >= 2, "it did retry");
}

// ------------------------------------------------------------------------------------------------
// Many in flight (W7) and the clock (W14)
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn w7_many_requests_in_flight_each_get_their_own_answer() {
    let env = plain().await;
    let client = Arc::new(Client::connect(cfg(env.addr, "alice", "pw")).await.unwrap());
    let mut tasks = Vec::new();
    for n in 0..100_i64 {
        let client = Arc::clone(&client);
        tasks.push(tokio::spawn(async move {
            (n, client.execute(call("echo", n)).await.unwrap())
        }));
    }
    for t in tasks {
        let (n, reply) = t.await.unwrap();
        assert_eq!(
            reply.result,
            Value::obj([("n", Value::Int(n))]),
            "request {n}"
        );
    }
}

async fn created_with_server_clock_at(server_ms: u64) -> (u64, i64) {
    let clock = Arc::new(ManualClock::new(Timestamp::from_unix_millis(server_ms)));
    let env = start(gateway_config(), clock).await;
    let client = Client::connect(cfg(env.addr, "alice", "pw")).await.unwrap();
    client.execute(call("echo", 1)).await.unwrap();
    let created = env.handler.seen.lock().unwrap()[0].created_ms;
    (created, client.clock_offset_ms().await.unwrap())
}

#[tokio::test]
async fn w14_a_client_clock_an_hour_fast_or_slow_still_stamps_the_servers_time() {
    let hour = 3_600_000_u64;
    for server_ms in [real_now_ms() - hour, real_now_ms() + hour] {
        let (created, offset) = created_with_server_clock_at(server_ms).await;
        assert!(
            created.abs_diff(server_ms) < 5_000,
            "created_at_ms {created} should be the server's {server_ms}"
        );
        assert!(
            offset.unsigned_abs().abs_diff(hour) < 5_000,
            "the offset is about an hour: {offset}"
        );
    }
}
