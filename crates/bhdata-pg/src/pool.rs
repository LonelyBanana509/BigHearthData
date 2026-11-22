//! One connection pool per login role, over the instance's Unix socket (spec sections 1 and 7).
//!
//! Each pool logs in as its own role through the `pg_ident` map, has a fixed size equal to its cap
//! and waits at most [`WAIT_TIMEOUT`] for a free connection, so a caller finds out quickly that
//! nothing is free ([`PoolError::Busy`], which a client sees as `overloaded`). A connection that
//! fails its recycle check (`DISCARD ALL`, which also fails inside an open transaction) is dropped
//! and replaced, never reused. The pools are separate, so SQL mode can never take a recipe
//! writer's connection. Nothing outside this crate sees the driver types except the connection
//! handed out by a getter.

use std::path::PathBuf;
use std::time::Duration;

use bhdata_types::config::PoolConfig;
use bhdata_types::error::Error;
use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod, Runtime, Timeouts};
use tokio_postgres::NoTls;

use crate::errors::{classify, PgError};
use crate::roles::{Role, DATABASE};

/// How long a caller waits for a free connection before the pool answers "busy".
pub const WAIT_TIMEOUT: Duration = Duration::from_millis(250);
/// How long making or checking a connection may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the instance listens: its socket folder and port (the port only names the socket file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The folder with the socket, `InstanceSpec::run_dir()` in the engine.
    pub socket_dir: PathBuf,
    /// The instance's port number.
    pub port: u16,
}

impl Target {
    pub(crate) fn config(&self, user: &str, dbname: &str) -> tokio_postgres::Config {
        let mut c = tokio_postgres::Config::new();
        c.host_path(&self.socket_dir)
            .port(self.port)
            .user(user)
            .dbname(dbname)
            .application_name("bhdata")
            .connect_timeout(CONNECT_TIMEOUT);
        c
    }
}

/// Classifies a driver error by its SQLSTATE; no code means the connection is gone.
#[must_use]
pub fn pg_error(e: &tokio_postgres::Error) -> PgError {
    classify(e.code().map(tokio_postgres::error::SqlState::code))
}

/// The driver error with the server's own message, for logs.
#[must_use]
pub fn describe(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => format!("{} ({})", db.message(), db.code().code()),
        None => e.to_string(),
    }
}

/// Why a connection could not be handed out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    /// Every connection of the pool is in use and none came free in time.
    #[error("no free {role} connection within {WAIT_TIMEOUT:?}")]
    Busy {
        /// The pool's role.
        role: Role,
    },
    /// The pool is shut down.
    #[error("the {role} pool is closed")]
    Closed {
        /// The pool's role.
        role: Role,
    },
    /// Making or checking a connection failed.
    #[error("{role} connection: {detail}")]
    Pg {
        /// The pool's role.
        role: Role,
        /// What kind of failure, and whether to retry.
        error: PgError,
        /// The driver's text, for logs.
        detail: String,
    },
}

impl PoolError {
    /// The error a client sees: always `overloaded` while the pool is busy or the database is
    /// coming back, and `internal` for anything else.
    #[must_use]
    pub fn client_error(&self) -> Error {
        match self {
            PoolError::Busy { .. } | PoolError::Closed { .. } => Error::Overloaded {
                retry_after: crate::errors::OVERLOAD_RETRY_AFTER,
            },
            PoolError::Pg { error, .. } => error.client_error(false),
        }
    }

    fn from_deadpool(role: Role, e: deadpool_postgres::PoolError) -> PoolError {
        use deadpool_postgres::{PoolError as P, TimeoutType};
        match e {
            P::Timeout(TimeoutType::Wait) => PoolError::Busy { role },
            P::Closed => PoolError::Closed { role },
            P::Backend(e) => PoolError::Pg {
                role,
                error: pg_error(&e),
                detail: describe(&e),
            },
            other => PoolError::Pg {
                role,
                error: classify(None),
                detail: other.to_string(),
            },
        }
    }
}

/// The four pools.
pub struct Pools {
    recipe: Pool,
    reader: Pool,
    sql: Pool,
    admin: Pool,
}

fn build(target: &Target, role: Role, size: u32) -> Result<Pool, PoolError> {
    let manager = Manager::from_config(
        target.config(role.name(), DATABASE),
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Clean,
        },
    );
    Pool::builder(manager)
        .max_size(size as usize)
        .timeouts(Timeouts {
            wait: Some(WAIT_TIMEOUT),
            create: Some(CONNECT_TIMEOUT),
            recycle: Some(CONNECT_TIMEOUT),
        })
        .runtime(Runtime::Tokio1)
        .build()
        .map_err(|e| PoolError::Pg {
            role,
            error: classify(None),
            detail: e.to_string(),
        })
}

