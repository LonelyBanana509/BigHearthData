//! The `logins` table and `LoginStore` for the gateway (wire-protocol spec, sections 6 and 10). Lane C.
//!
//! A login is a name, a SCRAM verifier (never the password), the four permissions, a recipe
//! allow-list, optional limits and a disabled flag. Everything here that decides something is a plain
//! function and is tested without a database:
//!
//! * [`Change::apply`]: what a `grant`, `revoke`, `disable` or `enable` does to a [`Policy`].
//! * [`Raw::into_record`]: a stored row as the gateway's [`LoginRecord`].
//!
//! The calls that talk to the server run as the admin role, each change in one transaction together
//! with its audit row (see [`crate::audit`]), so a change that is not in the audit log did not happen:
//!
//! * [`create`], [`set_password`], [`change`]: the commands of `bhdatactl login`.
//! * [`find`], [`list`]: reading.
//! * [`PgLoginStore`]: what the gateway calls through [`LoginStore`].
//! * [`blocking`]: the commands on a private runtime with one admin connection, for `bhdatactl`.
//!
//! The password reaches this module as bytes, becomes a verifier at once, and is not kept or printed.

use std::collections::BTreeSet;
use std::sync::Arc;

use bhdata_api::login::{
    AuditEvent, LoginRecord, LoginStore, Permissions, RecipeAllow, ScramVerifier,
};
use bhdata_pg::pool::{describe, pg_error, Pools};
use bhdata_proto::scram::Verifier;
use bhdata_types::ids::MAX_LOGIN_LEN;
use bhdata_types::{Error, LoginId};
use tokio_postgres::error::SqlState;
use tokio_postgres::{Client, GenericClient};

use crate::audit;
use crate::error::ControlError;

/// Why a login command failed.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// A login of that name exists already.
    #[error("a login named `{0}` exists already")]
    Exists(String),
    /// No login of that name.
    #[error("there is no login named `{0}`")]
    Unknown(String),
    /// What was asked is not valid; the sentence says what.
    #[error("{0}")]
    Invalid(String),
    /// The database or its pool failed.
    #[error(transparent)]
    Control(#[from] ControlError),
}

fn pg(step: &'static str) -> impl Fn(tokio_postgres::Error) -> LoginError {
    move |e| {
        LoginError::Control(ControlError::Pg {
            step,
            error: pg_error(&e),
            detail: describe(&e),
        })
    }
}

// ---- the rules, without a database ----

/// The four permissions of a login (design section 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// May run recipes.
    Execute,
    /// May register recipes.
    Register,
    /// May use the admin operations.
    Admin,
    /// May use SQL mode.
    Sql,
}

impl Permission {
    /// The word used on the command line and in the audit log.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Permission::Execute => "execute",
            Permission::Register => "register",
            Permission::Admin => "admin",
            Permission::Sql => "sql",
        }
    }

    /// Reads the word.
    ///
    /// # Errors
    /// [`LoginError::Invalid`] naming the four words.
    pub fn parse(word: &str) -> Result<Self, LoginError> {
        match word {
            "execute" => Ok(Permission::Execute),
            "register" => Ok(Permission::Register),
            "admin" => Ok(Permission::Admin),
            "sql" => Ok(Permission::Sql),
            other => Err(LoginError::Invalid(format!(
                "`{other}` is not a permission; they are execute, register, admin and sql"
            ))),
        }
    }

    fn slot(self, p: &mut Permissions) -> &mut bool {
        match self {
            Permission::Execute => &mut p.execute,
            Permission::Register => &mut p.register,
            Permission::Admin => &mut p.admin,
            Permission::Sql => &mut p.sql,
        }
    }
}

/// The three limits of a login. Without one, the gateway's own default applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// Requests per second.
    Rate,
    /// Burst.
    Burst,
    /// Connections.
    Connections,
}

impl Limit {
    /// The word used on the command line and in the audit log.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Limit::Rate => "rate",
            Limit::Burst => "burst",
            Limit::Connections => "connections",
        }
    }

    /// Reads the word.
    ///
    /// # Errors
    /// [`LoginError::Invalid`] naming the three words.
    pub fn parse(word: &str) -> Result<Self, LoginError> {
        match word {
            "rate" => Ok(Limit::Rate),
            "burst" => Ok(Limit::Burst),
            "connections" => Ok(Limit::Connections),
            other => Err(LoginError::Invalid(format!(
                "`{other}` is not a limit; they are rate, burst and connections"
            ))),
        }
    }

    fn slot(self, p: &mut Policy) -> &mut Option<u32> {
        match self {
            Limit::Rate => &mut p.rate_per_sec,
            Limit::Burst => &mut p.rate_burst,
            Limit::Connections => &mut p.max_connections,
        }
    }
}

