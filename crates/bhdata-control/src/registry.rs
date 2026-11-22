//! The registry (spec `recipe-compiler.md`, section 5): applying a schema, registering a recipe,
//! and reading both back.
//!
//! The rules that decide what happens are plain functions ([`decide_schema`],
//! [`decide_registration`], [`schema_hash`]), tested without a database. The calls that talk to
//! the server wrap them in one transaction each, run as the admin role:
//!
//! * [`apply_schema`]: takes a lock, reads the latest applied schema, and either finds nothing to
//!   do, applies only additive changes together with the new `schema_versions` row, or refuses the
//!   whole change when it holds a destructive one (nothing is applied then).
//! * [`register_recipe`]: parses and compiles the recipe against the latest applied schema, takes a
//!   lock on the recipe's name, and either finds the same plan as the latest version
//!   ("unchanged") or inserts the next version; in both cases it then re-applies the grants
//!   ([`reapply_grants`]), inside the same transaction.
//! * [`reapply_grants`]: gives the recipe role the union of the grants of every registered
//!   version, and nothing else. It rebuilds each plan from the stored source and the schema it was
//!   compiled against, and refuses to go on if a plan no longer hashes to what was stored.
//!
//! A registered row is never changed: the admin role may insert rows and change only a recipe's
//! `status` (see `bhdata_pg::internal`). Plans are stored as the exact canonical text.

use std::collections::BTreeMap;

use bhdata_pg::pool::{describe, pg_error};
use bhdata_pg::roles::{Role, SCHEMA_APP, SCHEMA_CONTROL};
use bhdata_recipe::canon::sha256_hex;
use bhdata_recipe::plan::Grants;
use bhdata_recipe::{compile, parse_recipe, Limits};
use bhdata_schema::ddl::{create_schema, statements_for};
use bhdata_schema::diff::diff;
use bhdata_schema::model::Schema;
use bhdata_schema::parse_schema;
use tokio_postgres::{Client, GenericClient};

use crate::error::ControlError;

/// Limits that accept everything. A limit is checked when a recipe is registered; a version that
/// is already registered keeps working when a limit is lowered later, so it is rebuilt without.
pub const UNLIMITED: Limits = Limits {
    max_steps: usize::MAX,
    max_list_limit: u32::MAX,
    max_result_values: usize::MAX,
    max_result_bytes: usize::MAX,
};

fn pg(step: &'static str) -> impl Fn(tokio_postgres::Error) -> ControlError {
    move |e| ControlError::Pg {
        step,
        error: pg_error(&e),
        detail: describe(&e),
    }
}

// ---- the rules, without a database ----

/// SHA-256 of a schema's statements, with the tables in name order, so the order of the file never
/// matters. 64 lower-case hex digits.
#[must_use]
pub fn schema_hash(schema: &Schema) -> String {
    let mut sorted = schema.clone();
    sorted.tables.sort_by(|a, b| a.name.cmp(&b.name));
    sha256_hex(&create_schema(&sorted).join(";\n"))
}

/// What applying a schema does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaDecision {
    /// The schema applied last is the same.
    Unchanged,
    /// Only additive changes: run these statements and record a new version.
    Apply {
        /// The statements, in order.
        statements: Vec<String>,
        /// One line per change, for the person who ran the command.
        changes: Vec<String>,
    },
    /// At least one change needs an explicit admin action: nothing is applied.
    Refuse {
        /// One line per destructive change.
        destructive: Vec<String>,
    },
}

/// Decides what to do for `new`, given the schema applied last (`None`: nothing applied yet).
#[must_use]
pub fn decide_schema(old: Option<&Schema>, new: &Schema) -> SchemaDecision {
    let empty = Schema { tables: Vec::new() };
    let d = diff(old.unwrap_or(&empty), new);
    if old.is_some() && d.is_empty() {
        return SchemaDecision::Unchanged;
    }
    let destructive: Vec<String> = d.destructive().map(|c| c.describe()).collect();
    if !destructive.is_empty() {
        return SchemaDecision::Refuse { destructive };
    }
    let mut statements = Vec::new();
    for change in d.additive() {
        statements.extend(statements_for(change).unwrap_or_default());
    }
    SchemaDecision::Apply {
        statements,
        changes: d.additive().map(|c| c.describe()).collect(),
    }
}

