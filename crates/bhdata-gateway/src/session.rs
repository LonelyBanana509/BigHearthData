//! One client connection, from the first frame to the close (wire-protocol spec, sections 3, 5 and 8).
//! Lane B.
//!
//! The session is generic over the stream, so it runs the same over TLS and over a plain loopback
//! connection. It owns no policy: framing, parsing and SCRAM come from `bhdata-proto`, the failure
//! counters and limits from `lockout`, `ratelimit` and `shed` (lane S), the login from `auth`, and the
//! request itself goes to the [`RequestHandler`].
//!
//! ## The life of a connection
//!
//! 1. **Handshake**, under one deadline (`handshake_timeout_secs`) that starts when the connection is
//!    accepted: `hello`, the three SCRAM steps, `ready`. Frames are limited to the before-login
//!    limits. A wrong step, a failed proof or a bad frame ends in one `error` frame and a close.
//! 2. **Requests**, under the after-login limits. Each `request` goes through the checks of decision 9
//!    in order (permission, rate limit, shed) and, if it passes, runs as its own task, so replies come
//!    back in any order. One writer task owns the write half; a reply that cannot be written within
//!    `write_timeout_secs` closes the connection.
//! 3. **Close**: on `bye` (open requests are still answered), on the idle timer, on a protocol error, on
//!    a write that timed out, or on shutdown (spec, section 8).
//!
//! A request refused at any check did no work and stored nothing, so a retry with the same request id
//! is safe (decision 9).

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bhdata_api::handler::{Call, Caller, Reply, RequestHandler};
use bhdata_api::login::{LoginRecord, LoginStore, RecipeAllow};
use bhdata_api::recipe::canon::Json;
use bhdata_api::shard::Status;
use bhdata_api::types::config::GatewayConfig;
use bhdata_api::types::{Clock, Error};
use bhdata_proto::error_map::{error_from_stored_name, to_wire, WireContext, WireError};
use bhdata_proto::frame::{self, Limits};
use bhdata_proto::json::{self, Value};
use bhdata_proto::message::{
    ClientMessage, ClientStep, Codec, JsonCodec, ProtoError, ReplyBody, Request, ServerHello,
    ServerMessage, ServerStep,
};
use bhdata_proto::scram::MECHANISM;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{sleep_until, timeout, timeout_at, Instant};

use crate::auth::{AuthFailure, Authenticator};
use crate::perm;
use crate::ratelimit::{Connections, RateLimiter};
use crate::shed::{Permit, Shed};

/// Called with the incident id and the real cause whenever the gateway answers `internal`. The wire
/// carries only the id (spec, section 4), so this is where the cause goes: the server wires it to its
/// log.
pub type IncidentHook = Arc<dyn Fn(&str, &Error) + Send + Sync>;

/// What the gateway needs from its host besides the config.
#[derive(Clone)]
pub struct Options {
    /// The clock: wall time for `hello` and the audit log, monotonic time for the counters. Tests pass
    /// a manual one.
    pub clock: Arc<dyn Clock>,
    /// The `server` text of `hello`, for the client's logs.
    pub server_name: String,
    /// Where the cause of an `internal` answer goes.
    pub on_incident: Option<IncidentHook>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            clock: Arc::new(bhdata_api::types::SystemClock::new()),
            server_name: concat!("bhdata-gateway/", env!("CARGO_PKG_VERSION")).to_string(),
            on_incident: None,
        }
    }
}

/// Everything the connections of one gateway copy share.
pub(crate) struct Shared<H, S> {
    pub config: GatewayConfig,
    pub handler: Arc<H>,
    pub auth: Authenticator<S>,
    pub limiter: Mutex<RateLimiter>,
    pub connections: Mutex<Connections>,
    pub shed: Shed,
    pub options: Options,
    pub shutdown: watch::Sender<bool>,
    incidents: AtomicU64,
}

impl<H, S> Shared<H, S> {
    pub(crate) fn new(
        config: GatewayConfig,
        handler: Arc<H>,
        auth: Authenticator<S>,
        options: Options,
        shutdown: watch::Sender<bool>,
    ) -> Self {
        Shared {
            limiter: Mutex::new(RateLimiter::from_config(&config)),
            connections: Mutex::new(Connections::from_config(
                &config,
                crate::shed::DEFAULT_RETRY_HINT,
            )),
            shed: Shed::from_config(&config),
            config,
            handler,
            auth,
            options,
            shutdown,
            incidents: AtomicU64::new(0),
        }
    }