/// What can change about a login after it is made, except its password.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Policy {
    /// The four permissions.
    pub permissions: Permissions,
    /// The recipe allow-list.
    pub recipes: RecipeAllow,
    /// Requests per second, or the gateway's default.
    pub rate_per_sec: Option<u32>,
    /// Burst, or the gateway's default.
    pub rate_burst: Option<u32>,
    /// Connections, or the gateway's default.
    pub max_connections: Option<u32>,
    /// A disabled login fails like a wrong password.
    pub disabled: bool,
}

impl From<&LoginRecord> for Policy {
    fn from(r: &LoginRecord) -> Self {
        Policy {
            permissions: r.permissions,
            recipes: r.recipes.clone(),
            rate_per_sec: r.rate_per_sec,
            rate_burst: r.rate_burst,
            max_connections: r.max_connections,
            disabled: r.disabled,
        }
    }
}

/// One change to a login: the `grant`, `revoke`, `disable` and `enable` of `bhdatactl login`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Give a permission.
    Grant(Permission),
    /// Take a permission away.
    Revoke(Permission),
    /// Add recipe names to the allow-list.
    GrantRecipes(Vec<String>),
    /// Allow every recipe (`*`).
    GrantAllRecipes,
    /// Remove recipe names from the allow-list.
    RevokeRecipes(Vec<String>),
    /// Take `*` away; the login may run no recipe until names are granted.
    RevokeAllRecipes,
    /// Set a limit.
    SetLimit(Limit, u32),
    /// Go back to the gateway's default for a limit.
    ClearLimit(Limit),
    /// Refuse the login from now on.
    Disable,
    /// Accept the login again.
    Enable,
}

/// What a [`Change`] did, as the audit log and the command's answer say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The audit event: one of `bhdata_pg::internal::AUDIT_EVENTS`.
    pub event: &'static str,
    /// One sentence; never a secret.
    pub detail: String,
}

/// `true` if `name` has the shape of a registered recipe name: `[a-z][a-z0-9_]{0,39}`.
#[must_use]
pub fn is_recipe_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 40
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The most a limit may be (the column is a 32-bit integer).
const MAX_LIMIT: u32 = i32::MAX as u32;

fn check_names(names: &[String]) -> Result<(), LoginError> {
    if names.is_empty() {
        return Err(LoginError::Invalid("no recipe name given".to_string()));
    }
    for n in names {
        if !is_recipe_name(n) {
            return Err(LoginError::Invalid(format!(
                "`{n}` is not a recipe name (a lower-case letter, then up to 39 letters, digits or `_`)"
            )));
        }
    }
    Ok(())
}

fn name_set(allow: &RecipeAllow, all_text: &str) -> Result<BTreeSet<String>, LoginError> {
    match allow {
        RecipeAllow::All => Err(LoginError::Invalid(all_text.to_string())),
        RecipeAllow::None => Ok(BTreeSet::new()),
        RecipeAllow::Names(n) => Ok(n.clone()),
    }
}

