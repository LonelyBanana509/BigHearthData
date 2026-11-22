//! Change log format, positions, readers and trimming (spec `write-path.md`, section 6).
//!
//! M4 slice A: the format and the reading rule, with no database.
//!
//! - [`entry`]: what one entry says (a row reference, never a value) and the [`entry::Collector`]
//!   that turns a command's writes into one entry per changed row.
//! - [`position`]: a position is `(xid, seq)`; the horizon rule that makes a reader see a gap-free
//!   prefix; the SQL of the read; and a pure model of that read that the tests drive.
//!
//! The migration, the reader on a real connection and the trim function arrive with slice B.

pub mod entry;
pub mod position;

pub use entry::{Collector, Entry, Op};
pub use position::Position;