    pub(crate) fn now_ms(&self) -> u64 {
        u64::try_from(self.options.clock.monotonic().as_millis()).unwrap_or(u64::MAX)
    }

    /// The wire form of `err`. An `internal` (or `wrong_shard`, which never goes out as itself) gets an
    /// incident id, and its cause goes to the hook.
    fn wire(&self, err: &Error, rate_limit_hint: Option<Duration>) -> WireError {
        let incident = matches!(err, Error::Internal(_) | Error::WrongShard).then(|| {
            let n = self.incidents.fetch_add(1, Ordering::Relaxed);
            let id = format!("gw-{n:08x}");
            if let Some(hook) = &self.options.on_incident {
                hook(&id, err);
            }
            id
        });
        to_wire(
            err,
            &WireContext {
                rate_limit_hint,
                incident,
            },
        )
    }

    fn encode_server(&self, msg: &ServerMessage) -> Vec<u8> {
        let body = JsonCodec.encode_server(msg);
        // A message over the frame limit cannot be sent. Every message built here is small except a
        // result, and `reply_frame` handles that case itself.
        frame::encode(&body, self.config.max_frame).unwrap_or_default()
    }

    fn error_frame(&self, err: &Error) -> Vec<u8> {
        self.encode_server(&ServerMessage::Error {
            error: self.wire(err, None),
        })
    }

    /// One `reply` frame. A result too big for a frame becomes an `internal` reply instead.
    fn reply_frame(&self, tag: u64, body: ReplyBody) -> Vec<u8> {
        let msg = ServerMessage::Reply { tag, body };
        let bytes = JsonCodec.encode_server(&msg);
        match frame::encode(&bytes, self.config.max_frame) {
            Ok(frame) => frame,
            Err(_) => {
                let err = Error::Internal("reply does not fit in a frame".to_string());
                self.encode_server(&ServerMessage::Reply {
                    tag,
                    body: ReplyBody::Err {
                        error: self.wire(&err, None),
                    },
                })
            }
        }
    }

    fn error_reply(&self, tag: u64, err: &Error, hint: Option<Duration>) -> Vec<u8> {
        self.reply_frame(
            tag,
            ReplyBody::Err {
                error: self.wire(err, hint),
            },
        )
    }

    /// The reply for what the handler returned.
    fn handler_reply(
        &self,
        tag: u64,
        result: Result<Reply, Error>,
        allow: &RecipeAllow,
    ) -> Vec<u8> {
        match result {
            Err(err) => self.error_reply(tag, &perm::present_error(allow, err), None),
            Ok(outcome) => match outcome.status {
                Status::Failed { error } => {
                    self.error_reply(tag, &error_from_stored_name(&error), None)
                }
                Status::Done => match json::parse(outcome.result.as_bytes(), &Limits::trusted()) {
                    Ok(result) => self.reply_frame(
                        tag,
                        ReplyBody::Ok {
                            replayed: outcome.replayed,
                            result,
                        },
                    ),
                    Err(e) => self.error_reply(
                        tag,
                        &Error::Internal(format!("stored result is not JSON: {e}")),
                        None,
                    ),
                },
            },
        }
    }
}

/// The request's arguments as the handler's exact JSON type. The parser already refused floats and
/// integers outside 64 bits, so nothing is lost.
fn to_json(v: Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(b),
        Value::Int(n) => Json::Int(n),
        Value::Str(s) => Json::Str(s),
        Value::Arr(items) => Json::Arr(items.into_iter().map(to_json).collect()),
        Value::Obj(map) => Json::Obj(map.into_iter().map(|(k, v)| (k, to_json(v))).collect()),
    }
}

/// Why a session stops: the peer is gone, or the connection ends with this `error` frame.
enum End {
    Gone,
    Send(Error),
}

impl From<ProtoError> for End {
    fn from(e: ProtoError) -> Self {
        End::Send(e.to_error())
    }
}

impl From<AuthFailure> for End {
    fn from(f: AuthFailure) -> Self {
        End::Send(f.to_error())
    }
}

/// Reads whole frames off the read half. Cancel-safe: bytes read are kept in the buffer, so a read
/// that loses a `select!` race loses nothing.
struct FrameReader<R> {
    reader: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    fn new(reader: R) -> Self {
        FrameReader {
            reader,
            buf: Vec::new(),
        }
    }

