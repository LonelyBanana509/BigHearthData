//! The client and server messages of wire-protocol spec section 4, and the [`Codec`] trait. Lane P2.
//!
//! A body is one JSON object with a `type`. A field that the spec does not list is refused, a listed
//! field of the wrong type is refused, and so is a missing one, with the exceptions the spec marks as
//! "or null": `version`, `counterparty` and `map_version` of a `request` may be absent, which means
//! null. Everything here is pure: bytes in, messages out, and back.

use std::collections::BTreeMap;

use bhdata_types::{RecipeVersion, RequestId, SubjectId, Timestamp};

use crate::error_map::WireError;
use crate::frame::{FrameError, Limits};
use crate::json::{self, JsonError, Value};

/// The one login mechanism of version 1 (the same text as [`crate::scram::MECHANISM`]).
pub use crate::scram::MECHANISM;

/// Longest `client` text of a `hello`, in bytes.
pub const MAX_CLIENT_NAME: usize = 64;

/// Why bytes are not a message. Every one of these closes the connection (spec, decision 4).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtoError {
    /// The frame layer refused it.
    #[error("{0}")]
    Frame(#[from] FrameError),
    /// The JSON layer refused it.
    #[error("{0}")]
    Json(#[from] JsonError),
    /// Well-formed JSON that is not a valid message.
    #[error("bad message: {0}")]
    Message(String),
}

pub(crate) fn bad<T>(text: impl Into<String>) -> Result<T, ProtoError> {
    Err(ProtoError::Message(text.into()))
}

/// The steps of the client's side of the login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientStep {
    /// The first SCRAM message.
    ClientFirst,
    /// The proof.
    ClientFinal,
}

/// The steps of the server's side of the login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerStep {
    /// The salt, the iteration count and the nonce.
    ServerFirst,
    /// The server's proof.
    ServerFinal,
}

/// One `request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The client's correlation tag, unique among its open requests.
    pub tag: u64,
    /// The client's request id.
    pub request_id: RequestId,
    /// When the client created the request (`created_at_ms`).
    pub created_at: Timestamp,
    /// The recipe name.
    pub recipe: String,
    /// The version named, or `None` for the latest.
    pub version: Option<RecipeVersion>,
    /// The subject's shard key.
    pub subject: SubjectId,
    /// The counterparty's shard key.
    pub counterparty: Option<SubjectId>,
    /// The arguments, always a JSON object.
    pub args: BTreeMap<String, Value>,
    /// The shard-map version. Accepted and ignored at stage 0.
    pub map_version: Option<u64>,
}

/// A message from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    /// The first frame.
    Hello {
        /// The highest version the client speaks.
        max_version: u8,
        /// The client's name, for logs.
        client: String,
    },
    /// One step of the login.
    Auth {
        /// Which step.
        step: ClientStep,
        /// The mechanism; present on `client_first` only.
        mechanism: Option<String>,
        /// The SCRAM text.
        message: String,
    },
    /// A request.
    Request(Box<Request>),
    /// Keeps the idle timer from firing.
    Ping,
    /// A clean close; open requests are still answered first.
    Bye,
}

/// The server's `hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    /// The version chosen.
    pub version: u8,
    /// The server's name, for logs.
    pub server: String,
    /// The server's clock, in milliseconds since 1970 (spec, decision 12).
    pub now_ms: u64,
    /// The largest frame the client may send after login.
    pub max_frame: u32,
    /// Requests the client may have open.
    pub max_inflight: u32,
    /// Seconds without a frame after which the server closes the connection.
    pub idle_timeout_secs: u64,
    /// The login mechanisms offered.
    pub mechanisms: Vec<String>,
}

/// What a `reply` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyBody {
    /// The command is done.
    Ok {
        /// `true` if an earlier attempt got there first.
        replayed: bool,
        /// The result, as stored.
        result: Value,
    },
    /// The command failed, or was refused.
    Err {
        /// The error.
        error: WireError,
    },
}

