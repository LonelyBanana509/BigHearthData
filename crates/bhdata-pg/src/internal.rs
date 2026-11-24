//! The internal tables and the narrow functions, as numbered migrations (spec sections 1, 3 and 7).
//!
//! The tables live in schema `bhdata_internal` and belong to the owner role. The recipe role has no
//! right on them; it can only call three functions, which run with the owner's rights
//! (`SECURITY DEFINER`, fixed `search_path`):
//!
//! * `claim_request`: first time seen, an identical retry, or a conflicting reuse of the id.
//! * `finish_request`: records the outcome of a running request, once.
//! * `append_change`: writes a change-log row for a running request.
//!
//! Migration 2 adds the registry in schema `bhdata_control` (spec `recipe-compiler.md`, section 5):
//! `schema_versions` and `recipes`. Only the admin role reaches them, and only to read, to insert,
//! and to change a recipe's `status`; a registered row is never edited or deleted.
//!
//! Migration 3 (write-path spec, section 11) gives `requests` its subject, creation time, finish time and
//! permanent flag, gives `change_log` the writing transaction's id (`xid`, so a reader can stop at a
//! safe horizon), replaces `claim_request` and `finish_request`, adds `request_history`, and adds
//! four functions for the admin role only: `read_changes`, `compact_requests`, `purge_history` and
//! `trim_change_log`. The recipe role gets none of them.
//!
//! Migration 4 (wire-protocol spec, sections 6, 7 and 10) adds the logins and the audit log to schema
//! `bhdata_control`: `logins` (the SCRAM verifier, the four permissions, the recipe allow-list, the
//! limits, the disabled flag) and `audit` (one row per login event and per change to a login). Only
//! the admin role reaches them: it reads and inserts, changes a login's own columns (never its name or
//! its creation), and never deletes or edits an audit row.
//!
//! Migrations are append-only: a released version is never edited. Slice B runs each as the owner
//! role (`SET ROLE bhdata_owner`), in one transaction together with [`record_version`]. A function
//! that refuses a call raises [`crate::errors::SQLSTATE_BHDATA`].

use bhdata_types::ids::{MAX_ID_LEN, MAX_LOGIN_LEN};

use crate::errors::SQLSTATE_BHDATA;
use crate::roles::{Role, SCHEMA_CONTROL, SCHEMA_INTERNAL};

/// The schema version this release writes into `meta`.
pub const SCHEMA_VERSION: u32 = 4;

/// One numbered step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The version the data is at after this step.
    pub version: u32,
    /// The statements, each sent as one command.
    pub statements: Vec<String>,
}

/// The data is from a newer release than this one. Never downgraded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooNew {
    /// The version found in `meta`.
    pub found: u32,
    /// The newest version this release knows.
    pub known: u32,
}

impl std::fmt::Display for TooNew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the database is at schema version {} but this release only knows up to {}; \
             it was written by a newer release and is never downgraded",
            self.found, self.known
        )
    }
}

impl std::error::Error for TooNew {}

/// The narrow functions of the write path (the recipe role may call them, nobody else): name and
/// argument types, as `GRANT` and `REVOKE` need them.
pub const FUNCTIONS: [(&str, &str); 3] = [
    (
        "claim_request",
        "text, text, bytea, text, integer, text, timestamptz, boolean",
    ),
    ("finish_request", "text, text, text, text, bytea"),
    ("append_change", "text, text, text, text, text, text"),
];

/// The functions only the admin role may call (migration 3): the change-log reader and trim, and
/// the request compaction and history purge. Same form as [`FUNCTIONS`].
pub const ADMIN_FUNCTIONS: [(&str, &str); 4] = [
    ("read_changes", "text, bigint, integer"),
    ("compact_requests", "timestamptz, integer"),
    ("purge_history", "timestamptz, integer, boolean"),
    ("trim_change_log", "text, bigint, timestamptz, integer"),
];

/// Reads the stored version. No row, or no table yet, means a new database: the caller treats a
/// missing `meta` table as version 0 (see [`pending`]).
pub const READ_VERSION_QUERY: &str =
    "SELECT value FROM bhdata_internal.meta WHERE key = 'schema_version'";

/// Does the `meta` table exist yet?
pub const META_EXISTS_QUERY: &str = "SELECT to_regclass('bhdata_internal.meta') IS NOT NULL";

/// The statement that records `version` as reached, run in the same transaction as its migration.
#[must_use]
pub fn record_version(version: u32) -> String {
    format!(
        "INSERT INTO bhdata_internal.meta (key, value) VALUES ('schema_version', '{version}') \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
    )
}

/// All migrations, oldest first.
#[must_use]
pub fn migrations() -> Vec<Migration> {
    vec![
        Migration {
            version: 1,
            statements: v1(),
        },
        Migration {
            version: 2,
            statements: v2(),
        },
        Migration {
            version: 3,
            statements: v3(),
        },
        Migration {
            version: 4,
            statements: v4(),
        },
    ]
}

/// The migrations still to run for data at `current` (`None`: nothing recorded yet).
///
/// # Errors
/// [`TooNew`] when the data is ahead of this release.
pub fn pending(current: Option<u32>) -> Result<Vec<Migration>, TooNew> {
    let known = SCHEMA_VERSION;
    let found = current.unwrap_or(0);
    if found > known {
        return Err(TooNew { found, known });
    }
    Ok(migrations()
        .into_iter()
        .filter(|m| m.version > found)
        .collect())
}

