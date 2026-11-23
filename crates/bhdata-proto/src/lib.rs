//! Wire frames and messages between the SDK and the gateway.
//!
//! Milestone M5. Each module below belongs to one lane (`docs/specs/m5-lanes.md`); a module that is
//! empty has not been built yet.

pub mod error_map;
pub mod frame;
pub mod json;
pub mod message;
pub mod scram;

/// The named errors and ids, re-exported for `bhdata-client`.
pub use bhdata_types as types;
