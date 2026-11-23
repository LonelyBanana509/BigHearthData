//! Write queue, admission control and command execution (spec `write-path.md`).
//!
//! M4 slice A: the parts that need no database, each a pure function or a small value type that
//! takes the time as an argument, so tests run on a fake clock.
//!
//! - [`time_rule`]: whether a request is too old or too far ahead (section 4).
//! - [`retry`]: what to do after a deadlock or serialization failure (section 5).
//! - [`admission`]: the queue's admission rule and the commit-latency watch (section 7).
//! - [`result`]: the stored result, in canonical JSON (section 7).
//!
//! M4 slice B adds what runs on a real connection:
//!
//! - [`exec`]: one command in one transaction, the heart of the write path (sections 3 and 5).
//! - [`convert`]: values to and from the database, by the plan's types.
//! - [`history`]: compaction and the history purge (section 8).
//!
//! M4 slice C: [`queue`], the bounded queue and its writer tasks in front of the executor, which
//! implements `ShardBackend`. [`compaction`], the timer job that moves old request records to the history. The host wiring and
//! `bhdatactl call` follow.

pub mod admission;
pub mod compaction;
pub mod convert;
pub mod exec;
pub mod history;
pub mod queue;
pub mod result;
pub mod retry;
pub mod startup;
pub mod time_rule;