    /// The next frame's body, or `None` at a clean end of stream. A length over `max_frame` is refused
    /// as soon as the four prefix bytes are in, so the buffer never grows past one frame and a chunk.
    async fn next(&mut self, max_frame: u32) -> Result<Option<Vec<u8>>, ProtoError> {
        loop {
            if let Some((body, used)) = frame::decode(&self.buf, max_frame)? {
                let body = body.to_vec();
                self.buf.drain(..used);
                return Ok(Some(body));
            }
            let mut chunk = [0u8; 8 * 1024];
            let n = self.reader.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

async fn write_all<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8], limit: Duration) -> bool {
    matches!(
        timeout(limit, async {
            w.write_all(bytes).await?;
            w.flush().await
        })
        .await,
        Ok(Ok(()))
    )
}

fn millis_since(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The writer task: the only code that writes to the socket after login.
async fn write_loop<W: AsyncWrite + Unpin>(
    mut w: W,
    mut rx: mpsc::Receiver<Vec<u8>>,
    limit: Duration,
    activity: Arc<AtomicU64>,
    start: Instant,
) {
    while let Some(bytes) = rx.recv().await {
        if !write_all(&mut w, &bytes, limit).await {
            return;
        }
        activity.store(millis_since(start), Ordering::Relaxed);
    }
    // A clean end: tell the peer (TLS close_notify) and let go.
    let _ = timeout(limit, w.shutdown()).await;
}

/// Gives a login connection's place back when the session ends, however it ends.
struct ConnectionSlot<'a, H, S> {
    shared: &'a Shared<H, S>,
    login: String,
}

impl<H, S> Drop for ConnectionSlot<'_, H, S> {
    fn drop(&mut self) {
        self.shared
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .close(&self.login);
    }
}

fn is_hello(msg: ClientMessage) -> Result<(), End> {
    match msg {
        ClientMessage::Hello { max_version, .. } if max_version >= frame::VERSION => Ok(()),
        ClientMessage::Hello { .. } => Err(End::Send(Error::BadArguments(
            "no protocol version in common".to_string(),
        ))),
        _ => Err(End::Send(Error::BadArguments(
            "the first message must be `hello`".to_string(),
        ))),
    }
}

/// Runs one connection to its end. `deadline` is when the handshake and login must be over.
pub(crate) async fn run<H, S, IO>(
    shared: Arc<Shared<H, S>>,
    io: IO,
    source: IpAddr,
    deadline: Instant,
) where
    H: RequestHandler + 'static,
    S: LoginStore + 'static,
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, mut write_half) = tokio::io::split(io);
    let mut reader = FrameReader::new(read_half);
    let write_limit = shared.config.write_timeout();

    let login = timeout_at(
        deadline,
        handshake(&shared, &mut reader, &mut write_half, source),
    )
    .await;
    let (record, slot) = match login {
        Ok(Ok(done)) => done,
        Ok(Err(End::Send(err))) => {
            let frame = shared.error_frame(&err);
            let _ = write_all(&mut write_half, &frame, write_limit).await;
            let _ = timeout(write_limit, write_half.shutdown()).await;
            return;
        }
        // The peer left, or took too long: nothing to tell it.
        Ok(Err(End::Gone)) | Err(_) => return,
    };
    serve(&shared, reader, write_half, record).await;
    drop(slot);
}

