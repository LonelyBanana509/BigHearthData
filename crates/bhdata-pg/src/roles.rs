//! The database roles, what each may do, and the login map, as SQL text and checks (spec sections
//! 1, 4 and 6).
//!
//! Nothing here runs SQL. Slice B's bootstrap runs these statements as the superuser, in order:
//! [`create_role`] and [`timeout_statements`] for every role (connected to `postgres`),
//! [`create_database`] unless [`DATABASE_EXISTS_QUERY`] finds it, then [`schema_statements`]
//! (connected to the new database), the internal migrations (see [`crate::internal`], run after
//! `SET ROLE bhdata_owner`), and last [`crate::internal::object_grants`]. Rights are revoked and
//! granted from a fixed list at every start, so a manual change does not survive a restart.
//!
//! Every name is a constant of lowercase letters and underscores, so none needs quoting and none
//! is ever built from outside input. The one outside value, the operating-system user name in the
//! login map, is checked by [`ident_map`].

use std::fmt;

/// The one database. Created with the instance's default locale (`engine.locale`).
pub const DATABASE: &str = "bhdata";
/// Requests, change log, meta, and the narrow functions.
pub const SCHEMA_INTERNAL: &str = "bhdata_internal";
/// The control plane's registry: schema versions and recipe versions (spec
/// `recipe-compiler.md`, section 5). Owned by the owner role; only the admin role reads and
/// inserts there.
pub const SCHEMA_CONTROL: &str = "bhdata_control";
/// Recipe-managed tables.
pub const SCHEMA_APP: &str = "app";
/// SQL-mode tables (design section 10c).
pub const SCHEMA_SQL: &str = "sql";
/// The four schemas, in the order they are created.
pub const SCHEMAS: [&str; 4] = [SCHEMA_INTERNAL, SCHEMA_CONTROL, SCHEMA_APP, SCHEMA_SQL];
/// The name of the `pg_ident` map, used in `pg_hba.conf` as `map=bhdata`.
pub const IDENT_MAP: &str = "bhdata";

/// Finds the database, as the superuser connected to `postgres`.
pub const DATABASE_EXISTS_QUERY: &str =
    "SELECT 1 FROM pg_catalog.pg_database WHERE datname = 'bhdata'";

/// A database role the data layer uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Owns the schemas, the internal objects and the narrow functions. Nobody logs in as it.
    Owner,
    /// The write path: only the narrow functions and the declared recipe tables.
    Recipe,
    /// SQL mode: reads `app`, writes only in `sql`.
    Sql,
    /// Reads.
    Reader,
    /// The data layer's own work: migrations, registration, backups. Not a superuser.
    Admin,
}

impl Role {
    /// Every role.
    pub const ALL: [Role; 5] = [
        Role::Owner,
        Role::Recipe,
        Role::Sql,
        Role::Reader,
        Role::Admin,
    ];
    /// The roles that log in, each for the pool of the same name.
    pub const LOGIN: [Role; 4] = [Role::Recipe, Role::Sql, Role::Reader, Role::Admin];

