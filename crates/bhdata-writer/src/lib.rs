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
//! The execution of a command on a real connection arrives with slice B.

pub mod admission;
pub mod result;
pub mod retry;
pub mod time_rule;