/// Hello and login. On success the connection holds a place under its login's connection cap, in the
/// returned [`ConnectionSlot`]; the slot gives the place back when it is dropped, including when the
/// handshake deadline drops this future half way.
async fn handshake<'a, H, S, R, W>(
    shared: &'a Shared<H, S>,
    reader: &mut FrameReader<R>,
    writer: &mut W,
    source: IpAddr,
) -> Result<(LoginRecord, ConnectionSlot<'a, H, S>), End>
where
    H: RequestHandler + 'static,
    S: LoginStore + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let cfg = &shared.config;
    let limits = Limits::before_login(cfg);
    let write_limit = cfg.write_timeout();

    async fn read_message<R: AsyncRead + Unpin>(
        reader: &mut FrameReader<R>,
        limits: &Limits,
    ) -> Result<ClientMessage, End> {
        let body = reader.next(limits.max_frame).await?.ok_or(End::Gone)?;
        Ok(JsonCodec.decode_client(&body, limits)?)
    }
    let send = |msg: ServerMessage| shared.encode_server(&msg);

    is_hello(read_message(reader, &limits).await?)?;
    let hello = send(ServerMessage::Hello(ServerHello {
        version: frame::VERSION,
        server: shared.options.server_name.clone(),
        now_ms: shared.options.clock.now().as_unix_millis(),
        max_frame: cfg.max_frame,
        max_inflight: cfg.max_inflight_per_connection,
        idle_timeout_secs: cfg.idle_timeout_secs,
        mechanisms: vec![MECHANISM.to_string()],
    }));
    if !write_all(writer, &hello, write_limit).await {
        return Err(End::Gone);
    }

    let wrong_step = || End::Send(Error::BadArguments("unexpected login step".to_string()));
    let ClientMessage::Auth {
        step: ClientStep::ClientFirst,
        mechanism: Some(mechanism),
        message: client_first,
    } = read_message(reader, &limits).await?
    else {
        return Err(wrong_step());
    };
    if mechanism != MECHANISM {
        return Err(End::Send(Error::BadArguments(format!(
            "unsupported mechanism `{mechanism}`"
        ))));
    }
    let (pending, server_first) = shared.auth.begin(&client_first, source).await?;
    let first = send(ServerMessage::Auth {
        step: ServerStep::ServerFirst,
        message: server_first,
    });
    if !write_all(writer, &first, write_limit).await {
        return Err(End::Gone);
    }

    let ClientMessage::Auth {
        step: ClientStep::ClientFinal,
        message: client_final,
        ..
    } = read_message(reader, &limits).await?
    else {
        return Err(wrong_step());
    };
    let (server_final, record) = shared.auth.finish(pending, &client_final).await?;

    // The cap on connections per login is checked once the login is known and proven.
    shared
        .connections
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .open(record.login_id.as_str(), record.max_connections)
        .map_err(End::Send)?;
    let slot = ConnectionSlot {
        shared,
        login: record.login_id.as_str().to_owned(),
    };
    let done = send(ServerMessage::Auth {
        step: ServerStep::ServerFinal,
        message: server_final,
    });
    let ready = send(ServerMessage::Ready);
    if !write_all(writer, &done, write_limit).await || !write_all(writer, &ready, write_limit).await
    {
        return Err(End::Gone);
    }
    Ok((record, slot))
}

/// What a finished request task hands back: its tag.
type Finished = u64;

/// The request loop of a logged-in connection.
async fn serve<H, S, R, W>(
    shared: &Arc<Shared<H, S>>,
    mut reader: FrameReader<R>,
    writer: W,
    record: LoginRecord,
) where
    H: RequestHandler + 'static,
    S: LoginStore + 'static,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let cfg = &shared.config;
    let limits = Limits::after_login(cfg);
    let idle = cfg.idle_timeout();
    let max_open = cfg.max_inflight_per_connection as usize;
    let start = Instant::now();
    let activity = Arc::new(AtomicU64::new(0));

    let (tx, rx) = mpsc::channel::<Vec<u8>>(max_open + 8);
    let mut writer_task = tokio::spawn(write_loop(
        writer,
        rx,
        cfg.write_timeout(),
        Arc::clone(&activity),
        start,
    ));
    let mut writer_done = false;

    let mut tasks: JoinSet<Finished> = JoinSet::new();
    let mut open: HashSet<u64> = HashSet::new();
    let mut shutdown = shared.shutdown.subscribe();
    let mut drain_until: Option<Instant> = None;
    let mut bye = false;

    // An error frame, then the end of the session.
    macro_rules! fail {
        ($err:expr) => {{
            let _ = tx.send(shared.error_frame(&$err)).await;
            break;
        }};
    }

    loop {
        while let Some(done) = tasks.try_join_next() {
            if let Ok(tag) = done {
                open.remove(&tag);
            }
        }
        if (bye || drain_until.is_some()) && tasks.is_empty() {
            break;
        }
        if drain_until.is_none() && *shutdown.borrow() {
            drain_until = Some(Instant::now() + cfg.drain());
            let msg = ServerMessage::Shutdown {
                drain_ms: cfg.drain_ms,
            };
            let _ = tx.send(shared.encode_server(&msg)).await;
        }
        let idle_deadline = start + Duration::from_millis(activity.load(Ordering::Relaxed)) + idle;
        let drain_deadline = drain_until.unwrap_or(idle_deadline);

        tokio::select! {
            _ = &mut writer_task, if !writer_done => {
                writer_done = true;
                break;
            }
            _ = shutdown.changed(), if drain_until.is_none() => {}
            Some(done) = tasks.join_next(), if !tasks.is_empty() => {
                if let Ok(tag) = done {
                    open.remove(&tag);
                }
            }
            () = sleep_until(drain_deadline), if drain_until.is_some() => break,
            () = sleep_until(idle_deadline) => {
                // The writer may have moved the mark since; only a connection that is still quiet closes.
                let quiet = millis_since(start).saturating_sub(activity.load(Ordering::Relaxed));
                if quiet >= u64::try_from(idle.as_millis()).unwrap_or(u64::MAX) {
                    break;
                }
            }
            frame = reader.next(limits.max_frame), if !bye => {
                let body = match frame {
                    Ok(Some(body)) => body,
                    Ok(None) => break,
                    Err(e) => fail!(e.to_error()),
                };
                activity.store(millis_since(start), Ordering::Relaxed);
                let msg = match JsonCodec.decode_client(&body, &limits) {
                    Ok(msg) => msg,
                    Err(e) => fail!(e.to_error()),
                };
                match msg {
                    ClientMessage::Ping => {
                        if tx.send(shared.encode_server(&ServerMessage::Pong)).await.is_err() {
                            break;
                        }
                    }
                    ClientMessage::Bye => bye = true,
                    ClientMessage::Request(req) => {
                        let tag = req.tag;
                        if !open.insert(tag) {
                            fail!(Error::BadArguments(format!("tag {tag} is already in use")));
                        }
                        match admit(shared, &record, *req, tag, &mut tasks, &tx, open.len() > max_open) {
                            Admission::Refused(frame) => {
                                open.remove(&tag);
                                if tx.send(frame).await.is_err() {
                                    break;
                                }
                            }
                            Admission::Running => {}
                        }
                    }
                    ClientMessage::Hello { .. } | ClientMessage::Auth { .. } => {
                        fail!(Error::BadArguments("`hello` and `auth` come before `ready`".to_string()));
                    }
                }
            }
        }
    }

    // Stop what is still running (nothing, after a clean `bye` or a finished drain), then let the
    // writer flush what is queued and close.
    tasks.shutdown().await;
    drop(tx);
    if !writer_done {
        let _ = timeout(cfg.write_timeout(), writer_task).await;
    }
}

