//! Config structs and their defaults. Defaults live here, in one place (plan section 6).
//!
//! Every value that the design states a default for is here: cache entries expire after 2 seconds,
//! a replica more than 1 second behind leaves rotation, request ids are kept for 24 hours, history
//! rows for 90 days, 7 daily backup copies, full durability, an update window at 03:00 UTC. Config for a crate that does not exist
//! yet is added with that crate's milestone, not guessed now.
//!
//! The structs read from TOML (the config format), accept any subset of keys, and refuse unknown
//! keys, so a typo is an error instead of a silently ignored setting. Reading the file is the
//! server's and `bhdatactl`'s job; this crate only defines the shape.
//!
//! Time values are plain numbers with the unit in the name (`ttl_ms`, `retry_window_secs`), which
//! is how they look in a TOML file. Use the accessor methods to get a [`Duration`].

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

/// Number of virtual slots that keys hash into (design section 6).
///
/// Fixed for the life of any system, because changing it re-hashes every key. It is a constant, not
/// a setting, and it must stay a power of two so that hash-to-slot is a mask.
pub const SLOT_COUNT: u32 = 4_096;

const _: () = assert!(SLOT_COUNT.is_power_of_two());

/// The mask that turns a 64-bit hash into a slot number: `hash & SLOT_MASK`.
pub const SLOT_MASK: u64 = (SLOT_COUNT as u64) - 1;

/// A config value that fails validation.
#[derive(Debug, Clone, PartialEq, Eq, ThisError)]
pub enum ConfigError {
    /// A value that must be above zero was zero.
    #[error("{field} must be greater than zero")]
    NotPositive {
        /// The dotted name of the setting, as written in the config file.
        field: &'static str,
    },
    /// A value outside what the setting allows.
    #[error("{field} {reason}")]
    Invalid {
        /// The dotted name of the setting, as written in the config file.
        field: &'static str,
        /// What is wrong, as a short phrase that follows the name ("must be ...").
        reason: &'static str,
    },
}

/// How a commit is made durable (design section 10b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Durability {
    /// A result is returned only after its commit is synced. The default, always.
    #[default]
    Full,
    /// Faster and unsafe: a crash can lose acknowledged writes. Must be asked for by name, and
    /// every benchmark that uses it must say so.
    Relaxed,
}

/// The memory cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// How long a cache entry lives. The cache is not cleared on writes, so this bounds staleness.
    pub ttl_ms: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig { ttl_ms: 2_000 }
    }
}

impl CacheConfig {
    /// How long a cache entry lives.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        Duration::from_millis(self.ttl_ms)
    }
}

/// Read replicas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReplicaConfig {
    /// A replica further behind than this is taken out of rotation.
    pub max_lag_ms: u64,
}

impl Default for ReplicaConfig {
    fn default() -> Self {
        ReplicaConfig { max_lag_ms: 1_000 }
    }
}

impl ReplicaConfig {
    /// The most a replica may lag and still serve reads.
    #[must_use]
    pub fn max_lag(&self) -> Duration {
        Duration::from_millis(self.max_lag_ms)
    }
}

/// How long request records and history are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestConfig {
    /// How long a stored result can answer a retry. Also the SDK's maximum retry window.
    pub retry_window_secs: u64,
    /// How long the compact history row of a finished command is kept.
    pub history_retention_days: u32,
}

impl Default for RequestConfig {
    fn default() -> Self {
        RequestConfig {
            retry_window_secs: 24 * 60 * 60,
            history_retention_days: 90,
        }
    }
}

impl RequestConfig {
    /// How long a stored result can answer a retry.
    #[must_use]
    pub fn retry_window(&self) -> Duration {
        Duration::from_secs(self.retry_window_secs)
    }

    /// How long a history row is kept.
    #[must_use]
    pub fn history_retention(&self) -> Duration {
        Duration::from_secs(u64::from(self.history_retention_days) * 24 * 60 * 60)
    }
}

/// Backups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupConfig {
    /// Daily copies kept, plus the log needed to restore any point in them.
    pub daily_copies: u32,
}

impl Default for BackupConfig {
    fn default() -> Self {
        BackupConfig { daily_copies: 7 }
    }
}

