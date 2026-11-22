//! Positions and the safe horizon (spec `write-path.md`, section 6).
//!
//! `change_log.seq` is handed out when a statement runs, not when the transaction commits, so a
//! reader that remembers "I have read up to seq 11" can miss seq 10 when its transaction commits
//! late. The fix is a position of `(xid, seq)` and a reader that stops at a horizon: every
//! transaction with an id below the snapshot's `xmin` has finished, so nothing can still appear
//! below it, and the reader sees a gap-free prefix in order.
//!
//! [`READ_AFTER_SQL`] is the statement. [`read_batch`] is the same rule on a list, so the rule is
//! tested without a database; slice B tests the statement itself.

/// A place in the log: the writing transaction's id, then the sequence number within the log.
/// Entries of one transaction share an `xid` and keep `seq` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    /// The transaction id (`xid8`, so it never wraps).
    pub xid: u64,
    /// The sequence number.
    pub seq: i64,
}

impl Position {
    /// Before everything. A reader that has read nothing starts here.
    pub const START: Position = Position { xid: 0, seq: 0 };

    /// A position.
    #[must_use]
    pub const fn new(xid: u64, seq: i64) -> Self {
        Position { xid, seq }
    }
}

/// The read of one batch: entries after `($1, $2)` and below the horizon, in order. The position is
/// compared as `(xid, seq)` with the parameters cast, so an index on `(xid, seq)` can be used. The horizon is
/// the oldest transaction that may still be running or about to commit, taken from the reading
/// transaction's own snapshot.
pub const READ_AFTER_SQL: &str = "\
SELECT xid::text::bigint, seq, tbl, row_key, op, subject FROM change_log \
WHERE (xid, seq) > ($1::text::xid8, $2::bigint) \
AND xid < pg_snapshot_xmin(pg_current_snapshot()) \
ORDER BY xid, seq LIMIT $3";