/// The audit events the table accepts: what the gateway reports, and what the control plane writes
/// when it changes a login.
pub const AUDIT_EVENTS: [&str; 11] = [
    "login_succeeded",
    "login_failed",
    "locked_out",
    "lockout_released",
    "login_created",
    "login_disabled",
    "login_enabled",
    "password_changed",
    "permissions_changed",
    "recipes_changed",
    "limits_changed",
];

/// Migration 4: the logins and the audit log (wire-protocol spec, sections 6, 7 and 10).
///
/// `recipes` is the allow-list when `all_recipes` is false; with `all_recipes` (the `*` of the spec)
/// the list must be empty. An audit row's `name` is whatever the client sent, so it is held to a size
/// only, not to the login id shape.
fn v4() -> Vec<String> {
    let c = SCHEMA_CONTROL;
    let events = AUDIT_EVENTS
        .iter()
        .map(|e| format!("'{e}'"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        format!(
            "CREATE TABLE {c}.logins (\
               login_id text PRIMARY KEY CHECK (login_id ~ '^[A-Za-z0-9._:-]{{1,{MAX_LOGIN_LEN}}}$'), \
               salt bytea NOT NULL CHECK (octet_length(salt) BETWEEN 1 AND 64), \
               iterations integer NOT NULL CHECK (iterations BETWEEN 1 AND 10000000), \
               stored_key bytea NOT NULL CHECK (octet_length(stored_key) = 32), \
               server_key bytea NOT NULL CHECK (octet_length(server_key) = 32), \
               can_execute boolean NOT NULL DEFAULT false, \
               can_register boolean NOT NULL DEFAULT false, \
               can_admin boolean NOT NULL DEFAULT false, \
               can_sql boolean NOT NULL DEFAULT false, \
               all_recipes boolean NOT NULL DEFAULT false, \
               recipes text[] NOT NULL DEFAULT '{{}}' \
                 CHECK (array_position(recipes, NULL) IS NULL \
                        AND NOT (all_recipes AND cardinality(recipes) > 0) \
                        AND array_to_string(recipes, ',') ~ '^([a-z][a-z0-9_]{{0,39}}(,[a-z][a-z0-9_]{{0,39}})*)?$'), \
               rate_per_sec integer CHECK (rate_per_sec >= 1), \
               rate_burst integer CHECK (rate_burst >= 1), \
               max_connections integer CHECK (max_connections >= 1), \
               disabled boolean NOT NULL DEFAULT false, \
               created_at timestamptz NOT NULL DEFAULT now(), \
               created_by text NOT NULL CHECK (octet_length(created_by) BETWEEN 1 AND {MAX_LOGIN_LEN}), \
               updated_at timestamptz NOT NULL DEFAULT now())"
        ),
        format!(
            "CREATE TABLE {c}.audit (\
               id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
               at timestamptz NOT NULL, \
               event text NOT NULL CHECK (event IN ({events})), \
               name text NOT NULL CHECK (octet_length(name) <= 1024), \
               name_exists boolean, \
               source inet, \
               actor text CHECK (octet_length(actor) BETWEEN 1 AND {MAX_LOGIN_LEN}), \
               detail text CHECK (octet_length(detail) <= 4096))"
        ),
        format!("CREATE INDEX audit_at ON {c}.audit (at)"),
        format!("CREATE INDEX audit_name ON {c}.audit (name, id)"),
    ]
}

fn v2() -> Vec<String> {
    let c = SCHEMA_CONTROL;
    let hex = "~ '^[0-9a-f]{64}$'";
    vec![
        format!(
            "CREATE TABLE {c}.schema_versions (\
               number integer PRIMARY KEY CHECK (number >= 1), \
               hash text NOT NULL CHECK (hash {hex}), \
               source text NOT NULL, \
               applied_at timestamptz NOT NULL DEFAULT now(), \
               applied_by text NOT NULL CHECK (octet_length(applied_by) BETWEEN 1 AND {MAX_LOGIN_LEN}))"
        ),
        format!(
            "CREATE TABLE {c}.recipes (\
               name text NOT NULL CHECK (name ~ '^[a-z][a-z0-9_]{{0,39}}$'), \
               version integer NOT NULL CHECK (version >= 1), \
               content_hash text NOT NULL CHECK (content_hash {hex}), \
               schema_version integer NOT NULL REFERENCES {c}.schema_versions (number), \
               source text NOT NULL, \
               plan text NOT NULL, \
               status text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'retired')), \
               registered_at timestamptz NOT NULL DEFAULT now(), \
               registered_by text NOT NULL CHECK (octet_length(registered_by) BETWEEN 1 AND {MAX_LOGIN_LEN}), \
               PRIMARY KEY (name, version))"
        ),
    ]
}