    /// The role's name in the database.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Role::Owner => "bhdata_owner",
            Role::Recipe => "bhdata_recipe",
            Role::Sql => "bhdata_sql",
            Role::Reader => "bhdata_reader",
            Role::Admin => "bhdata_admin",
        }
    }

    /// Whether the role may log in.
    #[must_use]
    pub fn can_login(self) -> bool {
        self != Role::Owner
    }

    /// The role's timeouts, set in the database so they hold even if a pool is misused
    /// (spec section 6). The owner never logs in, so it has none.
    #[must_use]
    pub fn timeouts(self) -> Option<Timeouts> {
        let t = |lock, statement, idle| {
            Some(Timeouts {
                lock,
                statement,
                idle_in_transaction: idle,
            })
        };
        match self {
            Role::Owner => None,
            Role::Recipe => t("2s", "5s", "10s"),
            Role::Reader => t("1s", "5s", "5s"),
            Role::Sql => t("2s", "30s", "30s"),
            Role::Admin => t("10s", "10min", "1min"),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The three timeouts of one role, as PostgreSQL time strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// `lock_timeout`.
    pub lock: &'static str,
    /// `statement_timeout`.
    pub statement: &'static str,
    /// `idle_in_transaction_session_timeout`.
    pub idle_in_transaction: &'static str,
}

/// Creates the role if it is missing. An existing role is left alone, whatever its attributes:
/// [`check_attrs`] is what notices a wrong one.
#[must_use]
pub fn create_role(role: Role) -> String {
    let login = if role.can_login() { "LOGIN" } else { "NOLOGIN" };
    let name = role.name();
    format!(
        "DO $$ BEGIN IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = '{name}') THEN \
         CREATE ROLE {name} {login} NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS; \
         END IF; END $$"
    )
}

/// `ALTER ROLE ... SET` for each timeout of the role. Safe to run again.
#[must_use]
pub fn timeout_statements(role: Role) -> Vec<String> {
    let Some(t) = role.timeouts() else {
        return Vec::new();
    };
    let name = role.name();
    vec![
        format!("ALTER ROLE {name} SET lock_timeout = '{}'", t.lock),
        format!(
            "ALTER ROLE {name} SET statement_timeout = '{}'",
            t.statement
        ),
        format!(
            "ALTER ROLE {name} SET idle_in_transaction_session_timeout = '{}'",
            t.idle_in_transaction
        ),
    ]
}

/// Creates the database, owned by the owner role. Run only when [`DATABASE_EXISTS_QUERY`] found
/// nothing: `CREATE DATABASE` cannot be made conditional in SQL.
#[must_use]
pub fn create_database() -> String {
    format!("CREATE DATABASE {DATABASE} OWNER {}", Role::Owner)
}

/// Schema rights, one row per schema and login role: the rights it ends up with. Every pair is
/// listed, with an empty string for "none", so a right that is not in this table is revoked.
pub const SCHEMA_RIGHTS: [(&str, Role, &str); 16] = [
    (SCHEMA_INTERNAL, Role::Recipe, "USAGE"), // to call the three functions, nothing else
    (SCHEMA_INTERNAL, Role::Reader, ""),
    (SCHEMA_INTERNAL, Role::Sql, ""),
    (SCHEMA_INTERNAL, Role::Admin, ""), // reaches the internal tables only by SET ROLE bhdata_owner
    (SCHEMA_CONTROL, Role::Recipe, ""),
    (SCHEMA_CONTROL, Role::Reader, ""),
    (SCHEMA_CONTROL, Role::Sql, ""),
    (SCHEMA_CONTROL, Role::Admin, "USAGE"), // reads and inserts registry rows; see object_grants
    (SCHEMA_APP, Role::Recipe, "USAGE"),
    (SCHEMA_APP, Role::Reader, "USAGE"),
    (SCHEMA_APP, Role::Sql, "USAGE"),
    (SCHEMA_APP, Role::Admin, "USAGE, CREATE"),
    (SCHEMA_SQL, Role::Recipe, ""),
    (SCHEMA_SQL, Role::Reader, ""),
    (SCHEMA_SQL, Role::Sql, "USAGE, CREATE"),
    (SCHEMA_SQL, Role::Admin, "USAGE, CREATE"),
];

/// Database, schema and default-privilege statements, run by the superuser connected to the new
/// database. Safe to run again.
#[must_use]
pub fn schema_statements() -> Vec<String> {
    let owner = Role::Owner;
    let mut out = Vec::new();

    // Nobody gets in or creates anything by default. Each login role may connect and nothing else
    // (no CREATE, so no extension can be loaded; no TEMP). The owner is revoked too: it is only
    // ever reached by SET ROLE inside a session that already exists.
    out.push(format!("REVOKE ALL ON DATABASE {DATABASE} FROM PUBLIC"));
    out.push(format!("REVOKE ALL ON DATABASE {DATABASE} FROM {owner}"));
    for r in Role::LOGIN {
        out.push(format!("REVOKE ALL ON DATABASE {DATABASE} FROM {r}"));
        out.push(format!("GRANT CONNECT ON DATABASE {DATABASE} TO {r}"));
    }
    out.push("REVOKE ALL ON SCHEMA public FROM PUBLIC".to_string());

    for s in SCHEMAS {
        out.push(format!(
            "CREATE SCHEMA IF NOT EXISTS {s} AUTHORIZATION {owner}"
        ));
        out.push(format!("REVOKE ALL ON SCHEMA {s} FROM PUBLIC"));
    }
    for (schema, role, rights) in SCHEMA_RIGHTS {
        out.push(format!("REVOKE ALL ON SCHEMA {schema} FROM {role}"));
        if !rights.is_empty() {
            out.push(format!("GRANT {rights} ON SCHEMA {schema} TO {role}"));
        }
    }

    // The admin may become the owner to run the internal migrations, but does not have the
    // owner's rights unless it does so.
    out.push(format!(
        "GRANT {owner} TO {} WITH INHERIT FALSE, SET TRUE",
        Role::Admin
    ));

    // A function is executable by PUBLIC unless that is revoked at creation.
    for creator in [Role::Owner, Role::Admin] {
        out.push(format!(
            "ALTER DEFAULT PRIVILEGES FOR ROLE {creator} REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC"
        ));
    }

    // Tables the admin creates at registration (M3): readers and SQL mode read `app`; SQL mode
    // has its own rights in `sql`. The recipe role gets rights per declared table, never by default.
    let admin = Role::Admin;
    for (schema, role) in [
        (SCHEMA_APP, Role::Recipe),
        (SCHEMA_APP, Role::Reader),
        (SCHEMA_APP, Role::Sql),
        (SCHEMA_SQL, Role::Recipe),
        (SCHEMA_SQL, Role::Reader),
        (SCHEMA_SQL, Role::Sql),
    ] {
        out.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {admin} IN SCHEMA {schema} REVOKE ALL ON TABLES FROM {role}"));
        out.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {admin} IN SCHEMA {schema} REVOKE ALL ON SEQUENCES FROM {role}"));
    }
    for role in [Role::Reader, Role::Sql] {
        out.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {admin} IN SCHEMA {SCHEMA_APP} GRANT SELECT ON TABLES TO {role}"));
    }
    out.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {admin} IN SCHEMA {SCHEMA_SQL} GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {}", Role::Sql));
    out.push(format!("ALTER DEFAULT PRIVILEGES FOR ROLE {admin} IN SCHEMA {SCHEMA_SQL} GRANT USAGE, SELECT ON SEQUENCES TO {}", Role::Sql));
    out
}

/// What is wrong with the operating-system user name given to [`ident_map`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("operating-system user name {0:?} cannot be written into pg_ident.conf (only a-z A-Z 0-9 . _ - are allowed, not first character '-')")]
pub struct BadOsUser(pub String);

/// The `pg_hba.conf` line for the Unix socket, in place of the engine's current one.
#[must_use]
pub fn hba_line() -> String {
    format!("local all all peer map={IDENT_MAP}")
}

/// The whole `pg_ident.conf`: the service user as its own superuser (for the bootstrap, never a
/// pool), and as each of the four login roles. All map from the one operating-system user, so
/// this limits what a bug in a pool can do; it does not isolate processes (spec decision 2).
///
/// # Errors
/// [`BadOsUser`] when the name has anything that would need quoting.
pub fn ident_map(os_user: &str) -> Result<String, BadOsUser> {
    let ok = !os_user.is_empty()
        && !os_user.starts_with('-')
        && os_user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !ok {
        return Err(BadOsUser(os_user.to_string()));
    }
    let mut text = String::from("# Managed by bhdata: rewritten at every start. Do not edit.\n# MAPNAME  OS-USER  DATABASE-USER\n");
    text.push_str(&format!("{IDENT_MAP} {os_user} {os_user}\n"));
    for r in Role::LOGIN {
        text.push_str(&format!("{IDENT_MAP} {os_user} {r}\n"));
    }
    Ok(text)
}

/// The attributes of a role that matter, as `pg_roles` reports them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleRow {
    /// `rolname`.
    pub name: String,
    /// `rolcanlogin`.
    pub can_login: bool,
    /// `rolsuper`.
    pub superuser: bool,
    /// `rolcreatedb`.
    pub create_db: bool,
    /// `rolcreaterole`.
    pub create_role: bool,
    /// `rolreplication`.
    pub replication: bool,
    /// `rolbypassrls`.
    pub bypass_rls: bool,
}

