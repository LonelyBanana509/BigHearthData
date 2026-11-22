//! Schema declaration, shard keys and validation (spec `docs/specs/recipe-format.md`, section 1).
//!
//! Everything here is pure: text in, data or SQL text out. Nothing opens a connection, reads a
//! clock or touches a file.
//!
//! * [`model`]: tables, columns and indexes as data.
//! * [`parse`]: the TOML schema file to the model, then [`validate`].
//! * [`validate`]: every table has a shard key or is global; children follow subjects.
//! * [`ddl`]: the model to `CREATE TABLE` and friends, in the `app` schema only.
//! * [`diff`]: two models to a list of changes; destructive ones are flagged for an admin.

pub mod ddl;
mod decimal;
pub mod diff;
pub mod model;
pub mod parse;
pub mod validate;

pub use parse::parse_schema;
pub use validate::validate;

/// Why a schema was refused: where, and in plain words. The first problem found is reported.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {message}")]
pub struct SchemaError {
    /// Where the problem is, like `table users, column balance`; empty for a file-level problem.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl SchemaError {
    pub(crate) fn new(path: impl Into<String>, message: impl Into<String>) -> SchemaError {
        SchemaError {
            path: path.into(),
            message: message.into(),
        }
    }
}