/// The engine manager: where the private PostgreSQL instance lives and when it may be updated
/// (design section 3, "Engine manager").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    /// Folder that holds the instance's data, socket and logs. The service user must own it.
    pub base_dir: PathBuf,
    /// Port number of the first shard's instance. The instance listens on a Unix socket only, so
    /// this number is not reachable from the network; it only names the socket file.
    pub port: u16,
    /// Most connections the instance accepts (the connection budget is checked against it).
    pub max_connections: u32,
    /// The hour of the day, in UTC (0 to 23), when the maintenance window opens. The window is one
    /// hour long. Minor-release updates restart the instance inside it.
    pub maintenance_hour_utc: u8,
    /// Locale the instance's database is created with. Fixed once the database exists.
    pub locale: String,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            base_dir: PathBuf::from("/var/lib/bhdata"),
            port: 54_320,
            max_connections: 100,
            maintenance_hour_utc: 3,
            locale: "C.UTF-8".to_string(),
        }
    }
}

impl EngineConfig {
    /// Checks the engine values that `serde` cannot.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |field, reason| Err(ConfigError::Invalid { field, reason });
        if !self.base_dir.is_absolute() {
            return bad("engine.base_dir", "must be an absolute path");
        }
        if self.port < 1_024 {
            return bad("engine.port", "must be 1024 or higher");
        }
        if self.max_connections < 10 {
            return bad("engine.max_connections", "must be at least 10");
        }
        if self.maintenance_hour_utc > 23 {
            return bad("engine.maintenance_hour_utc", "must be between 0 and 23");
        }
        if self.locale.is_empty() {
            return bad("engine.locale", "must not be empty");
        }
        Ok(())
    }
}

/// Connection pools to the shard's PostgreSQL, one per database role (design section 10c).
///
/// The caps are checked against `engine.max_connections` at start-up (by `bhdata-pg`). The defaults
/// are placeholders until the load runs of milestone M11 show what a real machine needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolConfig {
    /// Connections of the recipe role (the write path).
    pub recipe_writers: u32,
    /// Connections of the reader role.
    pub readers: u32,
    /// Most connections SQL mode may use, in total (unused until M12).
    pub sql_sessions: u32,
    /// Connections of the admin role (migrations, registration, the backups' own work).
    pub admin: u32,
    /// Connections held back for backups and replication.
    pub reserved_backup: u32,
}

impl Default for PoolConfig {
    fn default() -> Self {
        PoolConfig {
            recipe_writers: 16,
            readers: 8,
            sql_sessions: 8,
            admin: 2,
            reserved_backup: 4,
        }
    }
}

impl PoolConfig {
    /// Every connection the data layer may hold open at once, reserved ones included.
    #[must_use]
    pub fn total(&self) -> u64 {
        [
            self.recipe_writers,
            self.readers,
            self.sql_sessions,
            self.admin,
            self.reserved_backup,
        ]
        .into_iter()
        .map(u64::from)
        .sum()
    }
}

/// Limits on recipes, checked when one is registered (spec `recipe-compiler.md`, sections 2 and 8).
/// A registered version keeps working if a limit is lowered later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecipeConfig {
    /// Most steps in one recipe.
    pub max_steps: u32,
    /// Largest `limit` of a `list` or a derived read.
    pub max_list_limit: u32,
    /// Most values in a result.
    pub max_result_values: u32,
    /// Largest result, in bytes.
    pub max_result_bytes: u32,
}

impl Default for RecipeConfig {
    fn default() -> Self {
        RecipeConfig {
            max_steps: 16,
            max_list_limit: 100,
            max_result_values: 32,
            max_result_bytes: 64 * 1024,
        }
    }
}

