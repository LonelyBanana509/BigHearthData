//! Compaction and the history purge on a real connection (spec `write-path.md`, section 8).
//!
//! Both go through the admin-only functions of migration 3. `compact` moves finished records that
//! are older than the cutoff (see [`crate::time_rule::compaction_cutoff`]) into `request_history`,
//! in one statement that cannot leave half of a move behind; a record that is still `running`, or
//! permanent, never moves. The job that calls them on a timer comes with slice C.

use bhdata_api::types::Timestamp;
use tokio_postgres::Client;

const COMPACT_SQL: &str = "SELECT bhdata_internal.compact_requests(\
    'epoch'::timestamptz + $1::bigint * interval '1 millisecond', $2::integer)";
const PURGE_SQL: &str = "SELECT bhdata_internal.purge_history(\
    'epoch'::timestamptz + $1::bigint * interval '1 millisecond', $2::integer, $3::boolean)";

fn millis(t: Timestamp) -> i64 {
    i64::try_from(t.as_unix_millis()).unwrap_or(i64::MAX)
}

/// Moves up to `batch` records older than `cutoff` into the history; returns how many moved. Call
/// again until it returns less than `batch`.
///
/// # Errors
/// The driver's error.
pub async fn compact(
    client: &Client,
    cutoff: Timestamp,
    batch: u32,
) -> Result<u64, tokio_postgres::Error> {
    let batch = i32::try_from(batch).unwrap_or(i32::MAX);
    let row = client
        .query_one(COMPACT_SQL, &[&millis(cutoff), &batch])
        .await?;
    let moved: i64 = row.get(0);
    Ok(u64::try_from(moved).unwrap_or(0))
}

/// Deletes up to `batch` history rows finished before `cutoff`; with `require_copied` only rows
/// already copied to the central history. Returns how many went.
///
/// # Errors
/// The driver's error.
pub async fn purge(
    client: &Client,
    cutoff: Timestamp,
    batch: u32,
    require_copied: bool,
) -> Result<u64, tokio_postgres::Error> {
    let batch = i32::try_from(batch).unwrap_or(i32::MAX);
    let row = client
        .query_one(PURGE_SQL, &[&millis(cutoff), &batch, &require_copied])
        .await?;
    let gone: i64 = row.get(0);
    Ok(u64::try_from(gone).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_statements_call_the_admin_functions() {
        assert!(COMPACT_SQL.contains("bhdata_internal.compact_requests"));
        assert!(PURGE_SQL.contains("bhdata_internal.purge_history"));
    }

    #[test]
    fn a_time_is_sent_as_whole_milliseconds() {
        assert_eq!(
            millis(Timestamp::from_unix_millis(1_700_000_000_123)),
            1_700_000_000_123
        );
        assert_eq!(millis(Timestamp::EPOCH), 0);
    }
}