/// The newest registered version of a recipe name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRecipe {
    /// Its number.
    pub version: u32,
    /// Its plan's hash.
    pub content_hash: String,
    /// Whether it is active (not retired).
    pub active: bool,
}

/// What registering a plan does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationDecision {
    /// The latest version is active and has this very plan: nothing new.
    Unchanged {
        /// That version.
        version: u32,
    },
    /// A new version.
    New {
        /// Its number: one more than the latest, or 1.
        version: u32,
    },
}

/// Decides by comparing `hash` with the latest version only. Content equal to an older version
/// but not to the latest is a new version: the latest is what callers get, so going back to old
/// content is a change like any other.
#[must_use]
pub fn decide_registration(latest: Option<&LatestRecipe>, hash: &str) -> RegistrationDecision {
    match latest {
        Some(l) if l.active && l.content_hash == hash => {
            RegistrationDecision::Unchanged { version: l.version }
        }
        Some(l) => RegistrationDecision::New {
            version: l.version + 1,
        },
        None => RegistrationDecision::New { version: 1 },
    }
}

// ---- reading ----

/// A schema version as the registry holds it, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedSchema {
    /// Its number, from 1.
    pub number: u32,
    /// See [`schema_hash`].
    pub hash: String,
    /// The file as it was applied.
    pub source: String,
    /// The parsed schema.
    pub schema: Schema,
}

fn number(value: i32, what: &str) -> Result<u32, ControlError> {
    u32::try_from(value)
        .map_err(|_| ControlError::Inconsistent(format!("{what} {value} is not a valid number")))
}

fn parse_stored_schema(n: u32, source: &str) -> Result<Schema, ControlError> {
    parse_schema(source).map_err(|e| {
        ControlError::Inconsistent(format!("stored schema version {n} no longer parses: {e}"))
    })
}

/// The schema applied last; `None` before the first.
///
/// # Errors
/// A refused query, or stored text that no longer parses.
pub async fn current_schema(c: &impl GenericClient) -> Result<Option<AppliedSchema>, ControlError> {
    let sql = format!(
        "SELECT number, hash, source FROM {SCHEMA_CONTROL}.schema_versions \
         ORDER BY number DESC LIMIT 1"
    );
    let Some(row) = c
        .query_opt(&sql, &[])
        .await
        .map_err(pg("read the schema"))?
    else {
        return Ok(None);
    };
    let n = number(row.get(0), "schema version")?;
    let source: String = row.get(2);
    Ok(Some(AppliedSchema {
        number: n,
        hash: row.get(1),
        schema: parse_stored_schema(n, &source)?,
        source,
    }))
}

/// One registered recipe version, without its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeRow {
    /// The recipe's name.
    pub name: String,
    /// The version, from 1.
    pub version: u32,
    /// The plan's hash.
    pub content_hash: String,
    /// The schema version it was compiled against.
    pub schema_version: u32,
    /// `active` or `retired`.
    pub status: String,
    /// When it was registered, UTC, like `2026-11-22T15:10:00Z`.
    pub registered_at: String,
    /// Who registered it.
    pub registered_by: String,
}

/// A registered version with its source and its plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRecipe {
    /// The row.
    pub row: RecipeRow,
    /// The recipe file as it was registered.
    pub source: String,
    /// The plan, as the exact canonical JSON text that was hashed.
    pub plan_json: String,
}

const ROW_COLUMNS: &str = "name, version, content_hash, schema_version, status, \
     to_char(registered_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), registered_by";