impl Change {
    /// Applies the change to `policy`.
    ///
    /// # Errors
    /// [`LoginError::Invalid`] for a name that is no recipe name, a limit of 0 or above the column, names
    /// granted or revoked from a login that has `*` (revoke `*` first), or `*` revoked from a login that
    /// does not have it. `policy` is unchanged on an error.
    pub fn apply(&self, policy: &mut Policy) -> Result<Applied, LoginError> {
        let done = |event, detail: String| Ok(Applied { event, detail });
        match self {
            Change::Grant(p) => {
                *p.slot(&mut policy.permissions) = true;
                done("permissions_changed", format!("granted {}", p.name()))
            }
            Change::Revoke(p) => {
                *p.slot(&mut policy.permissions) = false;
                done("permissions_changed", format!("revoked {}", p.name()))
            }
            Change::GrantRecipes(names) => {
                check_names(names)?;
                let mut set = name_set(
                    &policy.recipes,
                    "the login already has every recipe (`*`); revoke `*` before granting names",
                )?;
                set.extend(names.iter().cloned());
                policy.recipes = RecipeAllow::Names(set);
                done(
                    "recipes_changed",
                    format!("granted recipes {}", names.join(", ")),
                )
            }
            Change::GrantAllRecipes => {
                policy.recipes = RecipeAllow::All;
                done("recipes_changed", "granted every recipe (`*`)".to_string())
            }
            Change::RevokeRecipes(names) => {
                check_names(names)?;
                let mut set = name_set(
                    &policy.recipes,
                    "the login has every recipe (`*`), not a list of names; revoke `*` instead",
                )?;
                for n in names {
                    set.remove(n);
                }
                policy.recipes = if set.is_empty() {
                    RecipeAllow::None
                } else {
                    RecipeAllow::Names(set)
                };
                done(
                    "recipes_changed",
                    format!("revoked recipes {}", names.join(", ")),
                )
            }
            Change::RevokeAllRecipes => {
                if policy.recipes != RecipeAllow::All {
                    return Err(LoginError::Invalid(
                        "the login does not have every recipe (`*`); revoke names instead"
                            .to_string(),
                    ));
                }
                policy.recipes = RecipeAllow::None;
                done("recipes_changed", "revoked every recipe (`*`)".to_string())
            }
            Change::SetLimit(limit, value) => {
                if *value == 0 || *value > MAX_LIMIT {
                    return Err(LoginError::Invalid(format!(
                        "the {} limit must be between 1 and {MAX_LIMIT}",
                        limit.name()
                    )));
                }
                *limit.slot(policy) = Some(*value);
                done(
                    "limits_changed",
                    format!("set {} limit to {value}", limit.name()),
                )
            }
            Change::ClearLimit(limit) => {
                *limit.slot(policy) = None;
                done(
                    "limits_changed",
                    format!("{} limit back to the gateway's default", limit.name()),
                )
            }
            Change::Disable => {
                policy.disabled = true;
                done("login_disabled", "disabled".to_string())
            }
            Change::Enable => {
                policy.disabled = false;
                done("login_enabled", "enabled".to_string())
            }
        }
    }
}

/// A row of `logins` as the server sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raw {
    /// `login_id`.
    pub login_id: String,
    /// `salt`.
    pub salt: Vec<u8>,
    /// `iterations`.
    pub iterations: i32,
    /// `stored_key`.
    pub stored_key: Vec<u8>,
    /// `server_key`.
    pub server_key: Vec<u8>,
    /// `can_execute`, `can_register`, `can_admin`, `can_sql`.
    pub can: [bool; 4],
    /// `all_recipes`.
    pub all_recipes: bool,
    /// `recipes`.
    pub recipes: Vec<String>,
    /// `rate_per_sec`.
    pub rate_per_sec: Option<i32>,
    /// `rate_burst`.
    pub rate_burst: Option<i32>,
    /// `max_connections`.
    pub max_connections: Option<i32>,
    /// `disabled`.
    pub disabled: bool,
}

/// The columns [`Raw`] reads, in order.
const COLUMNS: &str =
    "login_id, salt, iterations, stored_key, server_key, can_execute, can_register, \
                       can_admin, can_sql, all_recipes, recipes, rate_per_sec, rate_burst, \
                       max_connections, disabled";

fn bad(what: impl std::fmt::Display) -> ControlError {
    ControlError::Inconsistent(format!("a stored login: {what}"))
}

fn key(bytes: Vec<u8>, what: &str) -> Result<[u8; 32], ControlError> {
    bytes
        .try_into()
        .map_err(|_| bad(format!("{what} is not 32 bytes")))
}

fn limit(value: Option<i32>, what: &str) -> Result<Option<u32>, ControlError> {
    value
        .map(|v| u32::try_from(v).map_err(|_| bad(format!("{what} is negative"))))
        .transpose()
}

impl Raw {
    fn from_row(row: &tokio_postgres::Row) -> Self {
        Raw {
            login_id: row.get(0),
            salt: row.get(1),
            iterations: row.get(2),
            stored_key: row.get(3),
            server_key: row.get(4),
            can: [row.get(5), row.get(6), row.get(7), row.get(8)],
            all_recipes: row.get(9),
            recipes: row.get(10),
            rate_per_sec: row.get(11),
            rate_burst: row.get(12),
            max_connections: row.get(13),
            disabled: row.get(14),
        }
    }

    /// The row as the gateway's record.
    ///
    /// # Errors
    /// [`ControlError::Inconsistent`] for a row the table's checks should have refused.
    pub fn into_record(self) -> Result<LoginRecord, ControlError> {
        let login_id = LoginId::new(self.login_id.clone())
            .map_err(|_| bad(format!("`{}` is not a login id", self.login_id)))?;
        let recipes = if self.all_recipes {
            RecipeAllow::All
        } else if self.recipes.is_empty() {
            RecipeAllow::None
        } else {
            RecipeAllow::Names(self.recipes.into_iter().collect())
        };
        Ok(LoginRecord {
            login_id,
            verifier: ScramVerifier {
                salt: self.salt,
                iterations: u32::try_from(self.iterations)
                    .map_err(|_| bad("iterations is negative"))?,
                stored_key: key(self.stored_key, "stored_key")?,
                server_key: key(self.server_key, "server_key")?,
            },
            permissions: Permissions {
                execute: self.can[0],
                register: self.can[1],
                admin: self.can[2],
                sql: self.can[3],
            },
            recipes,
            rate_per_sec: limit(self.rate_per_sec, "rate_per_sec")?,
            rate_burst: limit(self.rate_burst, "rate_burst")?,
            max_connections: limit(self.max_connections, "max_connections")?,
            disabled: self.disabled,
        })
    }
}