/// The write path (spec `write-path.md`, sections 5, 7 and 8). The defaults are placeholders until
/// the load runs of milestone M11 show what a real machine needs, like the pool sizes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WriterConfig {
    /// Commands that may wait in a shard's queue.
    pub queue_capacity: u32,
    /// A new command is refused when the oldest queued one has waited longer than this.
    pub max_queue_wait_ms: u64,
    /// Tries of one command in all (the first plus retries after a deadlock or serialization failure).
    pub max_attempts: u32,
    /// The 99th-percentile commit time, over the last 10 seconds, that raises the latency alarm.
    pub commit_latency_alarm_ms: u64,
    /// How far ahead of the server's clock a request's creation time may be.
    pub request_time_tolerance_secs: u64,
    /// How long past the retry window a request record is kept before compaction.
    pub retention_grace_secs: u64,
    /// How often the compaction job runs.
    pub compaction_interval_secs: u64,
    /// Most records one compaction statement moves.
    pub compaction_batch: u32,
    /// How long a shutdown waits for running commands.
    pub shutdown_drain_ms: u64,
}

impl Default for WriterConfig {
    fn default() -> Self {
        WriterConfig {
            queue_capacity: 256,
            max_queue_wait_ms: 500,
            max_attempts: 3,
            commit_latency_alarm_ms: 250,
            request_time_tolerance_secs: 300,
            retention_grace_secs: 3_600,
            compaction_interval_secs: 60,
            compaction_batch: 1_000,
            shutdown_drain_ms: 5_000,
        }
    }
}

impl WriterConfig {
    /// The longest a queued command may wait, as a duration.
    #[must_use]
    pub fn max_queue_wait(&self) -> Duration {
        Duration::from_millis(self.max_queue_wait_ms)
    }

    /// How far ahead a request's creation time may be.
    #[must_use]
    pub fn request_time_tolerance(&self) -> Duration {
        Duration::from_secs(self.request_time_tolerance_secs)
    }

    /// How long past the retry window a record is kept.
    #[must_use]
    pub fn retention_grace(&self) -> Duration {
        Duration::from_secs(self.retention_grace_secs)
    }
}

/// The gateway (wire-protocol spec, sections 2 to 4 and 8). Every default is a placeholder until the
/// load runs of milestone M11, except the ones the spec fixes (frame and JSON limits, timers, lockout).
///
/// There is no auth-off switch: `insecure_local` only allows a plain connection, and only on a
/// loopback address (checked by [`GatewayConfig::validate`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GatewayConfig {
    /// Where the gateway listens.
    pub listen: SocketAddr,
    /// The certificate chain (PEM). Required unless `insecure_local` is set; read at start-up and on reload.
    pub cert_path: Option<PathBuf>,
    /// The private key (PEM) for the certificate.
    pub key_path: Option<PathBuf>,
    /// Allow a plain (non-TLS) connection. Refused on any address that is not loopback.
    pub insecure_local: bool,
    /// Largest frame before login, in bytes.
    pub max_frame_before_login: u32,
    /// Largest frame after login, in bytes.
    pub max_frame: u32,
    /// Deepest JSON nesting, before and after login.
    pub max_json_depth: u32,
    /// Most keys in one JSON object before login.
    pub max_keys_before_login: u32,
    /// Most keys in one JSON object after login.
    pub max_keys: u32,
    /// Longest single string before login, in bytes.
    pub max_string_before_login: u32,
    /// Longest single string after login, in bytes.
    pub max_string: u32,
    /// Requests one connection may have open.
    pub max_inflight_per_connection: u32,
    /// Requests the whole gateway may have open before it sheds with `overloaded`.
    pub max_inflight_total: u32,
    /// The handshake and login must finish within this long.
    pub handshake_timeout_secs: u64,
    /// A logged-in connection with no frame in either direction for this long is closed.
    pub idle_timeout_secs: u64,
    /// A reply that cannot be written within this long closes the connection.
    pub write_timeout_secs: u64,
    /// Login attempts allowed on one connection.
    pub max_login_attempts_per_connection: u32,
    /// Consecutive failed logins for one name that lock it.
    pub lockout_failures: u32,
    /// How long a locked name stays locked.
    pub lockout_secs: u64,
    /// Failed logins from one source address, in a minute, that close further attempts from it.
    pub source_failures_per_minute: u32,
    /// How long that source is closed out.
    pub source_block_secs: u64,
    /// PBKDF2 iterations for a new password verifier.
    pub scram_iterations: u32,
    /// Default requests per second per login (a token bucket); a login record may override it.
    pub rate_per_sec: u32,
    /// Default burst of that bucket.
    pub rate_burst: u32,
    /// Most connections one login may hold.
    pub max_connections_per_login: u32,
    /// How long a shutdown waits for open requests.
    pub drain_ms: u64,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 7_620)),
            cert_path: None,
            key_path: None,
            insecure_local: false,
            max_frame_before_login: 16 * 1_024,
            max_frame: 1_024 * 1_024,
            max_json_depth: 8,
            max_keys_before_login: 64,
            max_keys: 256,
            max_string_before_login: 4 * 1_024,
            max_string: 64 * 1_024,
            max_inflight_per_connection: 32,
            max_inflight_total: 1_024,
            handshake_timeout_secs: 10,
            idle_timeout_secs: 60,
            write_timeout_secs: 10,
            max_login_attempts_per_connection: 3,
            lockout_failures: 5,
            lockout_secs: 900,
            source_failures_per_minute: 20,
            source_block_secs: 60,
            scram_iterations: 100_000,
            rate_per_sec: 200,
            rate_burst: 400,
            max_connections_per_login: 32,
            drain_ms: 5_000,
        }
    }
}

