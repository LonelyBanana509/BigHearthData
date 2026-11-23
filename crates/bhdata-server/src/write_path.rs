//! Starts the write path inside the server process (spec `write-path.md`, section 11).
//!
//! It runs after the engine has started and bootstrapped PostgreSQL. It opens the pools, refuses to
//! go on if the server's `synchronous_commit` does not match the configured durability, then starts
//! the write queue with its writers and the compaction job on a small runtime of its own. Until the
//! gateway (M5) exists nothing sends the queue commands, but the compaction job is real work, and
//! `bhdatactl call` runs commands in its own process through the same code.

use std::sync::Arc;

use bhdata_api::types::{Clock, Config, SystemClock};
use bhdata_engine::host::Running;
use bhdata_pg::pool::{Pools, Target};
use bhdata_writer::compaction::{CompactionConfig, CompactionJob};
use bhdata_writer::exec::{ExecConfig, Executor};
use bhdata_writer::queue::WriteQueue;
use bhdata_writer::startup::verify_synchronous_commit;

/// Everything that must stay alive while the server runs.
struct WritePath {
    _runtime: tokio::runtime::Runtime,
    _pools: Arc<Pools>,
    _queue: WriteQueue<Executor>,
    _compaction: CompactionJob,
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
    let compaction =
        CompactionJob::start(pools.clone(), clock, CompactionConfig::from_config(config));
    eprintln!("bhdata-server: write path started (queue and compaction job)");
    Ok(Box::new(WritePath {
        _runtime: runtime,
        _pools: pools,
        _queue: queue,
        _compaction: compaction,
    }))
}