/// The two columns the allow-list is stored in.
fn recipe_columns(allow: &RecipeAllow) -> (bool, Vec<String>) {
    match allow {
        RecipeAllow::None => (false, Vec::new()),
        RecipeAllow::All => (true, Vec::new()),
        RecipeAllow::Names(names) => (false, names.iter().cloned().collect()),
    }
}

fn to_i32(v: Option<u32>) -> Option<i32> {
    v.and_then(|v| i32::try_from(v).ok())
}

fn check_actor(by: &str) -> Result<(), LoginError> {
    if by.is_empty() || by.len() > MAX_LOGIN_LEN {
        return Err(LoginError::Invalid(format!(
            "the name of who runs the command must be 1 to {MAX_LOGIN_LEN} bytes"
        )));
    }
    Ok(())
}

fn login_id(name: &str) -> Result<LoginId, LoginError> {
    LoginId::new(name).map_err(|e| LoginError::Invalid(e.to_string()))
}

fn derive(password: &[u8], iterations: u32) -> Result<Verifier, LoginError> {
    if password.is_empty() {
        return Err(LoginError::Invalid("the password is empty".to_string()));
    }
    Verifier::generate(password, iterations)
        .map_err(|e| LoginError::Invalid(format!("cannot make a verifier: {e}")))
}

// ---- the calls ----

/// Makes a login with no permission, no recipe, no limit of its own, and the verifier of `password`.
/// `iterations` is the SCRAM count (`gateway.scram_iterations`). `by` is who ran the command.
///
/// # Errors
/// [`LoginError::Invalid`] for a name that is no login id, an empty password, a count the verifier
/// refuses, or an empty `by`; [`LoginError::Exists`]; [`LoginError::Control`].
pub async fn create(
    client: &mut Client,
    name: &str,
    password: &[u8],
    iterations: u32,
    by: &str,
) -> Result<(), LoginError> {
    let id = login_id(name)?;
    check_actor(by)?;
    let v = derive(password, iterations)?;
    let tx = client.transaction().await.map_err(pg("begin"))?;
    let inserted = tx
        .execute(
            "INSERT INTO bhdata_control.logins \
               (login_id, salt, iterations, stored_key, server_key, created_by) \
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &id.as_str(),
                &v.salt,
                &i32::try_from(v.iterations).unwrap_or(i32::MAX),
                &v.stored_key.as_slice(),
                &v.server_key.as_slice(),
                &by,
            ],
        )
        .await;
    match inserted {
        Ok(_) => {}
        Err(e) if e.code() == Some(&SqlState::UNIQUE_VIOLATION) => {
            return Err(LoginError::Exists(name.to_owned()));
        }
        Err(e) => return Err(pg("create login")(e)),
    }
    audit::record_change(&tx, "login_created", id.as_str(), by, "created").await?;
    tx.commit().await.map_err(pg("commit"))?;
    Ok(())
}

/// Replaces the verifier of a login. Open connections stay open; the next login needs the new password.
///
/// # Errors
/// As [`create`], and [`LoginError::Unknown`] for a name that has no login.
pub async fn set_password(
    client: &mut Client,
    name: &str,
    password: &[u8],
    iterations: u32,
    by: &str,
) -> Result<(), LoginError> {
    let id = login_id(name)?;
    check_actor(by)?;
    let v = derive(password, iterations)?;
    let tx = client.transaction().await.map_err(pg("begin"))?;
    let n = tx
        .execute(
            "UPDATE bhdata_control.logins \
             SET salt = $2, iterations = $3, stored_key = $4, server_key = $5, updated_at = now() \
             WHERE login_id = $1",
            &[
                &id.as_str(),
                &v.salt,
                &i32::try_from(v.iterations).unwrap_or(i32::MAX),
                &v.stored_key.as_slice(),
                &v.server_key.as_slice(),
            ],
        )
        .await
        .map_err(pg("set password"))?;
    if n == 0 {
        return Err(LoginError::Unknown(name.to_owned()));
    }
    audit::record_change(
        &tx,
        "password_changed",
        id.as_str(),
        by,
        "password replaced",
    )
    .await?;
    tx.commit().await.map_err(pg("commit"))?;
    Ok(())
}

