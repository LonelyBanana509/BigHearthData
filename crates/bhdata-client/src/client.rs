//! Connect, key pins, request ids and the clock offset (spec, decisions 2, 12 and 13). Lane K.
//!
//! One [`Client`] holds one connection with many requests in flight (spec section 11: no pool at
//! stage 0). The connection is opened when the client is made, so a wrong pin or a wrong password
//! is reported before any request; after a loss the next call opens a new one.
//!
//! Every call keeps one request id and one `created_at_ms` for all its attempts, and the retry
//! table of spec section 5 ([`crate::retry`]) decides what each answer means.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bhdata_proto::error_map::WireError;
use bhdata_proto::frame::{self, Limits};
use bhdata_proto::json::Value;
use bhdata_proto::message::{
    ClientMessage, ClientStep, Codec, JsonCodec, ReplyBody, Request, ServerHello, ServerMessage,
    ServerStep, MECHANISM,
};
use bhdata_proto::scram;
use bhdata_proto::types::{Error, ErrorKind, RecipeVersion, RequestId, SubjectId, Timestamp};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::{DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{oneshot, Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsConnector;

use crate::retry::{Decision, GaveUp, Retry, Seen, Step, DEFAULT_MAX_RETRY};

/// The whole connect, hello and login must finish within this (spec section 3).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often an idle connection is pinged (spec section 3: the server closes after 60 s of silence).
pub const PING_EVERY: Duration = Duration::from_secs(20);

/// A reply that cannot be written within this closes the connection (spec section 3).
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// The default wait for one reply before the attempt counts as lost.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The largest frame the client will read from the server (the server's own limit after login is
/// 1 MiB by default; this is a ceiling so a broken peer cannot make the client allocate without bound).
const MAX_SERVER_FRAME: u32 = 16 * 1024 * 1024;

/// The name sent in `hello`, for the server's logs (at most 64 bytes).
const CLIENT_NAME: &str = "bhdata-client-rust";

// ---------------------------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------------------------

/// The SHA-256 of a server's `SubjectPublicKeyInfo`: what the client pins (spec, decision 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyPin([u8; 32]);

impl KeyPin {
    /// A pin from its 32 bytes.
    #[must_use]
    pub fn new(bytes: [u8; 32]) -> Self {
        KeyPin(bytes)
    }

    /// A pin from 64 hex digits (either case).
    ///
    /// # Errors
    /// A sentence if the text is not 64 hex digits.
    pub fn from_hex(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.len() != 64 || !text.is_ascii() {
            return Err("a key pin is 64 hex digits".to_string());
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16)
                .map_err(|_| "a key pin is 64 hex digits".to_string())?;
        }
        Ok(KeyPin(out))
    }

    /// The pin of the key in a DER certificate, or `None` if the certificate cannot be read.
    #[must_use]
    pub fn of_certificate(der: &[u8]) -> Option<Self> {
        let spki = spki_of(der)?;
        Some(KeyPin(Sha256::digest(spki).into()))
    }

    /// The 64 lower-case hex digits.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

/// How the client reaches the server and decides whether to trust it (spec, decisions 1 and 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Security {
    /// TLS 1.3; the server's key must match one of these pins. Two pins let a key rotation overlap.
    Pinned(Vec<KeyPin>),
    /// TLS 1.3 with no check of the server. Only for a loopback address.
    InsecureNoVerify,
    /// No TLS (the server's `insecure_local`). Only for a loopback address.
    PlainLoopback,
}

/// What a [`Client`] needs to connect.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// The gateway's address.
    pub addr: SocketAddr,
    /// The name sent in the TLS handshake. The pin decides trust, so this only has to be a valid
    /// name; the default is `localhost`.
    pub server_name: String,
    /// The login.
    pub login: String,
    /// The password, as bytes (SASLprep-ed when it is valid UTF-8).
    pub password: Vec<u8>,
    /// TLS and trust.
    pub security: Security,
    /// How long one call keeps retrying (spec section 5: `max_retry_secs`, default 60 s, never more
    /// than the retry window).
    pub max_retry: Duration,
    /// How long to wait for one reply before the attempt counts as lost.
    pub call_timeout: Duration,
}

