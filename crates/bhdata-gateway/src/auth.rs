//! The server side of the login (wire-protocol spec, decisions 6 to 8). Lane B.
//!
//! [`Authenticator`] joins three things the lane-S and P1 code leave apart: the SCRAM exchange
//! (`bhdata-proto`), the failure counters (`lockout`), and the login records and audit log behind
//! [`LoginStore`]. It does no I/O of its own beyond calling the store.
//!
//! What a client can and cannot learn, as the spec asks:
//!
//! * A name that does not exist, a login that is disabled and a wrong password all run the same code
//!   and end at the last step with the same `unauthorized`.
//! * A locked name is refused with `locked_out` before the password is looked at, whether or not the
//!   name exists (an unknown name locks like a real one).
//! * A failure that locks the name is stored in the audit log before the reply is sent; every other
//!   audit row is written in the background and never delays a reply.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};

use bhdata_api::login::{AuditEvent, LoginRecord, LoginStore, ScramVerifier};
use bhdata_api::types::config::GatewayConfig;
use bhdata_api::types::{Clock, Error};
use bhdata_proto::scram::{self, ClientFirst, Lookup, ScramError, ServerExchange, ServerSecret};

use crate::lockout::{Lockout, LockoutConfig};

/// Why a login did not go through. [`AuthFailure::to_error`] is what the client is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthFailure {
    /// A wrong proof, an unknown or disabled login. The client cannot tell which.
    Unauthorized,
    /// The name is locked.
    LockedOut,
    /// The client broke the protocol (a malformed SCRAM message, a feature that is not offered).
    Protocol(String),
    /// Something on the server side failed (the login store, the random source).
    Internal(String),
}

impl AuthFailure {
    /// The error the connection's last `error` frame carries.
    #[must_use]
    pub fn to_error(&self) -> Error {
        match self {
            AuthFailure::Unauthorized => Error::Unauthorized,
            AuthFailure::LockedOut => Error::LockedOut,
            AuthFailure::Protocol(text) => Error::BadArguments(text.clone()),
            AuthFailure::Internal(text) => Error::Internal(text.clone()),
        }
    }
}

fn from_scram(e: &ScramError) -> AuthFailure {
    match e {
        ScramError::Malformed(_) | ScramError::Unsupported(_) => {
            AuthFailure::Protocol(e.to_string())
        }
        ScramError::NonceMismatch | ScramError::ProofRejected => AuthFailure::Unauthorized,
        other => AuthFailure::Internal(other.to_string()),
    }
}

fn proto_verifier(v: &ScramVerifier) -> scram::Verifier {
    scram::Verifier {
        salt: v.salt.clone(),
        iterations: v.iterations,
        stored_key: v.stored_key,
        server_key: v.server_key,
    }
}

/// A login between its first and last message. Give it back to [`Authenticator::finish`].
pub struct Pending {
    exchange: ServerExchange,
    name: String,
    name_exists: bool,
    /// `Some` only for a login that exists and is enabled.
    record: Option<LoginRecord>,
    source: IpAddr,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pending(..)")
    }
}

/// The login logic of one gateway copy.
pub struct Authenticator<S> {
    store: Arc<S>,
    secret: ServerSecret,
    lockout: Mutex<Lockout>,
    clock: Arc<dyn Clock>,
    /// The count an unknown name's decoy claims: the configured default, as a real verifier has.
    decoy_iterations: u32,
}

