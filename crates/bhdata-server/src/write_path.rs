//! Starts the write path inside the server process (spec `write-path.md`, section 11).
//!
//! It runs after the engine has started and bootstrapped PostgreSQL. It opens the pools, refuses to
//! go on if the server's `synchronous_commit` does not match the configured durability, then starts
//! the write queue with its writers and the compaction job on a small runtime of its own. Until the
//! gateway (M5) exists nothing sends the queue commands, but the compaction job is real work, and
//! `bhdatactl call` runs commands in its own process through the same code.
//!
//! The gateway (wire-protocol spec) starts last, on the same runtime, when the `[gateway]` section
//! says how clients may connect: a certificate and key, or `insecure_local` on a loopback address.
//! With neither it is not started and the server says so in one line, so an installation that has
//! no certificate yet keeps working as it did at M4. `SIGHUP` reads the certificate again, and the
//! gateway stops as the spec's section 8 says: `shutdown` to every connection, a drain, then close.

use std::path::Path;
use std::sync::Arc;

use bhdata_api::types::{Clock, Config, SystemClock};
use bhdata_control::logins::PgLoginStore;
use bhdata_engine::host::Running;
use bhdata_gateway::listener::Gateway;
use bhdata_gateway::session::Options;
use bhdata_pg::pool::{Pools, Target};
use bhdata_writer::compaction::{CompactionConfig, CompactionJob};
use bhdata_writer::exec::{ExecConfig, Executor};
use bhdata_writer::queue::WriteQueue;
use bhdata_writer::startup::verify_synchronous_commit;
use tokio::task::JoinHandle;

use crate::handler::{PgPlans, ThinHandler};
use crate::secret;

type ServerGateway = Gateway<ThinHandler<PgPlans, WriteQueue<Executor>>, PgLoginStore>;

/// Everything that must stay alive while the server runs.
struct WritePath {
    runtime: tokio::runtime::Runtime,
    _pools: Arc<Pools>,
    queue: WriteQueue<Executor>,
    _compaction: CompactionJob,
    gateway: Option<Arc<ServerGateway>>,
    reload_task: Option<JoinHandle<()>>,
    drain: std::time::Duration,
}

impl Drop for WritePath {
    fn drop(&mut self) {
        if let Some(task) = self.reload_task.take() {
            task.abort();
        }
        // Gateway first (clients are told and open requests finish), then the queue.
        if let Some(gateway) = self.gateway.take() {
            if let Ok(gateway) = Arc::try_unwrap(gateway) {
                eprintln!(
                    "bhdata-server: gateway stopping (clients are told, open requests finish)"
                );
                self.runtime.block_on(gateway.shutdown());
                eprintln!("bhdata-server: gateway stopped");
            }
        }
        self.runtime.block_on(self.queue.shutdown(self.drain));
        eprintln!("bhdata-server: write queue drained");
    }
}

/// `true` when the `[gateway]` section says how clients connect (TLS files, or a plain loopback
/// listener); `false` when it is still the default, which has no certificate.
fn gateway_configured(c: &bhdata_api::types::config::GatewayConfig) -> bool {
    c.insecure_local || c.cert_path.is_some() || c.key_path.is_some()
}

fn start_gateway(
    runtime: &tokio::runtime::Runtime,
    config: &Config,
    pools: &Arc<Pools>,
    clock: &Arc<dyn Clock>,
    queue: &WriteQueue<Executor>,
) -> Result<Arc<ServerGateway>, String> {
    let base_dir: &Path = &config.engine.base_dir;
    let secret = secret::load_or_create(base_dir)?;
    let handler = ThinHandler::new(PgPlans::new(pools.clone(), clock.clone()), queue.clone());
    let store = PgLoginStore::new(pools.clone());
    let options = Options {
        clock: clock.clone(),
        on_incident: Some(Arc::new(|incident, cause| {
            // The wire carries only the id; the cause is logged here (spec, section 4).
            eprintln!("bhdata-server: incident {incident}: {cause}");
        })),
        ..Options::default()
    };
    let gateway = runtime
        .block_on(Gateway::start(
            config.gateway.clone(),
            Arc::new(handler),
            Arc::new(store),
            secret,
            options,
        ))
        .map_err(|e| format!("the gateway did not start: {e}"))?;
    Ok(Arc::new(gateway))
}

/// Reads the certificate and key again on `SIGHUP` (`bhdatactl gateway reload`). A failed reload
/// leaves the old pair in place and is logged.
fn watch_reload(
    runtime: &tokio::runtime::Runtime,
    gateway: &Arc<ServerGateway>,
) -> Result<JoinHandle<()>, String> {
    use tokio::signal::unix::{signal, SignalKind};
    let _entered = runtime.enter();
    let mut hangup =
        signal(SignalKind::hangup()).map_err(|e| format!("cannot watch for SIGHUP: {e}"))?;
    let gateway = Arc::clone(gateway);
    Ok(runtime.spawn(async move {
        while hangup.recv().await.is_some() {
            match gateway.reload() {
                Ok(()) => eprintln!("bhdata-server: gateway certificate reloaded"),
                Err(e) => {
                    eprintln!("bhdata-server: gateway reload failed, old certificate kept: {e}")
                }
            }
        }
    }))
}

/// The engine host's hook.
///
/// # Errors
/// A sentence: the runtime or a pool could not start, or `synchronous_commit` does not match.
pub fn start(config: &Config, target: &Target) -> Result<Running, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("the write path's runtime did not start: {e}"))?;
    let pools = runtime
        .block_on(Pools::connect(target, &config.pools))
        .map_err(|e| format!("the write path could not connect: {e}"))?;
    let pools = Arc::new(pools);
    runtime.block_on(verify_synchronous_commit(&pools, config.durability))?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let executor = Executor::new(
        pools.clone(),
        clock.clone(),
        ExecConfig::from_config(config),
    );
    // Starting tasks needs a runtime context.
    let _entered = runtime.enter();
    let queue = WriteQueue::start(
        executor,
        clock.clone(),
        &config.writer,
        config.pools.recipe_writers as usize,
    );
    let compaction = CompactionJob::start(
        pools.clone(),
        clock.clone(),
        CompactionConfig::from_config(config),
    );
    eprintln!("bhdata-server: write path started (queue and compaction job)");

    let (gateway, reload_task) = if gateway_configured(&config.gateway) {
        let gateway = start_gateway(&runtime, config, &pools, &clock, &queue)?;
        let reload_task = watch_reload(&runtime, &gateway)?;
        eprintln!(
            "bhdata-server: gateway listening on {} ({})",
            gateway.local_addr(),
            if config.gateway.tls_required() {
                "TLS 1.3"
            } else {
                "plain, loopback only"
            }
        );
        (Some(gateway), Some(reload_task))
    } else {
        eprintln!(
            "bhdata-server: gateway not started: [gateway] has no cert_path and key_path, and \
             insecure_local is off"
        );
        (None, None)
    };
    Ok(Box::new(WritePath {
        runtime,
        _pools: pools,
        queue,
        _compaction: compaction,
        gateway,
        reload_task,
        drain: std::time::Duration::from_millis(config.writer.shutdown_drain_ms),
    }))
}
