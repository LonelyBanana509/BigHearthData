//! Control plane: recipe registry, shard map, logins, audit and admin API.
//!
//! Milestone M3 (slice C) builds the registry first (spec `docs/specs/recipe-compiler.md`,
//! section 5); the shard map, logins and audit come with M5, the admin API with M8.
//!
//! * [`registry`]: applying a schema, registering a recipe, the grants, and reading them back.
//! * [`error`]: [`ControlError`] and the error a client would see.
//! * [`blocking`]: the same calls on a private runtime with one admin connection, for `bhdatactl`.

pub mod blocking;
pub mod error;
pub mod registry;

pub use bhdata_pg::pool::Target;
pub use error::ControlError;
