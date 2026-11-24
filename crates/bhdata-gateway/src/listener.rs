//! The gateway as a whole: bind, accept, hand each connection to a session, reload, shut down
//! (wire-protocol spec, sections 3 and 8; tests W12 and W13). Lane B.
//!
//! [`Gateway::start`] refuses a configuration that would run without TLS on a non-loopback address
//! before it opens a socket (W13): `GatewayConfig::validate` rejects `insecure_local` on such an address,
//! and without `insecure_local` the certificate and key must load.
//!
//! The gateway holds no durable state. [`Gateway::shutdown`] sends `shutdown {drain_ms}` to every
//! connection, answers new requests `overloaded`, waits up to `drain_ms` for open ones, then closes.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use bhdata_api::handler::RequestHandler;
use bhdata_api::login::LoginStore;
use bhdata_api::types::config::{ConfigError, GatewayConfig};
use bhdata_proto::scram::ServerSecret;
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{interval, sleep, timeout, timeout_at, Instant};

use crate::auth::Authenticator;
use crate::session::{self, Options, Shared};
use crate::tls::{TlsError, TlsState};

/// Most connections open at once, logged in or not. The spec bounds a login's connections and the
/// requests in flight, but a connection that has not logged in yet belongs to no login; this keeps
/// them from being unbounded. A placeholder until M11, like the other limits.
pub const MAX_CONNECTIONS: usize = 10_000;

/// How often locks that ran out are ended and idle counters dropped.
const SWEEP_EVERY: Duration = Duration::from_secs(5);

/// Why the gateway did not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The `[gateway]` settings are not usable, including a plain connection on a non-loopback address.
    #[error("{0}")]
    Config(#[from] ConfigError),
    /// TLS is required (no `insecure_local`) and `cert_path` or `key_path` is not set.
    #[error(
        "gateway.cert_path and gateway.key_path are required unless gateway.insecure_local is set"
    )]
    TlsNotConfigured,
    /// The certificate or key could not be used.
    #[error("{0}")]
    Tls(#[from] TlsError),
    /// The address could not be bound.
    #[error("cannot listen on {addr}: {source}")]
    Bind {
        /// The address.
        addr: SocketAddr,
        /// Why.
        source: std::io::Error,
    },
}

/// A running gateway copy.
pub struct Gateway<H, S> {
    local_addr: SocketAddr,
    shared: Arc<Shared<H, S>>,
    tls: Option<Arc<TlsState>>,
    accept_task: JoinHandle<()>,
    sweep_task: JoinHandle<()>,
    stopped: AtomicBool,
}

impl<H, S> Gateway<H, S>
where
    H: RequestHandler + 'static,
    S: LoginStore + 'static,
{
    /// Checks the configuration, loads the certificate, binds, and starts accepting.
    ///
    /// `secret` is the installation's secret for the decoy answers of unknown logins (it must be the
    /// same across restarts, see `bhdata_proto::scram::ServerSecret`).
    ///
    /// # Errors
    /// [`StartError`]. Nothing is listening after an error.
    pub async fn start(
        config: GatewayConfig,
        handler: Arc<H>,
        store: Arc<S>,
        secret: ServerSecret,
        options: Options,
    ) -> Result<Self, StartError> {
        config.validate()?;
        let tls = if config.tls_required() {
            let (Some(cert), Some(key)) = (&config.cert_path, &config.key_path) else {
                return Err(StartError::TlsNotConfigured);
            };
            Some(Arc::new(TlsState::load(cert, key)?))
        } else {
            None
        };
        let listener =
            TcpListener::bind(config.listen)
                .await
                .map_err(|source| StartError::Bind {
                    addr: config.listen,
                    source,
                })?;
        let local_addr = listener.local_addr().map_err(|source| StartError::Bind {
            addr: config.listen,
            source,
        })?;

        let (shutdown_tx, _) = watch::channel(false);
        let auth = Authenticator::new(&config, store, secret, Arc::clone(&options.clock));
        let shared = Arc::new(Shared::new(config, handler, auth, options, shutdown_tx));
        let accept_task = tokio::spawn(accept_loop(listener, Arc::clone(&shared), tls.clone()));
        let sweep_task = tokio::spawn(sweep_loop(Arc::clone(&shared)));
        Ok(Gateway {
            local_addr,
            shared,
            tls,
            accept_task,
            sweep_task,
            stopped: AtomicBool::new(false),
        })
    }

    /// The address the gateway listens on (useful when the configured port was 0).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Requests open across all connections right now.
    #[must_use]
    pub fn in_flight(&self) -> u64 {
        self.shared.shed.in_flight()
    }

    /// Reads the certificate and key again (`bhdatactl gateway reload` and `SIGHUP`). New connections
    /// use the new pair; open connections are not dropped (W12). On a plain gateway there is nothing to
    /// reload. On an error the old pair stays.
    ///
    /// # Errors
    /// [`TlsError`].
    pub fn reload(&self) -> Result<(), TlsError> {
        match &self.tls {
            Some(tls) => tls.reload(),
            None => Ok(()),
        }
    }

    /// Stops the gateway as the spec's section 8 says, and returns when every connection is closed.
    pub async fn shutdown(mut self) {
        self.stopped.store(true, Ordering::Release);
        // New requests are answered `overloaded` from here on, then every connection is told.
        self.shared.shed.start_draining();
        let _ = self.shared.shutdown.send(true);
        let _ = (&mut self.accept_task).await;
        self.sweep_task.abort();
    }
}