/// Applies one [`Change`] to a login and writes its audit row, in one transaction. The row is locked
/// while the change is worked out, so two changes at once both take effect.
///
/// # Errors
/// [`LoginError::Unknown`], [`LoginError::Invalid`] (see [`Change::apply`]), [`LoginError::Control`].
pub async fn change(
    client: &mut Client,
    name: &str,
    what: &Change,
    by: &str,
) -> Result<Applied, LoginError> {
    let id = login_id(name)?;
    check_actor(by)?;
    let tx = client.transaction().await.map_err(pg("begin"))?;
    let row = tx
        .query_opt(
            &format!("SELECT {COLUMNS} FROM bhdata_control.logins WHERE login_id = $1 FOR UPDATE"),
            &[&id.as_str()],
        )
        .await
        .map_err(pg("read login"))?
        .ok_or_else(|| LoginError::Unknown(name.to_owned()))?;
    let record = Raw::from_row(&row).into_record()?;
    let mut policy = Policy::from(&record);
    let applied = what.apply(&mut policy)?;
    let (all_recipes, recipes) = recipe_columns(&policy.recipes);
    tx.execute(
        "UPDATE bhdata_control.logins SET can_execute = $2, can_register = $3, can_admin = $4, \
           can_sql = $5, all_recipes = $6, recipes = $7, rate_per_sec = $8, rate_burst = $9, \
           max_connections = $10, disabled = $11, updated_at = now() \
         WHERE login_id = $1",
        &[
            &id.as_str(),
            &policy.permissions.execute,
            &policy.permissions.register,
            &policy.permissions.admin,
            &policy.permissions.sql,
            &all_recipes,
            &recipes,
            &to_i32(policy.rate_per_sec),
            &to_i32(policy.rate_burst),
            &to_i32(policy.max_connections),
            &policy.disabled,
        ],
    )
    .await
    .map_err(pg("change login"))?;
    audit::record_change(&tx, applied.event, id.as_str(), by, &applied.detail).await?;
    tx.commit().await.map_err(pg("commit"))?;
    Ok(applied)
}

/// The login of that name, or `None`. `name` may be anything a client sent: one that cannot be a login
/// id is answered without asking the server.
///
/// # Errors
/// [`ControlError::Pg`], or [`ControlError::Inconsistent`] for a row the table's checks should have
/// refused.
pub async fn find<C: GenericClient>(
    client: &C,
    name: &str,
) -> Result<Option<LoginRecord>, ControlError> {
    if LoginId::new(name).is_err() {
        return Ok(None);
    }
    let row = client
        .query_opt(
            &format!("SELECT {COLUMNS} FROM bhdata_control.logins WHERE login_id = $1"),
            &[&name],
        )
        .await
        .map_err(|e| ControlError::Pg {
            step: "find login",
            error: pg_error(&e),
            detail: describe(&e),
        })?;
    row.map(|r| Raw::from_row(&r).into_record()).transpose()
}

/// Every login, by name.
///
/// # Errors
/// As [`find`].
pub async fn list<C: GenericClient>(client: &C) -> Result<Vec<LoginRecord>, ControlError> {
    let rows = client
        .query(
            &format!("SELECT {COLUMNS} FROM bhdata_control.logins ORDER BY login_id"),
            &[],
        )
        .await
        .map_err(|e| ControlError::Pg {
            step: "list logins",
            error: pg_error(&e),
            detail: describe(&e),
        })?;
    rows.iter()
        .map(|r| Raw::from_row(r).into_record())
        .collect()
}

