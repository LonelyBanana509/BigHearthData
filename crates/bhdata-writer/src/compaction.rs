//! The compaction job (spec `write-path.md`, section 8).
//!
//! One task per shard, so at most one compaction runs at a time. Every `compaction_interval_secs`
//! it moves finished request records older than the cutoff into `request_history`, in batches of
//! `compaction_batch`, and then purges history rows older than the history retention. Both go
//! through the admin-only functions of migration 3 on a `bhdata_admin` connection. A failed round
//! is logged to standard error and tried again at the next tick; nothing is lost, because a
//! compaction statement moves a record completely or not at all.

use std::sync::Arc;
use std::time::Duration;

use bhdata_api::types::config::{Config, WriterConfig};
use bhdata_api::types::{Clock, Timestamp};
use bhdata_pg::pool::Pools;
use tokio::sync::watch;

use crate::history::{compact, purge};
use crate::time_rule::compaction_cutoff;

/// What the job needs from the config.
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// The writer settings (interval, batch, grace).
    pub writer: WriterConfig,
    /// How long a request can be retried.
    pub retry_window: Duration,
    /// How long a history row is kept.
    pub history_retention: Duration,
    /// Purge only history rows already copied to the central history (stage 2 and later).
    pub require_copied: bool,
}

impl CompactionConfig {
    /// The settings of a loaded config. `require_copied` is off at stage 0, where the shard's own
    /// tables are the history.
    #[must_use]
    pub fn from_config(config: &Config) -> CompactionConfig {
        CompactionConfig {
            writer: config.writer.clone(),
            retry_window: config.requests.retry_window(),
            history_retention: config.requests.history_retention(),
            require_copied: false,
        }
    }
}

/// What one round did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Round {
    /// Request records moved to the history.
    pub compacted: u64,
    /// History rows deleted.
    pub purged: u64,
}

/// One round: compacts until a batch comes back short, then purges the same way.
///
/// # Errors
/// A pool or driver error, as text. The records moved so far stay moved.
pub async fn run_round(
    pools: &Pools,
    now: Timestamp,
    cfg: &CompactionConfig,
) -> Result<Round, String> {
    let batch = cfg.writer.compaction_batch.max(1);
    let client = pools.admin().await.map_err(|e| e.to_string())?;
    let cutoff = compaction_cutoff(
        now,
        cfg.retry_window,
        Duration::from_secs(cfg.writer.retention_grace_secs),
    );
    let mut round = Round::default();
    loop {
        let moved = compact(&client, cutoff, batch)
            .await
            .map_err(|e| bhdata_pg::pool::describe(&e))?;
        round.compacted += moved;
        if moved < u64::from(batch) {
            break;
        }
    }
    let keep = now
        .as_unix_millis()
        .saturating_sub(u64::try_from(cfg.history_retention.as_millis()).unwrap_or(u64::MAX));
    let purge_before = Timestamp::from_unix_millis(keep);
    loop {
        let gone = purge(&client, purge_before, batch, cfg.require_copied)
            .await
            .map_err(|e| bhdata_pg::pool::describe(&e))?;
        round.purged += gone;
        if gone < u64::from(batch) {
            break;
        }
    }
    Ok(round)
}

/// The running job. Dropping it does not stop the task; call [`CompactionJob::stop`].
pub struct CompactionJob {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl CompactionJob {
    /// Starts the job on the current tokio runtime. The first round runs at once.
    #[must_use]
    pub fn start(pools: Arc<Pools>, clock: Arc<dyn Clock>, cfg: CompactionConfig) -> CompactionJob {
        let (stop, mut stopped) = watch::channel(false);
        let every = Duration::from_secs(cfg.writer.compaction_interval_secs.max(1));
        let task = tokio::spawn(async move {
            loop {
                if let Err(e) = run_round(&pools, clock.now(), &cfg).await {
                    eprintln!("bhdata compaction: {e}");
                }
                // Sleeps until the next round, or returns as soon as the job is told to stop.
                if tokio::time::timeout(every, stopped.changed()).await.is_ok() {
                    return;
                }
            }
        });
        CompactionJob { stop, task }
    }

    /// Stops the job after the round in progress and waits for it.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_keep_a_day_and_an_hour_and_ninety_days_of_history() {
        let cfg = CompactionConfig::from_config(&Config::default());
        assert_eq!(cfg.retry_window, Duration::from_secs(24 * 3600));
        assert_eq!(cfg.writer.retention_grace_secs, 3600);
        assert_eq!(cfg.history_retention, Duration::from_secs(90 * 24 * 3600));
        assert!(!cfg.require_copied);
    }
}