fn recipe_row(row: &tokio_postgres::Row) -> Result<RecipeRow, ControlError> {
    Ok(RecipeRow {
        name: row.get(0),
        version: number(row.get(1), "recipe version")?,
        content_hash: row.get(2),
        schema_version: number(row.get(3), "schema version")?,
        status: row.get(4),
        registered_at: row.get(5),
        registered_by: row.get(6),
    })
}

fn stored_recipe(row: &tokio_postgres::Row) -> Result<StoredRecipe, ControlError> {
    Ok(StoredRecipe {
        row: recipe_row(row)?,
        source: row.get(7),
        plan_json: row.get(8),
    })
}

/// Every registered version, by name and then version.
///
/// # Errors
/// A refused query.
pub async fn list(c: &impl GenericClient) -> Result<Vec<RecipeRow>, ControlError> {
    let sql = format!("SELECT {ROW_COLUMNS} FROM {SCHEMA_CONTROL}.recipes ORDER BY name, version");
    let rows = c.query(&sql, &[]).await.map_err(pg("list recipes"))?;
    rows.iter().map(recipe_row).collect()
}

/// One version of a recipe.
///
/// # Errors
/// [`ControlError::UnknownRecipe`] when there is none.
pub async fn get(
    c: &impl GenericClient,
    name: &str,
    version: u32,
) -> Result<StoredRecipe, ControlError> {
    let sql = format!(
        "SELECT {ROW_COLUMNS}, source, plan FROM {SCHEMA_CONTROL}.recipes \
         WHERE name = $1 AND version = $2"
    );
    let v = i32::try_from(version).unwrap_or(i32::MAX);
    match c
        .query_opt(&sql, &[&name, &v])
        .await
        .map_err(pg("read a recipe"))?
    {
        Some(row) => stored_recipe(&row),
        None => Err(ControlError::UnknownRecipe {
            name: name.to_string(),
            version: Some(version),
        }),
    }
}

/// The newest active version of a recipe.
///
/// # Errors
/// [`ControlError::UnknownRecipe`] when there is none.
pub async fn latest(c: &impl GenericClient, name: &str) -> Result<StoredRecipe, ControlError> {
    let sql = format!(
        "SELECT {ROW_COLUMNS}, source, plan FROM {SCHEMA_CONTROL}.recipes \
         WHERE name = $1 AND status = 'active' ORDER BY version DESC LIMIT 1"
    );
    match c
        .query_opt(&sql, &[&name])
        .await
        .map_err(pg("read a recipe"))?
    {
        Some(row) => stored_recipe(&row),
        None => Err(ControlError::UnknownRecipe {
            name: name.to_string(),
            version: None,
        }),
    }
}

// ---- schema apply ----

const LOCK_SCHEMA: &str = "SELECT pg_advisory_xact_lock(hashtext('bhdata_control.schema'), 0)";
const LOCK_RECIPE: &str =
    "SELECT pg_advisory_xact_lock(hashtext('bhdata_control.recipe'), hashtext($1))";

/// What [`apply_schema`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaOutcome {
    /// The schema applied last is the same; nothing was written.
    Unchanged {
        /// That version.
        number: u32,
    },
    /// A new version was recorded.
    Applied {
        /// Its number.
        number: u32,
        /// One line per change.
        changes: Vec<String>,
    },
}