/// Migration 3: the write path's final shape (write-path spec, sections 3, 4, 6 and 8).
///
/// Nothing is written to these tables by M2 or M3, so the new `NOT NULL` columns are added with a
/// default and the default is dropped again at once: every later insert must give a value.
fn v3() -> Vec<String> {
    let s = SCHEMA_INTERNAL;
    let bad = SQLSTATE_BHDATA;
    let mut out = vec![
        format!(
            "ALTER TABLE {s}.requests \
               ADD COLUMN subject text NOT NULL DEFAULT '', \
               ADD COLUMN requested_at timestamptz NOT NULL DEFAULT now(), \
               ADD COLUMN finished_at timestamptz, \
               ADD COLUMN permanent boolean NOT NULL DEFAULT false"
        ),
        format!(
            "ALTER TABLE {s}.requests \
               ALTER COLUMN subject DROP DEFAULT, \
               ALTER COLUMN requested_at DROP DEFAULT, \
               ALTER COLUMN permanent DROP DEFAULT"
        ),
        // The compaction scan: oldest finished, non-permanent records first.
        format!(
            "CREATE INDEX requests_compaction ON {s}.requests (requested_at) \
               WHERE NOT permanent AND status <> 'running'"
        ),
        // A position is (xid, seq): see the horizon rule in `read_changes`.
        format!(
            "ALTER TABLE {s}.change_log ADD COLUMN xid xid8 NOT NULL DEFAULT pg_current_xact_id()"
        ),
        format!("CREATE INDEX change_log_position ON {s}.change_log (xid, seq)"),
        format!(
            "CREATE TABLE {s}.request_history (\
               login_id text NOT NULL, \
               request_id text NOT NULL, \
               status text NOT NULL, \
               error_kind text, \
               recipe text, \
               recipe_version integer, \
               subject text NOT NULL, \
               requested_at timestamptz NOT NULL, \
               finished_at timestamptz NOT NULL, \
               copied_at timestamptz, \
               PRIMARY KEY (login_id, request_id), \
               CONSTRAINT history_status CHECK (status IN ('done', 'failed')))"
        ),
        format!("CREATE INDEX request_history_finished ON {s}.request_history (finished_at)"),
        format!("DROP FUNCTION {s}.claim_request(text, text, bytea, text, integer)"),
    ];

    out.push(format!(
        "CREATE FUNCTION {s}.claim_request(\
           p_login text, p_request_id text, p_payload_hash bytea, p_recipe text, p_recipe_version integer, \
           p_subject text, p_requested_at timestamptz, p_permanent boolean) \
         RETURNS TABLE (out_outcome text, out_status text, out_error_kind text, out_result bytea) \
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$ \
         DECLARE v_hash bytea; v_status text; v_error_kind text; v_result bytea; \
         BEGIN \
           INSERT INTO {s}.requests \
             (login_id, request_id, payload_hash, recipe, recipe_version, status, subject, requested_at, permanent) \
           VALUES (p_login, p_request_id, p_payload_hash, p_recipe, p_recipe_version, 'running', \
                   p_subject, p_requested_at, p_permanent) \
           ON CONFLICT (login_id, request_id) DO NOTHING; \
           IF FOUND THEN \
             RETURN QUERY SELECT 'new'::text, 'running'::text, NULL::text, NULL::bytea; \
             RETURN; \
           END IF; \
           SELECT r.payload_hash, r.status, r.error_kind, r.result \
             INTO v_hash, v_status, v_error_kind, v_result \
             FROM {s}.requests r WHERE r.login_id = p_login AND r.request_id = p_request_id; \
           IF v_hash IS DISTINCT FROM p_payload_hash THEN \
             RETURN QUERY SELECT 'conflict'::text, NULL::text, NULL::text, NULL::bytea; \
           ELSE \
             RETURN QUERY SELECT 'same'::text, v_status, v_error_kind, v_result; \
           END IF; \
         END $$"
    ));
    // Same arguments as in migration 1, so the grants stay; only `finished_at` is new.
    out.push(format!(
        "CREATE OR REPLACE FUNCTION {s}.finish_request(\
           p_login text, p_request_id text, p_status text, p_error_kind text, p_result bytea) \
         RETURNS void \
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$ \
         BEGIN \
           IF p_status NOT IN ('done', 'failed') THEN \
             RAISE EXCEPTION 'finish_request: status must be done or failed' USING ERRCODE = '{bad}'; \
           END IF; \
           UPDATE {s}.requests \
              SET status = p_status, error_kind = p_error_kind, result = p_result, finished_at = now() \
            WHERE login_id = p_login AND request_id = p_request_id AND status = 'running'; \
           IF NOT FOUND THEN \
             RAISE EXCEPTION 'finish_request: no running request with that id' USING ERRCODE = '{bad}'; \
           END IF; \
         END $$"
    ));
    // The reader: entries after a position and below the horizon. Every transaction with an id
    // below the snapshot's xmin has finished, so nothing can still appear below it. The caller must
    // not run it in REPEATABLE READ (the snapshot would be the old one).
    out.push(format!(
        "CREATE FUNCTION {s}.read_changes(p_xid text, p_seq bigint, p_limit integer) \
         RETURNS TABLE (out_xid text, out_seq bigint, out_table text, out_row_key text, out_op text, out_subject text) \
         LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$ \
         BEGIN \
           RETURN QUERY \
             SELECT c.xid::text, c.seq, c.table_name, c.row_key, c.op, c.subject \
               FROM {s}.change_log c \
              WHERE (c.xid, c.seq) > (p_xid::xid8, p_seq) \
                AND c.xid < pg_snapshot_xmin(pg_current_snapshot()) \
              ORDER BY c.xid, c.seq \
              LIMIT p_limit; \
         END $$"
    ));
    // Compaction: delete and insert are one statement, so a crash leaves both or neither, and a
    // `running` record or a permanent one is never touched. A record another transaction holds
    // locked is skipped, not waited for.
    out.push(format!(
        "CREATE FUNCTION {s}.compact_requests(p_cutoff timestamptz, p_batch integer) \
         RETURNS bigint \
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$ \
         DECLARE v_moved bigint; \
         BEGIN \
           WITH moved AS ( \
             DELETE FROM {s}.requests r \
              WHERE (r.login_id, r.request_id) IN ( \
                    SELECT q.login_id, q.request_id FROM {s}.requests q \
                     WHERE q.requested_at < p_cutoff AND NOT q.permanent AND q.status <> 'running' \
                     ORDER BY q.requested_at LIMIT p_batch FOR UPDATE SKIP LOCKED) \
             RETURNING r.login_id, r.request_id, r.status, r.error_kind, r.recipe, r.recipe_version, \
                       r.subject, r.requested_at, r.finished_at) \
           INSERT INTO {s}.request_history \
             (login_id, request_id, status, error_kind, recipe, recipe_version, subject, requested_at, finished_at) \
           SELECT m.login_id, m.request_id, m.status, m.error_kind, m.recipe, m.recipe_version, \
                  m.subject, m.requested_at, COALESCE(m.finished_at, m.requested_at) \
             FROM moved m \
           ON CONFLICT (login_id, request_id) DO UPDATE \
             SET status = EXCLUDED.status, error_kind = EXCLUDED.error_kind, recipe = EXCLUDED.recipe, \
                 recipe_version = EXCLUDED.recipe_version, subject = EXCLUDED.subject, \
                 requested_at = EXCLUDED.requested_at, finished_at = EXCLUDED.finished_at, copied_at = NULL; \
           GET DIAGNOSTICS v_moved = ROW_COUNT; \
           RETURN v_moved; \
         END $$"
    ));
    // With `p_require_copied` (stage 2, once the central history copy exists) only rows already
    // copied may go.
    out.push(format!(
        "CREATE FUNCTION {s}.purge_history(p_cutoff timestamptz, p_batch integer, p_require_copied boolean) \
         RETURNS bigint \
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$ \
         DECLARE v_gone bigint; \
         BEGIN \
           DELETE FROM {s}.request_history h \
            WHERE (h.login_id, h.request_id) IN ( \
                  SELECT q.login_id, q.request_id FROM {s}.request_history q \
                   WHERE q.finished_at < p_cutoff AND (NOT p_require_copied OR q.copied_at IS NOT NULL) \
                   ORDER BY q.finished_at LIMIT p_batch); \
           GET DIAGNOSTICS v_gone = ROW_COUNT; \
           RETURN v_gone; \
         END $$"
    ));
    // Entries at or before the position every reader has passed, and older than the retention.
    out.push(format!(
        "CREATE FUNCTION {s}.trim_change_log(p_xid text, p_seq bigint, p_before timestamptz, p_batch integer) \
         RETURNS bigint \
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$ \
         DECLARE v_gone bigint; \
         BEGIN \
           DELETE FROM {s}.change_log c \
            WHERE c.seq IN ( \
                  SELECT q.seq FROM {s}.change_log q \
                   WHERE (q.xid, q.seq) <= (p_xid::xid8, p_seq) AND q.changed_at < p_before \
                   ORDER BY q.xid, q.seq LIMIT p_batch); \
           GET DIAGNOSTICS v_gone = ROW_COUNT; \
           RETURN v_gone; \
         END $$"
    ));
    out
}