impl ClientConfig {
    /// A configuration with the defaults of spec section 5.
    #[must_use]
    pub fn new(
        addr: SocketAddr,
        login: impl Into<String>,
        password: impl Into<Vec<u8>>,
        security: Security,
    ) -> Self {
        ClientConfig {
            addr,
            server_name: "localhost".to_string(),
            login: login.into(),
            password: password.into(),
            security,
            max_retry: DEFAULT_MAX_RETRY,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Calls and errors
// ---------------------------------------------------------------------------------------------

/// One request to run a recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The recipe's name.
    pub recipe: String,
    /// The version, or `None` for the latest.
    pub version: Option<RecipeVersion>,
    /// The shard key of the subject.
    pub subject: SubjectId,
    /// The shard key of the counterparty, if the recipe has one.
    pub counterparty: Option<SubjectId>,
    /// The arguments, in the wire forms of spec section 4 (`i64` as an integer, `decimal` as a
    /// string with the plan's digits, `bytes` as base64, ...).
    pub args: BTreeMap<String, Value>,
    /// The request id. `None` makes one (32 lower-case hex digits of 128 random bits). An
    /// application that must not apply a command twice across a restart supplies its own, derived
    /// from the business event (spec, decision 13).
    pub request_id: Option<RequestId>,
}

/// A successful answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// The id the request went under. Keep it to retry across a restart.
    pub request_id: RequestId,
    /// `true` if an earlier attempt got there first.
    pub replayed: bool,
    /// The result, as stored.
    pub result: Value,
}

/// Why a call or a connection did not succeed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The configuration asks for something unsafe (no TLS or no verification off loopback, or no pins).
    #[error("{0}")]
    Insecure(String),
    /// The server's key matched none of the pins. Nothing was sent, not even a hello.
    #[error("the server's key matches none of the pins")]
    PinMismatch,
    /// The login was refused (`unauthorized`, `locked_out`, ...). Never retried.
    #[error("login refused: {}", .0.message)]
    Login(WireError),
    /// The server answered the request with an error. For a business error, match on
    /// `error.name`; for the rest, on `error.kind`.
    #[error("{}", .0.message)]
    Refused(WireError),
    /// The last attempt got no answer, so the request may or may not have been applied. Send the
    /// same request id again later, or read the state.
    #[error("outcome unknown: no answer for request {request_id}")]
    OutcomeUnknown {
        /// The id to send again.
        request_id: RequestId,
    },
    /// The server could not be reached, and nothing was sent.
    #[error("cannot reach the server: {0}")]
    Unreachable(String),
    /// The call itself is not valid (for example larger than the server accepts). Nothing was sent.
    #[error("{0}")]
    Invalid(Error),
    /// The server broke the protocol.
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl ClientError {
    /// The server's error object, for a [`ClientError::Refused`] or [`ClientError::Login`].
    #[must_use]
    pub fn wire(&self) -> Option<&WireError> {
        match self {
            ClientError::Refused(w) | ClientError::Login(w) => Some(w),
            _ => None,
        }
    }

    /// The kind of the server's error, if there is one.
    #[must_use]
    pub fn kind(&self) -> Option<ErrorKind> {
        self.wire().map(|w| w.kind)
    }
}

// ---------------------------------------------------------------------------------------------
// Key pinning
// ---------------------------------------------------------------------------------------------

/// Reads one DER element at the start of `input`: its tag, its content and the bytes after it.
fn der_element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let len = rest[..count]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
        (len, &rest[count..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// The whole `SubjectPublicKeyInfo` element (header included) of a DER X.509 certificate.
///
/// `Certificate ::= SEQUENCE { tbsCertificate, ... }` and the TBS part is
/// `[0] version OPTIONAL, serial, signature, issuer, validity, subject, subjectPublicKeyInfo, ...`.
fn spki_of(cert: &[u8]) -> Option<&[u8]> {
    let (tag, certificate, _) = der_element(cert)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, mut tbs, _) = der_element(certificate)?;
    if tag != 0x30 {
        return None;
    }
    // The optional explicit version.
    if tbs.first() == Some(&0xa0) {
        let (_, _, rest) = der_element(tbs)?;
        tbs = rest;
    }
    // Serial, signature algorithm, issuer, validity, subject.
    for _ in 0..5 {
        let (_, _, rest) = der_element(tbs)?;
        tbs = rest;
    }
    let (tag, content, _) = der_element(tbs)?;
    if tag != 0x30 {
        return None;
    }
    // The element as it stands, header and all: it is the first `tbs.len() - rest.len()` bytes.
    let total = tbs.len() - der_element(tbs).map(|(_, _, rest)| rest.len())?;
    let _ = content;
    tbs.get(..total)
}

/// Accepts a certificate if its key is pinned; the name and the dates do not matter (spec,
/// decision 2: the pin covers a man in the middle, and a renewal with the same key breaks nobody).
#[derive(Debug)]
struct PinVerifier {
    pins: Vec<KeyPin>,
    provider: Arc<CryptoProvider>,
    failed: Arc<AtomicBool>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match KeyPin::of_certificate(end_entity) {
            Some(pin) if self.pins.contains(&pin) => Ok(ServerCertVerified::assertion()),
            _ => {
                self.failed.store(true, Ordering::SeqCst);
                Err(rustls::Error::General(
                    "the server's key matches none of the pins".to_string(),
                ))
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not offered".to_string()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Accepts any certificate. Built only for a loopback address ([`Security::InsecureNoVerify`]).
#[derive(Debug)]
struct NoVerify {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not offered".to_string()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// How to open a transport, worked out once from the configuration.
struct Transport {
    addr: SocketAddr,
    tls: Option<(TlsConnector, ServerName<'static>)>,
    pin_failed: Arc<AtomicBool>,
}

impl Transport {
    fn new(cfg: &ClientConfig) -> Result<Self, ClientError> {
        let pin_failed = Arc::new(AtomicBool::new(false));
        let loopback_only = |what: &str| {
            if cfg.addr.ip().is_loopback() {
                Ok(())
            } else {
                Err(ClientError::Insecure(format!(
                    "{what} is only allowed for a loopback address, not {}",
                    cfg.addr
                )))
            }
        };
        let verifier: Arc<dyn ServerCertVerifier> = match &cfg.security {
            Security::PlainLoopback => {
                loopback_only("a connection without TLS")?;
                return Ok(Transport {
                    addr: cfg.addr,
                    tls: None,
                    pin_failed,
                });
            }
            Security::InsecureNoVerify => {
                loopback_only("`insecure_no_verify`")?;
                Arc::new(NoVerify {
                    provider: provider(),
                })
            }
            Security::Pinned(pins) => {
                if pins.is_empty() {
                    return Err(ClientError::Insecure(
                        "a client with no pins refuses to connect (pin the server's key, or use \
                         `insecure_no_verify` on a loopback address)"
                            .to_string(),
                    ));
                }
                Arc::new(PinVerifier {
                    pins: pins.clone(),
                    provider: provider(),
                    failed: Arc::clone(&pin_failed),
                })
            }
        };
        let config = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| ClientError::Insecure(format!("TLS is not usable: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        let name = ServerName::try_from(cfg.server_name.clone()).map_err(|_| {
            ClientError::Insecure(format!("`{}` is not a valid server name", cfg.server_name))
        })?;
        Ok(Transport {
            addr: cfg.addr,
            tls: Some((TlsConnector::from(Arc::new(config)), name)),
            pin_failed,
        })
    }

    async fn open(&self) -> Result<Box<dyn Io>, ClientError> {
        let tcp = TcpStream::connect(self.addr)
            .await
            .map_err(|e| ClientError::Unreachable(e.to_string()))?;
        // Small frames, and a reply must not wait for a full segment.
        let _ = tcp.set_nodelay(true);
        match &self.tls {
            None => Ok(Box::new(tcp)),
            Some((connector, name)) => match connector.connect(name.clone(), tcp).await {
                Ok(stream) => Ok(Box::new(stream)),
                Err(_) if self.pin_failed.load(Ordering::SeqCst) => Err(ClientError::PinMismatch),
                Err(e) => Err(ClientError::Unreachable(format!("TLS: {e}"))),
            },
        }
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

// ---------------------------------------------------------------------------------------------
// One connection
// ---------------------------------------------------------------------------------------------

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// What the reader hands to a waiting call.
enum Event {
    Reply(ReplyBody),
    Lost,
}

/// The part of a connection that its tasks share.
struct Shared {
    writer: AsyncMutex<WriteHalf<Box<dyn Io>>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Event>>>,
    dead: AtomicBool,
    draining: AtomicBool,
    max_frame: u32,
}

impl Shared {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Event>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ends the connection for good and tells every open request that its answer is lost.
    fn lose(&self) {
        self.dead.store(true, Ordering::SeqCst);
        let waiting: Vec<_> = self.pending().drain().map(|(_, tx)| tx).collect();
        for tx in waiting {
            let _ = tx.send(Event::Lost);
        }
    }

    async fn send(&self, msg: &ClientMessage) -> Result<(), ()> {
        let body = JsonCodec.encode_client(msg);
        let bytes = frame::encode(&body, self.max_frame).map_err(|_| ())?;
        let mut writer = self.writer.lock().await;
        let written = timeout(WRITE_TIMEOUT, async {
            writer.write_all(&bytes).await?;
            writer.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => Ok(()),
            _ => {
                drop(writer);
                self.lose();
                Err(())
            }
        }
    }
}

/// Removes a request from the pending map when its call stops waiting (an answer, a timeout, or
/// the application dropping the future), so an abandoned tag does not stay behind.
struct Waiting<'a> {
    shared: &'a Shared,
    tag: u64,
    _permit: OwnedSemaphorePermit,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.shared.pending().remove(&self.tag);
    }
}

struct Conn {
    shared: Arc<Shared>,
    next_tag: AtomicU64,
    inflight: Arc<Semaphore>,
    /// The server's clock minus ours, in milliseconds, when the server said hello.
    offset_ms: i64,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.shared.dead.store(true, Ordering::SeqCst);
    }
}

fn local_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A frame reader over any stream: bytes in, whole bodies out.
struct Reader<R> {
    io: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    /// The next frame's body; `None` when the stream ended; an error text when it is broken.
    async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        loop {
            match frame::decode(&self.buf, MAX_SERVER_FRAME) {
                Ok(Some((body, used))) => {
                    let body = body.to_vec();
                    self.buf.drain(..used);
                    return Ok(Some(body));
                }
                Ok(None) => {}
                Err(e) => return Err(e.to_string()),
            }
            let mut chunk = vec![0u8; 16 * 1024];
            match self.io.read(&mut chunk).await {
                Ok(0) | Err(_) => return Ok(None),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }
}

fn server_limits() -> Limits {
    Limits {
        max_frame: MAX_SERVER_FRAME,
        max_depth: 64,
        max_keys: u32::MAX,
        max_string: u32::MAX,
    }
}

fn decode_server(body: &[u8]) -> Result<ServerMessage, ClientError> {
    JsonCodec
        .decode_server(body, &server_limits())
        .map_err(|e| ClientError::Protocol(e.to_string()))
}

/// The login on a fresh stream: hello, the three SCRAM steps, ready. Returns what the server said.
async fn handshake(
    io: Box<dyn Io>,
    cfg: &ClientConfig,
) -> Result<
    (
        ServerHello,
        Reader<ReadHalf<Box<dyn Io>>>,
        WriteHalf<Box<dyn Io>>,
    ),
    ClientError,
> {
    let (read, mut write) = tokio::io::split(io);
    let mut reader = Reader {
        io: read,
        buf: Vec::new(),
    };
    async fn send(w: &mut WriteHalf<Box<dyn Io>>, msg: &ClientMessage) -> Result<(), ClientError> {
        let bytes = frame::encode(&JsonCodec.encode_client(msg), u32::MAX)
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        w.write_all(&bytes)
            .await
            .and(w.flush().await)
            .map_err(|e| ClientError::Unreachable(e.to_string()))
    }
    async fn recv(r: &mut Reader<ReadHalf<Box<dyn Io>>>) -> Result<ServerMessage, ClientError> {
        match r.next().await {
            Ok(Some(body)) => decode_server(&body),
            Ok(None) => Err(ClientError::Unreachable(
                "the server closed the connection during login".to_string(),
            )),
            Err(e) => Err(ClientError::Protocol(e)),
        }
    }
    let wire_failure = |error: WireError| match error.kind {
        // The gateway is full, not unwilling: let the caller wait the hint and try again.
        ErrorKind::Overloaded | ErrorKind::RateLimited => ClientError::Refused(error),
        _ => ClientError::Login(error),
    };

    send(
        &mut write,
        &ClientMessage::Hello {
            max_version: frame::VERSION,
            client: CLIENT_NAME.to_string(),
        },
    )
    .await?;
    let hello = match recv(&mut reader).await? {
        ServerMessage::Hello(h) => h,
        ServerMessage::Error { error } => return Err(wire_failure(error)),
        other => {
            return Err(ClientError::Protocol(format!(
                "expected hello, got {other:?}"
            )))
        }
    };
    if hello.version != frame::VERSION {
        return Err(ClientError::Protocol(format!(
            "the server speaks version {}, this client speaks {}",
            hello.version,
            frame::VERSION
        )));
    }
    if !hello.mechanisms.iter().any(|m| m == MECHANISM) {
        return Err(ClientError::Protocol(format!(
            "the server offers no {MECHANISM}"
        )));
    }

    let nonce = scram::random_nonce().map_err(|e| ClientError::Protocol(e.to_string()))?;
    let (state, first) = scram::Client::start(&cfg.login, &cfg.password, nonce)
        .map_err(|e| ClientError::Protocol(e.to_string()))?;
    send(
        &mut write,
        &ClientMessage::Auth {
            step: ClientStep::ClientFirst,
            mechanism: Some(MECHANISM.to_string()),
            message: first,
        },
    )
    .await?;
    let server_first = match recv(&mut reader).await? {
        ServerMessage::Auth {
            step: ServerStep::ServerFirst,
            message,
        } => message,
        ServerMessage::Error { error } => return Err(wire_failure(error)),
        other => {
            return Err(ClientError::Protocol(format!(
                "expected server_first, got {other:?}"
            )))
        }
    };
    let (state, client_final) = state
        .respond(&server_first)
        .map_err(|e| ClientError::Protocol(e.to_string()))?;
    send(
        &mut write,
        &ClientMessage::Auth {
            step: ClientStep::ClientFinal,
            mechanism: None,
            message: client_final,
        },
    )
    .await?;
    match recv(&mut reader).await? {
        ServerMessage::Auth {
            step: ServerStep::ServerFinal,
            message,
        } => state.verify(&message).map_err(|e| {
            ClientError::Protocol(format!("the server failed to prove itself: {e}"))
        })?,
        ServerMessage::Error { error } => return Err(wire_failure(error)),
        other => {
            return Err(ClientError::Protocol(format!(
                "expected server_final, got {other:?}"
            )))
        }
    }
    match recv(&mut reader).await? {
        ServerMessage::Ready => Ok((hello, reader, write)),
        ServerMessage::Error { error } => Err(wire_failure(error)),
        other => Err(ClientError::Protocol(format!(
            "expected ready, got {other:?}"
        ))),
    }
}

impl Conn {
    async fn open(transport: &Transport, cfg: &ClientConfig) -> Result<Conn, ClientError> {
        let attempt = async {
            let io = transport.open().await?;
            handshake(io, cfg).await
        };
        let (hello, mut reader, writer) =
            timeout(HANDSHAKE_TIMEOUT, attempt).await.map_err(|_| {
                ClientError::Unreachable("the login did not finish in time".to_string())
            })??;
        let offset_ms = i64::try_from(hello.now_ms)
            .unwrap_or(i64::MAX)
            .saturating_sub(i64::try_from(local_millis()).unwrap_or(i64::MAX));
        let shared = Arc::new(Shared {
            writer: AsyncMutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
            draining: AtomicBool::new(false),
            max_frame: hello.max_frame,
        });

        let read_shared = Arc::clone(&shared);
        let read_task = tokio::spawn(async move {
            while let Ok(Some(body)) = reader.next().await {
                match JsonCodec.decode_server(&body, &server_limits()) {
                    Ok(ServerMessage::Reply { tag, body }) => {
                        let waiter = read_shared.pending().remove(&tag);
                        if let Some(tx) = waiter {
                            let _ = tx.send(Event::Reply(body));
                        }
                    }
                    Ok(ServerMessage::Shutdown { .. }) => {
                        read_shared.draining.store(true, Ordering::SeqCst);
                    }
                    Ok(ServerMessage::Pong) => {}
                    // An `error` frame, a message out of place, or garbage: the connection is over.
                    _ => break,
                }
            }
            read_shared.lose();
        });

        let ping_shared = Arc::clone(&shared);
        let ping_task = tokio::spawn(async move {
            loop {
                sleep(PING_EVERY).await;
                if ping_shared.dead.load(Ordering::SeqCst)
                    || ping_shared.send(&ClientMessage::Ping).await.is_err()
                {
                    break;
                }
            }
        });

        Ok(Conn {
            next_tag: AtomicU64::new(1),
            inflight: Arc::new(Semaphore::new(
                usize::try_from(hello.max_inflight).unwrap_or(1).max(1),
            )),
            offset_ms,
            shared,
            tasks: vec![read_task, ping_task],
        })
    }

    fn usable(&self) -> bool {
        !self.shared.dead.load(Ordering::SeqCst) && !self.shared.draining.load(Ordering::SeqCst)
    }

    /// The server's idea of now, from our clock and the offset the hello gave (spec, decision 12).
    fn corrected_now(&self) -> Timestamp {
        let corrected = i128::from(local_millis()) + i128::from(self.offset_ms);
        Timestamp::from_unix_millis(u64::try_from(corrected.max(0)).unwrap_or(u64::MAX))
    }

    /// Sends one request and waits for its reply.
    async fn request(&self, request: Request, wait: Duration) -> Attempt {
        let Ok(permit) = Arc::clone(&self.inflight).acquire_owned().await else {
            return Attempt::NotSent;
        };
        let tag = self.next_tag.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        if self.shared.dead.load(Ordering::SeqCst) {
            return Attempt::NotSent;
        }
        self.shared.pending().insert(tag, tx);
        let _waiting = Waiting {
            shared: &self.shared,
            tag,
            _permit: permit,
        };
        let msg = ClientMessage::Request(Box::new(Request { tag, ..request }));
        if self.shared.send(&msg).await.is_err() {
            // Part of the frame may have gone out, so the outcome is unknown.
            return Attempt::Lost { sent: true };
        }
        match timeout(wait, rx).await {
            Ok(Ok(Event::Reply(body))) => Attempt::Answered(body),
            Ok(Ok(Event::Lost) | Err(_)) => Attempt::Lost { sent: true },
            Err(_) => Attempt::TimedOut,
        }
    }
}

/// What one attempt on one connection came to.
enum Attempt {
    Answered(ReplyBody),
    /// The connection went away (or never could carry the request).
    Lost {
        sent: bool,
    },
    TimedOut,
    /// The frame was never written.
    NotSent,
}

// ---------------------------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------------------------

/// A client of one gateway: one login, one connection at a time, many requests in flight.
pub struct Client {
    cfg: ClientConfig,
    transport: Transport,
    conn: AsyncMutex<Option<Arc<Conn>>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("addr", &self.cfg.addr)
            .field("login", &self.cfg.login)
            .finish_non_exhaustive()
    }
}

/// A fresh request id: 32 lower-case hex digits of 128 random bits.
///
/// # Errors
/// [`ClientError::Protocol`] if the operating system gives no randomness.
pub fn new_request_id() -> Result<RequestId, ClientError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| ClientError::Protocol(format!("no randomness: {e}")))?;
    RequestId::new(hex(&bytes)).map_err(ClientError::Invalid)
}

/// A random number in `[0, 1)` for the retry jitter.
fn random_unit() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.5;
    }
    // 53 random bits make an exactly representable fraction.
    let bits = u64::from_le_bytes(bytes) >> 11;
    // `bits` is below 2^53, so the conversion is exact.
    #[allow(clippy::cast_precision_loss)]
    let unit = bits as f64 / 9_007_199_254_740_992.0;
    unit
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

impl Client {
    /// Opens the connection and logs in. A wrong pin ([`ClientError::PinMismatch`]) or a refused
    /// login ([`ClientError::Login`]) is reported here, before any request.
    ///
    /// # Errors
    /// [`ClientError`]: the configuration, the pin, the login, or the network.
    pub async fn connect(cfg: ClientConfig) -> Result<Client, ClientError> {
        let transport = Transport::new(&cfg)?;
        let conn = Conn::open(&transport, &cfg).await?;
        Ok(Client {
            cfg,
            transport,
            conn: AsyncMutex::new(Some(Arc::new(conn))),
        })
    }

    /// The offset of the server's clock from ours, in milliseconds, as the last hello said.
    /// `None` if there is no connection right now.
    pub async fn clock_offset_ms(&self) -> Option<i64> {
        self.conn.lock().await.as_ref().map(|c| c.offset_ms)
    }

    /// A usable connection, opening one if the last was lost or is draining.
    async fn connection(&self) -> Result<Arc<Conn>, ClientError> {
        let mut slot = self.conn.lock().await;
        if let Some(conn) = slot.as_ref() {
            if conn.usable() {
                return Ok(Arc::clone(conn));
            }
        }
        *slot = None;
        let conn = Arc::new(Conn::open(&self.transport, &self.cfg).await?);
        *slot = Some(Arc::clone(&conn));
        Ok(conn)
    }

    /// Runs a recipe and returns its final outcome, retrying by the table of spec section 5.
    ///
    /// The request id and `created_at_ms` are fixed on the first attempt and reused by every retry,
    /// so the write path applies the command once.
    ///
    /// # Errors
    /// [`ClientError::Refused`] with the server's error (a business error such as
    /// `insufficient_funds` is final and replays), [`ClientError::OutcomeUnknown`] if the last
    /// attempt got no answer, or a connection or login error.
    pub async fn execute(&self, call: Call) -> Result<Reply, ClientError> {
        let request_id = match call.request_id.clone() {
            Some(id) => id,
            None => new_request_id()?,
        };
        let started = Instant::now();
        let mut retry = Retry::new(self.cfg.max_retry);
        let mut created_at: Option<Timestamp> = None;
        let mut maybe_applied = false;
        let mut last_error: Option<WireError> = None;

        loop {
            let seen_and_conn = match self.connection().await {
                Ok(conn) => Ok(conn),
                Err(ClientError::Unreachable(why)) => Err((Seen::ConnectionLost, Some(why), None)),
                // A full gateway: wait its hint like any `overloaded`.
                Err(ClientError::Refused(wire)) => Err((
                    Seen::Error {
                        kind: wire.kind,
                        retry_after_ms: wire.retry_after_ms,
                    },
                    None,
                    Some(wire),
                )),
                Err(fatal) => return Err(fatal),
            };

            let (seen, wire, reconnect) = match seen_and_conn {
                Err((seen, why, wire)) => {
                    last_error = wire.clone();
                    let _ = why;
                    (seen, wire, false)
                }
                Ok(conn) => {
                    let created = *created_at.get_or_insert_with(|| conn.corrected_now());
                    let request = Request {
                        tag: 0,
                        request_id: request_id.clone(),
                        created_at: created,
                        recipe: call.recipe.clone(),
                        version: call.version,
                        subject: call.subject.clone(),
                        counterparty: call.counterparty.clone(),
                        args: call.args.clone(),
                        map_version: None,
                    };
                    // Refuse a request the server would refuse, before it goes out.
                    let size = JsonCodec
                        .encode_client(&ClientMessage::Request(Box::new(request.clone())))
                        .len()
                        + 1;
                    if u32::try_from(size).map_or(true, |s| s > conn.shared.max_frame) {
                        return Err(ClientError::Invalid(Error::BadArguments(format!(
                            "the request is {size} bytes; the server accepts frames of at most {}",
                            conn.shared.max_frame
                        ))));
                    }
                    match conn.request(request, self.cfg.call_timeout).await {
                        Attempt::Answered(ReplyBody::Ok { replayed, result }) => {
                            return Ok(Reply {
                                request_id,
                                replayed,
                                result,
                            });
                        }
                        Attempt::Answered(ReplyBody::Err { error }) => {
                            last_error = Some(error.clone());
                            (
                                Seen::Error {
                                    kind: error.kind,
                                    retry_after_ms: error.retry_after_ms,
                                },
                                Some(error),
                                false,
                            )
                        }
                        Attempt::Lost { sent } => {
                            maybe_applied |= sent;
                            (Seen::ConnectionLost, None, true)
                        }
                        Attempt::TimedOut => {
                            // Drop this connection: a late reply must not be confused with a later one.
                            maybe_applied = true;
                            conn.shared.lose();
                            (Seen::Timeout, None, true)
                        }
                        Attempt::NotSent => (Seen::ConnectionLost, None, true),
                    }
                }
            };
            let _ = reconnect;

            match retry.decide(seen, started.elapsed(), random_unit()) {
                Decision::Do(Step::Return) => {
                    return Err(match wire {
                        Some(w) => ClientError::Refused(w),
                        None => ClientError::Protocol("no answer and no error".to_string()),
                    });
                }
                Decision::Do(Step::WaitThenSend(pause) | Step::ReconnectThenSend(pause)) => {
                    sleep(pause).await;
                }
                Decision::GiveUp(GaveUp::Answered) => {
                    return Err(match wire.or(last_error) {
                        Some(w) => ClientError::Refused(w),
                        None => ClientError::Unreachable("gave up retrying".to_string()),
                    });
                }
                Decision::GiveUp(GaveUp::OutcomeUnknown) => {
                    return Err(if maybe_applied {
                        ClientError::OutcomeUnknown { request_id }
                    } else {
                        ClientError::Unreachable(
                            "the server could not be reached in time; nothing was sent".to_string(),
                        )
                    });
                }
            }
        }
    }

    /// Says goodbye: the server answers the requests still open, then closes. Calls made after this
    /// open a new connection.
    pub async fn close(&self) {
        let conn = self.conn.lock().await.take();
        if let Some(conn) = conn {
            let _ = conn.shared.send(&ClientMessage::Bye).await;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tests: the pure parts. The wire tests (W1, W6, W11, W14) run against a real gateway in
// `bhdata-server/tests`, because that is where the gateway and the client meet.
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Vec<u8>, KeyPin) {
        let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let spki = made.key_pair.public_key_der();
        let pin = KeyPin(Sha256::digest(&spki).into());
        (made.cert.der().to_vec(), pin)
    }

    #[test]
    fn the_pin_is_the_hash_of_the_certificates_public_key_info() {
        let (der, expected) = pair();
        assert_eq!(KeyPin::of_certificate(&der), Some(expected));
    }

    #[test]
    fn a_renewed_certificate_with_the_same_key_has_the_same_pin() {
        let key = rcgen::KeyPair::generate().unwrap();
        let one = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let two = rcgen::CertificateParams::new(vec!["other.example".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        assert_ne!(one.der().as_ref(), two.der().as_ref());
        assert_eq!(
            KeyPin::of_certificate(one.der()),
            KeyPin::of_certificate(two.der())
        );
    }

    #[test]
    fn different_keys_have_different_pins() {
        assert_ne!(
            KeyPin::of_certificate(&pair().0),
            KeyPin::of_certificate(&pair().0)
        );
    }

    #[test]
    fn bytes_that_are_not_a_certificate_have_no_pin() {
        for junk in [
            &[][..],
            &[0x30],
            &[0x30, 0x84, 0xff, 0xff, 0xff, 0xff],
            b"hello",
        ] {
            assert_eq!(KeyPin::of_certificate(junk), None, "{junk:?}");
        }
        let (der, _) = pair();
        assert_eq!(KeyPin::of_certificate(&der[..der.len() / 2]), None);
    }

    #[test]
    fn a_pin_reads_and_writes_hex() {
        let (_, pin) = pair();
        assert_eq!(KeyPin::from_hex(&pin.to_hex()), Ok(pin));
        assert_eq!(KeyPin::from_hex(&pin.to_hex().to_uppercase()), Ok(pin));
        for bad in ["", "abc", &"zz".repeat(32), &"a".repeat(63)] {
            assert!(KeyPin::from_hex(bad).is_err(), "{bad}");
        }
    }

    fn config(addr: &str, security: Security) -> ClientConfig {
        ClientConfig::new(addr.parse().unwrap(), "u", "p", security)
    }

    #[test]
    fn nothing_unsafe_is_allowed_off_loopback() {
        for security in [Security::PlainLoopback, Security::InsecureNoVerify] {
            let e = Transport::new(&config("203.0.113.7:7000", security)).err();
            assert!(matches!(e, Some(ClientError::Insecure(_))), "{e:?}");
            assert!(Transport::new(&config("127.0.0.1:7000", Security::PlainLoopback)).is_ok());
        }
        assert!(Transport::new(&config("[::1]:7000", Security::InsecureNoVerify)).is_ok());
    }

    #[test]
    fn a_client_with_no_pins_refuses_to_connect() {
        let e = Transport::new(&config("127.0.0.1:7000", Security::Pinned(Vec::new()))).err();
        assert!(matches!(e, Some(ClientError::Insecure(m)) if m.contains("no pins")));
    }

    #[test]
    fn a_request_id_is_32_lower_case_hex_digits_and_never_repeats() {
        let a = new_request_id().unwrap();
        let b = new_request_id().unwrap();
        assert_ne!(a, b);
        for id in [a, b] {
            assert_eq!(id.as_str().len(), 32);
            assert!(id
                .as_str()
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
        }
    }

    #[test]
    fn the_random_unit_stays_in_range() {
        for _ in 0..1000 {
            let u = random_unit();
            assert!((0.0..1.0).contains(&u), "{u}");
        }
    }
}