/// Applies a schema file: its additive changes, and a new `schema_versions` row, in one
/// transaction. A schema with a destructive change is refused whole.
///
/// # Errors
/// [`ControlError::Schema`] for a file that is not a valid schema, [`ControlError::Destructive`]
/// for a change that needs an admin's say, or a refused step.
pub async fn apply_schema(
    client: &mut Client,
    text: &str,
    by: &str,
) -> Result<SchemaOutcome, ControlError> {
    let new = parse_schema(text).map_err(|e| ControlError::Schema(e.to_string()))?;
    let tx = client.transaction().await.map_err(pg("begin"))?;
    tx.batch_execute(LOCK_SCHEMA).await.map_err(pg("lock"))?;
    let current = current_schema(&tx).await?;
    match decide_schema(current.as_ref().map(|c| &c.schema), &new) {
        SchemaDecision::Unchanged => Ok(SchemaOutcome::Unchanged {
            number: current.map_or(0, |c| c.number),
        }),
        SchemaDecision::Refuse { destructive } => Err(ControlError::Destructive(destructive)),
        SchemaDecision::Apply {
            statements,
            changes,
        } => {
            for statement in &statements {
                tx.batch_execute(statement)
                    .await
                    .map_err(pg("apply the schema"))?;
            }
            let n = current.map_or(1, |c| c.number + 1);
            let sql = format!(
                "INSERT INTO {SCHEMA_CONTROL}.schema_versions (number, hash, source, applied_by) \
                 VALUES ($1, $2, $3, $4)"
            );
            let n_db = i32::try_from(n).unwrap_or(i32::MAX);
            tx.execute(&sql, &[&n_db, &schema_hash(&new), &text, &by])
                .await
                .map_err(pg("record the schema"))?;
            tx.commit().await.map_err(pg("commit"))?;
            Ok(SchemaOutcome::Applied { number: n, changes })
        }
    }
}

// ---- recipe register ----

/// What [`register_recipe`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// A new version was recorded.
    Registered {
        /// The recipe's name.
        name: String,
        /// Its new version.
        version: u32,
        /// The plan's hash.
        content_hash: String,
    },
    /// The latest version already has this plan; no version was added.
    Unchanged {
        /// The recipe's name.
        name: String,
        /// That version.
        version: u32,
    },
}

impl RegisterOutcome {
    /// One line for a person, like `registered spend@3` or `already registered as spend@3,
    /// unchanged`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            RegisterOutcome::Registered { name, version, .. } => {
                format!("registered {name}@{version}")
            }
            RegisterOutcome::Unchanged { name, version } => {
                format!("already registered as {name}@{version}, unchanged")
            }
        }
    }
}

/// Registers a recipe file against the schema applied last, and re-applies the grants, in one
/// transaction.
///
/// # Errors
/// [`ControlError::Rejected`] with the parser's or the compiler's rejection,
/// [`ControlError::NoSchema`] before any schema is applied, or a refused step.
pub async fn register_recipe(
    client: &mut Client,
    text: &str,
    limits: &Limits,
    by: &str,
) -> Result<RegisterOutcome, ControlError> {
    let recipe = parse_recipe(text, limits).map_err(ControlError::Rejected)?;
    let tx = client.transaction().await.map_err(pg("begin"))?;
    tx.execute(LOCK_RECIPE, &[&recipe.name])
        .await
        .map_err(pg("lock"))?;
    let schema = current_schema(&tx).await?.ok_or(ControlError::NoSchema)?;
    let plan = compile(&schema.schema, &recipe, &[], limits).map_err(ControlError::Rejected)?;
    let hash = plan.content_hash();

    let sql = format!(
        "SELECT version, content_hash, status FROM {SCHEMA_CONTROL}.recipes \
         WHERE name = $1 ORDER BY version DESC LIMIT 1"
    );
    let latest_row = match tx
        .query_opt(&sql, &[&recipe.name])
        .await
        .map_err(pg("read the latest version"))?
    {
        Some(row) => Some(LatestRecipe {
            version: number(row.get(0), "recipe version")?,
            content_hash: row.get(1),
            active: row.get::<_, String>(2) == "active",
        }),
        None => None,
    };

    let outcome = match decide_registration(latest_row.as_ref(), &hash) {
        RegistrationDecision::Unchanged { version } => RegisterOutcome::Unchanged {
            name: recipe.name.clone(),
            version,
        },
        RegistrationDecision::New { version } => {
            let sql = format!(
                "INSERT INTO {SCHEMA_CONTROL}.recipes \
                 (name, version, content_hash, schema_version, source, plan, registered_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)"
            );
            let v = i32::try_from(version).unwrap_or(i32::MAX);
            let sv = i32::try_from(schema.number).unwrap_or(i32::MAX);
            tx.execute(
                &sql,
                &[
                    &recipe.name,
                    &v,
                    &hash,
                    &sv,
                    &text,
                    &plan.canonical_json(),
                    &by,
                ],
            )
            .await
            .map_err(pg("register the recipe"))?;
            RegisterOutcome::Registered {
                name: recipe.name.clone(),
                version,
                content_hash: hash,
            }
        }
    };
    // Registering is not complete until the grants are applied. They are applied again for an
    // unchanged recipe too, so a repeated deploy also repairs a right that was taken away.
    reapply_grants(&tx).await?;
    tx.commit().await.map_err(pg("commit"))?;
    Ok(outcome)
}

