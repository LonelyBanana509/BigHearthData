//! Trait definitions for the seams (ShardBackend, ControlPlane, TxnCoordinator, DerivedStore, BackupTarget, RequestHandler); no implementations.
//!
//! Empty at M0: the real code arrives with milestone M3.

/// The shared types, re-exported so crates that sit above the seams do not list `bhdata-types` themselves.
pub use bhdata_types as types;
