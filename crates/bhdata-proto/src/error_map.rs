//! The mapping between `bhdata_types::Error` and the wire error object; the only place wire names
//! exist (wire-protocol spec, section 4). Lane P2.
//!
//! ```text
//! {"kind": <one of the 16 names>, "name": <text>, "message": <text>,
//!  "retry_after_ms": <only overloaded and rate_limited>, "transfer": <only pending>,
//!  "incident": <only internal>}
//! ```
//!
//! `name` equals `kind` except for `recipe_defined`, where it is the name the recipe gave its error.
//! `internal` carries no detail, only an incident id the server logs with the cause. `wrong_shard`
//! never reaches the wire: if one is handed over anyway it goes out as `internal`.
//!
//! `bhdata_types::Error::RateLimited` carries no retry hint, but the wire needs one, so the caller
//! passes it in [`WireContext`]. `Error::Overloaded` carries its own.

use std::collections::BTreeMap;
use std::time::Duration;

use bhdata_types::{Error, ErrorKind, TransferId};

use crate::json::Value;
use crate::message::{bad, Fields, ProtoError};

/// The hint sent with `rate_limited` when the caller has none.
pub const DEFAULT_RATE_LIMIT_HINT: Duration = Duration::from_secs(1);

/// What `to_wire` needs besides the error itself.
#[derive(Debug, Clone, Default)]
pub struct WireContext {
    /// How long a rate-limited caller should wait. Used for `rate_limited` only.
    pub rate_limit_hint: Option<Duration>,
    /// The incident id the server logged the cause under. Used for `internal` (and `wrong_shard`) only.
    pub incident: Option<String>,
}

/// An error as it travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    /// The kind: one of the 16 named errors. A client matches on this for everything but business errors.
    pub kind: ErrorKind,
    /// Equal to the kind's name, except for `recipe_defined` (the recipe's own name for it).
    pub name: String,
    /// For people, never for logic.
    pub message: String,
    /// Only for `overloaded` and `rate_limited`: when to try again.
    pub retry_after_ms: Option<u64>,
    /// Only for `pending`: the transfer to poll.
    pub transfer: Option<TransferId>,
    /// Only for `internal`: the id to quote.
    pub incident: Option<String>,
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The wire form of `err`.
#[must_use]
pub fn to_wire(err: &Error, ctx: &WireContext) -> WireError {
    let incident = || {
        ctx.incident
            .clone()
            .unwrap_or_else(|| "unknown".to_string())
    };
    let plain = |kind: ErrorKind, message: String| WireError {
        kind,
        name: kind.name().to_string(),
        message,
        retry_after_ms: None,
        transfer: None,
        incident: None,
    };
    match err {
        Error::Overloaded { retry_after } => WireError {
            retry_after_ms: Some(millis(*retry_after)),
            ..plain(ErrorKind::Overloaded, err.to_string())
        },
        Error::RateLimited => WireError {
            retry_after_ms: Some(millis(
                ctx.rate_limit_hint.unwrap_or(DEFAULT_RATE_LIMIT_HINT),
            )),
            ..plain(ErrorKind::RateLimited, err.to_string())
        },
        Error::Pending { transfer } => WireError {
            transfer: Some(transfer.clone()),
            ..plain(ErrorKind::Pending, err.to_string())
        },
        Error::BadArguments(text) => plain(ErrorKind::BadArguments, text.clone()),
        Error::RecipeDefined(name) => WireError {
            name: name.clone(),
            ..plain(ErrorKind::RecipeDefined, err.to_string())
        },
        // No detail leaves the server; `wrong_shard` is the planner's and is never shown.
        Error::Internal(_) | Error::WrongShard => WireError {
            incident: Some(incident()),
            ..plain(ErrorKind::Internal, "internal error".to_string())
        },
        other => plain(other.kind(), other.to_string()),
    }
}

/// The error a client reports for `w`. A `rate_limited` loses its hint here (the error type has no
/// place for it): the retry logic reads `retry_after_ms` from the [`WireError`] itself.
#[must_use]
pub fn from_wire(w: &WireError) -> Error {
    match w.kind {
        ErrorKind::Overloaded => Error::Overloaded {
            retry_after: Duration::from_millis(w.retry_after_ms.unwrap_or(0)),
        },
        ErrorKind::InsufficientFunds => Error::InsufficientFunds,
        ErrorKind::Overflow => Error::Overflow,
        ErrorKind::AlreadyExists => Error::AlreadyExists,
        ErrorKind::AlreadyImported => Error::AlreadyImported,
        ErrorKind::RequestIdConflict => Error::RequestIdConflict,
        ErrorKind::RequestExpired => Error::RequestExpired,
        ErrorKind::Unauthorized => Error::Unauthorized,
        ErrorKind::RateLimited => Error::RateLimited,
        ErrorKind::LockedOut => Error::LockedOut,
        ErrorKind::UnknownRecipe => Error::UnknownRecipe,
        ErrorKind::BadArguments => Error::BadArguments(w.message.clone()),
        ErrorKind::WrongShard => Error::WrongShard,
        ErrorKind::Pending => match &w.transfer {
            Some(transfer) => Error::Pending {
                transfer: transfer.clone(),
            },
            None => Error::Internal("pending reply without a transfer".to_string()),
        },
        ErrorKind::RecipeDefined => Error::RecipeDefined(w.name.clone()),
        ErrorKind::Internal => Error::Internal(format!(
            "incident {}",
            w.incident.as_deref().unwrap_or("unknown")
        )),
    }
}

