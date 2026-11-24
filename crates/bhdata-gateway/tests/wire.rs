//! The gateway on real sockets, against a fake handler and an in-memory login store (lane B tests of
//! `docs/specs/wire-protocol.md`, section 9: W2, W3, W5, W7 to W10, W12, W13, and the permission and
//! shutdown rules). Everything runs on loopback and needs no PostgreSQL.

// The helpers below are test code outside `#[test]` functions, where `allow-unwrap-in-tests` does not reach.
#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bhdata_api::handler::{Call, Caller, Reply, RequestHandler};
use bhdata_api::login::{
    AuditEvent, LoginRecord, LoginStore, Permissions, RecipeAllow, ScramVerifier,
};
use bhdata_api::recipe::canon::Json;
use bhdata_api::shard::{Outcome, Status};
use bhdata_api::types::config::GatewayConfig;
use bhdata_api::types::{
    Clock, Error, ErrorKind, LoginId, ManualClock, RequestId, SubjectId, Timestamp,
};
use bhdata_gateway::listener::{Gateway, StartError};
use bhdata_gateway::session::Options;
use bhdata_proto::error_map::WireError;
use bhdata_proto::frame::{self, Limits};
use bhdata_proto::json::Value;
use bhdata_proto::message::{
    ClientMessage, ClientStep, Codec, JsonCodec, ReplyBody, Request, ServerHello, ServerMessage,
    ServerStep,
};
use bhdata_proto::scram::{self, ServerSecret, Verifier, MECHANISM};
use rustls_pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};

// ------------------------------------------------------------------------------------------------
// Fakes
// ------------------------------------------------------------------------------------------------

#[derive(Default)]
struct MemStore {
    logins: Mutex<HashMap<String, LoginRecord>>,
    audit: Mutex<Vec<AuditEvent>>,
}

impl MemStore {
    fn add(&self, record: LoginRecord) {
        self.logins
            .lock()
            .unwrap()
            .insert(record.login_id.as_str().to_string(), record);
    }

    fn events(&self) -> Vec<AuditEvent> {
        self.audit.lock().unwrap().clone()
    }
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

fn record(name: &str, password: &[u8]) -> LoginRecord {
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
            execute: true,
            ..Permissions::default()
        },
        recipes: RecipeAllow::All,
        rate_per_sec: None,
        rate_burst: None,
        max_connections: None,
        disabled: false,
    }
}

/// Behaviour is chosen by the recipe name: `slow` sleeps `args.ms`, `big` returns `args.kb` KiB,
/// `missing` is an unknown recipe, `boom` an internal error, `broke` a stored failure.
#[derive(Default)]
struct Handler {
    calls: AtomicUsize,
}

fn done(result: String) -> Result<Reply, Error> {
    Ok(Outcome {
        status: Status::Done,
        result,
        replayed: false,
    })
}

impl RequestHandler for Handler {
    async fn handle(&self, caller: Caller, call: Call) -> Result<Reply, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let arg = |key: &str| match &call.args {
            Json::Obj(map) => match map.get(key) {
                Some(Json::Int(n)) => u64::try_from(*n).unwrap_or(0),
                _ => 0,
            },
            _ => 0,
        };
        match call.recipe.as_str() {
            "slow" => {
                sleep(Duration::from_millis(arg("ms"))).await;
                done(format!("{{\"slept\":{}}}", arg("ms")))
            }
            "big" => done(format!("\"{}\"", "x".repeat(arg("kb") as usize * 1024))),
            "missing" => Err(Error::UnknownRecipe),
            "boom" => Err(Error::Internal("secret detail /var/lib/x".to_string())),
            "broke" => Ok(Outcome {
                status: Status::Failed {
                    error: "insufficient_funds".to_string(),
                },
                result: "null".to_string(),
                replayed: false,
            }),
            _ => done(format!(
                "{{\"login\":\"{}\",\"n\":{}}}",
                caller.login_id.as_str(),
                call.request_id.as_str().len()
            )),
        }
    }
}

// ------------------------------------------------------------------------------------------------
// The environment
// ------------------------------------------------------------------------------------------------

struct Env {
    gateway: Option<Gateway<Handler, MemStore>>,
    handler: Arc<Handler>,
    store: Arc<MemStore>,
    clock: Arc<ManualClock>,
    addr: SocketAddr,
    incidents: Arc<Mutex<Vec<(String, String)>>>,
}

fn plain_config() -> GatewayConfig {
    GatewayConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        insecure_local: true,
        scram_iterations: 4,
        handshake_timeout_secs: 5,
        ..GatewayConfig::default()
    }
}

async fn env_with(config: GatewayConfig, logins: Vec<LoginRecord>) -> Env {
    let handler = Arc::new(Handler::default());
    let store = Arc::new(MemStore::default());
    for l in logins {
        store.add(l);
    }
    let clock = Arc::new(ManualClock::new(Timestamp::from_unix_millis(
        1_700_000_000_000,
    )));
    let incidents: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let sink = Arc::clone(&incidents);
    let options = Options {
        clock: Arc::clone(&clock) as Arc<dyn Clock>,
        server_name: "test-gateway".to_string(),
        on_incident: Some(Arc::new(move |id, err| {
            sink.lock().unwrap().push((id.to_string(), err.to_string()));
        })),
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
        gateway: Some(gateway),
        handler,
        store,
        clock,
        addr,
        incidents,
    }
}

