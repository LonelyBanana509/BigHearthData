//! Trait definitions for the seams (ShardBackend, ControlPlane, TxnCoordinator, DerivedStore, BackupTarget, RequestHandler); no implementations.
//!
//! `handler` (the request seam) and `login` (the login store and audit events) arrive with M5.
//! `shard` holds the write side from M4: [`shard::Command`], [`shard::Outcome`], [`shard::ShardBackend`]
//! and the payload hash. The other seams arrive with their milestones.

pub mod handler;
pub mod login;
pub mod shard;

/// The recipe crate, re-exported: crates above the seams use the plan and the evaluator through here.
pub use bhdata_recipe as recipe;

/// The shared types, re-exported so crates that sit above the seams do not list `bhdata-types` themselves.
pub use bhdata_types as types;