fn v1() -> Vec<String> {
    let s = SCHEMA_INTERNAL;
    let bad = SQLSTATE_BHDATA;
    let mut out = vec![
        format!("CREATE TABLE {s}.meta (key text PRIMARY KEY, value text NOT NULL)"),
        format!(
            "CREATE TABLE {s}.requests (\
               login_id text NOT NULL, \
               request_id text NOT NULL, \
               payload_hash bytea NOT NULL, \
               recipe text, \
               recipe_version integer, \
               status text NOT NULL, \
               error_kind text, \
               result bytea, \
               created_at timestamptz NOT NULL DEFAULT now(), \
               PRIMARY KEY (login_id, request_id), \
               CONSTRAINT requests_login_len CHECK (octet_length(login_id) BETWEEN 1 AND {MAX_LOGIN_LEN}), \
               CONSTRAINT requests_id_len CHECK (octet_length(request_id) BETWEEN 1 AND {MAX_ID_LEN}), \
               CONSTRAINT requests_status CHECK (status IN ('running', 'done', 'failed')), \
               CONSTRAINT requests_error_kind CHECK ((status = 'failed') = (error_kind IS NOT NULL)))"
        ),
        format!(
            "CREATE TABLE {s}.change_log (\
               seq bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
               request_login text NOT NULL, \
               request_id text NOT NULL, \
               subject text NOT NULL, \
               table_name text NOT NULL, \
               op text NOT NULL, \
               row_key text NOT NULL, \
               changed_at timestamptz NOT NULL DEFAULT now(), \
               CONSTRAINT change_log_op CHECK (op IN ('insert', 'update', 'delete')))"
        ),
    ];

    // Every function pins its search_path and qualifies every name. Output columns are prefixed
    // `out_` so that they never clash with the table's columns inside the function.
    out.push(format!(
        "CREATE FUNCTION {s}.claim_request(\
           p_login text, p_request_id text, p_payload_hash bytea, p_recipe text, p_recipe_version integer) \
         RETURNS TABLE (out_outcome text, out_status text, out_error_kind text, out_result bytea) \
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$ \
         DECLARE v_hash bytea; v_status text; v_error_kind text; v_result bytea; \
         BEGIN \
           INSERT INTO {s}.requests (login_id, request_id, payload_hash, recipe, recipe_version, status) \
           VALUES (p_login, p_request_id, p_payload_hash, p_recipe, p_recipe_version, 'running') \
           ON CONFLICT (login_id, request_id) DO NOTHING; \
           IF FOUND THEN \
             RETURN QUERY SELECT 'new'::text, 'running'::text, NULL::text, NULL::bytea; \
             RETURN; \
           END IF; \
           SELECT r.payload_hash, r.status, r.error_kind, r.result \
             INTO v_hash, v_status, v_error_kind, v_result \
             FROM {s}.requests r WHERE r.login_id = p_login AND r.request_id = p_request_id; \
           IF v_hash IS DISTINCT FROM p_payload_hash THEN \
             RETURN QUERY SELECT 'conflict'::text, NULL::text, NULL::text, NULL::bytea; \
           ELSE \
             RETURN QUERY SELECT 'same'::text, v_status, v_error_kind, v_result; \
           END IF; \
         END $$"
    ));
    out.push(format!(
        "CREATE FUNCTION {s}.finish_request(\
           p_login text, p_request_id text, p_status text, p_error_kind text, p_result bytea) \
         RETURNS void \
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$ \
         BEGIN \
           IF p_status NOT IN ('done', 'failed') THEN \
             RAISE EXCEPTION 'finish_request: status must be done or failed' USING ERRCODE = '{bad}'; \
           END IF; \
           UPDATE {s}.requests \
              SET status = p_status, error_kind = p_error_kind, result = p_result \
            WHERE login_id = p_login AND request_id = p_request_id AND status = 'running'; \
           IF NOT FOUND THEN \
             RAISE EXCEPTION 'finish_request: no running request with that id' USING ERRCODE = '{bad}'; \
           END IF; \
         END $$"
    ));
    out.push(format!(
        "CREATE FUNCTION {s}.append_change(\
           p_login text, p_request_id text, p_subject text, p_table text, p_op text, p_row_key text) \
         RETURNS bigint \
         LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$ \
         DECLARE v_seq bigint; \
         BEGIN \
           PERFORM 1 FROM {s}.requests \
            WHERE login_id = p_login AND request_id = p_request_id AND status = 'running' FOR KEY SHARE; \
           IF NOT FOUND THEN \
             RAISE EXCEPTION 'append_change: no running request with that id' USING ERRCODE = '{bad}'; \
           END IF; \
           INSERT INTO {s}.change_log (request_login, request_id, subject, table_name, op, row_key) \
           VALUES (p_login, p_request_id, p_subject, p_table, p_op, p_row_key) RETURNING seq INTO v_seq; \
           RETURN v_seq; \
         END $$"
    ));
    out
}