impl<H, S> Drop for Gateway<H, S> {
    fn drop(&mut self) {
        // A gateway that is dropped without `shutdown` stops at once.
        if !self.stopped.load(Ordering::Acquire) {
            self.accept_task.abort();
            self.sweep_task.abort();
        }
    }
}

async fn sweep_loop<H, S>(shared: Arc<Shared<H, S>>)
where
    H: RequestHandler + 'static,
    S: LoginStore + 'static,
{
    let mut tick = interval(SWEEP_EVERY);
    loop {
        tick.tick().await;
        shared.auth.sweep();
        shared
            .limiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sweep(shared.now_ms());
    }
}

async fn accept_loop<H, S>(
    listener: TcpListener,
    shared: Arc<Shared<H, S>>,
    tls: Option<Arc<TlsState>>,
) where
    H: RequestHandler + 'static,
    S: LoginStore + 'static,
{
    let mut shutdown = shared.shutdown.subscribe();
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut sessions: JoinSet<()> = JoinSet::new();
    loop {
        // A place first, so a flood waits in the kernel's backlog instead of in memory.
        let permit = tokio::select! {
            _ = shutdown.changed() => break,
            permit = Arc::clone(&slots).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let (stream, peer) = tokio::select! {
            _ = shutdown.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                // Out of file descriptors and the like: back off instead of spinning.
                Err(_) => {
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        while sessions.try_join_next().is_some() {}
        // Too many failed logins from this address lately: close without spending a handshake on it.
        if shared.auth.source_blocked(peer.ip()) {
            continue;
        }
        let shared = Arc::clone(&shared);
        let tls = tls.clone();
        sessions.spawn(async move {
            let _place = permit;
            let _ = stream.set_nodelay(true);
            // One deadline for the TLS handshake, `hello` and the login together.
            let deadline = Instant::now() + shared.config.handshake_timeout();
            match tls {
                Some(tls) => {
                    if let Ok(Ok(secured)) =
                        timeout_at(deadline, tls.acceptor().accept(stream)).await
                    {
                        session::run(shared, secured, peer.ip(), deadline).await;
                    }
                }
                None => session::run(shared, stream, peer.ip(), deadline).await,
            }
        });
    }
    // Shutting down: no new connections; the open ones were told through the watch channel and close
    // when their requests are done or the drain time is up.
    drop(listener);
    let grace = shared.config.drain() + shared.config.write_timeout() + Duration::from_secs(1);
    let _ = timeout(grace, async {
        while sessions.join_next().await.is_some() {}
    })
    .await;
    sessions.shutdown().await;
}