/// A message from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    /// The answer to `hello`.
    Hello(ServerHello),
    /// One step of the login.
    Auth {
        /// Which step.
        step: ServerStep,
        /// The SCRAM text.
        message: String,
    },
    /// Login is done; requests may follow.
    Ready,
    /// The answer to one request.
    Reply {
        /// The tag of the request.
        tag: u64,
        /// What came back.
        body: ReplyBody,
    },
    /// The answer to `ping`.
    Pong,
    /// The server is stopping.
    Shutdown {
        /// How long it waits for open requests.
        drain_ms: u64,
    },
    /// A connection-level failure; the connection is closed after it.
    Error {
        /// The error.
        error: WireError,
    },
}

/// Turns bodies into messages and back. Version 1 is [`JsonCodec`]; a binary body is a later version.
pub trait Codec {
    /// A client message, as the server reads it.
    ///
    /// # Errors
    /// [`ProtoError`] if the bytes are not exactly one valid client message under `limits`.
    fn decode_client(&self, body: &[u8], limits: &Limits) -> Result<ClientMessage, ProtoError>;
    /// A client message, as the client writes it.
    fn encode_client(&self, msg: &ClientMessage) -> Vec<u8>;
    /// A server message, as the client reads it.
    ///
    /// # Errors
    /// [`ProtoError`] if the bytes are not exactly one valid server message under `limits`.
    fn decode_server(&self, body: &[u8], limits: &Limits) -> Result<ServerMessage, ProtoError>;
    /// A server message, as the server writes it.
    fn encode_server(&self, msg: &ServerMessage) -> Vec<u8>;
}

/// Version 1: a UTF-8 JSON object.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonCodec;

impl Codec for JsonCodec {
    fn decode_client(&self, body: &[u8], limits: &Limits) -> Result<ClientMessage, ProtoError> {
        client_from_value(json::parse(body, limits)?)
    }

    fn encode_client(&self, msg: &ClientMessage) -> Vec<u8> {
        json::to_string(&client_to_value(msg)).into_bytes()
    }

    fn decode_server(&self, body: &[u8], limits: &Limits) -> Result<ServerMessage, ProtoError> {
        server_from_value(json::parse(body, limits)?)
    }

    fn encode_server(&self, msg: &ServerMessage) -> Vec<u8> {
        json::to_string(&server_to_value(msg)).into_bytes()
    }
}

/// The fields of one JSON object, taken one by one; what is left over is an unknown field.
pub(crate) struct Fields {
    map: BTreeMap<String, Value>,
    what: String,
}

impl Fields {
    pub(crate) fn new(v: Value, what: &str) -> Result<Fields, ProtoError> {
        match v {
            Value::Obj(map) => Ok(Fields {
                map,
                what: what.to_string(),
            }),
            _ => bad(format!("{what} must be a JSON object")),
        }
    }

    pub(crate) fn value(&mut self, key: &str) -> Result<Value, ProtoError> {
        match self.map.remove(key) {
            Some(v) => Ok(v),
            None => bad(format!("{} has no `{key}`", self.what)),
        }
    }

    pub(crate) fn string(&mut self, key: &str) -> Result<String, ProtoError> {
        match self.value(key)? {
            Value::Str(s) => Ok(s),
            _ => bad(format!("`{key}` of {} must be text", self.what)),
        }
    }

    /// Text, or `None` if the field is absent or null.
    pub(crate) fn opt_string(&mut self, key: &str) -> Result<Option<String>, ProtoError> {
        match self.map.remove(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Str(s)) => Ok(Some(s)),
            Some(_) => bad(format!("`{key}` of {} must be text or null", self.what)),
        }
    }

    pub(crate) fn u64(&mut self, key: &str) -> Result<u64, ProtoError> {
        match self.value(key)? {
            Value::Int(n) => match u64::try_from(n) {
                Ok(n) => Ok(n),
                Err(_) => bad(format!("`{key}` of {} must not be negative", self.what)),
            },
            _ => bad(format!("`{key}` of {} must be a whole number", self.what)),
        }
    }

    /// A number, or `None` if the field is absent or null.
    pub(crate) fn opt_u64(&mut self, key: &str) -> Result<Option<u64>, ProtoError> {
        match self.map.remove(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Int(n)) => match u64::try_from(n) {
                Ok(n) => Ok(Some(n)),
                Err(_) => bad(format!("`{key}` of {} must not be negative", self.what)),
            },
            Some(_) => bad(format!(
                "`{key}` of {} must be a whole number or null",
                self.what
            )),
        }
    }

    pub(crate) fn bool(&mut self, key: &str) -> Result<bool, ProtoError> {
        match self.value(key)? {
            Value::Bool(b) => Ok(b),
            _ => bad(format!("`{key}` of {} must be true or false", self.what)),
        }
    }

    pub(crate) fn finish(self) -> Result<(), ProtoError> {
        match self.map.keys().next() {
            None => Ok(()),
            Some(extra) => bad(format!("{} has an unknown field `{extra}`", self.what)),
        }
    }
}