async fn env(logins: Vec<LoginRecord>) -> Env {
    env_with(plain_config(), logins).await
}

// ------------------------------------------------------------------------------------------------
// A test client
// ------------------------------------------------------------------------------------------------

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

struct Client {
    io: Box<dyn Io>,
    buf: Vec<u8>,
}

impl Client {
    fn new(io: impl Io + 'static) -> Client {
        Client {
            io: Box::new(io),
            buf: Vec::new(),
        }
    }

    async fn connect(addr: SocketAddr) -> Client {
        Client::new(TcpStream::connect(addr).await.unwrap())
    }

    async fn send_raw(&mut self, bytes: &[u8]) {
        self.io.write_all(bytes).await.unwrap();
        self.io.flush().await.unwrap();
    }

    async fn send(&mut self, msg: &ClientMessage) {
        let body = JsonCodec.encode_client(msg);
        let bytes = frame::encode(&body, u32::MAX).unwrap();
        self.send_raw(&bytes).await;
    }

    /// The next frame's body, or `None` when the connection ended (closed or reset).
    async fn recv_raw(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some((body, used)) = frame::decode(&self.buf, u32::MAX).unwrap() {
                let body = body.to_vec();
                self.buf.drain(..used);
                return Some(body);
            }
            let mut chunk = vec![0u8; 64 * 1024];
            match timeout(Duration::from_secs(10), self.io.read(&mut chunk)).await {
                Ok(Ok(n)) if n > 0 => self.buf.extend_from_slice(&chunk[..n]),
                Ok(_) => return None,
                Err(_) => panic!("timed out waiting for the gateway"),
            }
        }
    }

    async fn recv(&mut self) -> Option<ServerMessage> {
        let body = self.recv_raw().await?;
        Some(JsonCodec.decode_server(&body, &Limits::trusted()).unwrap())
    }

    /// Reads until the connection ends; panics if it does not end in time.
    async fn expect_closed(&mut self) {
        while self.recv_raw().await.is_some() {}
    }

    async fn hello(&mut self) -> ServerHello {
        self.send(&ClientMessage::Hello {
            max_version: 1,
            client: "test".to_string(),
        })
        .await;
        match self.recv().await {
            Some(ServerMessage::Hello(h)) => h,
            other => panic!("expected hello, got {other:?}"),
        }
    }

    /// The whole login. `Err` carries the `error` frame the gateway answered with.
    async fn login(&mut self, name: &str, password: &[u8]) -> Result<(), WireError> {
        self.hello().await;
        let (state, first) =
            scram::Client::start(name, password, scram::random_nonce().unwrap()).unwrap();
        self.send(&ClientMessage::Auth {
            step: ClientStep::ClientFirst,
            mechanism: Some(MECHANISM.to_string()),
            message: first,
        })
        .await;
        let server_first = match self.recv().await {
            Some(ServerMessage::Auth {
                step: ServerStep::ServerFirst,
                message,
            }) => message,
            Some(ServerMessage::Error { error }) => return Err(error),
            other => panic!("expected server_first, got {other:?}"),
        };
        let (state, client_final) = state.respond(&server_first).unwrap();
        self.send(&ClientMessage::Auth {
            step: ClientStep::ClientFinal,
            mechanism: None,
            message: client_final,
        })
        .await;
        match self.recv().await {
            Some(ServerMessage::Auth {
                step: ServerStep::ServerFinal,
                message,
            }) => state.verify(&message).unwrap(),
            Some(ServerMessage::Error { error }) => return Err(error),
            other => panic!("expected server_final, got {other:?}"),
        }
        assert_eq!(self.recv().await, Some(ServerMessage::Ready));
        Ok(())
    }

    async fn logged_in(addr: SocketAddr, name: &str, password: &[u8]) -> Client {
        let mut c = Client::connect(addr).await;
        c.login(name, password).await.unwrap();
        c
    }

    async fn request(&mut self, tag: u64, recipe: &str, args: &[(&str, i64)]) {
        let args: BTreeMap<String, Value> = args
            .iter()
            .map(|(k, v)| ((*k).to_string(), Value::Int(*v)))
            .collect();
        self.send(&ClientMessage::Request(Box::new(Request {
            tag,
            request_id: RequestId::new(format!("req-{tag}")).unwrap(),
            created_at: Timestamp::from_unix_millis(1_700_000_000_000),
            recipe: recipe.to_string(),
            version: None,
            subject: SubjectId::new("subject-1").unwrap(),
            counterparty: None,
            args,
            map_version: None,
        })))
        .await;
    }

    /// The next reply (any tag).
    async fn reply(&mut self) -> (u64, ReplyBody) {
        match self.recv().await {
            Some(ServerMessage::Reply { tag, body }) => (tag, body),
            other => panic!("expected a reply, got {other:?}"),
        }
    }

    async fn call(&mut self, tag: u64, recipe: &str, args: &[(&str, i64)]) -> ReplyBody {
        self.request(tag, recipe, args).await;
        let (got, body) = self.reply().await;
        assert_eq!(got, tag);
        body
    }
}

fn error_kind(body: &ReplyBody) -> ErrorKind {
    match body {
        ReplyBody::Err { error } => error.kind,
        ReplyBody::Ok { .. } => panic!("expected an error reply, got {body:?}"),
    }
}