impl GatewayConfig {
    /// `true` unless `insecure_local` is set: TLS 1.3 is the only transport otherwise.
    #[must_use]
    pub fn tls_required(&self) -> bool {
        !self.insecure_local
    }

    /// The handshake-and-login deadline, as a duration.
    #[must_use]
    pub fn handshake_timeout(&self) -> Duration {
        Duration::from_secs(self.handshake_timeout_secs)
    }

    /// The idle limit of a logged-in connection, as a duration.
    #[must_use]
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }

    /// The write deadline, as a duration.
    #[must_use]
    pub fn write_timeout(&self) -> Duration {
        Duration::from_secs(self.write_timeout_secs)
    }

    /// How long a locked name stays locked, as a duration.
    #[must_use]
    pub fn lockout(&self) -> Duration {
        Duration::from_secs(self.lockout_secs)
    }

    /// How long a shutdown waits for open requests, as a duration.
    #[must_use]
    pub fn drain(&self) -> Duration {
        Duration::from_millis(self.drain_ms)
    }

    /// Checks what `serde` cannot: nothing is zero, the after-login limits are not below the
    /// before-login ones, the burst is not below the rate, and a plain connection is only allowed
    /// on a loopback address (wire-protocol spec, decision 1 and test W13).
    pub fn validate(&self) -> Result<(), ConfigError> {
        let positive: [(&'static str, u64); 22] = [
            (
                "gateway.max_frame_before_login",
                u64::from(self.max_frame_before_login),
            ),
            ("gateway.max_frame", u64::from(self.max_frame)),
            ("gateway.max_json_depth", u64::from(self.max_json_depth)),
            (
                "gateway.max_keys_before_login",
                u64::from(self.max_keys_before_login),
            ),
            ("gateway.max_keys", u64::from(self.max_keys)),
            (
                "gateway.max_string_before_login",
                u64::from(self.max_string_before_login),
            ),
            ("gateway.max_string", u64::from(self.max_string)),
            (
                "gateway.max_inflight_per_connection",
                u64::from(self.max_inflight_per_connection),
            ),
            (
                "gateway.max_inflight_total",
                u64::from(self.max_inflight_total),
            ),
            (
                "gateway.handshake_timeout_secs",
                self.handshake_timeout_secs,
            ),
            ("gateway.idle_timeout_secs", self.idle_timeout_secs),
            ("gateway.write_timeout_secs", self.write_timeout_secs),
            (
                "gateway.max_login_attempts_per_connection",
                u64::from(self.max_login_attempts_per_connection),
            ),
            ("gateway.lockout_failures", u64::from(self.lockout_failures)),
            ("gateway.lockout_secs", self.lockout_secs),
            (
                "gateway.source_failures_per_minute",
                u64::from(self.source_failures_per_minute),
            ),
            ("gateway.source_block_secs", self.source_block_secs),
            ("gateway.scram_iterations", u64::from(self.scram_iterations)),
            ("gateway.rate_per_sec", u64::from(self.rate_per_sec)),
            ("gateway.rate_burst", u64::from(self.rate_burst)),
            (
                "gateway.max_connections_per_login",
                u64::from(self.max_connections_per_login),
            ),
            ("gateway.drain_ms", self.drain_ms),
        ];
        for (field, value) in positive {
            if value == 0 {
                return Err(ConfigError::NotPositive { field });
            }
        }
        let bad = |field, reason| Err(ConfigError::Invalid { field, reason });
        if self.max_frame < self.max_frame_before_login {
            return bad(
                "gateway.max_frame",
                "must not be below max_frame_before_login",
            );
        }
        if self.max_keys < self.max_keys_before_login {
            return bad(
                "gateway.max_keys",
                "must not be below max_keys_before_login",
            );
        }
        if self.max_string < self.max_string_before_login {
            return bad(
                "gateway.max_string",
                "must not be below max_string_before_login",
            );
        }
        if self.rate_burst < self.rate_per_sec {
            return bad("gateway.rate_burst", "must not be below rate_per_sec");
        }
        if self.insecure_local && !self.listen.ip().is_loopback() {
            return bad(
                "gateway.insecure_local",
                "is only allowed on a loopback listen address",
            );
        }
        Ok(())
    }
}

