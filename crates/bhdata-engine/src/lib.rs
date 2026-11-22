//! Engine manager: install, run and update the private PostgreSQL (design section 3).
//!
//! * [`instance`]: create one private instance (own data folder, own socket, no network port) and
//!   write the settings the data layer manages.
//! * [`supervise`]: start it, watch it, restart it with backoff when it dies or stops answering.
//! * [`install`]: call the privileged helper (`bhdata-helper`) for package actions.
//! * [`update`]: minor-release updates, inside the maintenance window, one instance at a time.
//! * [`host`]: what the service runs at M1: load the config, start and watch the instance, apply
//!   updates in the window; plus the report behind `bhdatactl status`.
//!
//! Only this crate and `bhdata-pg` talk to PostgreSQL (plan section 5). This crate never opens a
//! database connection: health checks use `pg_isready`, because `bhdata-pg` is built at M2.
//!
//! Nothing here runs as root. The one root step, installing packages, is the helper's job.

pub mod error;
pub mod host;
pub mod install;
pub mod instance;
pub mod supervise;
mod sys;
#[cfg(test)]
mod testutil;
pub mod update;

pub use error::EngineError;

/// The PostgreSQL major version this release is pinned to. Must equal `PG_MAJOR` in
/// `bhdata-helper` (a test compares the two source files, since the helper has no dependencies).
pub const PG_MAJOR: u32 = 17;

/// Where the PostgreSQL packages put their programs for the pinned major version.
pub fn default_pg_bin_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/usr/lib/postgresql/{PG_MAJOR}/bin"))
}
