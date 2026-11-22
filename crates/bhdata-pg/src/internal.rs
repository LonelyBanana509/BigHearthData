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
//! Migrations are append-only: a released version is never edited. Slice B runs each as the owner
//! role (`SET ROLE bhdata_owner`), in one transaction together with [`record_version`]. A function
//! that refuses a call raises [`crate::errors::SQLSTATE_BHDATA`].

use bhdata_types::ids::{MAX_ID_LEN, MAX_LOGIN_LEN};

use crate::errors::SQLSTATE_BHDATA;
use crate::roles::{Role, SCHEMA_CONTROL, SCHEMA_INTERNAL};

/// The schema version this release writes into `meta`.
pub const SCHEMA_VERSION: u32 = 2;

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

/// The narrow functions: name and argument types, as `GRANT` and `REVOKE` need them.
pub const FUNCTIONS: [(&str, &str); 3] = [
    ("claim_request", "text, text, bytea, text, integer"),
    ("finish_request", "text, text, text, text, bytea"),
    ("append_change", "text, text, text, text, text, text"),
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
    for (name, args) in FUNCTIONS {
        out.push(format!(
            "REVOKE ALL ON FUNCTION {s}.{name}({args}) FROM PUBLIC, {others}"
        ));
        out.push(format!(
            "GRANT EXECUTE ON FUNCTION {s}.{name}({args}) TO {}",
            Role::Recipe
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
        assert_eq!(all.len(), 2);
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

    #[test]
    fn every_listed_function_is_created_exactly_once_and_locked_down() {
        let stmts = v1();
        for (name, _) in FUNCTIONS {
            let created: Vec<&String> = stmts
                .iter()
                .filter(|s| s.starts_with(&format!("CREATE FUNCTION {SCHEMA_INTERNAL}.{name}(")))
                .collect();
            assert_eq!(created.len(), 1, "{name}");
            let sql = created[0];
            assert!(sql.contains("SECURITY DEFINER"), "{name}");
            assert!(
                sql.contains("SET search_path = pg_catalog, pg_temp"),
                "{name}"
            );
        }
        let functions = stmts
            .iter()
            .filter(|s| s.starts_with("CREATE FUNCTION"))
            .count();
        assert_eq!(functions, FUNCTIONS.len());
    }

    #[test]
    fn function_argument_lists_match_their_grants() {
        let stmts = v1();
        // claim_request: five parameters, finish_request: five, append_change: six.
        for (name, args) in FUNCTIONS {
            let sql = stmts
                .iter()
                .find(|s| s.starts_with(&format!("CREATE FUNCTION {SCHEMA_INTERNAL}.{name}(")))
                .unwrap();
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
    fn the_version_row_is_an_upsert() {
        let sql = record_version(7);
        assert!(
            sql.contains("'schema_version', '7'") && sql.contains("ON CONFLICT (key) DO UPDATE")
        );
    }
}
