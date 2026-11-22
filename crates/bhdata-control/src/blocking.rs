//! The registry calls on a private runtime, for `bhdatactl` (spec `recipe-compiler.md`, section
//! 5: at M3 the commands run as the local admin only). Each call opens one admin connection, does
//! its work and closes it again; the server itself will hold the admin pool and call
//! [`crate::registry`] directly.

use std::future::Future;

use bhdata_pg::pool::{Pools, Target};
use bhdata_recipe::Limits;
use bhdata_types::config::PoolConfig;
use tokio_postgres::Client;

use crate::error::ControlError;
use crate::registry::{self, RecipeRow, RegisterOutcome, SchemaOutcome, StoredRecipe};

/// One admin connection and nothing else.
fn admin_only() -> PoolConfig {
    PoolConfig {
        recipe_writers: 0,
        readers: 0,
        sql_sessions: 0,
        admin: 1,
        reserved_backup: 0,
    }
}

fn block_on<F: Future>(future: F) -> Result<F::Output, ControlError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(ControlError::Runtime)?;
    Ok(rt.block_on(future))
}

async fn connected(target: &Target) -> Result<Pools, ControlError> {
    Ok(Pools::connect(target, &admin_only()).await?)
}

/// [`registry::apply_schema`] with its own connection.
///
/// # Errors
/// As [`registry::apply_schema`], or when no connection can be made.
pub fn apply_schema(target: &Target, text: &str, by: &str) -> Result<SchemaOutcome, ControlError> {
    block_on(async {
        let pools = connected(target).await?;
        let mut conn = pools.admin().await?;
        let client: &mut Client = &mut conn;
        let result = registry::apply_schema(client, text, by).await;
        drop(conn);
        pools.close();
        result
    })?
}

/// [`registry::register_recipe`] with its own connection.
///
/// # Errors
/// As [`registry::register_recipe`], or when no connection can be made.
pub fn register_recipe(
    target: &Target,
    text: &str,
    limits: &Limits,
    by: &str,
) -> Result<RegisterOutcome, ControlError> {
    block_on(async {
        let pools = connected(target).await?;
        let mut conn = pools.admin().await?;
        let client: &mut Client = &mut conn;
        let result = registry::register_recipe(client, text, limits, by).await;
        drop(conn);
        pools.close();
        result
    })?
}

/// [`registry::list`] with its own connection.
///
/// # Errors
/// As [`registry::list`], or when no connection can be made.
pub fn list(target: &Target) -> Result<Vec<RecipeRow>, ControlError> {
    block_on(async {
        let pools = connected(target).await?;
        let conn = pools.admin().await?;
        let result = registry::list(&**conn).await;
        drop(conn);
        pools.close();
        result
    })?
}

/// [`registry::get`] with its own connection.
///
/// # Errors
/// As [`registry::get`], or when no connection can be made.
pub fn get(target: &Target, name: &str, version: u32) -> Result<StoredRecipe, ControlError> {
    block_on(async {
        let pools = connected(target).await?;
        let conn = pools.admin().await?;
        let result = registry::get(&**conn, name, version).await;
        drop(conn);
        pools.close();
        result
    })?
}

/// [`registry::latest`] with its own connection.
///
/// # Errors
/// As [`registry::latest`], or when no connection can be made.
pub fn latest(target: &Target, name: &str) -> Result<StoredRecipe, ControlError> {
    block_on(async {
        let pools = connected(target).await?;
        let conn = pools.admin().await?;
        let result = registry::latest(&**conn, name).await;
        drop(conn);
        pools.close();
        result
    })?
}

/// [`registry::reapply_grants`] with its own connection: the recipe role ends up with the union of
/// the grants of every registered version and nothing else. Returns how many statements ran.
///
/// # Errors
/// As [`registry::reapply_grants`], or when no connection can be made.
pub fn reapply_grants(target: &Target) -> Result<usize, ControlError> {
    block_on(async {
        let pools = connected(target).await?;
        let mut conn = pools.admin().await?;
        let client: &mut Client = &mut conn;
        let result = match client.transaction().await {
            Ok(tx) => match registry::reapply_grants(&tx).await {
                Ok(n) => tx.commit().await.map(|()| n).map_err(|e| ControlError::Pg {
                    step: "commit",
                    error: bhdata_pg::pool::pg_error(&e),
                    detail: bhdata_pg::pool::describe(&e),
                }),
                Err(e) => Err(e),
            },
            Err(e) => Err(ControlError::Pg {
                step: "begin",
                error: bhdata_pg::pool::pg_error(&e),
                detail: bhdata_pg::pool::describe(&e),
            }),
        };
        drop(conn);
        pools.close();
        result
    })?
}