/// The whole config. Every field has a default, so an empty file is a valid config.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How commits are made durable.
    pub durability: Durability,
    /// The memory cache.
    pub cache: CacheConfig,
    /// Read replicas.
    pub replicas: ReplicaConfig,
    /// Request records and history.
    pub requests: RequestConfig,
    /// Backups.
    pub backup: BackupConfig,
    /// The engine manager.
    pub engine: EngineConfig,
    /// Connection pools.
    pub pools: PoolConfig,
    /// Recipe limits.
    pub recipes: RecipeConfig,
    /// The write path.
    pub writer: WriterConfig,
    /// The gateway (M5).
    pub gateway: GatewayConfig,
}

impl Config {
    /// Checks the values that `serde` cannot: anything that must be above zero.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let checks: [(&'static str, u64); 21] = [
            ("cache.ttl_ms", self.cache.ttl_ms),
            ("replicas.max_lag_ms", self.replicas.max_lag_ms),
            (
                "requests.retry_window_secs",
                self.requests.retry_window_secs,
            ),
            (
                "requests.history_retention_days",
                u64::from(self.requests.history_retention_days),
            ),
            ("backup.daily_copies", u64::from(self.backup.daily_copies)),
            ("pools.recipe_writers", u64::from(self.pools.recipe_writers)),
            ("pools.readers", u64::from(self.pools.readers)),
            ("pools.admin", u64::from(self.pools.admin)),
            ("recipes.max_steps", u64::from(self.recipes.max_steps)),
            (
                "recipes.max_list_limit",
                u64::from(self.recipes.max_list_limit),
            ),
            (
                "recipes.max_result_values",
                u64::from(self.recipes.max_result_values),
            ),
            (
                "recipes.max_result_bytes",
                u64::from(self.recipes.max_result_bytes),
            ),
            (
                "writer.queue_capacity",
                u64::from(self.writer.queue_capacity),
            ),
            ("writer.max_queue_wait_ms", self.writer.max_queue_wait_ms),
            ("writer.max_attempts", u64::from(self.writer.max_attempts)),
            (
                "writer.commit_latency_alarm_ms",
                self.writer.commit_latency_alarm_ms,
            ),
            (
                "writer.request_time_tolerance_secs",
                self.writer.request_time_tolerance_secs,
            ),
            (
                "writer.retention_grace_secs",
                self.writer.retention_grace_secs,
            ),
            (
                "writer.compaction_interval_secs",
                self.writer.compaction_interval_secs,
            ),
            (
                "writer.compaction_batch",
                u64::from(self.writer.compaction_batch),
            ),
            ("writer.shutdown_drain_ms", self.writer.shutdown_drain_ms),
        ];
        for (field, value) in checks {
            if value == 0 {
                return Err(ConfigError::NotPositive { field });
            }
        }
        self.engine.validate()?;
        self.gateway.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_ones_in_the_design() {
        let c = Config::default();
        assert_eq!(c.durability, Durability::Full);
        assert_eq!(c.cache.ttl(), Duration::from_secs(2));
        assert_eq!(c.replicas.max_lag(), Duration::from_secs(1));
        assert_eq!(c.requests.retry_window(), Duration::from_secs(24 * 3600));
        assert_eq!(
            c.requests.history_retention(),
            Duration::from_secs(90 * 24 * 3600)
        );
        assert_eq!(c.backup.daily_copies, 7);
        assert_eq!(
            (
                c.recipes.max_steps,
                c.recipes.max_list_limit,
                c.recipes.max_result_values,
                c.recipes.max_result_bytes
            ),
            (16, 100, 32, 65_536)
        );
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn slots_are_4096_and_the_mask_matches() {
        assert_eq!(SLOT_COUNT, 4_096);
        assert_eq!(SLOT_MASK, 0xfff);
        assert_eq!(SLOT_MASK + 1, u64::from(SLOT_COUNT));
        assert_eq!(0xdead_beef_u64 & SLOT_MASK, 0xeef);
    }

    #[test]
    fn writer_defaults_are_the_ones_in_the_write_path_spec() {
        let w = WriterConfig::default();
        assert_eq!(w.queue_capacity, 256);
        assert_eq!(w.max_queue_wait(), Duration::from_millis(500));
        assert_eq!(w.max_attempts, 3);
        assert_eq!(w.commit_latency_alarm_ms, 250);
        assert_eq!(w.request_time_tolerance(), Duration::from_secs(300));
        assert_eq!(w.retention_grace(), Duration::from_secs(3_600));
        assert_eq!(w.compaction_interval_secs, 60);
        assert_eq!(w.compaction_batch, 1_000);
        assert_eq!(w.shutdown_drain_ms, 5_000);
        assert_eq!(Config::default().writer, w);
    }

    #[test]
    fn writer_settings_can_be_partial_but_not_misspelled_or_zero() {
        let c: Config = toml::from_str("[writer]\nmax_attempts = 5\n").unwrap();
        assert_eq!(c.writer.max_attempts, 5);
        assert_eq!(c.writer.queue_capacity, 256);
        assert!(toml::from_str::<Config>("[writer]\nmax_attempt = 5\n").is_err());
        type Zero = (&'static str, fn(&mut WriterConfig));
        let cases: [Zero; 5] = [
            ("writer.queue_capacity", |w| w.queue_capacity = 0),
            ("writer.max_queue_wait_ms", |w| w.max_queue_wait_ms = 0),
            ("writer.max_attempts", |w| w.max_attempts = 0),
            ("writer.compaction_batch", |w| w.compaction_batch = 0),
            ("writer.shutdown_drain_ms", |w| w.shutdown_drain_ms = 0),
        ];
        for (field, set) in cases {
            let mut c = Config::default();
            set(&mut c.writer);
            assert_eq!(c.validate(), Err(ConfigError::NotPositive { field }));
        }
    }

    #[test]
    fn an_empty_file_is_the_default_config() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn a_partial_file_keeps_defaults_for_the_rest() {
        let c: Config = toml::from_str("[cache]\nttl_ms = 500\n").unwrap();
        assert_eq!(c.cache.ttl(), Duration::from_millis(500));
        assert_eq!(c.replicas, ReplicaConfig::default());
        assert_eq!(c.requests, RequestConfig::default());
    }

    #[test]
    fn relaxed_durability_must_be_asked_for_by_name() {
        let c: Config = toml::from_str("durability = \"relaxed\"").unwrap();
        assert_eq!(c.durability, Durability::Relaxed);
        assert!(toml::from_str::<Config>("durability = \"fast\"").is_err());
    }

    #[test]
    fn unknown_keys_are_errors_not_ignored() {
        assert!(toml::from_str::<Config>("cache_ttl = 5").is_err());
        assert!(toml::from_str::<Config>("[cache]\nttl = 5\n").is_err());
    }

    #[test]
    fn a_config_round_trips_through_toml() {
        let c = Config {
            durability: Durability::Relaxed,
            backup: BackupConfig { daily_copies: 14 },
            ..Config::default()
        };
        let text = toml::to_string(&c).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), c);
    }

    #[test]
    fn engine_defaults_are_valid_and_private() {
        let e = EngineConfig::default();
        assert_eq!(e.base_dir, PathBuf::from("/var/lib/bhdata"));
        assert_eq!(e.port, 54_320);
        assert_eq!(e.maintenance_hour_utc, 3);
        assert_eq!(e.validate(), Ok(()));
    }

    #[test]
    fn engine_values_out_of_range_are_named() {
        let cases: [(&str, EngineConfig); 5] = [
            (
                "engine.base_dir",
                EngineConfig {
                    base_dir: PathBuf::from("relative/dir"),
                    ..EngineConfig::default()
                },
            ),
            (
                "engine.port",
                EngineConfig {
                    port: 80,
                    ..EngineConfig::default()
                },
            ),
            (
                "engine.max_connections",
                EngineConfig {
                    max_connections: 3,
                    ..EngineConfig::default()
                },
            ),
            (
                "engine.maintenance_hour_utc",
                EngineConfig {
                    maintenance_hour_utc: 24,
                    ..EngineConfig::default()
                },
            ),
            (
                "engine.locale",
                EngineConfig {
                    locale: String::new(),
                    ..EngineConfig::default()
                },
            ),
        ];
        for (name, engine) in cases {
            match engine.validate() {
                Err(ConfigError::Invalid { field, .. }) => assert_eq!(field, name),
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    #[test]
    fn engine_settings_read_from_toml_and_reach_config_validation() {
        let c: Config =
            toml::from_str("[engine]\nmaintenance_hour_utc = 5\nport = 6000\n").unwrap();
        assert_eq!(c.engine.maintenance_hour_utc, 5);
        assert_eq!(c.engine.port, 6000);
        assert_eq!(c.engine.max_connections, 100);
        assert_eq!(c.validate(), Ok(()));
        let c: Config = toml::from_str("[engine]\nmaintenance_hour_utc = 30\n").unwrap();
        assert!(c.validate().is_err());
        assert!(toml::from_str::<Config>("[engine]\nlisten_address = \"0.0.0.0\"\n").is_err());
    }

    #[test]
    fn zero_values_fail_validation_and_name_the_setting() {
        let mut c = Config::default();
        c.requests.retry_window_secs = 0;
        assert_eq!(
            c.validate(),
            Err(ConfigError::NotPositive {
                field: "requests.retry_window_secs"
            })
        );
        let mut c = Config::default();
        c.cache.ttl_ms = 0;
        assert_eq!(
            c.validate(),
            Err(ConfigError::NotPositive {
                field: "cache.ttl_ms"
            })
        );
    }

    #[test]
    fn pool_defaults_total_38() {
        let p = PoolConfig::default();
        assert_eq!(
            (
                p.recipe_writers,
                p.readers,
                p.sql_sessions,
                p.admin,
                p.reserved_backup
            ),
            (16, 8, 8, 2, 4)
        );
        assert_eq!(p.total(), 38);
        assert_eq!(Config::default().validate(), Ok(()));
    }

    #[test]
    fn pool_section_reads_a_subset_and_refuses_unknown_keys() {
        let c: Config = toml::from_str("[pools]\nreaders = 12\n").unwrap();
        assert_eq!(c.pools.readers, 12);
        assert_eq!(c.pools.recipe_writers, 16);
        assert!(toml::from_str::<Config>("[pools]\nrecipe_writer = 3\n").is_err());
    }

    #[test]
    fn pools_that_must_exist_cannot_be_zero_but_sql_and_backup_can() {
        for (field, set) in [
            (
                "pools.recipe_writers",
                (|p: &mut PoolConfig| p.recipe_writers = 0) as fn(&mut PoolConfig),
            ),
            ("pools.readers", |p| p.readers = 0),
            ("pools.admin", |p| p.admin = 0),
        ] {
            let mut c = Config::default();
            set(&mut c.pools);
            assert_eq!(c.validate(), Err(ConfigError::NotPositive { field }));
        }
        let c = Config {
            pools: PoolConfig {
                sql_sessions: 0,
                reserved_backup: 0,
                ..PoolConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn pool_total_does_not_overflow() {
        let p = PoolConfig {
            recipe_writers: u32::MAX,
            readers: u32::MAX,
            sql_sessions: u32::MAX,
            admin: u32::MAX,
            reserved_backup: u32::MAX,
        };
        assert_eq!(p.total(), 5 * u64::from(u32::MAX));
    }

    #[test]
    fn gateway_defaults_are_the_ones_in_the_wire_protocol_spec() {
        let g = GatewayConfig::default();
        assert_eq!(g.max_frame_before_login, 16 * 1_024);
        assert_eq!(g.max_frame, 1_024 * 1_024);
        assert_eq!(
            (g.max_json_depth, g.max_keys_before_login, g.max_keys),
            (8, 64, 256)
        );
        assert_eq!(
            (g.max_string_before_login, g.max_string),
            (4 * 1_024, 64 * 1_024)
        );
        assert_eq!(
            (g.max_inflight_per_connection, g.max_inflight_total),
            (32, 1_024)
        );
        assert_eq!(
            (
                g.handshake_timeout_secs,
                g.idle_timeout_secs,
                g.write_timeout_secs
            ),
            (10, 60, 10)
        );
        assert_eq!(g.max_login_attempts_per_connection, 3);
        assert_eq!((g.lockout_failures, g.lockout_secs), (5, 900));
        assert_eq!(
            (g.source_failures_per_minute, g.source_block_secs),
            (20, 60)
        );
        assert_eq!(g.scram_iterations, 100_000);
        assert_eq!(
            (g.rate_per_sec, g.rate_burst, g.max_connections_per_login),
            (200, 400, 32)
        );
        assert_eq!(g.drain_ms, 5_000);
        assert!(g.tls_required());
        assert!(g.listen.ip().is_loopback());
        assert_eq!(Config::default().gateway, g);
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn gateway_settings_can_be_partial_but_not_misspelled_or_zero() {
        let c: Config = toml::from_str("[gateway]\nlockout_failures = 7\n").unwrap();
        assert_eq!(c.gateway.lockout_failures, 7);
        assert_eq!(c.gateway.lockout_secs, 900);
        assert!(toml::from_str::<Config>("[gateway]\nlockout_failure = 7\n").is_err());
        type Zero = (&'static str, fn(&mut GatewayConfig));
        let zeros: [Zero; 6] = [
            ("gateway.max_frame", |g| g.max_frame = 0),
            ("gateway.max_json_depth", |g| g.max_json_depth = 0),
            ("gateway.idle_timeout_secs", |g| g.idle_timeout_secs = 0),
            ("gateway.lockout_failures", |g| g.lockout_failures = 0),
            ("gateway.rate_per_sec", |g| g.rate_per_sec = 0),
            ("gateway.drain_ms", |g| g.drain_ms = 0),
        ];
        for (field, set) in zeros {
            let mut c = Config::default();
            set(&mut c.gateway);
            assert_eq!(c.validate(), Err(ConfigError::NotPositive { field }));
        }
    }

    #[test]
    fn gateway_limits_after_login_are_not_below_those_before_it() {
        let mut g = GatewayConfig::default();
        g.max_frame = g.max_frame_before_login - 1;
        assert!(g.validate().is_err());
        let mut g = GatewayConfig::default();
        g.rate_burst = g.rate_per_sec - 1;
        assert!(g.validate().is_err());
    }

    #[test]
    fn a_plain_gateway_connection_is_only_allowed_on_loopback() {
        let mut g = GatewayConfig {
            insecure_local: true,
            ..GatewayConfig::default()
        };
        assert!(g.validate().is_ok());
        assert!(!g.tls_required());
        g.listen = "0.0.0.0:7620".parse().unwrap();
        assert!(g.validate().is_err());
        g.listen = "[::1]:7620".parse().unwrap();
        assert!(g.validate().is_ok());
        g.insecure_local = false;
        g.listen = "10.0.0.5:7620".parse().unwrap();
        assert!(g.validate().is_ok());
    }
}
