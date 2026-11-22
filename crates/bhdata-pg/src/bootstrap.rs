//! The one idempotent step that makes the database ready (spec section 5), run as the superuser
//! over the socket at server start:
//!
//! 1. create the roles and their timeouts, then check their attributes (a role with wrong
//!    attributes is an error that names it, not something silently fixed);
//! 2. create the database if it is missing;
//! 3. schemas and their rights, then check the schema owners;
//! 4. the internal migrations, run as `bhdata_admin` after `SET ROLE bhdata_owner`, each in one
//!    transaction together with its row in `meta`;
//! 5. the rights on the internal objects.
//!
//! Running it again changes nothing. Data from a newer release stops it ([`TooNew`]).

use bhdata_types::error::Error;
use tokio_postgres::{Client, NoTls};

use crate::errors::PgError;
use crate::internal::{self, TooNew};
use crate::pool::{describe, pg_error, Target};
use crate::roles::{
    self, AttrError, Role, RoleRow, SchemaOwnerError, DATABASE, DATABASE_EXISTS_QUERY,
};

/// What a bootstrap did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    /// The schema version the data is at afterwards.
    pub schema_version: u32,
    /// The migrations applied by this run, oldest first. Empty on every run after the first.
    pub applied: Vec<u32>,
    /// Whether this run created the database.
    pub created_database: bool,
}