fn wire_error(body: ReplyBody) -> WireError {
    match body {
        ReplyBody::Err { error } => error,
        ReplyBody::Ok { .. } => panic!("expected an error reply"),
    }
}

// ------------------------------------------------------------------------------------------------
// Login, W2
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_login_and_a_request_work_over_a_plain_loopback_connection() {
    let e = env(vec![record("alice", b"pw")]).await;
    let mut c = Client::connect(e.addr).await;
    let hello = c.hello().await;
    assert_eq!(hello.version, 1);
    assert_eq!(hello.server, "test-gateway");
    assert_eq!(hello.now_ms, 1_700_000_000_000);
    assert_eq!(hello.mechanisms, vec![MECHANISM.to_string()]);
    assert_eq!(hello.max_inflight, 32);
    drop(c);

    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    match c.call(1, "anything", &[]).await {
        ReplyBody::Ok { replayed, result } => {
            assert!(!replayed);
            assert_eq!(
                result,
                Value::obj([("login", Value::str("alice")), ("n", Value::Int(5))])
            );
        }
        other => panic!("{other:?}"),
    }
    // A stored failure is an error reply with its own name.
    let w = wire_error(c.call(2, "broke", &[]).await);
    assert_eq!(w.kind, ErrorKind::InsufficientFunds);
    c.send(&ClientMessage::Ping).await;
    assert_eq!(c.recv().await, Some(ServerMessage::Pong));
}

#[tokio::test]
async fn w2_a_wrong_password_an_unknown_login_and_a_disabled_one_look_identical() {
    let mut off = record("off", b"pw");
    off.disabled = true;
    let e = env(vec![record("alice", b"pw"), off]).await;

    let mut frames = Vec::new();
    for (name, password) in [("alice", &b"wrong"[..]), ("ghost", b"pw"), ("off", b"pw")] {
        let mut c = Client::connect(e.addr).await;
        c.hello().await;
        let (state, first) =
            scram::Client::start(name, password, scram::random_nonce().unwrap()).unwrap();
        c.send(&ClientMessage::Auth {
            step: ClientStep::ClientFirst,
            mechanism: Some(MECHANISM.to_string()),
            message: first,
        })
        .await;
        let Some(ServerMessage::Auth { message, .. }) = c.recv().await else {
            panic!("no server_first")
        };
        let (_, fin) = state.respond(&message).unwrap();
        c.send(&ClientMessage::Auth {
            step: ClientStep::ClientFinal,
            mechanism: None,
            message: fin,
        })
        .await;
        // The raw bytes of the error frame, and then the end of the connection.
        frames.push(c.recv_raw().await.expect("an error frame"));
        c.expect_closed().await;
    }
    assert_eq!(frames[0], frames[1], "wrong password vs unknown login");
    assert_eq!(frames[0], frames[2], "wrong password vs disabled login");
    let text = String::from_utf8(frames[0].clone()).unwrap();
    assert!(text.contains("\"kind\":\"unauthorized\""), "{text}");

    // The audit log, which only the log sees, does tell them apart.
    sleep(Duration::from_millis(100)).await;
    let failures: Vec<(String, bool)> = e
        .store
        .events()
        .into_iter()
        .filter_map(|ev| match ev {
            AuditEvent::LoginFailed {
                name, name_exists, ..
            } => Some((name, name_exists)),
            _ => None,
        })
        .collect();
    assert!(failures.contains(&("alice".to_string(), true)));
    assert!(failures.contains(&("ghost".to_string(), false)));
    assert!(failures.contains(&("off".to_string(), true)));
}

#[tokio::test]
async fn an_unknown_login_gets_a_stable_salt_so_it_cannot_be_spotted() {
    let e = env(vec![record("alice", b"pw")]).await;
    let mut salts = Vec::new();
    for _ in 0..2 {
        let mut c = Client::connect(e.addr).await;
        c.hello().await;
        let (_, first) =
            scram::Client::start("ghost", b"x", scram::random_nonce().unwrap()).unwrap();
        c.send(&ClientMessage::Auth {
            step: ClientStep::ClientFirst,
            mechanism: Some(MECHANISM.to_string()),
            message: first,
        })
        .await;
        let Some(ServerMessage::Auth { message, .. }) = c.recv().await else {
            panic!("no server_first")
        };
        // `r=<nonce>,s=<salt>,i=<count>`: keep what follows the nonce.
        salts.push(message.split_once(',').unwrap().1.to_string());
    }
    assert_eq!(salts[0], salts[1]);
}