/// The rights on the internal objects, run by the superuser after the migrations, at every start.
/// Nothing but the owner reaches the tables; the recipe role alone may call the three functions.
#[must_use]
pub fn object_grants() -> Vec<String> {
    let s = SCHEMA_INTERNAL;
    let logins: Vec<&str> = Role::LOGIN.iter().map(|r| r.name()).collect();
    let all_logins = logins.join(", ");
    let others: Vec<&str> = Role::LOGIN
        .iter()
        .filter(|r| **r != Role::Recipe)
        .map(|r| r.name())
        .collect();
    let others = others.join(", ");
    let mut out = vec![
        format!("REVOKE ALL ON ALL TABLES IN SCHEMA {s} FROM PUBLIC, {all_logins}"),
        format!("REVOKE ALL ON ALL SEQUENCES IN SCHEMA {s} FROM PUBLIC, {all_logins}"),
    ];
    // The registry: the admin role reads and inserts, and may change only a recipe's status.
    let c = SCHEMA_CONTROL;
    out.push(format!(
        "REVOKE ALL ON ALL TABLES IN SCHEMA {c} FROM PUBLIC, {all_logins}"
    ));
    out.push(format!(
        "GRANT SELECT, INSERT ON {c}.schema_versions, {c}.recipes TO {}",
        Role::Admin
    ));
    out.push(format!(
        "GRANT UPDATE (status) ON {c}.recipes TO {}",
        Role::Admin
    ));
    // The logins and the audit log (migration 4): the admin role reads and inserts both, changes only the
    // columns of a login that are meant to change, and never deletes or edits an audit row.
    out.push(format!(
        "GRANT SELECT, INSERT ON {c}.logins, {c}.audit TO {}",
        Role::Admin
    ));
    out.push(format!(
        "GRANT UPDATE (salt, iterations, stored_key, server_key, can_execute, can_register, can_admin, \
         can_sql, all_recipes, recipes, rate_per_sec, rate_burst, max_connections, disabled, updated_at) \
         ON {c}.logins TO {}",
        Role::Admin
    ));
    for (name, args) in FUNCTIONS {
        out.push(format!(
            "REVOKE ALL ON FUNCTION {s}.{name}({args}) FROM PUBLIC, {others}"
        ));
        out.push(format!(
            "GRANT EXECUTE ON FUNCTION {s}.{name}({args}) TO {}",
            Role::Recipe
        ));
    }
    // The admin-only functions: everyone else, the recipe role included, is revoked.
    let not_admin: Vec<&str> = Role::LOGIN
        .iter()
        .filter(|r| **r != Role::Admin)
        .map(|r| r.name())
        .collect();
    let not_admin = not_admin.join(", ");
    for (name, args) in ADMIN_FUNCTIONS {
        out.push(format!(
            "REVOKE ALL ON FUNCTION {s}.{name}({args}) FROM PUBLIC, {not_admin}"
        ));
        out.push(format!(
            "GRANT EXECUTE ON FUNCTION {s}.{name}({args}) TO {}",
            Role::Admin
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_start_at_one_and_have_no_gaps() {
        let all = migrations();
        for (i, m) in all.iter().enumerate() {
            assert_eq!(m.version as usize, i + 1);
            assert!(!m.statements.is_empty());
        }
        assert_eq!(all.last().map(|m| m.version), Some(SCHEMA_VERSION));
    }

    #[test]
    fn a_new_database_runs_everything_and_a_current_one_nothing() {
        assert_eq!(pending(None).unwrap().len(), migrations().len());
        assert_eq!(pending(Some(0)).unwrap().len(), migrations().len());
        assert!(pending(Some(SCHEMA_VERSION)).unwrap().is_empty());
    }

    #[test]
    fn a_newer_database_stops_start_up_and_says_why() {
        let err = pending(Some(SCHEMA_VERSION + 1)).unwrap_err();
        assert_eq!(
            err,
            TooNew {
                found: SCHEMA_VERSION + 1,
                known: SCHEMA_VERSION
            }
        );
        let text = err.to_string();
        assert!(
            text.contains("never downgraded") && text.contains(&(SCHEMA_VERSION + 1).to_string()),
            "{text}"
        );
    }

    #[test]
    fn migrations_never_change_the_running_role() {
        for m in migrations() {
            for st in &m.statements {
                assert!(!st.to_uppercase().contains("SET ROLE"), "{st}");
                assert!(
                    !st.to_uppercase().contains("GRANT "),
                    "grants belong to object_grants: {st}"
                );
            }
        }
    }

    #[test]
    fn migration_two_makes_the_registry_and_nothing_else() {
        let all = migrations();
        assert!(all.len() >= 2);
        let sql = all[1].statements.join("\n");
        assert!(sql.contains("CREATE TABLE bhdata_control.schema_versions"));
        assert!(sql.contains("CREATE TABLE bhdata_control.recipes"));
        assert_eq!(all[1].statements.len(), 2);
        // Plans are kept as the exact canonical text, not as jsonb, so the hash can be checked.
        assert!(sql.contains("plan text NOT NULL"), "{sql}");
        assert!(sql.contains("PRIMARY KEY (name, version)"));
        assert!(sql.contains("status IN ('active', 'retired')"));
        assert!(sql.contains(&format!("BETWEEN 1 AND {MAX_LOGIN_LEN}")));
        for st in &all[1].statements {
            let up = st.to_uppercase();
            assert!(!up.contains("SET ROLE") && !up.contains("GRANT "), "{st}");
            assert!(up.starts_with("CREATE TABLE BHDATA_CONTROL."), "{st}");
        }
    }

    #[test]
    fn the_admin_may_read_and_insert_the_registry_and_change_only_a_status() {
        let grants = object_grants().join("\n");
        assert!(grants.contains(
            "GRANT SELECT, INSERT ON bhdata_control.schema_versions, bhdata_control.recipes TO bhdata_admin"
        ));
        assert!(grants.contains("GRANT UPDATE (status) ON bhdata_control.recipes TO bhdata_admin"));
        assert!(grants.contains("REVOKE ALL ON ALL TABLES IN SCHEMA bhdata_control FROM PUBLIC"));
        // No DELETE or TRUNCATE anywhere in the registry's rights.
        for line in grants.lines().filter(|l| l.contains("bhdata_control")) {
            assert!(
                !line.contains("DELETE") && !line.contains("TRUNCATE"),
                "{line}"
            );
        }
        // The registry's rights go to the admin role alone.
        for line in grants
            .lines()
            .filter(|l| l.starts_with("GRANT") && l.contains("bhdata_control"))
        {
            assert!(line.ends_with("TO bhdata_admin"), "{line}");
        }
    }

    #[test]
    fn id_length_limits_come_from_bhdata_types() {
        let sql = v1().join("\n");
        assert!(sql.contains(&format!("BETWEEN 1 AND {MAX_LOGIN_LEN}")));
        assert!(sql.contains(&format!("BETWEEN 1 AND {MAX_ID_LEN}")));
        assert_eq!((MAX_LOGIN_LEN, MAX_ID_LEN), (64, 128));
    }

    /// Every statement of every migration, oldest first.
    fn all_statements() -> Vec<String> {
        migrations()
            .into_iter()
            .flat_map(|m| m.statements)
            .collect()
    }

    /// The newest definition of a function: the last `CREATE [OR REPLACE] FUNCTION` for it.
    fn latest_definition(name: &str) -> Option<String> {
        let plain = format!("CREATE FUNCTION {SCHEMA_INTERNAL}.{name}(");
        let replace = format!("CREATE OR REPLACE FUNCTION {SCHEMA_INTERNAL}.{name}(");
        all_statements()
            .into_iter()
            .rfind(|s| s.starts_with(&plain) || s.starts_with(&replace))
    }

    #[test]
    fn every_listed_function_is_defined_and_locked_down() {
        for (name, _) in FUNCTIONS.iter().chain(ADMIN_FUNCTIONS.iter()) {
            let sql = latest_definition(name).unwrap_or_else(|| panic!("{name} is not defined"));
            assert!(sql.contains("SECURITY DEFINER"), "{name}");
            assert!(
                sql.contains("SET search_path = pg_catalog, pg_temp"),
                "{name}"
            );
        }
        // No function exists that the two lists do not name (so none is left without a grant).
        let mut created: Vec<String> = all_statements()
            .iter()
            .filter_map(|s| {
                let rest = s
                    .strip_prefix("CREATE FUNCTION ")
                    .or_else(|| s.strip_prefix("CREATE OR REPLACE FUNCTION "))?;
                let rest = rest.strip_prefix(&format!("{SCHEMA_INTERNAL}."))?;
                Some(rest.split('(').next()?.to_string())
            })
            .collect();
        created.sort();
        created.dedup();
        let mut listed: Vec<String> = FUNCTIONS
            .iter()
            .chain(ADMIN_FUNCTIONS.iter())
            .map(|(n, _)| (*n).to_string())
            .collect();
        listed.sort();
        assert_eq!(created, listed);
    }

    #[test]
    fn function_argument_lists_match_their_grants() {
        for (name, args) in FUNCTIONS.iter().chain(ADMIN_FUNCTIONS.iter()) {
            let sql = latest_definition(name).unwrap();
            let params = sql
                .split(") RETURNS")
                .next()
                .unwrap()
                .split('(')
                .nth(1)
                .unwrap();
            assert_eq!(params.split(',').count(), args.split(',').count(), "{name}");
        }
    }

    #[test]
    fn refusals_use_our_own_sqlstate() {
        let sql = v1().join("\n");
        assert!(sql.contains(&format!("ERRCODE = '{SQLSTATE_BHDATA}'")));
        assert_eq!(SQLSTATE_BHDATA.len(), 5);
    }

    #[test]
    fn grants_give_the_recipe_role_the_functions_and_no_table() {
        let g = object_grants();
        for (name, args) in FUNCTIONS {
            let target = format!("{SCHEMA_INTERNAL}.{name}({args})");
            assert!(
                g.contains(&format!(
                    "GRANT EXECUTE ON FUNCTION {target} TO bhdata_recipe"
                )),
                "{name}"
            );
            let revoke = g
                .iter()
                .find(|s| s.starts_with("REVOKE") && s.contains(&target))
                .unwrap();
            assert!(
                revoke.contains("FROM PUBLIC, bhdata_sql, bhdata_reader, bhdata_admin"),
                "{revoke}"
            );
            assert!(!revoke.contains("bhdata_recipe"));
        }
        // The only grants that are not function grants belong to the admin role, on the registry.
        assert!(!g.iter().any(|s| s.starts_with("GRANT")
            && !s.contains("EXECUTE ON FUNCTION")
            && !s.ends_with("TO bhdata_admin")));
        assert!(g[0].starts_with(
            "REVOKE ALL ON ALL TABLES IN SCHEMA bhdata_internal FROM PUBLIC, bhdata_recipe"
        ));
    }

    #[test]
    fn grants_are_revoked_before_they_are_given() {
        let g = object_grants();
        for (name, _) in FUNCTIONS {
            let r = g
                .iter()
                .position(|s| s.starts_with("REVOKE") && s.contains(&format!(".{name}(")))
                .unwrap();
            let e = g
                .iter()
                .position(|s| s.starts_with("GRANT") && s.contains(&format!(".{name}(")))
                .unwrap();
            assert!(r < e, "{name}");
        }
    }

    #[test]
    fn migration_three_changes_the_tables_the_spec_names() {
        let all = migrations();
        assert_eq!(all[2].version, 3);
        let sql = all[2].statements.join("\n");
        for part in [
            "ADD COLUMN subject text NOT NULL",
            "ADD COLUMN requested_at timestamptz NOT NULL",
            "ADD COLUMN finished_at timestamptz",
            "ADD COLUMN permanent boolean NOT NULL",
            "ADD COLUMN xid xid8 NOT NULL DEFAULT pg_current_xact_id()",
            "CREATE INDEX change_log_position ON bhdata_internal.change_log (xid, seq)",
            "CREATE TABLE bhdata_internal.request_history",
            "DROP FUNCTION bhdata_internal.claim_request(text, text, bytea, text, integer)",
        ] {
            assert!(sql.contains(part), "{part}");
        }
        // The history keeps no payload hash and no result: it can never re-apply anything.
        let history = all[2]
            .statements
            .iter()
            .find(|s| s.starts_with("CREATE TABLE bhdata_internal.request_history"))
            .unwrap();
        assert!(!history.contains("payload_hash") && !history.contains("result"));
        // The temporary defaults that let the columns be added are dropped again.
        assert!(sql.contains("ALTER COLUMN subject DROP DEFAULT"));
        assert!(sql.contains("ALTER COLUMN requested_at DROP DEFAULT"));
        assert!(sql.contains("ALTER COLUMN permanent DROP DEFAULT"));
    }

    #[test]
    fn the_reader_stops_at_the_horizon_and_orders_by_position() {
        let sql = latest_definition("read_changes").unwrap();
        for part in [
            "(c.xid, c.seq) > (p_xid::xid8, p_seq)",
            "c.xid < pg_snapshot_xmin(pg_current_snapshot())",
            "ORDER BY c.xid, c.seq",
            "LIMIT p_limit",
        ] {
            assert!(sql.contains(part), "{part}");
        }
    }

    #[test]
    fn compaction_moves_in_one_statement_and_never_touches_running_or_permanent_records() {
        let sql = latest_definition("compact_requests").unwrap();
        assert!(sql.contains("WITH moved AS ( DELETE FROM bhdata_internal.requests"));
        assert!(sql.contains("INSERT INTO bhdata_internal.request_history"));
        assert!(sql.contains("NOT q.permanent AND q.status <> 'running'"));
        assert!(sql.contains("FOR UPDATE SKIP LOCKED"));
        // One DELETE statement and one INSERT statement, joined in a single WITH.
        assert_eq!(sql.matches("DELETE FROM").count(), 1);
    }

    #[test]
    fn the_replaced_claim_takes_the_new_columns_and_the_finish_stamps_the_time() {
        let claim = latest_definition("claim_request").unwrap();
        assert!(claim.contains("p_subject text, p_requested_at timestamptz, p_permanent boolean"));
        assert!(claim.contains("ON CONFLICT (login_id, request_id) DO NOTHING"));
        let finish = latest_definition("finish_request").unwrap();
        assert!(finish.starts_with("CREATE OR REPLACE FUNCTION"));
        assert!(finish.contains("finished_at = now()"));
    }

    #[test]
    fn admin_functions_are_revoked_from_everyone_else_and_granted_to_the_admin() {
        let g = object_grants();
        for (name, args) in ADMIN_FUNCTIONS {
            let target = format!("{SCHEMA_INTERNAL}.{name}({args})");
            let revoke = g
                .iter()
                .find(|s| s.starts_with("REVOKE") && s.contains(&target))
                .unwrap();
            assert!(
                revoke.contains("FROM PUBLIC, bhdata_recipe, bhdata_sql, bhdata_reader"),
                "{revoke}"
            );
            assert!(!revoke.contains("bhdata_admin"));
            assert!(g.contains(&format!(
                "GRANT EXECUTE ON FUNCTION {target} TO bhdata_admin"
            )));
            // Never granted to the recipe role.
            assert!(!g.iter().any(|s| s.starts_with("GRANT")
                && s.contains(&target)
                && s.ends_with("bhdata_recipe")));
        }
    }

    #[test]
    fn the_version_row_is_an_upsert() {
        let sql = record_version(7);
        assert!(
            sql.contains("'schema_version', '7'") && sql.contains("ON CONFLICT (key) DO UPDATE")
        );
    }

    #[test]
    fn migration_four_makes_the_logins_and_the_audit_log_and_nothing_else() {
        let all = migrations();
        let sql = all[3].statements.join("\n");
        assert_eq!(all[3].version, 4);
        assert!(sql.contains("CREATE TABLE bhdata_control.logins"));
        assert!(sql.contains("CREATE TABLE bhdata_control.audit"));
        for column in ["salt bytea", "stored_key bytea", "server_key bytea"] {
            assert!(sql.contains(column), "{column}");
        }
        assert!(!sql.contains("password text") && !sql.contains("password bytea"));
        assert!(sql.contains(&format!("{{1,{MAX_LOGIN_LEN}}}")));
        for event in AUDIT_EVENTS {
            assert!(sql.contains(&format!("'{event}'")), "{event}");
        }
        assert!(!sql.contains("ALTER TABLE") && !sql.contains("DROP"));
    }

    #[test]
    fn the_admin_reads_and_inserts_logins_and_audit_and_may_not_delete_or_edit_an_audit_row() {
        let grants = object_grants();
        assert!(grants.contains(
            &"GRANT SELECT, INSERT ON bhdata_control.logins, bhdata_control.audit TO bhdata_admin"
                .to_string()
        ));
        let update = grants
            .iter()
            .find(|g| g.starts_with("GRANT UPDATE (salt"))
            .expect("a column-level update grant on logins");
        assert!(update.ends_with("ON bhdata_control.logins TO bhdata_admin"));
        assert!(!update.contains("login_id") && !update.contains("created_"));
        assert!(!grants
            .iter()
            .any(|g| g.contains("UPDATE") && g.contains("bhdata_control.audit")));
    }
}