/// Why the bootstrap stopped.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// The server refused a step.
    #[error("bootstrap step {step}: {detail}")]
    Pg {
        /// Which step.
        step: &'static str,
        /// What kind of failure, and whether it is worth another try.
        error: PgError,
        /// The server's text.
        detail: String,
    },
    /// A role exists with the wrong attributes.
    #[error(transparent)]
    Role(#[from] AttrError),
    /// A schema exists with the wrong owner.
    #[error(transparent)]
    Schema(#[from] SchemaOwnerError),
    /// The data is from a newer release.
    #[error(transparent)]
    TooNew(#[from] TooNew),
    /// The stored schema version is not a number.
    #[error("the stored schema version {0:?} is not a number")]
    BadVersion(String),
    /// The private runtime could not be built.
    #[error("could not start the bootstrap runtime: {0}")]
    Runtime(std::io::Error),
}

impl BootstrapError {
    /// The error a client would see if this ever reached one: never anything but `internal`.
    #[must_use]
    pub fn client_error(&self) -> Error {
        Error::Internal(self.to_string())
    }
}

fn step(step: &'static str) -> impl Fn(tokio_postgres::Error) -> BootstrapError {
    move |e| BootstrapError::Pg {
        step,
        error: pg_error(&e),
        detail: describe(&e),
    }
}

async fn connect(
    target: &Target,
    user: &str,
    dbname: &str,
    what: &'static str,
) -> Result<Client, BootstrapError> {
    let (client, connection) = target
        .config(user, dbname)
        .connect(NoTls)
        .await
        .map_err(step(what))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

async fn run_all(
    client: &Client,
    statements: &[String],
    what: &'static str,
) -> Result<(), BootstrapError> {
    for s in statements {
        client.batch_execute(s).await.map_err(step(what))?;
    }
    Ok(())
}

/// Bootstraps the database. `superuser` is the service user, the instance's superuser.
///
/// # Errors
/// [`BootstrapError`]: a refused step, a wrong role or schema owner, or newer data.
pub async fn bootstrap(
    target: &Target,
    superuser: &str,
) -> Result<BootstrapReport, BootstrapError> {
    // 1 and 2: roles and the database, from the maintenance database.
    let admin_db = connect(target, superuser, "postgres", "connect to postgres").await?;
    for role in Role::ALL {
        run_all(&admin_db, &[roles::create_role(role)], "create roles").await?;
    }
    let rows = admin_db
        .query(roles::ROLE_ATTRS_QUERY, &[])
        .await
        .map_err(step("read roles"))?;
    let attrs: Vec<RoleRow> = rows
        .iter()
        .map(|r| RoleRow {
            name: r.get(0),
            can_login: r.get(1),
            superuser: r.get(2),
            create_db: r.get(3),
            create_role: r.get(4),
            replication: r.get(5),
            bypass_rls: r.get(6),
        })
        .collect();
    roles::check_attrs(&attrs)?;
    for role in Role::ALL {
        run_all(
            &admin_db,
            &roles::timeout_statements(role),
            "set role timeouts",
        )
        .await?;
    }
    let exists = admin_db
        .query_opt(DATABASE_EXISTS_QUERY, &[])
        .await
        .map_err(step("look for the database"))?
        .is_some();
    if !exists {
        run_all(
            &admin_db,
            &[roles::create_database()],
            "create the database",
        )
        .await?;
    }

    // 3: schemas and rights, from the new database.
    let db = connect(target, superuser, DATABASE, "connect to bhdata").await?;
    run_all(&db, &roles::schema_statements(), "schemas and rights").await?;
    let owners: Vec<(String, String)> = db
        .query(roles::SCHEMA_OWNER_QUERY, &[])
        .await
        .map_err(step("read schema owners"))?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    roles::check_schema_owners(&owners)?;

    // 4: migrations, as the admin role acting as the owner. Only the owner can read `meta`.
    let applied = migrate(target).await?;

    // 5: rights on the objects the migrations made.
    run_all(&db, &internal::object_grants(), "object rights").await?;

    Ok(BootstrapReport {
        schema_version: internal::SCHEMA_VERSION,
        applied,
        created_database: !exists,
    })
}

async fn migrate(target: &Target) -> Result<Vec<u32>, BootstrapError> {
    let mut client = connect(target, Role::Admin.name(), DATABASE, "connect as admin").await?;
    client
        .batch_execute(&format!("SET ROLE {}", Role::Owner))
        .await
        .map_err(step("act as the owner"))?;
    let has_meta: bool = client
        .query_one(internal::META_EXISTS_QUERY, &[])
        .await
        .map_err(step("look for meta"))?
        .get(0);
    let current = if has_meta {
        match client
            .query_opt(internal::READ_VERSION_QUERY, &[])
            .await
            .map_err(step("read the schema version"))?
        {
            Some(row) => {
                let text: String = row.get(0);
                Some(
                    text.parse::<u32>()
                        .map_err(|_| BootstrapError::BadVersion(text))?,
                )
            }
            None => None,
        }
    } else {
        None
    };
    let mut applied = Vec::new();
    for migration in internal::pending(current)? {
        let tx = client
            .transaction()
            .await
            .map_err(step("begin a migration"))?;
        for statement in &migration.statements {
            tx.batch_execute(statement).await.map_err(step("migrate"))?;
        }
        tx.batch_execute(&internal::record_version(migration.version))
            .await
            .map_err(step("record the version"))?;
        tx.commit().await.map_err(step("commit a migration"))?;
        applied.push(migration.version);
    }
    Ok(applied)
}

/// [`bootstrap`] on a small private runtime built for this one call, for the host while the
/// server itself is not async (until M5).
///
/// # Errors
/// As [`bootstrap`], or [`BootstrapError::Runtime`].
pub fn bootstrap_blocking(
    target: &Target,
    superuser: &str,
) -> Result<BootstrapReport, BootstrapError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(BootstrapError::Runtime)?;
    rt.block_on(bootstrap(target, superuser))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrong_role_a_wrong_owner_and_newer_data_name_what_is_wrong() {
        let e = BootstrapError::from(AttrError::Wrong {
            role: "bhdata_reader",
            attribute: "createdb",
            expected: false,
        });
        assert!(e.to_string().contains("bhdata_reader") && e.to_string().contains("createdb"));
        let e = BootstrapError::from(SchemaOwnerError::Missing("app"));
        assert!(e.to_string().contains("app"));
        let e = BootstrapError::from(TooNew { found: 9, known: 1 });
        assert!(e.to_string().contains("never downgraded"));
        assert!(BootstrapError::BadVersion("x".into())
            .to_string()
            .contains("not a number"));
    }

    #[test]
    fn nothing_from_the_bootstrap_reaches_a_client_as_anything_but_internal() {
        let e = BootstrapError::from(TooNew { found: 9, known: 1 });
        assert_eq!(
            e.client_error().kind(),
            bhdata_types::error::ErrorKind::Internal
        );
    }
}