fn small<T: TryFrom<u64>>(n: u64, key: &str, what: &str) -> Result<T, ProtoError> {
    T::try_from(n).or_else(|_| bad(format!("`{key}` of {what} is too large")))
}

fn id<T, E: std::fmt::Display>(made: Result<T, E>, key: &str, what: &str) -> Result<T, ProtoError> {
    made.or_else(|e| bad(format!("`{key}` of {what}: {e}")))
}

fn client_from_value(v: Value) -> Result<ClientMessage, ProtoError> {
    let mut f = Fields::new(v, "message")?;
    let ty = f.string("type")?;
    let msg = match ty.as_str() {
        "hello" => {
            let max_version = small(f.u64("max_version")?, "max_version", "hello")?;
            let client = f.string("client")?;
            if client.len() > MAX_CLIENT_NAME {
                return bad(format!("`client` of hello is over {MAX_CLIENT_NAME} bytes"));
            }
            ClientMessage::Hello {
                max_version,
                client,
            }
        }
        "auth" => {
            let step = match f.string("step")?.as_str() {
                "client_first" => ClientStep::ClientFirst,
                "client_final" => ClientStep::ClientFinal,
                other => return bad(format!("auth step `{other}` is not a client step")),
            };
            let mechanism = f.opt_string("mechanism")?;
            match (step, &mechanism) {
                (ClientStep::ClientFirst, None) => {
                    return bad("auth `client_first` needs a `mechanism`")
                }
                (ClientStep::ClientFinal, Some(_)) => {
                    return bad("only auth `client_first` has a `mechanism`")
                }
                _ => {}
            }
            ClientMessage::Auth {
                step,
                mechanism,
                message: f.string("message")?,
            }
        }
        "request" => ClientMessage::Request(Box::new(request_from_fields(&mut f)?)),
        "ping" => ClientMessage::Ping,
        "bye" => ClientMessage::Bye,
        other => return bad(format!("unknown message type `{other}` from a client")),
    };
    f.finish()?;
    Ok(msg)
}

fn request_from_fields(f: &mut Fields) -> Result<Request, ProtoError> {
    const W: &str = "request";
    let tag = f.u64("tag")?;
    let request_id = id(RequestId::new(f.string("request_id")?), "request_id", W)?;
    let created_at = Timestamp::from_unix_millis(f.u64("created_at_ms")?);
    let recipe = f.string("recipe")?;
    if recipe.is_empty() {
        return bad("`recipe` of request must not be empty");
    }
    let version = match f.opt_u64("version")? {
        None => None,
        Some(0) => return bad("`version` of request must be 1 or more"),
        Some(n) => Some(RecipeVersion::new(small(n, "version", W)?)),
    };
    let subject = id(SubjectId::new(f.string("subject")?), "subject", W)?;
    let counterparty = match f.opt_string("counterparty")? {
        None => None,
        Some(s) => Some(id(SubjectId::new(s), "counterparty", W)?),
    };
    let args = match f.value("args")? {
        Value::Obj(map) => map,
        _ => return bad("`args` of request must be a JSON object"),
    };
    let map_version = f.opt_u64("map_version")?;
    Ok(Request {
        tag,
        request_id,
        created_at,
        recipe,
        version,
        subject,
        counterparty,
        args,
        map_version,
    })
}

fn typed(ty: &str, fields: Vec<(&str, Value)>) -> Value {
    let mut map: BTreeMap<String, Value> = fields
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    map.insert("type".to_string(), Value::str(ty));
    Value::Obj(map)
}

