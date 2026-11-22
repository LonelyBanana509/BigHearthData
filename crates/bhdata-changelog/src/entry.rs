//! What one change-log entry says, and how a command's writes become entries (spec
//! `write-path.md`, section 6).
//!
//! An entry names a changed **row**; it carries no values. Consumers must not assume that entries
//! of one row arrive in commit order (the log is ordered by transaction id), so a consumer that
//! needs the new value reads the row, and converges whatever the order.

use std::collections::BTreeMap;

/// What happened to the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    /// The command created the row (an `ensure` that affected one row, or an `insert`).
    Insert,
    /// The row existed and the command changed it.
    Update,
}

impl Op {
    /// The text stored in the `op` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Op::Insert => "insert",
            Op::Update => "update",
        }
    }
}

/// One entry, as `append_change` takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The table.
    pub table: String,
    /// The row's key as text (see [`subject_row_key`] and [`child_row_key`]).
    pub row_key: String,
    /// Insert or update.
    pub op: Op,
    /// The shard key of the row's owner, so a worker can route by it (the counterparty's id for a
    /// counterparty row).
    pub subject: String,
}

/// The row key of a subject row: the owner's id.
#[must_use]
pub fn subject_row_key(owner: &str) -> String {
    owner.to_string()
}

/// The row key of a child row: `"<owner>/<seq>"`.
#[must_use]
pub fn child_row_key(owner: &str, seq: i64) -> String {
    format!("{owner}/{seq}")
}

/// Collects the rows a command changed, in the order they were first touched, one entry per row.
///
/// Two steps that touch the same row give one entry, and it is an `insert` if any step created the
/// row. A step that affected no row (an `ensure` on a row that already exists) is not a change and
/// is not [`note`](Collector::note)d at all, or is noted with `affected == 0`, which is ignored.
#[derive(Debug, Default)]
pub struct Collector {
    order: Vec<(String, String)>,
    rows: BTreeMap<(String, String), (Op, String)>,
}

impl Collector {
    /// An empty collector (a fresh attempt starts with one; a retried attempt drops the old one).
    #[must_use]
    pub fn new() -> Self {
        Collector::default()
    }

    /// Notes one write step: `affected` rows changed in `table` at `row_key`, owned by `subject`;
    /// `created` is true when the step made the row. Does nothing if `affected` is 0.
    pub fn note(
        &mut self,
        table: &str,
        row_key: &str,
        subject: &str,
        created: bool,
        affected: u64,
    ) {
        if affected == 0 {
            return;
        }
        let key = (table.to_string(), row_key.to_string());
        let op = if created { Op::Insert } else { Op::Update };
        match self.rows.get_mut(&key) {
            Some((existing, _)) => {
                if op == Op::Insert {
                    *existing = Op::Insert;
                }
            }
            None => {
                self.order.push(key.clone());
                self.rows.insert(key, (op, subject.to_string()));
            }
        }
    }

    /// How many rows changed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The entries, in the order the rows were first touched.
    #[must_use]
    pub fn entries(&self) -> Vec<Entry> {
        self.order
            .iter()
            .filter_map(|key| {
                self.rows.get(key).map(|(op, subject)| Entry {
                    table: key.0.clone(),
                    row_key: key.1.clone(),
                    op: *op,
                    subject: subject.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_keys() {
        assert_eq!(subject_row_key("alice"), "alice");
        assert_eq!(child_row_key("alice", 7), "alice/7");
        assert_eq!(Op::Insert.as_str(), "insert");
        assert_eq!(Op::Update.as_str(), "update");
    }

    #[test]
    fn one_entry_per_changed_row_in_first_touch_order() {
        let mut c = Collector::new();
        c.note("account", "alice", "alice", false, 1);
        c.note("account", "bob", "bob", false, 1);
        c.note("account", "alice", "alice", false, 1);
        let e = c.entries();
        assert_eq!(e.len(), 2);
        assert_eq!(
            (e[0].table.as_str(), e[0].row_key.as_str()),
            ("account", "alice")
        );
        assert_eq!(e[1].row_key, "bob");
        assert_eq!(e[1].subject, "bob");
        assert!(e.iter().all(|x| x.op == Op::Update));
    }

    #[test]
    fn a_row_created_by_any_step_is_an_insert() {
        let mut c = Collector::new();
        c.note("account", "alice", "alice", true, 1); // ensure created it
        c.note("account", "alice", "alice", false, 1); // add changed it
        assert_eq!(c.entries()[0].op, Op::Insert);
        let mut c = Collector::new();
        c.note("account", "alice", "alice", false, 1);
        c.note("account", "alice", "alice", true, 1);
        assert_eq!(c.entries()[0].op, Op::Insert);
    }

    #[test]
    fn an_ensure_that_found_the_row_is_not_a_change() {
        let mut c = Collector::new();
        c.note("account", "alice", "alice", true, 0);
        assert!(c.is_empty());
        assert!(c.entries().is_empty());
    }

    #[test]
    fn child_rows_and_counterparty_rows_keep_their_owner() {
        let mut c = Collector::new();
        c.note("ledger", &child_row_key("alice", 1), "alice", true, 1);
        c.note("account", "bob", "bob", false, 1);
        let e = c.entries();
        assert_eq!(e[0].row_key, "alice/1");
        assert_eq!(e[0].subject, "alice");
        assert_eq!(e[1].subject, "bob");
        assert_eq!(c.len(), 2);
    }
}