// ---- grants ----

/// The union of the grants of every registered version, rebuilt from the stored sources.
///
/// # Errors
/// [`ControlError::Inconsistent`] when a stored version does not compile, or compiles to another
/// plan than the one that was hashed when it was registered.
pub async fn registered_grants(c: &impl GenericClient) -> Result<Grants, ControlError> {
    let sql =
        format!("SELECT number, source FROM {SCHEMA_CONTROL}.schema_versions ORDER BY number");
    let mut schemas: BTreeMap<u32, Schema> = BTreeMap::new();
    for row in c.query(&sql, &[]).await.map_err(pg("read schemas"))? {
        let n = number(row.get(0), "schema version")?;
        let source: String = row.get(1);
        schemas.insert(n, parse_stored_schema(n, &source)?);
    }
    let sql = format!(
        "SELECT name, version, content_hash, schema_version, source \
         FROM {SCHEMA_CONTROL}.recipes ORDER BY name, version"
    );
    let mut all = Grants::default();
    for row in c.query(&sql, &[]).await.map_err(pg("read recipes"))? {
        let name: String = row.get(0);
        let version = number(row.get(1), "recipe version")?;
        let stored_hash: String = row.get(2);
        let sv = number(row.get(3), "schema version")?;
        let source: String = row.get(4);
        let which = format!("{name}@{version}");
        let schema = schemas.get(&sv).ok_or_else(|| {
            ControlError::Inconsistent(format!(
                "{which} names schema version {sv}, which is missing"
            ))
        })?;
        let recipe = parse_recipe(&source, &UNLIMITED)
            .map_err(|e| ControlError::Inconsistent(format!("{which} no longer parses: {e}")))?;
        let plan = compile(schema, &recipe, &[], &UNLIMITED)
            .map_err(|e| ControlError::Inconsistent(format!("{which} no longer compiles: {e}")))?;
        if plan.content_hash() != stored_hash {
            return Err(ControlError::Inconsistent(format!(
                "{which}: its source now compiles to a different plan than the one registered"
            )));
        }
        all.merge(&plan.grants);
    }
    Ok(all)
}

/// The statements that leave the recipe role with exactly `grants` on the data tables: everything
/// it had is revoked first, so a right nobody registered does not survive.
#[must_use]
pub fn grant_statements(grants: &Grants) -> Vec<String> {
    let role = Role::Recipe.name();
    let mut out = vec![format!(
        "REVOKE ALL ON ALL TABLES IN SCHEMA {SCHEMA_APP} FROM {role}"
    )];
    out.extend(grants.statements(role));
    out
}

