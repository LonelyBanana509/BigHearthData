//! The audit log table and its writer (wire-protocol spec, section 7). Lane C.
//!
//! One row per login event the gateway reports ([`AuditEvent`]: a success, a failure, a lockout and
//! its release) and one per change the control plane makes to a login (created, disabled, enabled,
//! password, permissions, recipes, limits). Individual requests are not here: the request table
//! already records every command.
//!
//! What a row holds, and what it never holds:
//!
//! * `name` is what the client sent for a failure or a lockout (it may not be a valid login id, so it
//!   is cut to the column's size, not refused). `name_exists` is for the log only; a client never
//!   learns it.
//! * `detail` says what changed ("granted execute"). It never holds a password, a verifier or a key.
//! * The admin role can read and insert rows; no right to edit or delete one is ever given
//!   (`bhdata_pg::internal`).
//!
//! The mapping from an event to a row ([`parts`]) is a plain function, tested without a database. The
//! calls that write and read take any client, so the caller decides the transaction: a change to a
//! login writes its audit row in the same one.

use bhdata_api::login::AuditEvent;
use bhdata_pg::pool::{describe, pg_error};
use tokio_postgres::GenericClient;

use crate::error::ControlError;

/// The most bytes of `name` a row keeps (the column's check).
pub const MAX_NAME_BYTES: usize = 1024;

/// An event, as the columns of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventParts {
    /// The `event` column: one of `bhdata_pg::internal::AUDIT_EVENTS`.
    pub event: &'static str,
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: i64,
    /// The `name` column, cut to [`MAX_NAME_BYTES`].
    pub name: String,
    /// The `name_exists` column (only for a failed login).
    pub name_exists: Option<bool>,
    /// The `source` column as text (only for a login success or failure).
    pub source: Option<String>,
}

/// `name` cut to at most [`MAX_NAME_BYTES`] bytes, at a character boundary.
#[must_use]
pub fn clip_name(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_owned();
    }
    let mut cut = 0;
    for (i, c) in name.char_indices() {
        let end = i + c.len_utf8();
        if end > MAX_NAME_BYTES {
            break;
        }
        cut = end;
    }
    name[..cut].to_owned()
}

fn millis(at: bhdata_types::Timestamp) -> i64 {
    i64::try_from(at.as_unix_millis()).unwrap_or(i64::MAX)
}

/// The columns of a row for what the gateway reported.
#[must_use]
pub fn parts(event: &AuditEvent) -> EventParts {
    match event {
        AuditEvent::LoginSucceeded {
            at,
            login_id,
            source,
        } => EventParts {
            event: "login_succeeded",
            at_ms: millis(*at),
            name: clip_name(login_id.as_str()),
            name_exists: Some(true),
            source: Some(source.to_string()),
        },
        AuditEvent::LoginFailed {
            at,
            name,
            name_exists,
            source,
        } => EventParts {
            event: "login_failed",
            at_ms: millis(*at),
            name: clip_name(name),
            name_exists: Some(*name_exists),
            source: Some(source.to_string()),
        },
        AuditEvent::LockedOut { at, name } => EventParts {
            event: "locked_out",
            at_ms: millis(*at),
            name: clip_name(name),
            name_exists: None,
            source: None,
        },
        AuditEvent::LockoutReleased { at, name } => EventParts {
            event: "lockout_released",
            at_ms: millis(*at),
            name: clip_name(name),
            name_exists: None,
            source: None,
        },
    }
}

fn pg(step: &'static str) -> impl Fn(tokio_postgres::Error) -> ControlError {
    move |e| ControlError::Pg {
        step,
        error: pg_error(&e),
        detail: describe(&e),
    }
}

/// Stores what the gateway reported.
///
/// # Errors
/// [`ControlError::Pg`] when the server refuses the row.
pub async fn record_event<C: GenericClient>(
    client: &C,
    event: &AuditEvent,
) -> Result<(), ControlError> {
    let p = parts(event);
    client
        .execute(
            "INSERT INTO bhdata_control.audit (at, event, name, name_exists, source) \
             VALUES ('epoch'::timestamptz + $1::bigint * interval '1 millisecond', $2, $3, $4, \
                     $5::text::inet)",
            &[&p.at_ms, &p.event, &p.name, &p.name_exists, &p.source],
        )
        .await
        .map_err(pg("audit insert"))?;
    Ok(())
}