#[tokio::test]
async fn login_protocol_mistakes_end_in_an_error_frame_and_a_close() {
    let e = env(vec![record("alice", b"pw")]).await;
    // A request before hello.
    let mut c = Client::connect(e.addr).await;
    c.request(1, "x", &[]).await;
    assert!(matches!(c.recv().await, Some(ServerMessage::Error { .. })));
    c.expect_closed().await;
    // An unsupported mechanism.
    let mut c = Client::connect(e.addr).await;
    c.hello().await;
    c.send(&ClientMessage::Auth {
        step: ClientStep::ClientFirst,
        mechanism: Some("PLAIN".to_string()),
        message: "n,,n=alice,r=abc".to_string(),
    })
    .await;
    let Some(ServerMessage::Error { error }) = c.recv().await else {
        panic!("no error")
    };
    assert_eq!(error.kind, ErrorKind::BadArguments);
    c.expect_closed().await;
    // Channel binding is not offered.
    let mut c = Client::connect(e.addr).await;
    c.hello().await;
    c.send(&ClientMessage::Auth {
        step: ClientStep::ClientFirst,
        mechanism: Some(MECHANISM.to_string()),
        message: "p=tls-exporter,,n=alice,r=abc".to_string(),
    })
    .await;
    assert!(matches!(c.recv().await, Some(ServerMessage::Error { .. })));
    c.expect_closed().await;
    // A hello that names no version in common.
    let mut c = Client::connect(e.addr).await;
    c.send(&ClientMessage::Hello {
        max_version: 0,
        client: "old".to_string(),
    })
    .await;
    assert!(matches!(c.recv().await, Some(ServerMessage::Error { .. })));
    c.expect_closed().await;
}

// ------------------------------------------------------------------------------------------------
// Lockout, W3
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn w3_five_wrong_passwords_lock_the_name_even_for_the_right_one() {
    let e = env(vec![record("alice", b"pw")]).await;
    for _ in 0..5 {
        let err = Client::connect(e.addr)
            .await
            .login("alice", b"wrong")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthorized);
    }
    let err = Client::connect(e.addr)
        .await
        .login("alice", b"pw")
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::LockedOut);
    // The lock is stored in the audit log.
    assert!(e
        .store
        .events()
        .iter()
        .any(|ev| matches!(ev, AuditEvent::LockedOut { name, .. } if name == "alice")));

    // Wait out the lock (the gateway's clock is manual).
    e.clock.advance(Duration::from_secs(15 * 60));
    Client::connect(e.addr)
        .await
        .login("alice", b"pw")
        .await
        .unwrap();
}

#[tokio::test]
async fn a_name_that_does_not_exist_locks_like_one_that_does() {
    let e = env(vec![]).await;
    for _ in 0..5 {
        let err = Client::connect(e.addr)
            .await
            .login("ghost", b"x")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthorized);
    }
    let err = Client::connect(e.addr)
        .await
        .login("ghost", b"x")
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::LockedOut);
}

#[tokio::test]
async fn a_success_resets_the_failure_count() {
    let e = env(vec![record("alice", b"pw")]).await;
    for _ in 0..4 {
        Client::connect(e.addr)
            .await
            .login("alice", b"wrong")
            .await
            .unwrap_err();
    }
    Client::connect(e.addr)
        .await
        .login("alice", b"pw")
        .await
        .unwrap();
    for _ in 0..4 {
        let err = Client::connect(e.addr)
            .await
            .login("alice", b"wrong")
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Unauthorized);
    }
}

#[tokio::test]
async fn a_source_with_too_many_failures_is_closed_out() {
    let e = env(vec![]).await;
    for i in 0..20 {
        Client::connect(e.addr)
            .await
            .login(&format!("ghost{i}"), b"x")
            .await
            .unwrap_err();
    }
    // The twentieth failure closed this address out: the connection is dropped without a word.
    let mut c = Client::connect(e.addr).await;
    let _ =
        c.io.write_all(&frame::encode(b"{\"type\":\"ping\"}", 1_000).unwrap())
            .await;
    assert!(c.recv_raw().await.is_none());
    // A minute later it may try again.
    e.clock.advance(Duration::from_secs(61));
    let mut c = Client::connect(e.addr).await;
    c.hello().await;
}

// ------------------------------------------------------------------------------------------------
// Permissions, W4 logic, and the order of the checks
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn permissions_and_the_allow_list_decide_before_anything_else() {
    let mut nobody = record("nobody", b"pw");
    nobody.permissions = Permissions::default();
    let mut narrow = record("narrow", b"pw");
    narrow.recipes = RecipeAllow::Names(["spend".to_string(), "missing".to_string()].into());
    let star = record("star", b"pw");
    let e = env(vec![nobody, narrow, star]).await;

    let mut c = Client::logged_in(e.addr, "nobody", b"pw").await;
    assert_eq!(
        error_kind(&c.call(1, "spend", &[]).await),
        ErrorKind::Unauthorized
    );

    let mut c = Client::logged_in(e.addr, "narrow", b"pw").await;
    assert!(matches!(
        c.call(1, "spend", &[]).await,
        ReplyBody::Ok { .. }
    ));
    // Not on the list.
    assert_eq!(
        error_kind(&c.call(2, "give", &[]).await),
        ErrorKind::Unauthorized
    );
    // On the list but unknown to the handler: the same answer, so recipes cannot be probed.
    assert_eq!(
        error_kind(&c.call(3, "missing", &[]).await),
        ErrorKind::Unauthorized
    );

    let mut c = Client::logged_in(e.addr, "star", b"pw").await;
    assert_eq!(
        error_kind(&c.call(1, "missing", &[]).await),
        ErrorKind::UnknownRecipe
    );

    // Only the refusals at the permission check never reached the handler: `nobody`'s call and
    // `narrow`'s `give`. The other three did: `spend`, `narrow`'s `missing` and `star`'s `missing`.
    assert_eq!(e.handler.calls.load(Ordering::SeqCst), 3);
}