/// The error of a stored `failed` outcome, by the name the write path stored: `insufficient_funds`
/// and `overflow` are their own kinds, anything else is a name the recipe defined.
#[must_use]
pub fn error_from_stored_name(name: &str) -> Error {
    match name {
        "insufficient_funds" => Error::InsufficientFunds,
        "overflow" => Error::Overflow,
        other => Error::RecipeDefined(other.to_string()),
    }
}

impl ProtoError {
    /// How a connection-level protocol failure is reported to the peer in its last `error` frame:
    /// `bad_arguments` with the reason (it says what was wrong with the peer's own bytes).
    #[must_use]
    pub fn to_error(&self) -> Error {
        Error::BadArguments(self.to_string())
    }
}

impl WireError {
    pub(crate) fn to_value(&self) -> Value {
        let mut map: BTreeMap<String, Value> = BTreeMap::new();
        map.insert("kind".into(), Value::str(self.kind.name()));
        map.insert("name".into(), Value::str(self.name.as_str()));
        map.insert("message".into(), Value::str(self.message.as_str()));
        if let Some(ms) = self.retry_after_ms {
            map.insert(
                "retry_after_ms".into(),
                Value::Int(i64::try_from(ms).unwrap_or(i64::MAX)),
            );
        }
        if let Some(t) = &self.transfer {
            map.insert("transfer".into(), Value::str(t.as_str()));
        }
        if let Some(i) = &self.incident {
            map.insert("incident".into(), Value::str(i.as_str()));
        }
        Value::Obj(map)
    }