impl Pools {
    /// Builds the pools and takes one connection from each (a size of zero is skipped), so a
    /// role that cannot log in is found now and not by the first request. Must run inside a tokio
    /// runtime.
    ///
    /// # Errors
    /// The first pool whose test connection fails.
    pub async fn connect(target: &Target, sizes: &PoolConfig) -> Result<Pools, PoolError> {
        let pools = Pools {
            recipe: build(target, Role::Recipe, sizes.recipe_writers)?,
            reader: build(target, Role::Reader, sizes.readers)?,
            sql: build(target, Role::Sql, sizes.sql_sessions)?,
            admin: build(target, Role::Admin, sizes.admin)?,
        };
        for role in Role::LOGIN {
            if pools.pool(role).status().max_size > 0 {
                drop(pools.get(role).await?);
            }
        }
        Ok(pools)
    }

    fn pool(&self, role: Role) -> &Pool {
        match role {
            Role::Recipe | Role::Owner => &self.recipe,
            Role::Reader => &self.reader,
            Role::Sql => &self.sql,
            Role::Admin => &self.admin,
        }
    }

    async fn get(&self, role: Role) -> Result<Object, PoolError> {
        self.pool(role)
            .get()
            .await
            .map_err(|e| PoolError::from_deadpool(role, e))
    }

    /// A connection that is logged in as `bhdata_recipe`.
    ///
    /// # Errors
    /// [`PoolError::Busy`] when none is free in time.
    pub async fn recipe(&self) -> Result<Object, PoolError> {
        self.get(Role::Recipe).await
    }

    /// A connection that is logged in as `bhdata_reader`.
    ///
    /// # Errors
    /// [`PoolError::Busy`] when none is free in time.
    pub async fn reader(&self) -> Result<Object, PoolError> {
        self.get(Role::Reader).await
    }

    /// A connection that is logged in as `bhdata_sql`.
    ///
    /// # Errors
    /// [`PoolError::Busy`] when none is free in time (always, when `sql_sessions` is zero).
    pub async fn sql(&self) -> Result<Object, PoolError> {
        self.get(Role::Sql).await
    }

    /// A connection that is logged in as `bhdata_admin`.
    ///
    /// # Errors
    /// [`PoolError::Busy`] when none is free in time.
    pub async fn admin(&self) -> Result<Object, PoolError> {
        self.get(Role::Admin).await
    }

    /// `(size, in_use)` of a login role's pool, for status output.
    #[must_use]
    pub fn usage(&self, role: Role) -> (usize, usize) {
        let s = self.pool(role).status();
        (s.max_size, s.size.saturating_sub(s.available))
    }

    /// Closes every pool; later requests get [`PoolError::Closed`].
    pub fn close(&self) {
        for role in Role::LOGIN {
            self.pool(role).close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_names_the_socket_folder_port_role_and_database() {
        let t = Target {
            socket_dir: PathBuf::from("/var/lib/bhdata/shard0/run"),
            port: 54_320,
        };
        let c = t.config("bhdata_recipe", "bhdata");
        assert_eq!(c.get_ports(), [54_320]);
        assert_eq!(c.get_user(), Some("bhdata_recipe"));
        assert_eq!(c.get_dbname(), Some("bhdata"));
        assert_eq!(
            c.get_hosts(),
            [tokio_postgres::config::Host::Unix(PathBuf::from(
                "/var/lib/bhdata/shard0/run"
            ))]
        );
        assert_eq!(c.get_connect_timeout(), Some(&CONNECT_TIMEOUT));
    }

    #[test]
    fn a_busy_or_closed_pool_is_overloaded_for_the_client() {
        for e in [
            PoolError::Busy { role: Role::Recipe },
            PoolError::Closed { role: Role::Reader },
        ] {
            assert_eq!(
                e.client_error().kind(),
                bhdata_types::error::ErrorKind::Overloaded
            );
        }
    }

    #[test]
    fn a_failed_login_is_internal_and_a_lost_connection_is_overloaded() {
        let pg = |code| PoolError::Pg {
            role: Role::Recipe,
            error: classify(code),
            detail: String::new(),
        };
        assert_eq!(
            pg(Some("28000")).client_error().kind(),
            bhdata_types::error::ErrorKind::Internal
        );
        assert_eq!(
            pg(None).client_error().kind(),
            bhdata_types::error::ErrorKind::Overloaded
        );
    }

    #[test]
    fn the_error_text_names_the_role_and_the_wait() {
        let text = PoolError::Busy { role: Role::Sql }.to_string();
        assert!(
            text.contains("bhdata_sql") && text.contains("250ms"),
            "{text}"
        );
    }
}
