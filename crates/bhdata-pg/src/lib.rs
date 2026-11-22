//! PostgreSQL access: pools, roles and error mapping (design sections 9 and 10c).
//!
//! Slice A of milestone M2 (spec: `docs/specs/pg-access.md`) holds the parts that need no running
//! server, so each can be tested alone:
//!
//! * [`errors`]: maps a SQLSTATE code to a kind, a retry rule and the error a client would see.
//! * [`conn_budget`]: checks that all pools fit inside the instance's connection limit.
//! * [`roles`]: the database roles, their rights and timeouts, and the login map, as SQL text.
//! * [`internal`]: the internal tables, the narrow functions and their migrations, as SQL text.
//!
//! Pools and the bootstrap that runs this SQL arrive with slice B. Nothing here opens a
//! connection.

pub mod conn_budget;
pub mod errors;
pub mod internal;
pub mod roles;

pub use errors::{classify, PgError, PgErrorKind, Retry};