fn int(n: u64) -> Value {
    Value::Int(i64::try_from(n).unwrap_or(i64::MAX))
}

fn client_to_value(m: &ClientMessage) -> Value {
    match m {
        ClientMessage::Hello {
            max_version,
            client,
        } => typed(
            "hello",
            vec![
                ("max_version", int(u64::from(*max_version))),
                ("client", Value::str(client.as_str())),
            ],
        ),
        ClientMessage::Auth {
            step,
            mechanism,
            message,
        } => {
            let mut fields = vec![
                (
                    "step",
                    Value::str(match step {
                        ClientStep::ClientFirst => "client_first",
                        ClientStep::ClientFinal => "client_final",
                    }),
                ),
                ("message", Value::str(message.as_str())),
            ];
            if let Some(mech) = mechanism {
                fields.push(("mechanism", Value::str(mech.as_str())));
            }
            typed("auth", fields)
        }
        ClientMessage::Request(r) => typed(
            "request",
            vec![
                ("tag", int(r.tag)),
                ("request_id", Value::str(r.request_id.as_str())),
                ("created_at_ms", int(r.created_at.as_unix_millis())),
                ("recipe", Value::str(r.recipe.as_str())),
                (
                    "version",
                    r.version.map_or(Value::Null, |v| int(u64::from(v.get()))),
                ),
                ("subject", Value::str(r.subject.as_str())),
                (
                    "counterparty",
                    r.counterparty
                        .as_ref()
                        .map_or(Value::Null, |c| Value::str(c.as_str())),
                ),
                ("args", Value::Obj(r.args.clone())),
                ("map_version", r.map_version.map_or(Value::Null, int)),
            ],
        ),
        ClientMessage::Ping => typed("ping", vec![]),
        ClientMessage::Bye => typed("bye", vec![]),
    }
}

