//! The change-log reader and trim on a real connection (spec `write-path.md`, section 6).
//!
//! Both go through the admin-only functions of migration 3: `read_changes` (entries after a
//! [`Position`] and below the safe horizon, in order) and `trim_change_log`. The caller passes the
//! admin pool's connection (`&connection`: it derefs to the driver's client) and keeps the position it has
//! read up to. Entries carry no values and need not arrive in commit order for one row; see
//! [`crate::entry`].

use bhdata_pg::pool::{describe, pg_error};
use bhdata_pg::PgError;
use bhdata_types::Timestamp;
use tokio_postgres::Client;

use crate::entry::{Entry, Op};
use crate::position::{Position, READ_CHANGES_SQL};

/// One entry with its place in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    /// Where it is. A reader that has handled it stores this as its position.
    pub position: Position,
    /// What it says.
    pub entry: Entry,
}

/// Why a read or a trim failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LogError {
    /// The server refused or the connection broke.
    #[error("change log: {detail}")]
    Pg {
        /// What kind of failure, and whether it is worth another try.
        error: PgError,
        /// The server's text, for logs.
        detail: String,
    },
    /// A row came back that this release cannot read (a newer release's `op`, say).
    #[error("change log row not understood: {0}")]
    Bad(String),
}

fn pg(e: &tokio_postgres::Error) -> LogError {
    LogError::Pg {
        error: pg_error(e),
        detail: describe(e),
    }
}

fn op_of(text: &str) -> Result<Op, LogError> {
    match text {
        "insert" => Ok(Op::Insert),
        "update" => Ok(Op::Update),
        other => Err(LogError::Bad(format!("op `{other}`"))),
    }
}

/// Up to `limit` entries after `after`, below the horizon, in position order.
///
/// # Errors
/// [`LogError`].
pub async fn read_after(
    client: &Client,
    after: Position,
    limit: u32,
) -> Result<Vec<Logged>, LogError> {
    let limit = i32::try_from(limit).unwrap_or(i32::MAX);
    let rows = client
        .query(
            READ_CHANGES_SQL,
            &[&after.xid.to_string(), &after.seq, &limit],
        )
        .await
        .map_err(|e| pg(&e))?;
    rows.iter()
        .map(|row| {
            let xid: String = row.get(0);
            let xid = xid
                .parse::<u64>()
                .map_err(|_| LogError::Bad(format!("xid `{xid}`")))?;
            let op: String = row.get(4);
            Ok(Logged {
                position: Position::new(xid, row.get(1)),
                entry: Entry {
                    table: row.get(2),
                    row_key: row.get(3),
                    op: op_of(&op)?,
                    subject: row.get(5),
                },
            })
        })
        .collect()
}

/// The statement of [`trim`].
const TRIM_SQL: &str = "SELECT bhdata_internal.trim_change_log($1::text, $2::bigint, \
    'epoch'::timestamptz + $3::bigint * interval '1 millisecond', $4::integer)";

/// Deletes up to `batch` entries at or before `up_to` (the position every reader has passed) and
/// written before `older_than` (the retention). Returns how many went. Call again until it
/// returns less than `batch`.
///
/// # Errors
/// [`LogError`].
pub async fn trim(
    client: &Client,
    up_to: Position,
    older_than: Timestamp,
    batch: u32,
) -> Result<u64, LogError> {
    let millis = i64::try_from(older_than.as_unix_millis()).unwrap_or(i64::MAX);
    let batch = i32::try_from(batch).unwrap_or(i32::MAX);
    let row = client
        .query_one(
            TRIM_SQL,
            &[&up_to.xid.to_string(), &up_to.seq, &millis, &batch],
        )
        .await
        .map_err(|e| pg(&e))?;
    let gone: i64 = row.get(0);
    Ok(u64::try_from(gone).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_ops_this_release_writes_are_understood() {
        assert_eq!(op_of("insert"), Ok(Op::Insert));
        assert_eq!(op_of("update"), Ok(Op::Update));
        // The table allows `delete` for a later release; nothing here may guess what it means.
        assert!(matches!(op_of("delete"), Err(LogError::Bad(_))));
        assert!(matches!(op_of(""), Err(LogError::Bad(_))));
    }

    #[test]
    fn the_trim_statement_names_the_function_and_every_argument() {
        for part in ["bhdata_internal.trim_change_log", "$1", "$2", "$3", "$4"] {
            assert!(TRIM_SQL.contains(part), "{part}");
        }
    }
}
