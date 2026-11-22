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
}

impl Config {
    /// Checks the values that `serde` cannot: anything that must be above zero.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let checks: [(&'static str, u64); 12] = [
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
        ];
        for (field, value) in checks {
            if value == 0 {
                return Err(ConfigError::NotPositive { field });
            }
        }
        self.engine.validate()
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
}