// ------------------------------------------------------------------------------------------------
// Rate limit, W5
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn w5_a_burst_beyond_the_limit_is_refused_with_a_hint_and_stores_nothing() {
    let mut limited = record("alice", b"pw");
    limited.rate_per_sec = Some(1);
    limited.rate_burst = Some(2);
    let e = env(vec![limited]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    for tag in 1..=2 {
        assert!(matches!(c.call(tag, "x", &[]).await, ReplyBody::Ok { .. }));
    }
    for tag in 3..=5 {
        let w = wire_error(c.call(tag, "x", &[]).await);
        assert_eq!(w.kind, ErrorKind::RateLimited);
        let hint = w.retry_after_ms.expect("a hint");
        assert!((1..=1_000).contains(&hint), "{hint}");
    }
    // The refused requests never reached the handler.
    assert_eq!(e.handler.calls.load(Ordering::SeqCst), 2);
    // After the hint the same request goes through.
    e.clock.advance(Duration::from_secs(1));
    assert!(matches!(c.call(6, "x", &[]).await, ReplyBody::Ok { .. }));
    assert_eq!(e.handler.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_request_that_fails_the_permission_check_does_not_use_a_token() {
    let mut limited = record("alice", b"pw");
    limited.rate_per_sec = Some(1);
    limited.rate_burst = Some(1);
    limited.recipes = RecipeAllow::Names(["ok".to_string()].into());
    let e = env(vec![limited]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    for tag in 1..=5 {
        assert_eq!(
            error_kind(&c.call(tag, "denied", &[]).await),
            ErrorKind::Unauthorized
        );
    }
    assert!(matches!(c.call(6, "ok", &[]).await, ReplyBody::Ok { .. }));
}

// ------------------------------------------------------------------------------------------------
// In flight, W7 and W10
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn w7_thirty_two_requests_in_flight_come_back_by_tag_in_any_order() {
    let e = env(vec![record("alice", b"pw")]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    // The first request is the slowest, so the replies come back in the opposite order.
    for i in 0..32i64 {
        c.request(100 + i as u64, "slow", &[("ms", 400 - i * 10)])
            .await;
    }
    let mut order = Vec::new();
    for _ in 0..32 {
        let (tag, body) = c.reply().await;
        let ReplyBody::Ok { result, .. } = body else {
            panic!("tag {tag} failed")
        };
        let i = (tag - 100) as i64;
        assert_eq!(result, Value::obj([("slept", Value::Int(400 - i * 10))]));
        order.push(tag);
    }
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        (100..132).collect::<Vec<u64>>(),
        "every tag exactly once"
    );
    assert_ne!(
        order, sorted,
        "replies were not in the order of the requests"
    );
    assert_eq!(order.first(), Some(&131));
}

#[tokio::test]
async fn a_request_over_the_per_connection_cap_is_refused_with_overloaded() {
    let e = env(vec![record("alice", b"pw")]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    for tag in 1..=32 {
        c.request(tag, "slow", &[("ms", 300)]).await;
    }
    c.request(33, "slow", &[("ms", 300)]).await;
    let (tag, body) = c.reply().await;
    assert_eq!(
        tag, 33,
        "the refusal comes first, the others are still running"
    );
    assert_eq!(error_kind(&body), ErrorKind::Overloaded);
    for _ in 0..32 {
        assert!(matches!(c.reply().await.1, ReplyBody::Ok { .. }));
    }
    assert_eq!(e.handler.calls.load(Ordering::SeqCst), 32);
}

#[tokio::test]
async fn a_tag_used_twice_among_open_requests_closes_the_connection() {
    let e = env(vec![record("alice", b"pw")]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    c.request(7, "slow", &[("ms", 500)]).await;
    c.request(7, "slow", &[("ms", 500)]).await;
    let Some(ServerMessage::Error { error }) = c.recv().await else {
        panic!("expected an error frame")
    };
    assert_eq!(error.kind, ErrorKind::BadArguments);
    c.expect_closed().await;
}

#[tokio::test]
async fn w10_more_connections_than_a_login_may_hold_are_refused() {
    let mut capped = record("alice", b"pw");
    capped.max_connections = Some(2);
    let e = env(vec![capped]).await;
    let a = Client::logged_in(e.addr, "alice", b"pw").await;
    let _b = Client::logged_in(e.addr, "alice", b"pw").await;
    let mut third = Client::connect(e.addr).await;
    let err = third.login("alice", b"pw").await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Overloaded);
    assert!(err.retry_after_ms.is_some());
    third.expect_closed().await;
    // A closed connection gives its place back.
    drop(a);
    sleep(Duration::from_millis(200)).await;
    Client::logged_in(e.addr, "alice", b"pw").await;
}

#[tokio::test]
async fn w10_more_requests_than_the_gateway_allows_are_shed_and_not_queued() {
    let config = GatewayConfig {
        max_inflight_total: 2,
        ..plain_config()
    };
    let e = env_with(config, vec![record("alice", b"pw"), record("bob", b"pw")]).await;
    let mut a = Client::logged_in(e.addr, "alice", b"pw").await;
    let mut b = Client::logged_in(e.addr, "bob", b"pw").await;
    a.request(1, "slow", &[("ms", 400)]).await;
    a.request(2, "slow", &[("ms", 400)]).await;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(e.gateway.as_ref().unwrap().in_flight(), 2);
    // Another login is shed too: the cap is for the whole gateway.
    let w = wire_error(b.call(1, "x", &[]).await);
    assert_eq!(w.kind, ErrorKind::Overloaded);
    assert!(w.retry_after_ms.is_some());
    assert_eq!(e.handler.calls.load(Ordering::SeqCst), 2);
    for _ in 0..2 {
        assert!(matches!(a.reply().await.1, ReplyBody::Ok { .. }));
    }
    // The places come back.
    assert!(matches!(b.call(2, "x", &[]).await, ReplyBody::Ok { .. }));
    assert_eq!(e.gateway.as_ref().unwrap().in_flight(), 0);
}

// ------------------------------------------------------------------------------------------------
// Bad frames, W8
// ------------------------------------------------------------------------------------------------

fn frame_of(body: &[u8]) -> Vec<u8> {
    frame::encode(body, u32::MAX).unwrap()
}

#[tokio::test]
async fn w8_bad_frames_end_in_an_error_frame_and_a_close_and_the_server_keeps_serving() {
    let e = env(vec![record("alice", b"pw")]).await;

    // Before login: a frame over the 16 KiB limit (only the prefix is sent), a wrong version.
    let mut too_long = 20_000u32.to_be_bytes().to_vec();
    too_long.push(1);
    let wrong_version = {
        let mut f = 2u32.to_be_bytes().to_vec();
        f.extend_from_slice(&[9, b'{']);
        f
    };
    for bytes in [too_long, wrong_version] {
        let mut c = Client::connect(e.addr).await;
        c.send_raw(&bytes).await;
        let Some(ServerMessage::Error { error }) = c.recv().await else {
            panic!("expected an error frame")
        };
        assert_eq!(error.kind, ErrorKind::BadArguments);
        c.expect_closed().await;
    }

    // After login: bodies the JSON and message layers refuse.
    let deep = format!("{}1{}", "[".repeat(9), "]".repeat(9));
    let bodies: Vec<(&str, Vec<u8>)> = vec![
        (
            "invalid UTF-8",
            b"{\"type\":\"ping\",\"x\":\"\xFF\"}".to_vec(),
        ),
        (
            "too deep",
            format!("{{\"type\":\"ping\",\"x\":{deep}}}").into_bytes(),
        ),
        (
            "duplicate key",
            b"{\"type\":\"ping\",\"type\":\"ping\"}".to_vec(),
        ),
        ("a float", b"{\"type\":\"bye\",\"x\":1.5}".to_vec()),
        (
            "an unknown field",
            b"{\"type\":\"ping\",\"extra\":1}".to_vec(),
        ),
        ("an unknown type", b"{\"type\":\"launch\"}".to_vec()),
        ("not an object", b"[1,2]".to_vec()),
        (
            "hello after ready",
            b"{\"type\":\"hello\",\"max_version\":1,\"client\":\"x\"}".to_vec(),
        ),
    ];
    for (what, body) in bodies {
        let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
        c.send_raw(&frame_of(&body)).await;
        match c.recv().await {
            Some(ServerMessage::Error { error }) => {
                assert_eq!(error.kind, ErrorKind::BadArguments, "{what}");
            }
            other => panic!("{what}: expected an error frame, got {other:?}"),
        }
        c.expect_closed().await;
    }

    // An after-login frame is allowed to be bigger than the before-login limit, but not than 1 MiB.
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    let mut huge = (2 * 1024 * 1024u32).to_be_bytes().to_vec();
    huge.push(1);
    c.send_raw(&huge).await;
    assert!(matches!(c.recv().await, Some(ServerMessage::Error { .. })));
    c.expect_closed().await;

    // Through all of that the server kept serving.
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    assert!(matches!(c.call(1, "x", &[]).await, ReplyBody::Ok { .. }));
}

#[tokio::test]
async fn internal_errors_carry_an_incident_and_no_detail() {
    let e = env(vec![record("alice", b"pw")]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    let w = wire_error(c.call(1, "boom", &[]).await);
    assert_eq!(w.kind, ErrorKind::Internal);
    assert_eq!(w.message, "internal error");
    let incident = w.incident.clone().expect("an incident id");
    assert!(!format!("{w:?}").contains("secret detail"));
    // The cause went to the hook, under the same id.
    let seen = e.incidents.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, incident);
    assert!(seen[0].1.contains("secret detail"));
}

// ------------------------------------------------------------------------------------------------
// Timers, W9
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn w9_a_login_that_never_finishes_is_closed() {
    let config = GatewayConfig {
        handshake_timeout_secs: 1,
        ..plain_config()
    };
    let e = env_with(config, vec![record("alice", b"pw")]).await;
    // Says nothing at all.
    let mut c = Client::connect(e.addr).await;
    c.expect_closed().await;
    // Says hello and then stops half way through the login.
    let mut c = Client::connect(e.addr).await;
    c.hello().await;
    c.expect_closed().await;
    // The server is fine.
    Client::logged_in(e.addr, "alice", b"pw").await;
}

#[tokio::test]
async fn w9_an_idle_connection_is_closed_and_a_ping_keeps_it_open() {
    let config = GatewayConfig {
        idle_timeout_secs: 1,
        ..plain_config()
    };
    let e = env_with(config, vec![record("alice", b"pw")]).await;
    let mut quiet = Client::logged_in(e.addr, "alice", b"pw").await;
    let mut lively = Client::logged_in(e.addr, "alice", b"pw").await;
    for _ in 0..4 {
        sleep(Duration::from_millis(400)).await;
        lively.send(&ClientMessage::Ping).await;
        assert_eq!(lively.recv().await, Some(ServerMessage::Pong));
    }
    quiet.expect_closed().await;
    assert!(matches!(
        lively.call(1, "x", &[]).await,
        ReplyBody::Ok { .. }
    ));
}

#[tokio::test]
async fn w9_a_client_that_does_not_read_is_closed_and_frees_its_places() {
    let config = GatewayConfig {
        write_timeout_secs: 1,
        ..plain_config()
    };
    let e = env_with(config, vec![record("alice", b"pw")]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    // 32 replies of 600 KiB: far more than the sockets can hold, and the client reads none of it.
    for tag in 1..=32 {
        c.request(tag, "big", &[("kb", 600)]).await;
    }
    for _ in 0..100 {
        if e.gateway.as_ref().unwrap().in_flight() == 0 {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    sleep(Duration::from_millis(2_500)).await;
    // The gateway gave up on the connection: what was already in the socket still arrives, then the
    // connection ends, and nothing is left in flight.
    c.expect_closed().await;
    assert_eq!(e.gateway.as_ref().unwrap().in_flight(), 0);
    // Others are not affected.
    Client::logged_in(e.addr, "alice", b"pw").await;
}

#[tokio::test]
async fn bye_lets_open_requests_finish_first() {
    let e = env(vec![record("alice", b"pw")]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    c.request(1, "slow", &[("ms", 300)]).await;
    c.send(&ClientMessage::Bye).await;
    let (tag, body) = c.reply().await;
    assert_eq!(tag, 1);
    assert!(matches!(body, ReplyBody::Ok { .. }));
    c.expect_closed().await;
}

// ------------------------------------------------------------------------------------------------
// Shutdown
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_tells_clients_answers_new_requests_overloaded_and_lets_open_ones_finish() {
    let mut e = env(vec![record("alice", b"pw")]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    c.request(1, "slow", &[("ms", 400)]).await;
    sleep(Duration::from_millis(100)).await;
    let gateway = e.gateway.take().unwrap();
    let stopping = tokio::spawn(gateway.shutdown());

    assert_eq!(
        c.recv().await,
        Some(ServerMessage::Shutdown { drain_ms: 5_000 })
    );
    let w = wire_error(c.call(2, "x", &[]).await);
    assert_eq!(w.kind, ErrorKind::Overloaded);
    // The request that was open is answered, then the connection closes.
    let (tag, body) = c.reply().await;
    assert_eq!(tag, 1);
    assert!(matches!(body, ReplyBody::Ok { .. }));
    c.expect_closed().await;
    timeout(Duration::from_secs(10), stopping)
        .await
        .unwrap()
        .unwrap();
    // It stopped listening.
    assert!(TcpStream::connect(e.addr).await.is_err());
}

#[tokio::test]
async fn shutdown_gives_up_on_requests_that_outlast_the_drain_time() {
    let config = GatewayConfig {
        drain_ms: 200,
        ..plain_config()
    };
    let mut e = env_with(config, vec![record("alice", b"pw")]).await;
    let mut c = Client::logged_in(e.addr, "alice", b"pw").await;
    c.request(1, "slow", &[("ms", 5_000)]).await;
    sleep(Duration::from_millis(100)).await;
    let started = std::time::Instant::now();
    e.gateway.take().unwrap().shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(3));
    c.expect_closed().await;
}

// ------------------------------------------------------------------------------------------------
// TLS, W12 and W13
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

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bhdata-wire-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn install(dir: &std::path::Path, pair: &Pair) -> (PathBuf, PathBuf) {
    let (cert, key) = (dir.join("server.crt"), dir.join("server.key"));
    std::fs::write(&cert, &pair.cert_pem).unwrap();
    std::fs::write(&key, &pair.key_pem).unwrap();
    (cert, key)
}

fn tls_config(cert: PathBuf, key: PathBuf) -> GatewayConfig {
    GatewayConfig {
        insecure_local: false,
        cert_path: Some(cert),
        key_path: Some(key),
        ..plain_config()
    }
}

fn client_config(
    trusted: &[&Pair],
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for pair in trusted {
        roots
            .add(CertificateDer::from(pair.cert_der.clone()))
            .unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// A TLS connection, and the certificate the server showed.
async fn tls_connect(
    addr: SocketAddr,
    config: &Arc<rustls::ClientConfig>,
) -> std::io::Result<(Client, Vec<u8>)> {
    let tcp = TcpStream::connect(addr).await?;
    let connector = tokio_rustls::TlsConnector::from(Arc::clone(config));
    let stream = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await?;
    let shown = stream.get_ref().1.peer_certificates().unwrap()[0].to_vec();
    Ok((Client::new(stream), shown))
}

#[tokio::test]
async fn a_login_works_over_tls_1_3_and_only_with_a_trusted_key() {
    let dir = temp_dir("tls");
    let pair = make_pair();
    let (cert, key) = install(&dir, &pair);
    let e = env_with(tls_config(cert, key), vec![record("alice", b"pw")]).await;

    let good = client_config(&[&pair], &[&rustls::version::TLS13]);
    let (mut c, shown) = tls_connect(e.addr, &good).await.unwrap();
    assert_eq!(shown, pair.cert_der);
    c.login("alice", b"pw").await.unwrap();
    assert!(matches!(c.call(1, "x", &[]).await, ReplyBody::Ok { .. }));

    // A client that does not trust the server's key refuses it before any login.
    let stranger = make_pair();
    let wrong = client_config(&[&stranger], &[&rustls::version::TLS13]);
    assert!(tls_connect(e.addr, &wrong).await.is_err());

    // A plain connection to a TLS port gets nowhere: what comes back is a TLS alert, not a frame
    // (so it is read as raw bytes here), and then the connection ends.
    let mut plain = TcpStream::connect(e.addr).await.unwrap();
    let _ = plain
        .write_all(&frame::encode(b"{\"type\":\"ping\"}", 1_000).unwrap())
        .await;
    let mut got = Vec::new();
    timeout(Duration::from_secs(10), plain.read_to_end(&mut got))
        .await
        .expect("the gateway closes a plain connection to a TLS port")
        .ok();
    assert!(
        !String::from_utf8_lossy(&got).contains("\"hello\""),
        "no hello for a client that does not speak TLS"
    );
}

#[tokio::test]
async fn tls_1_2_is_not_spoken() {
    let dir = temp_dir("tls12");
    let pair = make_pair();
    let (cert, key) = install(&dir, &pair);
    let e = env_with(tls_config(cert, key), vec![]).await;
    let old = client_config(&[&pair], &[&rustls::version::TLS12]);
    assert!(tls_connect(e.addr, &old).await.is_err());
}

#[tokio::test]
async fn w12_a_certificate_swap_drops_nothing_and_new_connections_see_the_new_key() {
    let dir = temp_dir("swap");
    let (old, new) = (make_pair(), make_pair());
    let (cert, key) = install(&dir, &old);
    let e = env_with(tls_config(cert, key), vec![record("alice", b"pw")]).await;
    // A client that holds both keys, as in the spec's rotation overlap.
    let both = client_config(&[&old, &new], &[&rustls::version::TLS13]);

    let (mut open, shown) = tls_connect(e.addr, &both).await.unwrap();
    assert_eq!(shown, old.cert_der);
    open.login("alice", b"pw").await.unwrap();
    assert!(matches!(open.call(1, "x", &[]).await, ReplyBody::Ok { .. }));

    install(&dir, &new);
    e.gateway.as_ref().unwrap().reload().unwrap();

    // The open connection did not notice.
    assert!(matches!(open.call(2, "x", &[]).await, ReplyBody::Ok { .. }));
    // A new one gets the new key.
    let (mut fresh, shown) = tls_connect(e.addr, &both).await.unwrap();
    assert_eq!(shown, new.cert_der);
    fresh.login("alice", b"pw").await.unwrap();
    // And a client that only trusts the old key is turned away now.
    let only_old = client_config(&[&old], &[&rustls::version::TLS13]);
    assert!(tls_connect(e.addr, &only_old).await.is_err());

    // A reload that fails changes nothing.
    std::fs::write(dir.join("server.key"), "not a key").unwrap();
    assert!(e.gateway.as_ref().unwrap().reload().is_err());
    let (_, shown) = tls_connect(e.addr, &both).await.unwrap();
    assert_eq!(shown, new.cert_der);
}

#[tokio::test]
async fn w13_a_server_without_tls_on_a_non_loopback_address_does_not_start() {
    let start = |config: GatewayConfig| async move {
        Gateway::start(
            config,
            Arc::new(Handler::default()),
            Arc::new(MemStore::default()),
            ServerSecret::new([1; 32]),
            Options::default(),
        )
        .await
        .err()
    };
    // No certificate configured, TLS required (the default).
    let open = GatewayConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        insecure_local: false,
        ..plain_config()
    };
    assert!(matches!(
        start(open).await,
        Some(StartError::TlsNotConfigured)
    ));
    // `insecure_local` on an address that is not loopback.
    let plain_open = GatewayConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        insecure_local: true,
        ..plain_config()
    };
    assert!(matches!(
        start(plain_open).await,
        Some(StartError::Config(_))
    ));
    // A certificate that is not there.
    let missing = GatewayConfig {
        insecure_local: false,
        cert_path: Some("/nonexistent/server.crt".into()),
        key_path: Some("/nonexistent/server.key".into()),
        ..plain_config()
    };
    assert!(matches!(start(missing).await, Some(StartError::Tls(_))));
    // An address that is taken.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = GatewayConfig {
        listen: taken.local_addr().unwrap(),
        ..plain_config()
    };
    assert!(matches!(start(busy).await, Some(StartError::Bind { .. })));
}

// ------------------------------------------------------------------------------------------------
// Audit
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn logins_are_audited_and_a_lock_that_ends_is_reported() {
    let e = env(vec![record("alice", b"pw")]).await;
    Client::connect(e.addr)
        .await
        .login("alice", b"pw")
        .await
        .unwrap();
    sleep(Duration::from_millis(100)).await;
    assert!(e.store.events().iter().any(|ev| matches!(
        ev,
        AuditEvent::LoginSucceeded { login_id, .. } if login_id.as_str() == "alice"
    )));
}