enum Admission {
    /// The request is running as a task; its reply will come from there.
    Running,
    /// Refused at a check: this is the reply, and nothing was stored.
    Refused(Vec<u8>),
}

/// The checks of decision 9, in order, then the handler as its own task.
fn admit<H, S>(
    shared: &Arc<Shared<H, S>>,
    record: &LoginRecord,
    req: Request,
    tag: u64,
    tasks: &mut JoinSet<Finished>,
    tx: &mpsc::Sender<Vec<u8>>,
    over_connection_cap: bool,
) -> Admission
where
    H: RequestHandler + 'static,
    S: LoginStore + 'static,
{
    // One connection may have only so many requests open (decision 5). The answer is `overloaded`.
    if over_connection_cap {
        let err = Error::Overloaded {
            retry_after: crate::shed::DEFAULT_RETRY_HINT,
        };
        return Admission::Refused(shared.error_reply(tag, &err, None));
    }
    // 1. Permission.
    if let Err(err) = perm::check_execute(record.permissions, &record.recipes, &req.recipe) {
        return Admission::Refused(shared.error_reply(tag, &err, None));
    }
    // 2. Rate limit. Counts every request that passed the permission check.
    let limited = shared
        .limiter
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .check(
            record.login_id.as_str(),
            record.rate_per_sec,
            record.rate_burst,
            shared.now_ms(),
        );
    if let Err(limited) = limited {
        return Admission::Refused(shared.error_reply(
            tag,
            &Error::RateLimited,
            Some(limited.retry_after),
        ));
    }
    // 3. Shed.
    let permit = match shared.shed.admit() {
        Ok(permit) => permit,
        Err(err) => return Admission::Refused(shared.error_reply(tag, &err, None)),
    };
    // 4. The handler.
    let call = Call {
        request_id: req.request_id,
        created_at: req.created_at,
        recipe: req.recipe,
        version: req.version,
        subject: req.subject,
        counterparty: req.counterparty,
        args: to_json(Value::Obj(req.args)),
        map_version: req.map_version,
    };
    let caller = Caller {
        login_id: record.login_id.clone(),
    };
    let allow = record.recipes.clone();
    let shared = Arc::clone(shared);
    let tx = tx.clone();
    tasks.spawn(async move {
        let result = shared.handler.handle(caller, call).await;
        let frame = shared.handler_reply(tag, result, &allow);
        drop::<Permit>(permit);
        let _ = tx.send(frame).await;
        tag
    });
    Admission::Running
}
