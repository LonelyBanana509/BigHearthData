//! Ids, named errors, config structs, clock and version types shared by every crate.
//!
//! This crate imports nothing from the workspace. Everything that more than one crate needs to
//! agree on lives here, once:
//!
//! * [`ids`]: request, login, subject and transfer ids, and the deterministic id derivation.
//! * [`error`]: the named errors (plan section 3). Mapped to wire codes in `bhdata-proto`.
//! * [`config`]: config structs and their defaults.
//! * [`time`]: the [`Clock`] trait, so tests control time.
//! * [`version`]: recipe, map, epoch and term versions (stage 2 types exist from day one).

pub mod config;
pub mod error;
pub mod ids;
pub mod time;
pub mod version;

pub use config::{Config, ConfigError, Durability, EngineConfig, SLOT_COUNT};
pub use error::{Error, ErrorKind, Result};
pub use ids::{LoginId, RequestId, SubjectId, TransferId};
pub use time::{Clock, ManualClock, SystemClock, Timestamp};
pub use version::{Epoch, MapVersion, RecipeVersion, Term};