/// Gives the recipe role the union of the grants of every registered version, and nothing else.
/// Returns how many statements it ran. Safe to run again.
///
/// # Errors
/// As [`registered_grants`], or a refused statement.
pub async fn reapply_grants(c: &impl GenericClient) -> Result<usize, ControlError> {
    let statements = grant_statements(&registered_grants(c).await?);
    for statement in &statements {
        c.batch_execute(statement)
            .await
            .map_err(pg("apply the grants"))?;
    }
    Ok(statements.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bhdata_recipe::plan::RECIPE_ROLE;

    const SCHEMA: &str = r#"
[[table]]
name = "users"
kind = "subject"
columns = [{ name = "balance", type = "i64", min = 0, max = 1_000_000_000 }]

[[table]]
name = "entries"
kind = "child"
owner = "users"
columns = [{ name = "delta", type = "i64" }]
"#;

    fn schema(text: &str) -> Schema {
        parse_schema(text).expect("schema")
    }

    #[test]
    fn the_schema_hash_ignores_the_order_of_tables_and_changes_with_content() {
        let a = schema(SCHEMA);
        let reordered = schema(
            r#"
[[table]]
name = "users"
kind = "subject"
columns = [{ name = "balance", type = "i64", min = 0, max = 1_000_000_000 }]
[[table]]
name = "audit"
kind = "subject"
columns = [{ name = "n", type = "i64" }]
[[table]]
name = "entries"
kind = "child"
owner = "users"
columns = [{ name = "delta", type = "i64" }]
"#,
        );
        let swapped = schema(
            r#"
[[table]]
name = "audit"
kind = "subject"
columns = [{ name = "n", type = "i64" }]
[[table]]
name = "entries"
kind = "child"
owner = "users"
columns = [{ name = "delta", type = "i64" }]
[[table]]
name = "users"
kind = "subject"
columns = [{ name = "balance", type = "i64", min = 0, max = 1_000_000_000 }]
"#,
        );
        assert_eq!(schema_hash(&reordered), schema_hash(&swapped));
        assert_ne!(schema_hash(&a), schema_hash(&reordered));
        let h = schema_hash(&a);
        assert_eq!(h.len(), 64);
        assert!(h
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    #[test]
    fn the_first_schema_creates_every_table() {
        let new = schema(SCHEMA);
        let SchemaDecision::Apply {
            statements,
            changes,
        } = decide_schema(None, &new)
        else {
            panic!("the first schema is applied");
        };
        assert_eq!(changes, ["add table users", "add table entries"]);
        assert!(statements[0].starts_with("CREATE TABLE app.\"users\""));
        assert!(statements
            .iter()
            .any(|s| s.starts_with("CREATE TABLE app.\"entries\"")));
    }

    #[test]
    fn the_same_schema_again_is_unchanged_even_in_another_order() {
        let a = schema(SCHEMA);
        assert_eq!(decide_schema(Some(&a), &a), SchemaDecision::Unchanged);
        let mut flipped = a.clone();
        flipped.tables.reverse();
        assert_eq!(decide_schema(Some(&a), &flipped), SchemaDecision::Unchanged);
    }

    #[test]
    fn additive_changes_are_applied_and_listed() {
        let old = schema(SCHEMA);
        let new = schema(&format!(
            "{SCHEMA}\n[[table]]\nname = \"teams\"\nkind = \"subject\"\ncolumns = [{{ name = \"size\", type = \"i64\" }}]\n"
        ));
        let SchemaDecision::Apply {
            statements,
            changes,
        } = decide_schema(Some(&old), &new)
        else {
            panic!("additive");
        };
        assert_eq!(changes, ["add table teams"]);
        assert_eq!(statements.len(), 1);

        let with_column = schema(&SCHEMA.replace(
            "columns = [{ name = \"delta\", type = \"i64\" }]",
            "columns = [{ name = \"delta\", type = \"i64\" }, { name = \"note\", type = \"text\", max_len = 10, nullable = true }]",
        ));
        let SchemaDecision::Apply {
            statements,
            changes,
        } = decide_schema(Some(&old), &with_column)
        else {
            panic!("a nullable column is additive");
        };
        assert_eq!(changes, ["add column entries.note"]);
        assert!(statements[0].contains("ADD COLUMN"), "{statements:?}");
    }

    #[test]
    fn a_destructive_change_refuses_the_whole_schema_even_beside_additive_ones() {
        let old = schema(SCHEMA);
        // Drops the `entries` table and adds `teams`.
        let new = schema(
            r#"
[[table]]
name = "users"
kind = "subject"
columns = [{ name = "balance", type = "i64", min = 0, max = 1_000_000_000 }]
[[table]]
name = "teams"
kind = "subject"
columns = [{ name = "size", type = "i64" }]
"#,
        );
        assert_eq!(
            decide_schema(Some(&old), &new),
            SchemaDecision::Refuse {
                destructive: vec!["drop table entries".to_string()]
            }
        );
        // A tighter bound, a required new column and a type change are destructive too.
        for text in [
            SCHEMA.replace("max = 1_000_000_000", "max = 5"),
            SCHEMA.replace(
                "columns = [{ name = \"delta\", type = \"i64\" }]",
                "columns = [{ name = \"delta\", type = \"i64\" }, { name = \"must\", type = \"i64\" }]",
            ),
            SCHEMA.replace("{ name = \"delta\", type = \"i64\" }", "{ name = \"delta\", type = \"bool\" }"),
        ] {
            assert!(
                matches!(decide_schema(Some(&old), &schema(&text)), SchemaDecision::Refuse { .. }),
                "{text}"
            );
        }
    }

    fn latest_of(version: u32, hash: &str, active: bool) -> LatestRecipe {
        LatestRecipe {
            version,
            content_hash: hash.to_string(),
            active,
        }
    }

    #[test]
    fn registration_numbers_versions_and_finds_a_repeat_of_the_latest() {
        assert_eq!(
            decide_registration(None, "a"),
            RegistrationDecision::New { version: 1 }
        );
        assert_eq!(
            decide_registration(Some(&latest_of(3, "a", true)), "a"),
            RegistrationDecision::Unchanged { version: 3 }
        );
        assert_eq!(
            decide_registration(Some(&latest_of(3, "a", true)), "b"),
            RegistrationDecision::New { version: 4 }
        );
        // A retired latest version is not reused, even for identical content.
        assert_eq!(
            decide_registration(Some(&latest_of(3, "a", false)), "a"),
            RegistrationDecision::New { version: 4 }
        );
    }

    #[test]
    fn outcomes_read_like_the_spec_says() {
        let same = RegisterOutcome::Unchanged {
            name: "spend".into(),
            version: 3,
        };
        assert_eq!(same.describe(), "already registered as spend@3, unchanged");
        let new = RegisterOutcome::Registered {
            name: "spend".into(),
            version: 4,
            content_hash: "x".into(),
        };
        assert_eq!(new.describe(), "registered spend@4");
    }

    #[test]
    fn the_grant_statements_revoke_everything_first_and_name_the_recipe_role() {
        assert_eq!(Role::Recipe.name(), RECIPE_ROLE);
        let mut grants = Grants::default();
        let statements = grant_statements(&grants);
        assert_eq!(
            statements,
            ["REVOKE ALL ON ALL TABLES IN SCHEMA app FROM bhdata_recipe"]
        );
        let recipe = parse_recipe(
            "[recipe]\nname = \"r\"\nsubject = \"users\"\n[[step]]\nop = \"ensure\"\n[recipe.result]\nreturn = []\n",
            &UNLIMITED,
        )
        .expect("parses");
        let plan = compile(&schema(SCHEMA), &recipe, &[], &UNLIMITED).expect("compiles");
        grants.merge(&plan.grants);
        let statements = grant_statements(&grants);
        assert_eq!(statements.len(), 2);
        assert!(statements[0].starts_with("REVOKE ALL"));
        assert!(statements[1].starts_with("GRANT INSERT (\"balance\", \"id\")"));
        assert!(statements[1].ends_with("TO \"bhdata_recipe\""));
    }

    #[test]
    fn the_unlimited_limits_accept_a_recipe_the_default_limits_refuse() {
        let steps: String = (0..20).map(|_| "[[step]]\nop = \"ensure\"\n").collect();
        let text = format!(
            "[recipe]\nname = \"r\"\nsubject = \"users\"\n{steps}[recipe.result]\nreturn = []\n"
        );
        assert!(parse_recipe(&text, &Limits::default()).is_err());
        assert!(parse_recipe(&text, &UNLIMITED).is_ok());
    }
}