/// The reading rule on a list of entries `(position, payload)`: those after `after` and with an
/// `xid` below `horizon`, in position order, at most `limit`. `log` need not be sorted.
#[must_use]
pub fn read_batch<T: Clone>(
    log: &[(Position, T)],
    after: Position,
    horizon: u64,
    limit: usize,
) -> Vec<(Position, T)> {
    let mut out: Vec<(Position, T)> = log
        .iter()
        .filter(|(p, _)| *p > after && p.xid < horizon)
        .cloned()
        .collect();
    out.sort_by_key(|(p, _)| *p);
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    #[test]
    fn positions_order_by_transaction_first_then_sequence() {
        assert!(Position::new(10, 99) < Position::new(11, 1));
        assert!(Position::new(10, 1) < Position::new(10, 2));
        assert!(Position::START < Position::new(1, 1));
        assert_eq!(Position::new(3, 4), Position::new(3, 4));
    }

    #[test]
    fn the_statement_reads_after_a_position_below_the_horizon_in_order() {
        for part in [
            "(xid, seq) > ($1::text::xid8, $2::bigint)",
            "xid < pg_snapshot_xmin(pg_current_snapshot())",
            "ORDER BY xid, seq",
            "LIMIT $3",
        ] {
            assert!(READ_AFTER_SQL.contains(part), "{part}");
        }
    }

    /// The example of the spec: A takes seq 10 and B takes seq 11, B commits first.
    #[test]
    fn a_late_commit_is_not_lost() {
        let a = (Position::new(100, 10), "A");
        let b = (Position::new(101, 11), "B");
        // B has committed, A has not: the horizon is A's id, so the reader sees nothing of either.
        let seen = read_batch(&[b], Position::START, 100, 10);
        assert!(seen.is_empty());
        // A commits: the horizon moves past both, and the reader gets A then B.
        let seen = read_batch(&[a, b], Position::START, 102, 10);
        assert_eq!(seen, vec![a, b]);
    }

    #[test]
    fn a_batch_is_limited_and_resumes_after_its_last_position() {
        let log: Vec<(Position, u32)> = (1..=5)
            .map(|i| (Position::new(i, i as i64), i as u32))
            .collect();
        let first = read_batch(&log, Position::START, 100, 2);
        assert_eq!(first.len(), 2);
        let last = first.last().map(|(p, _)| *p).unwrap();
        let next = read_batch(&log, last, 100, 2);
        assert_eq!(next.iter().map(|(_, v)| *v).collect::<Vec<_>>(), vec![3, 4]);
    }

    #[test]
    fn entries_of_one_transaction_stay_together_in_seq_order() {
        let log = [
            (Position::new(7, 5), 'b'),
            (Position::new(7, 4), 'a'),
            (Position::new(6, 9), 'z'),
        ];
        let seen: Vec<char> = read_batch(&log, Position::START, 8, 10)
            .into_iter()
            .map(|(_, c)| c)
            .collect();
        assert_eq!(seen, vec!['z', 'a', 'b']);
    }

    /// One transaction in the simulation: its id, the seq numbers it took, and when it commits.
    #[derive(Debug, Clone)]
    struct Tx {
        xid: u64,
        seqs: Vec<i64>,
        commit_at: usize,
    }

    proptest! {
        /// Whatever order transactions commit in, and whenever the reader polls, it never loses an
        /// entry, never sees one twice, and sees them in position order. After the last commit it
        /// has seen every entry.
        #[test]
        fn a_reader_never_loses_or_repeats_an_entry(
            sizes in proptest::collection::vec(1usize..4, 1..12),
            order in proptest::collection::vec(any::<u32>(), 12),
            polls in proptest::collection::vec(0usize..14, 1..10),
            batch in 1usize..6,
        ) {
            // Transactions take ids in order, and seqs in the order their statements run: here, id
            // order too (the worst case for the horizon is the other way round, covered by the
            // commit order below).
            let mut next_seq = 1i64;
            let mut txs: Vec<Tx> = Vec::new();
            for (i, n) in sizes.iter().enumerate() {
                let seqs: Vec<i64> = (0..*n).map(|_| { let s = next_seq; next_seq += 1; s }).collect();
                txs.push(Tx { xid: 100 + i as u64, seqs, commit_at: 0 });
            }
            // A commit order: a permutation of the transactions from the random keys.
            let mut idx: Vec<usize> = (0..txs.len()).collect();
            idx.sort_by_key(|i| order[*i % order.len()]);
            for (step, i) in idx.iter().enumerate() {
                txs[*i].commit_at = step + 1;
            }
            let total = txs.len();
            let mut polls = polls;
            polls.push(total + 1); // a last poll after everything committed
            polls.sort_unstable();

            let mut after = Position::START;
            let mut seen: Vec<Position> = Vec::new();
            for at in polls {
                // At step `at`, transactions with commit_at <= at are committed.
                let committed: Vec<&Tx> = txs.iter().filter(|t| t.commit_at <= at).collect();
                let log: Vec<(Position, ())> = committed
                    .iter()
                    .flat_map(|t| t.seqs.iter().map(|s| (Position::new(t.xid, *s), ())))
                    .collect();
                // The horizon: the oldest transaction that has not finished, else one past the last.
                let horizon = txs
                    .iter()
                    .filter(|t| t.commit_at > at)
                    .map(|t| t.xid)
                    .min()
                    .unwrap_or(100 + total as u64);
                loop {
                    let got = read_batch(&log, after, horizon, batch);
                    if got.is_empty() { break; }
                    for (p, ()) in &got {
                        prop_assert!(*p > after, "went backwards");
                        after = *p;
                        seen.push(*p);
                    }
                }
            }
            let unique: BTreeSet<Position> = seen.iter().copied().collect();
            prop_assert_eq!(unique.len(), seen.len(), "an entry was read twice");
            let expected: BTreeSet<Position> = txs
                .iter()
                .flat_map(|t| t.seqs.iter().map(|s| Position::new(t.xid, *s)))
                .collect();
            prop_assert_eq!(unique, expected, "an entry was lost");
            prop_assert!(seen.windows(2).all(|w| w[0] < w[1]), "out of order");
        }

        /// The reader never returns anything at or above the horizon.
        #[test]
        fn nothing_at_or_above_the_horizon_is_returned(
            xids in proptest::collection::vec(1u64..50, 0..30),
            horizon in 0u64..60,
        ) {
            let log: Vec<(Position, ())> = xids.iter().enumerate()
                .map(|(i, x)| (Position::new(*x, i as i64 + 1), ()))
                .collect();
            let got = read_batch(&log, Position::START, horizon, usize::MAX);
            prop_assert!(got.iter().all(|(p, ())| p.xid < horizon));
            let want = log.iter().filter(|(p, ())| p.xid < horizon).count();
            prop_assert_eq!(got.len(), want);
        }
    }
}