impl<S: LoginStore + 'static> Authenticator<S> {
    /// A fresh authenticator with empty failure counters.
    #[must_use]
    pub fn new(
        config: &GatewayConfig,
        store: Arc<S>,
        secret: ServerSecret,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Authenticator {
            store,
            secret,
            lockout: Mutex::new(Lockout::new(LockoutConfig::from_config(config))),
            clock,
            decoy_iterations: config.scram_iterations,
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.monotonic().as_millis()).unwrap_or(u64::MAX)
    }

    fn counters(&self) -> std::sync::MutexGuard<'_, Lockout> {
        self.lockout.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes an audit row without making the caller wait for it.
    fn audit_later(&self, event: AuditEvent) {
        let store = Arc::clone(&self.store);
        tokio::spawn(async move {
            let _ = store.audit(event).await;
        });
    }

    /// `true` if too many logins failed from `source` lately. The listener asks before it spends
    /// anything on the connection.
    #[must_use]
    pub fn source_blocked(&self, source: IpAddr) -> bool {
        let now = self.now_ms();
        self.counters().check_source(source, now).is_err()
    }

    /// The first client message: finds the login, answers with the first server message.
    ///
    /// # Errors
    /// [`AuthFailure::LockedOut`] for a locked name (before anything about the name is looked up),
    /// [`AuthFailure::Protocol`] for a bad message, [`AuthFailure::Internal`] for a store failure.
    pub async fn begin(
        &self,
        message: &str,
        source: IpAddr,
    ) -> Result<(Pending, String), AuthFailure> {
        let first = ClientFirst::parse(message).map_err(|e| from_scram(&e))?;
        let name = first.user().to_owned();
        let now = self.now_ms();
        if self.counters().check_name(&name, now).is_err() {
            return Err(AuthFailure::LockedOut);
        }
        let found = self
            .store
            .find(&name)
            .await
            .map_err(|e| AuthFailure::Internal(e.to_string()))?;
        let name_exists = found.is_some();
        // A disabled login fails like a wrong password: it is answered from a decoy.
        let record = found.filter(|r| !r.disabled);
        let verifier = record.as_ref().map(|r| proto_verifier(&r.verifier));
        let lookup = match &verifier {
            Some(v) => Lookup::Known(v),
            None => Lookup::Unknown {
                secret: &self.secret,
                iterations: self.decoy_iterations,
            },
        };
        let server_nonce = scram::random_nonce().map_err(|e| from_scram(&e))?;
        let (exchange, server_first) = ServerExchange::begin(&first, lookup, &server_nonce)
            .map_err(|e| match e {
                // A stored verifier the code cannot use is the server's problem, not the client's.
                ScramError::IterationsOutOfRange => AuthFailure::Internal(e.to_string()),
                other => from_scram(&other),
            })?;
        Ok((
            Pending {
                exchange,
                name,
                name_exists,
                record,
                source,
            },
            server_first,
        ))
    }

    /// The last client message. On success returns the final server message and the login's record.
    ///
    /// # Errors
    /// [`AuthFailure::Unauthorized`] for a wrong proof (and for an unknown or disabled login, which
    /// cannot pass), [`AuthFailure::Protocol`] for a bad message.
    pub async fn finish(
        &self,
        pending: Pending,
        message: &str,
    ) -> Result<(String, LoginRecord), AuthFailure> {
        let Pending {
            exchange,
            name,
            name_exists,
            record,
            source,
        } = pending;
        let outcome = exchange.finish(message).map_err(|e| from_scram(&e));
        match (outcome, record) {
            (Ok(server_final), Some(record)) => {
                self.counters().record_success(&name);
                self.audit_later(AuditEvent::LoginSucceeded {
                    at: self.clock.now(),
                    login_id: record.login_id.clone(),
                    source,
                });
                Ok((server_final, record))
            }
            // The decoy's keys fit no password, so an exchange for a login that does not exist (or
            // is disabled) cannot succeed; if it ever did, it still must not log in.
            (Ok(_), None) | (Err(AuthFailure::Unauthorized), _) => {
                self.fail(&name, name_exists, source).await;
                Err(AuthFailure::Unauthorized)
            }
            (Err(other), _) => Err(other),
        }
    }

    async fn fail(&self, name: &str, name_exists: bool, source: IpAddr) {
        let now = self.now_ms();
        let effect = self.counters().record_failure(name, source, now);
        self.audit_later(AuditEvent::LoginFailed {
            at: self.clock.now(),
            name: name.to_owned(),
            name_exists,
            source,
        });
        if effect.name_locked {
            // Stored before any `locked_out` answer can go out (spec, section 7).
            let _ = self
                .store
                .audit(AuditEvent::LockedOut {
                    at: self.clock.now(),
                    name: name.to_owned(),
                })
                .await;
        }
    }

    /// Ends locks that ran out and writes a `LockoutReleased` row for each. The listener calls it
    /// every few seconds.
    pub fn sweep(&self) {
        let now = self.now_ms();
        let released = self.counters().sweep(now);
        for name in released {
            self.audit_later(AuditEvent::LockoutReleased {
                at: self.clock.now(),
                name,
            });
        }
    }
}