fn server_from_value(v: Value) -> Result<ServerMessage, ProtoError> {
    let mut f = Fields::new(v, "message")?;
    let ty = f.string("type")?;
    let msg = match ty.as_str() {
        "hello" => {
            let mechanisms = match f.value("mechanisms")? {
                Value::Arr(items) => items
                    .into_iter()
                    .map(|i| match i {
                        Value::Str(s) => Ok(s),
                        _ => bad("`mechanisms` of hello must be a list of text"),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return bad("`mechanisms` of hello must be a list"),
            };
            ServerMessage::Hello(ServerHello {
                version: small(f.u64("version")?, "version", "hello")?,
                server: f.string("server")?,
                now_ms: f.u64("now_ms")?,
                max_frame: small(f.u64("max_frame")?, "max_frame", "hello")?,
                max_inflight: small(f.u64("max_inflight")?, "max_inflight", "hello")?,
                idle_timeout_secs: f.u64("idle_timeout_secs")?,
                mechanisms,
            })
        }
        "auth" => {
            let step = match f.string("step")?.as_str() {
                "server_first" => ServerStep::ServerFirst,
                "server_final" => ServerStep::ServerFinal,
                other => return bad(format!("auth step `{other}` is not a server step")),
            };
            ServerMessage::Auth {
                step,
                message: f.string("message")?,
            }
        }
        "ready" => ServerMessage::Ready,
        "reply" => {
            let tag = f.u64("tag")?;
            let body = if f.bool("ok")? {
                ReplyBody::Ok {
                    replayed: f.bool("replayed")?,
                    result: f.value("result")?,
                }
            } else {
                ReplyBody::Err {
                    error: WireError::from_value(f.value("error")?)?,
                }
            };
            ServerMessage::Reply { tag, body }
        }
        "pong" => ServerMessage::Pong,
        "shutdown" => ServerMessage::Shutdown {
            drain_ms: f.u64("drain_ms")?,
        },
        "error" => ServerMessage::Error {
            error: WireError::from_value(f.value("error")?)?,
        },
        other => return bad(format!("unknown message type `{other}` from a server")),
    };
    f.finish()?;
    Ok(msg)
}

fn server_to_value(m: &ServerMessage) -> Value {
    match m {
        ServerMessage::Hello(h) => typed(
            "hello",
            vec![
                ("version", int(u64::from(h.version))),
                ("server", Value::str(h.server.as_str())),
                ("now_ms", int(h.now_ms)),
                ("max_frame", int(u64::from(h.max_frame))),
                ("max_inflight", int(u64::from(h.max_inflight))),
                ("idle_timeout_secs", int(h.idle_timeout_secs)),
                (
                    "mechanisms",
                    Value::Arr(
                        h.mechanisms
                            .iter()
                            .map(|s| Value::str(s.as_str()))
                            .collect(),
                    ),
                ),
            ],
        ),
        ServerMessage::Auth { step, message } => typed(
            "auth",
            vec![
                (
                    "step",
                    Value::str(match step {
                        ServerStep::ServerFirst => "server_first",
                        ServerStep::ServerFinal => "server_final",
                    }),
                ),
                ("message", Value::str(message.as_str())),
            ],
        ),
        ServerMessage::Ready => typed("ready", vec![]),
        ServerMessage::Reply { tag, body } => match body {
            ReplyBody::Ok { replayed, result } => typed(
                "reply",
                vec![
                    ("tag", int(*tag)),
                    ("ok", Value::Bool(true)),
                    ("replayed", Value::Bool(*replayed)),
                    ("result", result.clone()),
                ],
            ),
            ReplyBody::Err { error } => typed(
                "reply",
                vec![
                    ("tag", int(*tag)),
                    ("ok", Value::Bool(false)),
                    ("error", error.to_value()),
                ],
            ),
        },
        ServerMessage::Pong => typed("pong", vec![]),
        ServerMessage::Shutdown { drain_ms } => {
            typed("shutdown", vec![("drain_ms", int(*drain_ms))])
        }
        ServerMessage::Error { error } => typed("error", vec![("error", error.to_value())]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_map::{to_wire, WireContext};
    use crate::json::JsonErrorKind;
    use bhdata_types::config::GatewayConfig;
    use bhdata_types::Error;
    use std::time::Duration;

    fn after() -> Limits {
        Limits::after_login(&GatewayConfig::default())
    }

    fn before() -> Limits {
        Limits::before_login(&GatewayConfig::default())
    }

    fn client(text: &str) -> Result<ClientMessage, ProtoError> {
        JsonCodec.decode_client(text.as_bytes(), &after())
    }

    fn server(text: &str) -> Result<ServerMessage, ProtoError> {
        JsonCodec.decode_server(text.as_bytes(), &after())
    }

    fn request() -> Request {
        Request {
            tag: 7,
            request_id: RequestId::new("0123456789abcdef0123456789abcdef").unwrap(),
            created_at: Timestamp::from_unix_millis(1_700_000_000_123),
            recipe: "spend".into(),
            version: Some(RecipeVersion::new(3)),
            subject: SubjectId::new("player-1").unwrap(),
            counterparty: Some(SubjectId::new("shop-9").unwrap()),
            args: BTreeMap::from([
                ("amount".to_string(), Value::str("1.50")),
                ("n".to_string(), Value::Int(-4)),
                ("ok".to_string(), Value::Bool(true)),
            ]),
            map_version: Some(12),
        }
    }

    fn every_client_message() -> Vec<ClientMessage> {
        vec![
            ClientMessage::Hello {
                max_version: 1,
                client: "bhdata-rs/0.0".into(),
            },
            ClientMessage::Auth {
                step: ClientStep::ClientFirst,
                mechanism: Some(MECHANISM.into()),
                message: "n,,n=app,r=abc".into(),
            },
            ClientMessage::Auth {
                step: ClientStep::ClientFinal,
                mechanism: None,
                message: "c=biws,r=abcdef,p=xyz=".into(),
            },
            ClientMessage::Request(Box::new(request())),
            ClientMessage::Request(Box::new(Request {
                version: None,
                counterparty: None,
                map_version: None,
                args: BTreeMap::new(),
                ..request()
            })),
            ClientMessage::Ping,
            ClientMessage::Bye,
        ]
    }

    fn every_server_message() -> Vec<ServerMessage> {
        let ctx = WireContext {
            rate_limit_hint: Some(Duration::from_millis(40)),
            incident: Some("inc-1".into()),
        };
        vec![
            ServerMessage::Hello(ServerHello {
                version: 1,
                server: "bhdata/0.0".into(),
                now_ms: 1_700_000_000_000,
                max_frame: 1_048_576,
                max_inflight: 32,
                idle_timeout_secs: 60,
                mechanisms: vec![MECHANISM.into()],
            }),
            ServerMessage::Auth {
                step: ServerStep::ServerFirst,
                message: "r=abcdef,s=c2FsdA==,i=100000".into(),
            },
            ServerMessage::Auth {
                step: ServerStep::ServerFinal,
                message: "v=sig=".into(),
            },
            ServerMessage::Ready,
            ServerMessage::Reply {
                tag: 7,
                body: ReplyBody::Ok {
                    replayed: true,
                    result: Value::obj([("balance", Value::str("98.50"))]),
                },
            },
            ServerMessage::Reply {
                tag: 8,
                body: ReplyBody::Err {
                    error: to_wire(&Error::InsufficientFunds, &ctx),
                },
            },
            ServerMessage::Reply {
                tag: 9,
                body: ReplyBody::Err {
                    error: to_wire(&Error::RateLimited, &ctx),
                },
            },
            ServerMessage::Pong,
            ServerMessage::Shutdown { drain_ms: 5_000 },
            ServerMessage::Error {
                error: to_wire(&Error::LockedOut, &ctx),
            },
        ]
    }

    #[test]
    fn every_client_message_round_trips() {
        for m in every_client_message() {
            let bytes = JsonCodec.encode_client(&m);
            assert_eq!(JsonCodec.decode_client(&bytes, &after()).unwrap(), m);
        }
    }

    #[test]
    fn every_server_message_round_trips() {
        for m in every_server_message() {
            let bytes = JsonCodec.encode_server(&m);
            assert_eq!(JsonCodec.decode_server(&bytes, &after()).unwrap(), m);
        }
    }

    #[test]
    fn the_wire_text_is_the_one_in_the_spec() {
        let text = String::from_utf8(
            JsonCodec.encode_client(&ClientMessage::Request(Box::new(request()))),
        )
        .unwrap();
        assert_eq!(
            text,
            "{\"args\":{\"amount\":\"1.50\",\"n\":-4,\"ok\":true},\"counterparty\":\"shop-9\",\
             \"created_at_ms\":1700000000123,\"map_version\":12,\"recipe\":\"spend\",\
             \"request_id\":\"0123456789abcdef0123456789abcdef\",\"subject\":\"player-1\",\
             \"tag\":7,\"type\":\"request\",\"version\":3}"
        );
        assert_eq!(
            String::from_utf8(JsonCodec.encode_server(&ServerMessage::Pong)).unwrap(),
            "{\"type\":\"pong\"}"
        );
    }

    #[test]
    fn a_request_may_leave_out_its_or_null_fields() {
        let m = client(
            "{\"type\":\"request\",\"tag\":1,\"request_id\":\"r1\",\"created_at_ms\":5,\
             \"recipe\":\"x\",\"subject\":\"s\",\"args\":{}}",
        )
        .unwrap();
        let ClientMessage::Request(r) = m else {
            panic!("not a request")
        };
        assert_eq!(
            (r.version, r.counterparty, r.map_version),
            (None, None, None)
        );
        let m = client(
            "{\"type\":\"request\",\"tag\":1,\"request_id\":\"r1\",\"created_at_ms\":5,\
             \"recipe\":\"x\",\"subject\":\"s\",\"args\":{},\"version\":null,\
             \"counterparty\":null,\"map_version\":null}",
        )
        .unwrap();
        assert!(matches!(m, ClientMessage::Request(_)));
    }

    #[test]
    fn unknown_fields_and_types_are_refused_for_every_message() {
        for m in every_client_message() {
            let Value::Obj(mut map) = client_to_value(&m) else {
                unreachable!()
            };
            map.insert("surprise".into(), Value::Int(1));
            let text = json::to_string(&Value::Obj(map));
            assert!(client(&text).is_err(), "{text}");
        }
        for m in every_server_message() {
            let Value::Obj(mut map) = server_to_value(&m) else {
                unreachable!()
            };
            map.insert("surprise".into(), Value::Int(1));
            let text = json::to_string(&Value::Obj(map));
            assert!(server(&text).is_err(), "{text}");
        }
        assert!(client("{\"type\":\"launch\"}").is_err());
        assert!(client("{\"type\":\"ready\"}").is_err());
        assert!(server("{\"type\":\"request\"}").is_err());
        assert!(client("{\"type\":\"ping\",\"extra\":null}").is_err());
        assert!(client("{}").is_err());
        assert!(client("{\"type\":5}").is_err());
    }

    #[test]
    fn bodies_that_are_not_objects_are_refused() {
        for text in ["[]", "null", "5", "\"hello\"", "true"] {
            assert!(
                matches!(client(text), Err(ProtoError::Message(_))),
                "{text}"
            );
            assert!(
                matches!(server(text), Err(ProtoError::Message(_))),
                "{text}"
            );
        }
    }

    #[test]
    fn bad_json_in_a_body_is_a_json_error() {
        assert!(matches!(
            client("{\"type\":\"ping\",\"type\":\"ping\"}"),
            Err(ProtoError::Json(_))
        ));
        assert!(matches!(
            client("{\"type\":\"hello\",\"max_version\":1.0,\"client\":\"x\"}"),
            Err(ProtoError::Json(e)) if e.kind == JsonErrorKind::Float
        ));
        assert!(matches!(
            JsonCodec.decode_client(b"{\"type\":\"hello\",\"client\":\"\xFF\",\"max_version\":1}", &after()),
            Err(ProtoError::Json(e)) if e.kind == JsonErrorKind::NotUtf8
        ));
        let deep = format!("{}1{}", "[".repeat(9), "]".repeat(9));
        let text = format!("{{\"type\":\"request\",\"args\":{deep}}}");
        assert!(matches!(
            client(&text),
            Err(ProtoError::Json(e)) if e.kind == JsonErrorKind::TooDeep
        ));
        // The message object is level 1 and `args` level 2, so seven nested objects reach level 8
        // (allowed) and eight reach level 9 (refused).
        let ok = format!("{}1{}", "{\"a\":".repeat(7), "}".repeat(7));
        let text = format!(
            "{{\"type\":\"request\",\"tag\":1,\"request_id\":\"r\",\"created_at_ms\":1,\
             \"recipe\":\"x\",\"subject\":\"s\",\"args\":{ok}}}"
        );
        assert!(client(&text).is_ok());
        let too = format!("{}1{}", "{\"a\":".repeat(8), "}".repeat(8));
        assert!(client(&text.replace(&ok, &too)).is_err());
    }

    #[test]
    fn the_limits_before_login_are_tighter() {
        let big = format!(
            "{{\"type\":\"hello\",\"max_version\":1,\"client\":\"{}\"}}",
            "x".repeat(5_000)
        );
        assert!(matches!(
            JsonCodec.decode_client(big.as_bytes(), &before()),
            Err(ProtoError::Json(e)) if e.kind == JsonErrorKind::StringTooLong
        ));
        let mut many = String::from("{\"type\":\"ping\"");
        for i in 0..70 {
            many.push_str(&format!(",\"k{i}\":1"));
        }
        many.push('}');
        assert!(matches!(
            JsonCodec.decode_client(many.as_bytes(), &before()),
            Err(ProtoError::Json(e)) if e.kind == JsonErrorKind::TooManyKeys
        ));
    }

    #[test]
    fn hello_is_checked() {
        let hello = |extra: &str| client(&format!("{{\"type\":\"hello\",{extra}}}"));
        assert!(hello("\"max_version\":1,\"client\":\"x\"").is_ok());
        assert!(hello("\"max_version\":256,\"client\":\"x\"").is_err());
        assert!(hello("\"max_version\":-1,\"client\":\"x\"").is_err());
        assert!(hello("\"max_version\":\"1\",\"client\":\"x\"").is_err());
        assert!(hello("\"client\":\"x\"").is_err());
        assert!(hello("\"max_version\":1").is_err());
        assert!(hello(&format!(
            "\"max_version\":1,\"client\":\"{}\"",
            "c".repeat(64)
        ))
        .is_ok());
        assert!(hello(&format!(
            "\"max_version\":1,\"client\":\"{}\"",
            "c".repeat(65)
        ))
        .is_err());
    }

    #[test]
    fn auth_steps_and_mechanism_go_together() {
        let auth = |fields: &str| client(&format!("{{\"type\":\"auth\",{fields}}}"));
        assert!(auth(
            "\"step\":\"client_first\",\"mechanism\":\"SCRAM-SHA-256\",\"message\":\"m\""
        )
        .is_ok());
        assert!(auth("\"step\":\"client_first\",\"message\":\"m\"").is_err());
        assert!(auth(
            "\"step\":\"client_final\",\"mechanism\":\"SCRAM-SHA-256\",\"message\":\"m\""
        )
        .is_err());
        assert!(auth("\"step\":\"client_final\",\"message\":\"m\"").is_ok());
        assert!(auth("\"step\":\"server_first\",\"message\":\"m\"").is_err());
        assert!(auth("\"step\":\"client_final\"").is_err());
        assert!(auth("\"message\":\"m\"").is_err());
    }

    #[test]
    fn a_request_is_checked_field_by_field() {
        let good: Value = client_to_value(&ClientMessage::Request(Box::new(request())));
        let edit = |f: &dyn Fn(&mut BTreeMap<String, Value>)| {
            let Value::Obj(mut map) = good.clone() else {
                unreachable!()
            };
            f(&mut map);
            client(&json::to_string(&Value::Obj(map)))
        };
        assert!(edit(&|_| {}).is_ok());
        for key in [
            "tag",
            "request_id",
            "created_at_ms",
            "recipe",
            "subject",
            "args",
        ] {
            assert!(
                edit(&|m| {
                    m.remove(key);
                })
                .is_err(),
                "missing {key}"
            );
        }
        assert!(edit(&|m| {
            m.insert("tag".into(), Value::Int(-1));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("tag".into(), Value::str("7"));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("request_id".into(), Value::str("bad id!"));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("request_id".into(), Value::str(""));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("subject".into(), Value::str("a b"));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("counterparty".into(), Value::str("a/b"));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("recipe".into(), Value::str(""));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("version".into(), Value::Int(0));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("version".into(), Value::Int(4_294_967_296));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("version".into(), Value::Int(4_294_967_295));
        })
        .is_ok());
        assert!(edit(&|m| {
            m.insert("args".into(), Value::Arr(vec![]));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("args".into(), Value::Null);
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("map_version".into(), Value::str("1"));
        })
        .is_err());
        assert!(edit(&|m| {
            m.insert("created_at_ms".into(), Value::Int(-5));
        })
        .is_err());
    }

    #[test]
    fn a_reply_needs_the_fields_of_its_kind() {
        assert!(server(
            "{\"type\":\"reply\",\"tag\":1,\"ok\":true,\"replayed\":false,\"result\":null}"
        )
        .is_ok());
        assert!(server("{\"type\":\"reply\",\"tag\":1,\"ok\":true,\"result\":null}").is_err());
        assert!(server("{\"type\":\"reply\",\"tag\":1,\"ok\":true,\"replayed\":false}").is_err());
        assert!(server("{\"type\":\"reply\",\"tag\":1,\"ok\":false}").is_err());
        assert!(server("{\"type\":\"reply\",\"tag\":1,\"ok\":true,\"replayed\":false,\"result\":1,\"error\":{}}").is_err());
        assert!(server(
            "{\"type\":\"reply\",\"tag\":1,\"ok\":false,\"result\":1,\"error\":\
             {\"kind\":\"unauthorized\",\"name\":\"unauthorized\",\"message\":\"m\"}}"
        )
        .is_err());
    }

    #[test]
    fn a_message_never_breaks_the_codec_whatever_the_bytes() {
        for bytes in [
            &b""[..],
            b"\0",
            b"{",
            b"{\"type\":",
            b"\xFF\xFF\xFF",
            b"{\"type\":\"request\",\"args\":{\"a\":{\"a\":{\"a\":{\"a\":{\"a\":{\"a\":{\"a\":{\"a\":{}}}}}}}}}}",
        ] {
            let _ = JsonCodec.decode_client(bytes, &after());
            let _ = JsonCodec.decode_server(bytes, &after());
        }
    }
}