/// Stores a change the control plane made to the login `name`, stamped with the server's time.
/// `event` is one of `bhdata_pg::internal::AUDIT_EVENTS`; `actor` is who ran the command.
///
/// # Errors
/// [`ControlError::Pg`] when the server refuses the row (for instance an unknown `event`).
pub async fn record_change<C: GenericClient>(
    client: &C,
    event: &str,
    name: &str,
    actor: &str,
    detail: &str,
) -> Result<(), ControlError> {
    client
        .execute(
            "INSERT INTO bhdata_control.audit (at, event, name, actor, detail) \
             VALUES (now(), $1, $2, $3, $4)",
            &[&event, &clip_name(name), &actor, &detail],
        )
        .await
        .map_err(pg("audit insert"))?;
    Ok(())
}

/// One stored row, for `bhdatactl` and the tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRow {
    /// The row number; rows only ever get later numbers.
    pub id: i64,
    /// When, as UTC text with milliseconds.
    pub at: String,
    /// What happened.
    pub event: String,
    /// The login name.
    pub name: String,
    /// For a failed login: whether the name existed.
    pub name_exists: Option<bool>,
    /// For a login: where it came from.
    pub source: Option<String>,
    /// For a change: who made it.
    pub actor: Option<String>,
    /// For a change: what it was.
    pub detail: Option<String>,
}

/// The newest `limit` rows, newest first, for one name or for all.
///
/// # Errors
/// [`ControlError::Pg`].
pub async fn list<C: GenericClient>(
    client: &C,
    name: Option<&str>,
    limit: i64,
) -> Result<Vec<AuditRow>, ControlError> {
    let rows = client
        .query(
            "SELECT id, to_char(at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), \
                    event, name, name_exists, host(source), actor, detail \
             FROM bhdata_control.audit \
             WHERE ($1::text IS NULL OR name = $1) \
             ORDER BY id DESC LIMIT $2",
            &[&name, &limit],
        )
        .await
        .map_err(pg("audit list"))?;
    Ok(rows
        .iter()
        .map(|r| AuditRow {
            id: r.get(0),
            at: r.get(1),
            event: r.get(2),
            name: r.get(3),
            name_exists: r.get(4),
            source: r.get(5),
            actor: r.get(6),
            detail: r.get(7),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use bhdata_pg::internal::AUDIT_EVENTS;
    use bhdata_types::{LoginId, Timestamp};

    use super::*;

    const SOURCE: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));

    fn at() -> Timestamp {
        Timestamp::from_unix_millis(1_700_000_000_123)
    }

    #[test]
    fn each_gateway_event_becomes_one_row_the_table_accepts() {
        let alice = LoginId::new("alice").unwrap();
        let all = [
            AuditEvent::LoginSucceeded {
                at: at(),
                login_id: alice,
                source: SOURCE,
            },
            AuditEvent::LoginFailed {
                at: at(),
                name: "ghost".into(),
                name_exists: false,
                source: SOURCE,
            },
            AuditEvent::LockedOut {
                at: at(),
                name: "ghost".into(),
            },
            AuditEvent::LockoutReleased {
                at: at(),
                name: "ghost".into(),
            },
        ];
        let events: Vec<&str> = all.iter().map(|e| parts(e).event).collect();
        assert_eq!(
            events,
            [
                "login_succeeded",
                "login_failed",
                "locked_out",
                "lockout_released"
            ]
        );
        for event in events {
            assert!(AUDIT_EVENTS.contains(&event), "{event}");
        }
    }

    #[test]
    fn a_failed_login_keeps_the_name_as_sent_and_whether_it_existed() {
        let p = parts(&AuditEvent::LoginFailed {
            at: at(),
            name: "not a valid id!".into(),
            name_exists: false,
            source: SOURCE,
        });
        assert_eq!(p.name, "not a valid id!");
        assert_eq!(p.name_exists, Some(false));
        assert_eq!(p.source.as_deref(), Some("10.0.0.7"));
        assert_eq!(p.at_ms, 1_700_000_000_123);
        let l = parts(&AuditEvent::LockedOut {
            at: at(),
            name: "x".into(),
        });
        assert_eq!((l.name_exists, l.source), (None, None));
    }

    #[test]
    fn a_long_name_is_cut_at_a_character_boundary_inside_the_limit() {
        assert_eq!(clip_name("abc"), "abc");
        let long = "x".repeat(MAX_NAME_BYTES + 50);
        assert_eq!(clip_name(&long).len(), MAX_NAME_BYTES);
        let straddle = format!("{}é{}", "x".repeat(MAX_NAME_BYTES - 1), "tail");
        assert_eq!(clip_name(&straddle), "x".repeat(MAX_NAME_BYTES - 1));
        let exact = format!("{}é", "x".repeat(MAX_NAME_BYTES - 2));
        assert_eq!(clip_name(&exact), exact);
    }
}