/// Reads [`RoleRow`]s, in the column order of that struct.
pub const ROLE_ATTRS_QUERY: &str = "SELECT rolname, rolcanlogin, rolsuper, rolcreatedb, rolcreaterole, \
     rolreplication, rolbypassrls FROM pg_catalog.pg_roles \
     WHERE rolname IN ('bhdata_owner', 'bhdata_recipe', 'bhdata_sql', 'bhdata_reader', 'bhdata_admin') \
     ORDER BY rolname";

/// The attributes a role must have.
#[must_use]
pub fn expected_attrs(role: Role) -> RoleRow {
    RoleRow {
        name: role.name().to_string(),
        can_login: role.can_login(),
        superuser: false,
        create_db: false,
        create_role: false,
        replication: false,
        bypass_rls: false,
    }
}

/// A role that exists with the wrong attributes. Never fixed silently.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttrError {
    /// The role is missing from the rows read.
    #[error("role {0} is missing")]
    Missing(&'static str),
    /// One attribute differs.
    #[error("role {role} has the wrong attribute {attribute} (must be {expected})")]
    Wrong {
        /// The role.
        role: &'static str,
        /// The attribute.
        attribute: &'static str,
        /// What it must be.
        expected: bool,
    },
}

/// Checks the rows read with [`ROLE_ATTRS_QUERY`] against what every role must be.
///
/// # Errors
/// The first role that is missing or has a wrong attribute.
pub fn check_attrs(rows: &[RoleRow]) -> Result<(), AttrError> {
    for role in Role::ALL {
        let want = expected_attrs(role);
        let name = role.name();
        let Some(got) = rows.iter().find(|r| r.name == name) else {
            return Err(AttrError::Missing(name));
        };
        let pairs = [
            ("login", got.can_login, want.can_login),
            ("superuser", got.superuser, want.superuser),
            ("createdb", got.create_db, want.create_db),
            ("createrole", got.create_role, want.create_role),
            ("replication", got.replication, want.replication),
            ("bypassrls", got.bypass_rls, want.bypass_rls),
        ];
        for (attribute, have, expected) in pairs {
            if have != expected {
                return Err(AttrError::Wrong {
                    role: name,
                    attribute,
                    expected,
                });
            }
        }
    }
    Ok(())
}