    pub(crate) fn from_value(v: Value) -> Result<WireError, ProtoError> {
        let mut f = Fields::new(v, "error")?;
        let kind_text = f.string("kind")?;
        let Some(kind) = ErrorKind::ALL
            .iter()
            .copied()
            .find(|k| k.name() == kind_text)
        else {
            return bad(format!("unknown error kind `{kind_text}`"));
        };
        let name = f.string("name")?;
        if kind != ErrorKind::RecipeDefined && name != kind.name() {
            return bad(format!(
                "error name `{name}` does not match its kind `{kind_text}`"
            ));
        }
        let message = f.string("message")?;
        let retry_after_ms = f.opt_u64("retry_after_ms")?;
        let wants_hint = matches!(kind, ErrorKind::Overloaded | ErrorKind::RateLimited);
        if retry_after_ms.is_some() != wants_hint {
            return bad(format!(
                "`retry_after_ms` is {} for error kind `{kind_text}`",
                if wants_hint {
                    "required"
                } else {
                    "not allowed"
                }
            ));
        }
        let transfer = match f.opt_string("transfer")? {
            Some(t) => {
                Some(TransferId::new(t).or_else(|e| bad(format!("`transfer` of error: {e}")))?)
            }
            None => None,
        };
        if transfer.is_some() != (kind == ErrorKind::Pending) {
            return bad(format!(
                "`transfer` is {} for error kind `{kind_text}`",
                if kind == ErrorKind::Pending {
                    "required"
                } else {
                    "not allowed"
                }
            ));
        }
        let incident = f.opt_string("incident")?;
        if incident.is_some() != (kind == ErrorKind::Internal) {
            return bad(format!(
                "`incident` is {} for error kind `{kind_text}`",
                if kind == ErrorKind::Internal {
                    "required"
                } else {
                    "not allowed"
                }
            ));
        }
        f.finish()?;
        Ok(WireError {
            kind,
            name,
            message,
            retry_after_ms,
            transfer,
            incident,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bhdata_types::{ids::derive_transfer_id, LoginId, RequestId};

    fn ctx() -> WireContext {
        WireContext {
            rate_limit_hint: Some(Duration::from_millis(250)),
            incident: Some("inc-42".to_string()),
        }
    }

    fn transfer() -> TransferId {
        derive_transfer_id(
            &LoginId::new("app").unwrap(),
            &RequestId::new("req-1").unwrap(),
        )
    }

    fn every_error() -> Vec<Error> {
        vec![
            Error::Overloaded {
                retry_after: Duration::from_millis(80),
            },
            Error::InsufficientFunds,
            Error::Overflow,
            Error::AlreadyExists,
            Error::AlreadyImported,
            Error::RequestIdConflict,
            Error::RequestExpired,
            Error::Unauthorized,
            Error::RateLimited,
            Error::LockedOut,
            Error::UnknownRecipe,
            Error::BadArguments("argument `amount` is missing".to_string()),
            Error::Pending {
                transfer: transfer(),
            },
            Error::RecipeDefined("already_claimed".to_string()),
            Error::Internal("disk on fire: /var/lib/x".to_string()),
        ]
    }

    #[test]
    fn every_error_survives_the_trip_through_the_object() {
        for err in every_error() {
            let wire = to_wire(&err, &ctx());
            let back = WireError::from_value(wire.to_value()).unwrap();
            assert_eq!(back, wire, "{err:?}");
            assert_eq!(wire.kind, err.kind(), "{err:?}");
        }
    }

    #[test]
    fn from_wire_gives_back_the_error_except_where_the_wire_hides_it() {
        for err in every_error() {
            let back = from_wire(&to_wire(&err, &ctx()));
            match &err {
                Error::Internal(_) => assert_eq!(back, Error::Internal("incident inc-42".into())),
                other => assert_eq!(&back, other),
            }
        }
    }

    #[test]
    fn the_sixteen_names_are_the_kinds_names() {
        assert_eq!(ErrorKind::ALL.len(), 16);
        for kind in ErrorKind::ALL {
            let found = ErrorKind::ALL.iter().find(|k| k.name() == kind.name());
            assert_eq!(found, Some(&kind));
        }
    }

    #[test]
    fn name_equals_kind_except_for_recipe_defined() {
        for err in every_error() {
            let w = to_wire(&err, &ctx());
            if w.kind == ErrorKind::RecipeDefined {
                assert_eq!(w.name, "already_claimed");
            } else {
                assert_eq!(w.name, w.kind.name());
            }
        }
    }

    #[test]
    fn hints_travel_only_with_overloaded_and_rate_limited() {
        let o = to_wire(
            &Error::Overloaded {
                retry_after: Duration::from_millis(80),
            },
            &ctx(),
        );
        assert_eq!(o.retry_after_ms, Some(80));
        assert_eq!(
            to_wire(&Error::RateLimited, &ctx()).retry_after_ms,
            Some(250)
        );
        assert_eq!(
            to_wire(&Error::RateLimited, &WireContext::default()).retry_after_ms,
            Some(1_000)
        );
        assert_eq!(to_wire(&Error::Unauthorized, &ctx()).retry_after_ms, None);
        assert_eq!(to_wire(&Error::RequestExpired, &ctx()).retry_after_ms, None);
    }

    #[test]
    fn internal_carries_no_detail_only_an_incident() {
        let w = to_wire(
            &Error::Internal("password=hunter2 in /etc/x".into()),
            &ctx(),
        );
        assert_eq!(w.message, "internal error");
        assert_eq!(w.incident.as_deref(), Some("inc-42"));
        let text = crate::json::to_string(&w.to_value());
        assert!(!text.contains("hunter2") && !text.contains("/etc/x"));
        assert_eq!(to_wire(&Error::Unauthorized, &ctx()).incident, None);
    }

    #[test]
    fn wrong_shard_never_goes_out_as_itself() {
        let w = to_wire(&Error::WrongShard, &ctx());
        assert_eq!(w.kind, ErrorKind::Internal);
        assert_eq!(w.incident.as_deref(), Some("inc-42"));
    }

    #[test]
    fn a_stored_failure_name_becomes_its_error() {
        assert_eq!(
            error_from_stored_name("insufficient_funds"),
            Error::InsufficientFunds
        );
        assert_eq!(error_from_stored_name("overflow"), Error::Overflow);
        assert_eq!(
            error_from_stored_name("already_claimed"),
            Error::RecipeDefined("already_claimed".into())
        );
    }

    #[test]
    fn a_malformed_error_object_is_refused() {
        let base = || to_wire(&Error::Unauthorized, &ctx()).to_value();
        let with = |key: &str, v: Value| match base() {
            Value::Obj(mut m) => {
                m.insert(key.into(), v);
                Value::Obj(m)
            }
            _ => unreachable!(),
        };
        let without = |key: &str| match base() {
            Value::Obj(mut m) => {
                m.remove(key);
                Value::Obj(m)
            }
            _ => unreachable!(),
        };
        assert!(WireError::from_value(base()).is_ok());
        assert!(WireError::from_value(with("kind", Value::str("nope"))).is_err());
        assert!(WireError::from_value(with("name", Value::str("locked_out"))).is_err());
        assert!(WireError::from_value(with("retry_after_ms", Value::Int(5))).is_err());
        assert!(WireError::from_value(with("transfer", Value::str("t"))).is_err());
        assert!(WireError::from_value(with("incident", Value::str("i"))).is_err());
        assert!(WireError::from_value(with("extra", Value::Null)).is_err());
        assert!(WireError::from_value(without("kind")).is_err());
        assert!(WireError::from_value(without("message")).is_err());
        assert!(WireError::from_value(Value::Null).is_err());
        let ov = to_wire(
            &Error::Overloaded {
                retry_after: Duration::ZERO,
            },
            &ctx(),
        )
        .to_value();
        let Value::Obj(mut m) = ov else {
            unreachable!()
        };
        m.remove("retry_after_ms");
        assert!(WireError::from_value(Value::Obj(m)).is_err());
    }

    #[test]
    fn a_protocol_failure_is_reported_as_bad_arguments() {
        let e = ProtoError::Message("x".into()).to_error();
        assert!(matches!(e, Error::BadArguments(text) if text.contains('x')));
    }
}