/// One line of `bhdatactl login list` for a record.
#[must_use]
pub fn describe_login(r: &LoginRecord) -> String {
    let p = r.permissions;
    let perms: Vec<&str> = [
        (p.execute, "execute"),
        (p.register, "register"),
        (p.admin, "admin"),
        (p.sql, "sql"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    let recipes = match &r.recipes {
        RecipeAllow::None => "none".to_string(),
        RecipeAllow::All => "*".to_string(),
        RecipeAllow::Names(n) => n.iter().cloned().collect::<Vec<_>>().join(","),
    };
    let show = |v: Option<u32>| v.map_or_else(|| "default".to_string(), |v| v.to_string());
    format!(
        "{}  {}  permissions {}  recipes {}  rate {}  burst {}  connections {}",
        r.login_id,
        if r.disabled { "disabled" } else { "enabled" },
        if perms.is_empty() {
            "none".to_string()
        } else {
            perms.join(",")
        },
        recipes,
        show(r.rate_per_sec),
        show(r.rate_burst),
        show(r.max_connections),
    )
}

/// What the gateway calls: logins and audit rows through the admin pool.
pub struct PgLoginStore {
    pools: Arc<Pools>,
}

impl PgLoginStore {
    /// A store that takes its connections from `pools` (only the admin pool is used).
    #[must_use]
    pub fn new(pools: Arc<Pools>) -> Self {
        PgLoginStore { pools }
    }
}

fn internal(what: &str, e: &impl std::fmt::Display) -> Error {
    Error::Internal(format!("{what}: {e}"))
}

impl LoginStore for PgLoginStore {
    async fn find(&self, name: &str) -> Result<Option<LoginRecord>, Error> {
        let conn = self
            .pools
            .admin()
            .await
            .map_err(|e| internal("login store", &e))?;
        find(&**conn, name)
            .await
            .map_err(|e| internal("login store", &e))
    }

    async fn audit(&self, event: AuditEvent) -> Result<(), Error> {
        let conn = self
            .pools
            .admin()
            .await
            .map_err(|e| internal("audit log", &e))?;
        audit::record_event(&**conn, &event)
            .await
            .map_err(|e| internal("audit log", &e))
    }
}

/// The commands on a private runtime, for `bhdatactl`: each opens one admin connection, does its work
/// and closes it again, as [`crate::blocking`] does for the registry.
pub mod blocking {
    use std::future::Future;

    use bhdata_api::login::LoginRecord;
    use bhdata_pg::pool::{Pools, Target};
    use bhdata_types::config::PoolConfig;
    use tokio_postgres::Client;

    use super::{Applied, Change, LoginError};
    use crate::audit::{self, AuditRow};
    use crate::error::ControlError;

    fn admin_only() -> PoolConfig {
        PoolConfig {
            recipe_writers: 0,
            readers: 0,
            sql_sessions: 0,
            admin: 1,
            reserved_backup: 0,
        }
    }

    fn block_on<F: Future>(future: F) -> Result<F::Output, LoginError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LoginError::Control(ControlError::Runtime(e)))?;
        Ok(rt.block_on(future))
    }

    async fn connected(target: &Target) -> Result<Pools, LoginError> {
        Ok(Pools::connect(target, &admin_only())
            .await
            .map_err(ControlError::from)?)
    }

    /// [`super::create`] with its own connection.
    ///
    /// # Errors
    /// As [`super::create`], or when no connection can be made.
    pub fn create(
        target: &Target,
        name: &str,
        password: &[u8],
        iterations: u32,
        by: &str,
    ) -> Result<(), LoginError> {
        block_on(async {
            let pools = connected(target).await?;
            let mut conn = pools.admin().await.map_err(ControlError::from)?;
            let client: &mut Client = &mut conn;
            let result = super::create(client, name, password, iterations, by).await;
            drop(conn);
            pools.close();
            result
        })?
    }

    /// [`super::set_password`] with its own connection.
    ///
    /// # Errors
    /// As [`super::set_password`], or when no connection can be made.
    pub fn set_password(
        target: &Target,
        name: &str,
        password: &[u8],
        iterations: u32,
        by: &str,
    ) -> Result<(), LoginError> {
        block_on(async {
            let pools = connected(target).await?;
            let mut conn = pools.admin().await.map_err(ControlError::from)?;
            let client: &mut Client = &mut conn;
            let result = super::set_password(client, name, password, iterations, by).await;
            drop(conn);
            pools.close();
            result
        })?
    }

    /// [`super::change`] with its own connection.
    ///
    /// # Errors
    /// As [`super::change`], or when no connection can be made.
    pub fn change(
        target: &Target,
        name: &str,
        what: &Change,
        by: &str,
    ) -> Result<Applied, LoginError> {
        block_on(async {
            let pools = connected(target).await?;
            let mut conn = pools.admin().await.map_err(ControlError::from)?;
            let client: &mut Client = &mut conn;
            let result = super::change(client, name, what, by).await;
            drop(conn);
            pools.close();
            result
        })?
    }

    /// [`super::list`] with its own connection.
    ///
    /// # Errors
    /// As [`super::list`], or when no connection can be made.
    pub fn list(target: &Target) -> Result<Vec<LoginRecord>, LoginError> {
        block_on(async {
            let pools = connected(target).await?;
            let conn = pools.admin().await.map_err(ControlError::from)?;
            let result = super::list(&**conn).await.map_err(LoginError::from);
            drop(conn);
            pools.close();
            result
        })?
    }

    /// [`audit::list`] with its own connection.
    ///
    /// # Errors
    /// As [`audit::list`], or when no connection can be made.
    pub fn audit_list(
        target: &Target,
        name: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AuditRow>, LoginError> {
        block_on(async {
            let pools = connected(target).await?;
            let conn = pools.admin().await.map_err(ControlError::from)?;
            let result = audit::list(&**conn, name, limit)
                .await
                .map_err(LoginError::from);
            drop(conn);
            pools.close();
            result
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn apply(policy: &mut Policy, c: Change) -> Applied {
        c.apply(policy).expect("applies")
    }

    #[test]
    fn a_grant_and_a_revoke_flip_one_permission_and_name_it_in_the_audit_event() {
        let mut p = Policy::default();
        let a = apply(&mut p, Change::Grant(Permission::Execute));
        assert!(p.permissions.execute && !p.permissions.admin);
        assert_eq!(
            (a.event, a.detail.as_str()),
            ("permissions_changed", "granted execute")
        );
        let a = apply(&mut p, Change::Revoke(Permission::Execute));
        assert!(!p.permissions.execute);
        assert_eq!(a.detail, "revoked execute");
    }

    #[test]
    fn recipe_names_are_added_removed_and_an_empty_list_is_none() {
        let mut p = Policy::default();
        apply(&mut p, Change::GrantRecipes(names(&["spend", "narrow"])));
        apply(&mut p, Change::GrantRecipes(names(&["spend", "star"])));
        assert_eq!(
            p.recipes,
            RecipeAllow::Names(
                ["narrow", "spend", "star"]
                    .map(String::from)
                    .into_iter()
                    .collect()
            )
        );
        apply(
            &mut p,
            Change::RevokeRecipes(names(&["narrow", "star", "never_there"])),
        );
        assert!(p.recipes.permits("spend") && !p.recipes.permits("narrow"));
        apply(&mut p, Change::RevokeRecipes(names(&["spend"])));
        assert_eq!(p.recipes, RecipeAllow::None);
    }

    #[test]
    fn star_and_names_do_not_mix_and_nothing_changes_on_a_refusal() {
        let mut p = Policy::default();
        apply(&mut p, Change::GrantAllRecipes);
        assert_eq!(p.recipes, RecipeAllow::All);
        let before = p.clone();
        assert!(Change::GrantRecipes(names(&["spend"]))
            .apply(&mut p)
            .is_err());
        assert!(Change::RevokeRecipes(names(&["spend"]))
            .apply(&mut p)
            .is_err());
        assert_eq!(p, before);
        apply(&mut p, Change::RevokeAllRecipes);
        assert_eq!(p.recipes, RecipeAllow::None);
        // `*` cannot be revoked from a login that does not have it.
        assert!(Change::RevokeAllRecipes.apply(&mut p).is_err());
    }

    #[test]
    fn a_recipe_name_has_the_registered_shape() {
        for ok in ["a", "spend", "a_1", &"a".repeat(40)] {
            assert!(is_recipe_name(ok), "{ok}");
        }
        for bad in ["", "Spend", "1a", "_a", "a-b", "a b", &"a".repeat(41), "*"] {
            assert!(!is_recipe_name(bad), "{bad}");
        }
        let mut p = Policy::default();
        assert!(Change::GrantRecipes(names(&["Bad"])).apply(&mut p).is_err());
        assert!(Change::GrantRecipes(vec![]).apply(&mut p).is_err());
        assert_eq!(p, Policy::default());
    }

    #[test]
    fn limits_are_set_cleared_and_kept_inside_the_column() {
        let mut p = Policy::default();
        apply(&mut p, Change::SetLimit(Limit::Rate, 50));
        apply(&mut p, Change::SetLimit(Limit::Burst, 100));
        apply(&mut p, Change::SetLimit(Limit::Connections, 4));
        assert_eq!(
            (p.rate_per_sec, p.rate_burst, p.max_connections),
            (Some(50), Some(100), Some(4))
        );
        apply(&mut p, Change::ClearLimit(Limit::Burst));
        assert_eq!(p.rate_burst, None);
        assert!(Change::SetLimit(Limit::Rate, 0).apply(&mut p).is_err());
        assert!(Change::SetLimit(Limit::Rate, u32::MAX)
            .apply(&mut p)
            .is_err());
        assert_eq!(p.rate_per_sec, Some(50));
    }

    #[test]
    fn disable_and_enable_are_their_own_events() {
        let mut p = Policy::default();
        assert_eq!(apply(&mut p, Change::Disable).event, "login_disabled");
        assert!(p.disabled);
        assert_eq!(apply(&mut p, Change::Enable).event, "login_enabled");
        assert!(!p.disabled);
    }

    #[test]
    fn every_event_a_change_makes_is_one_the_table_accepts() {
        let mut p = Policy::default();
        let all = [
            Change::Grant(Permission::Sql),
            Change::GrantRecipes(names(&["a"])),
            Change::RevokeRecipes(names(&["a"])),
            Change::GrantAllRecipes,
            Change::RevokeAllRecipes,
            Change::SetLimit(Limit::Rate, 1),
            Change::ClearLimit(Limit::Rate),
            Change::Disable,
            Change::Enable,
        ];
        for c in all {
            let a = apply(&mut p, c);
            assert!(
                bhdata_pg::internal::AUDIT_EVENTS.contains(&a.event),
                "{}",
                a.event
            );
        }
    }

    #[test]
    fn the_words_of_the_command_line_are_read_and_others_name_the_choices() {
        assert_eq!(Permission::parse("execute").unwrap(), Permission::Execute);
        assert_eq!(Limit::parse("connections").unwrap(), Limit::Connections);
        let e = Permission::parse("root").unwrap_err().to_string();
        assert!(e.contains("execute, register, admin and sql"), "{e}");
        let e = Limit::parse("speed").unwrap_err().to_string();
        assert!(e.contains("rate, burst and connections"), "{e}");
    }

    fn raw() -> Raw {
        Raw {
            login_id: "alice".into(),
            salt: vec![1; 16],
            iterations: 4096,
            stored_key: vec![2; 32],
            server_key: vec![3; 32],
            can: [true, false, false, true],
            all_recipes: false,
            recipes: names(&["spend", "star"]),
            rate_per_sec: Some(10),
            rate_burst: None,
            max_connections: Some(2),
            disabled: true,
        }
    }

    #[test]
    fn a_stored_row_becomes_the_record_the_gateway_reads() {
        let r = raw().into_record().unwrap();
        assert_eq!(r.login_id.as_str(), "alice");
        assert_eq!(
            (
                r.verifier.iterations,
                r.verifier.stored_key,
                r.verifier.server_key
            ),
            (4096, [2; 32], [3; 32])
        );
        assert_eq!(
            r.permissions,
            Permissions {
                execute: true,
                register: false,
                admin: false,
                sql: true
            }
        );
        assert!(r.recipes.permits("spend") && !r.recipes.permits("narrow"));
        assert_eq!(
            (r.rate_per_sec, r.rate_burst, r.max_connections),
            (Some(10), None, Some(2))
        );
        assert!(r.disabled);
        // `*` wins, and an empty list is none.
        let mut all = raw();
        all.all_recipes = true;
        all.recipes = vec![];
        assert_eq!(all.into_record().unwrap().recipes, RecipeAllow::All);
        let mut none = raw();
        none.recipes = vec![];
        assert_eq!(none.into_record().unwrap().recipes, RecipeAllow::None);
    }

    #[test]
    fn a_row_the_checks_should_have_refused_is_inconsistent_not_a_panic() {
        for mutate in [
            (|r: &mut Raw| r.login_id = "bad name".into()) as fn(&mut Raw),
            |r| r.stored_key = vec![0; 31],
            |r| r.server_key = vec![],
            |r| r.iterations = -1,
            |r| r.rate_per_sec = Some(-5),
        ] {
            let mut r = raw();
            mutate(&mut r);
            assert!(matches!(
                r.into_record(),
                Err(ControlError::Inconsistent(_))
            ));
        }
    }

    #[test]
    fn a_policy_round_trips_through_the_two_recipe_columns() {
        for allow in [
            RecipeAllow::None,
            RecipeAllow::All,
            RecipeAllow::Names(["a".to_string(), "b".to_string()].into_iter().collect()),
        ] {
            let (all, list) = recipe_columns(&allow);
            let mut r = raw();
            r.all_recipes = all;
            r.recipes = list;
            assert_eq!(r.into_record().unwrap().recipes, allow);
        }
    }

    #[test]
    fn the_listing_line_shows_permissions_recipes_and_limits_and_no_secret() {
        let line = describe_login(&raw().into_record().unwrap());
        assert_eq!(
            line,
            "alice  disabled  permissions execute,sql  recipes spend,star  rate 10  burst default  connections 2"
        );
        assert!(!line.contains("salt") && !line.contains("key"));
    }

    #[test]
    fn a_login_made_without_a_password_or_with_a_bad_name_is_refused_before_the_server() {
        assert!(derive(b"", 4096).is_err());
        assert!(derive(b"pw", 0).is_err());
        assert!(login_id("has space").is_err());
        assert!(check_actor("").is_err());
        assert!(check_actor(&"x".repeat(MAX_LOGIN_LEN + 1)).is_err());
        assert!(derive(b"pw", 4096).is_ok());
    }
}