/// Reads each schema's owner: rows of `(schema name, owner name)`.
pub const SCHEMA_OWNER_QUERY: &str = "SELECT n.nspname, r.rolname FROM pg_catalog.pg_namespace n \
     JOIN pg_catalog.pg_roles r ON r.oid = n.nspowner \
     WHERE n.nspname IN ('bhdata_internal', 'bhdata_control', 'app', 'sql') ORDER BY n.nspname";

/// A schema that exists with another owner than the owner role.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaOwnerError {
    /// The schema is missing from the rows read.
    #[error("schema {0} is missing")]
    Missing(&'static str),
    /// The schema belongs to someone else.
    #[error("schema {schema} is owned by {found}, not by {expected}")]
    Wrong {
        /// The schema.
        schema: &'static str,
        /// Its owner.
        found: String,
        /// Who must own it.
        expected: &'static str,
    },
}

/// Checks the rows read with [`SCHEMA_OWNER_QUERY`].
///
/// # Errors
/// The first schema that is missing or has the wrong owner.
pub fn check_schema_owners(rows: &[(String, String)]) -> Result<(), SchemaOwnerError> {
    for schema in SCHEMAS {
        match rows.iter().find(|(s, _)| s == schema) {
            None => return Err(SchemaOwnerError::Missing(schema)),
            Some((_, owner)) if owner != Role::Owner.name() => {
                return Err(SchemaOwnerError::Wrong {
                    schema,
                    found: owner.clone(),
                    expected: Role::Owner.name(),
                });
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<RoleRow> {
        Role::ALL.iter().map(|r| expected_attrs(*r)).collect()
    }

    #[test]
    fn names_are_plain_and_distinct() {
        let mut seen = std::collections::HashSet::new();
        for r in Role::ALL {
            let n = r.name();
            assert!(
                n.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{n}"
            );
            assert!(seen.insert(n));
        }
        for n in [
            DATABASE,
            SCHEMA_INTERNAL,
            SCHEMA_CONTROL,
            SCHEMA_APP,
            SCHEMA_SQL,
            IDENT_MAP,
        ] {
            assert!(
                n.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{n}"
            );
        }
    }

    #[test]
    fn the_attribute_and_owner_queries_name_every_role_and_schema() {
        for r in Role::ALL {
            assert!(ROLE_ATTRS_QUERY.contains(&format!("'{}'", r.name())), "{r}");
        }
        for s in SCHEMAS {
            assert!(SCHEMA_OWNER_QUERY.contains(&format!("'{s}'")), "{s}");
        }
    }

    #[test]
    fn only_the_owner_cannot_log_in() {
        assert!(!Role::Owner.can_login());
        assert!(Role::LOGIN.iter().all(|r| r.can_login()));
        assert!(!Role::LOGIN.contains(&Role::Owner));
        assert_eq!(Role::LOGIN.len() + 1, Role::ALL.len());
    }

    #[test]
    fn create_role_is_conditional_and_least_privilege() {
        for r in Role::ALL {
            let sql = create_role(r);
            assert!(
                sql.contains("IF NOT EXISTS") && sql.contains(&format!("rolname = '{}'", r.name()))
            );
            for word in [
                "NOSUPERUSER",
                "NOCREATEDB",
                "NOCREATEROLE",
                "NOREPLICATION",
                "NOBYPASSRLS",
            ] {
                assert!(sql.contains(word), "{r}: {word}");
            }
            assert_eq!(sql.contains(" NOLOGIN "), !r.can_login(), "{r}");
        }
    }

    #[test]
    fn timeouts_follow_the_spec_table() {
        let t = |r: Role| {
            r.timeouts()
                .map(|t| (t.lock, t.statement, t.idle_in_transaction))
        };
        assert_eq!(t(Role::Recipe), Some(("2s", "5s", "10s")));
        assert_eq!(t(Role::Reader), Some(("1s", "5s", "5s")));
        assert_eq!(t(Role::Sql), Some(("2s", "30s", "30s")));
        assert_eq!(t(Role::Admin), Some(("10s", "10min", "1min")));
        assert_eq!(t(Role::Owner), None);
        assert!(timeout_statements(Role::Owner).is_empty());
        let s = timeout_statements(Role::Recipe);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], "ALTER ROLE bhdata_recipe SET lock_timeout = '2s'");
        assert!(s[2].contains("idle_in_transaction_session_timeout = '10s'"));
    }

    #[test]
    fn the_database_is_owned_by_the_owner() {
        assert_eq!(
            create_database(),
            "CREATE DATABASE bhdata OWNER bhdata_owner"
        );
        assert!(DATABASE_EXISTS_QUERY.contains("'bhdata'"));
    }

    #[test]
    fn schema_rights_list_every_pair_once_and_never_create_internal() {
        for s in SCHEMAS {
            for r in Role::LOGIN {
                let n = SCHEMA_RIGHTS
                    .iter()
                    .filter(|(a, b, _)| *a == s && *b == r)
                    .count();
                assert_eq!(n, 1, "{s} {r}");
            }
        }
        for (s, r, rights) in SCHEMA_RIGHTS {
            if s == SCHEMA_INTERNAL {
                assert!(!rights.contains("CREATE"), "{r} may create in {s}");
                assert!(
                    r == Role::Recipe || rights.is_empty(),
                    "{r} has rights on {s}"
                );
            }
            if r == Role::Recipe {
                assert!(!rights.contains("CREATE"), "recipe may create in {s}");
            }
        }
    }

    #[test]
    fn schema_statements_lock_the_database_down_then_grant_from_the_list() {
        let all = schema_statements();
        let has = |t: &str| all.iter().any(|s| s == t);
        assert_eq!(all[0], "REVOKE ALL ON DATABASE bhdata FROM PUBLIC");
        assert!(has("REVOKE ALL ON SCHEMA public FROM PUBLIC"));
        for r in Role::LOGIN {
            assert!(has(&format!("GRANT CONNECT ON DATABASE bhdata TO {r}")));
            assert!(!all.iter().any(|s| s.contains("GRANT")
                && s.contains("ON DATABASE")
                && !s.contains("CONNECT")));
        }
        for s in SCHEMAS {
            assert!(has(&format!(
                "CREATE SCHEMA IF NOT EXISTS {s} AUTHORIZATION bhdata_owner"
            )));
        }
        assert!(has(
            "GRANT USAGE ON SCHEMA bhdata_internal TO bhdata_recipe"
        ));
        assert!(has("GRANT USAGE, CREATE ON SCHEMA sql TO bhdata_sql"));
        assert!(!all.iter().any(|s| s.starts_with("GRANT")
            && s.contains("bhdata_internal")
            && s.contains("CREATE")));
        assert!(has(
            "GRANT bhdata_owner TO bhdata_admin WITH INHERIT FALSE, SET TRUE"
        ));
        assert!(has("ALTER DEFAULT PRIVILEGES FOR ROLE bhdata_owner REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC"));
    }

    #[test]
    fn revokes_come_before_the_grant_of_the_same_schema_and_role() {
        let all = schema_statements();
        for (schema, role, rights) in SCHEMA_RIGHTS {
            if rights.is_empty() {
                continue;
            }
            let revoke = all
                .iter()
                .position(|s| *s == format!("REVOKE ALL ON SCHEMA {schema} FROM {role}"));
            let grant = all
                .iter()
                .position(|s| *s == format!("GRANT {rights} ON SCHEMA {schema} TO {role}"));
            assert!(
                revoke.is_some() && grant.is_some() && revoke < grant,
                "{schema} {role}"
            );
        }
    }

    #[test]
    fn hba_and_ident_use_one_map_and_exactly_the_login_roles() {
        assert_eq!(hba_line(), "local all all peer map=bhdata");
        let text = ident_map("bhdata").unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            lines,
            [
                "bhdata bhdata bhdata",
                "bhdata bhdata bhdata_recipe",
                "bhdata bhdata bhdata_sql",
                "bhdata bhdata bhdata_reader",
                "bhdata bhdata bhdata_admin",
            ]
        );
        assert!(!text.contains("bhdata_owner"));
    }

    #[test]
    fn ident_map_refuses_names_that_need_quoting() {
        for bad in ["", "-x", "a b", "a\nb", "a#b", "a\"b", "/re.*", "é", "a,b"] {
            assert_eq!(ident_map(bad), Err(BadOsUser(bad.to_string())), "{bad:?}");
        }
        assert!(ident_map("svc.user_1-x").is_ok());
    }

    #[test]
    fn good_roles_pass_the_attribute_check() {
        assert_eq!(check_attrs(&rows()), Ok(()));
    }

    #[test]
    fn a_wrong_attribute_is_named_not_fixed() {
        type Change = fn(&mut RoleRow);
        let cases: [(&str, Change, &str); 6] = [
            ("bhdata_recipe", |r| r.superuser = true, "superuser"),
            ("bhdata_sql", |r| r.create_db = true, "createdb"),
            ("bhdata_reader", |r| r.create_role = true, "createrole"),
            ("bhdata_admin", |r| r.replication = true, "replication"),
            ("bhdata_recipe", |r| r.bypass_rls = true, "bypassrls"),
            ("bhdata_owner", |r| r.can_login = true, "login"),
        ];
        for (name, change, attribute) in cases {
            let mut all = rows();
            change(all.iter_mut().find(|r| r.name == name).unwrap());
            let err = check_attrs(&all).unwrap_err();
            let text = err.to_string();
            assert!(text.contains(name) && text.contains(attribute), "{text}");
        }
    }

    #[test]
    fn a_missing_role_is_named() {
        let mut all = rows();
        all.retain(|r| r.name != "bhdata_reader");
        assert_eq!(check_attrs(&all), Err(AttrError::Missing("bhdata_reader")));
    }

    #[test]
    fn schema_owners_are_checked() {
        let ok: Vec<(String, String)> = SCHEMAS
            .iter()
            .map(|s| ((*s).to_string(), "bhdata_owner".to_string()))
            .collect();
        assert_eq!(check_schema_owners(&ok), Ok(()));
        let mut bad = ok.clone();
        let app = bad.iter().position(|(s, _)| s == "app").unwrap_or(0);
        bad[app].1 = "bhdata".to_string();
        let text = check_schema_owners(&bad).unwrap_err().to_string();
        assert!(
            text.contains("app") && text.contains("bhdata_owner"),
            "{text}"
        );
        let mut gone = ok;
        gone.pop();
        assert_eq!(
            check_schema_owners(&gone),
            Err(SchemaOwnerError::Missing("sql"))
        );
    }
}
