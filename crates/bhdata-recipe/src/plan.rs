//! The plan: what a checked recipe becomes (spec `recipe-compiler.md` sections 1 and 4). Pure: the
//! same schema and recipe always give the same plan, byte for byte.
//!
//! **The writer evaluates expressions, in Rust, with exact arithmetic.** Their values reach the
//! database only as bound parameters: `$1` is always the id of the row's owner, `$2`, `$3`, ... are
//! the step's values in the order of its `params`. So the SQL has no arithmetic of its own that
//! could round differently from the compiler's types and ranges, and every identifier and every
//! number written into the text comes from the validated schema, never from input.
//!
//! The reasons for the statement shapes:
//!
//! * **`add` is one `UPDATE`** whose `WHERE` holds the bounds that have a named error, so a hot
//!   row costs one statement. When no row matches, `miss_sql` locks the row and reads it; the
//!   writer then picks `below` or `above` by comparing in exact arithmetic, and only if neither
//!   applies (another command changed the row meanwhile) runs the `UPDATE` once more.
//! * **A `read` takes `FOR UPDATE`** when a later step of the same recipe writes that row, so the
//!   value the writer computed with cannot go stale.
//! * **An `insert` first takes a transaction advisory lock** for (table, owner), in its own
//!   statement. `seq` is the owner's highest `seq` plus one, and only a statement that starts after
//!   the lock is held can see the previous insert. An advisory lock needs no table privilege, so
//!   the recipe role gets no `UPDATE` right on the owner row just to insert a child.
//! * **`ensure` writes a start value into every non-nullable column**: the allowed value nearest
//!   to zero (`0`, or `min` when `min > 0`, or `max` when `max < 0`; `''`, `false`, the epoch, and
//!   empty bytes for the other types). Nullable columns start `NULL`. It uses
//!   `ON CONFLICT DO NOTHING`, so an existing row is untouched.
//! * Decimals and timestamps travel as text and as whole microseconds; the plan names each value's
//!   type (`as`), so the writer and the database agree without a float anywhere.
//!
//! The savepoint: the request record is written before it, outside it. The steps of each shard's
//! step list run after it; a named error rolls back to it (design section 9).

use std::collections::{BTreeMap, BTreeSet};

use bhdata_schema::ddl::APP_SCHEMA;
use bhdata_schema::decimal::format_scaled;
use bhdata_schema::model::{Column, ColumnType, Schema, Table};

use crate::canon::{sha256_hex, Json};
use crate::check::{check, DerivedStore, Lane};
use crate::model::{Op, Recipe, Who};
use crate::parse::Limits;
use crate::reject::{Code, Rejection};

/// The version of the plan's JSON layout. Part of every plan, so a future layout is never mistaken
/// for this one.
pub const PLAN_FORMAT: i64 = 1;

/// How long one run may take, in milliseconds. The writer enforces it as a statement timeout.
pub const RUN_TIME_LIMIT_MS: u64 = 2_000;

/// The database role recipes run as (spec `recipe-compiler.md` section 4).
pub const RECIPE_ROLE: &str = "bhdata_recipe";

/// A value the writer computes and binds: the expression's canonical text, and the type to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The expression, in its canonical, fully parenthesised text.
    pub expr: String,
    /// The type of the value sent (the target column's type, see [`type_text`]).
    pub ty: String,
}

/// A value a statement returns: where it comes from and what to call it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutColumn {
    /// The result name (`balance`, or `to.balance` for the counterparty's row).
    pub name: String,
    /// The column in the table.
    pub column: String,
    /// The type returned (decimals as text, timestamps as whole microseconds).
    pub ty: String,
}

/// A named error and the column limit that triggers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limit {
    /// The error raised.
    pub error: String,
    /// The `min` or `max`, written as the column writes it.
    pub limit: String,
}

/// One step, compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepPlan {
    /// Create the row with start values if it is missing.
    Ensure {
        /// The statement.
        sql: String,
    },
    /// Read one row.
    Read {
        /// The statement (`$1` is the owner's id).
        sql: String,
        /// What it returns, in order.
        columns: Vec<OutColumn>,
    },
    /// A pure condition, evaluated by the writer.
    Check {
        /// The condition.
        require: String,
        /// The error when it is false.
        error: String,
    },
    /// Add to a column within its bounds.
    Add {
        /// The bounded `UPDATE`; it returns the new value when a row matched.
        sql: String,
        /// `$2`.
        params: Vec<Param>,
        /// The column.
        column: String,
        /// The column's type.
        ty: String,
        /// Raised when the result would be under `min`.
        below: Option<Limit>,
        /// Raised when the result would be over `max`.
        above: Option<Limit>,
        /// Locks and reads the row after a miss. `None` when no bound is enforced.
        miss_sql: Option<String>,
    },
    /// Replace columns of the row.
    Set {
        /// The `UPDATE`.
        sql: String,
        /// `$2`, `$3`, ... in order.
        params: Vec<Param>,
    },
    /// Add a child row.
    Insert {
        /// The advisory lock, in its own statement; `None` when an earlier step already took it.
        lock_sql: Option<String>,
        /// The `INSERT`.
        sql: String,
        /// `$2`, `$3`, ... in order.
        params: Vec<Param>,
    },
    /// Read the newest child rows.
    List {
        /// The statement.
        sql: String,
        /// What each row holds.
        columns: Vec<OutColumn>,
        /// The list's name in the result.
        into: String,
        /// At most this many rows.
        limit: u32,
    },
    /// Read a derived store.
    ReadDerived {
        /// The store.
        store: String,
        /// Its window.
        window: String,
        /// The stalest result accepted.
        max_age_ms: u64,
        /// At most this many entries.
        limit: u32,
    },
}

/// A step with its place in the recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    /// 1-based, as in a rejection and in error reports.
    pub n: usize,
    /// The author's label.
    pub label: Option<String>,
    /// Whose row it reaches; `None` for a `check` and a derived read.
    pub of: Option<Who>,
    /// What it does.
    pub plan: StepPlan,
}

/// The steps that run on one key's shard, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardSteps {
    /// The key.
    pub of: Who,
    /// Step numbers, in order. The savepoint is taken before the first.
    pub steps: Vec<usize>,
}

/// Where a result value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The values a `read` or `list` step returned.
    Step(usize),
    /// The final read of the subject row (`result_fetch`).
    Fetch,
}

/// One value of the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultItem {
    /// Its name.
    pub name: String,
    /// Where it comes from.
    pub source: Source,
}

/// The read of the subject row after all steps, for result values that are plain columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetch {
    /// The statement (`$1` is the subject's id).
    pub sql: String,
    /// What it returns.
    pub columns: Vec<OutColumn>,
}

/// An argument of the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanArg {
    /// Its name.
    pub name: String,
    /// Its type (see [`type_text`]).
    pub ty: String,
    /// Smallest value, as the column writes it.
    pub min: Option<String>,
    /// Largest value, as the column writes it.
    pub max: Option<String>,
}

/// What one run of the recipe may cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Steps in the recipe.
    pub steps: usize,
    /// Most rows touched or returned in one run.
    pub rows: u64,
    /// The time limit of one run.
    pub time_ms: u64,
}

/// The columns of one table that the recipe role needs rights on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableGrant {
    /// Columns it may read.
    pub select: BTreeSet<String>,
    /// Columns it may write in a new row.
    pub insert: BTreeSet<String>,
    /// Columns it may change.
    pub update: BTreeSet<String>,
}

/// The exact rights a recipe needs: named columns of named tables, never a whole table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    /// By table name.
    pub tables: BTreeMap<String, TableGrant>,
}

impl Grants {
    fn entry(&mut self, table: &str) -> &mut TableGrant {
        self.tables.entry(table.to_string()).or_default()
    }

    fn select<'a>(&mut self, table: &str, columns: impl IntoIterator<Item = &'a str>) {
        self.entry(table)
            .select
            .extend(columns.into_iter().map(str::to_string));
    }

    fn insert<'a>(&mut self, table: &str, columns: impl IntoIterator<Item = &'a str>) {
        self.entry(table)
            .insert
            .extend(columns.into_iter().map(str::to_string));
    }

    fn update<'a>(&mut self, table: &str, columns: impl IntoIterator<Item = &'a str>) {
        self.entry(table)
            .update
            .extend(columns.into_iter().map(str::to_string));
    }

    /// Adds everything `other` grants. The role's rights are the union over all registered
    /// versions, and never more.
    pub fn merge(&mut self, other: &Grants) {
        for (table, grant) in &other.tables {
            let mine = self.entry(table);
            mine.select.extend(grant.select.iter().cloned());
            mine.insert.extend(grant.insert.iter().cloned());
            mine.update.extend(grant.update.iter().cloned());
        }
    }

    /// The `GRANT` statements for `role`, sorted, one per table and right.
    #[must_use]
    pub fn statements(&self, role: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (table, grant) in &self.tables {
            for (right, columns) in [
                ("SELECT", &grant.select),
                ("INSERT", &grant.insert),
                ("UPDATE", &grant.update),
            ] {
                if columns.is_empty() {
                    continue;
                }
                let list: Vec<String> = columns.iter().map(|c| quote(c)).collect();
                out.push(format!(
                    "GRANT {right} ({}) ON TABLE {} TO {}",
                    list.join(", "),
                    qualified(table),
                    quote(role)
                ));
            }
        }
        out
    }

    fn to_json(&self) -> Json {
        let list = |set: &BTreeSet<String>| Json::strs(set.iter().cloned());
        Json::Obj(
            self.tables
                .iter()
                .map(|(table, g)| {
                    (
                        table.clone(),
                        Json::obj([
                            ("insert", list(&g.insert)),
                            ("select", list(&g.select)),
                            ("update", list(&g.update)),
                        ]),
                    )
                })
                .collect(),
        )
    }
}

/// A compiled recipe. The version is not part of it: the registry numbers a plan when it is
/// registered, and the plan's hash is what tells whether it is new.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The recipe's name.
    pub name: String,
    /// The table whose row the command owns.
    pub subject: String,
    /// The second subject's table, if any.
    pub counterparty: Option<String>,
    /// The arguments, sorted by name.
    pub args: Vec<PlanArg>,
    /// How many shards a run touches.
    pub lane: Lane,
    /// The steps, in order.
    pub steps: Vec<PlanStep>,
    /// The step lists per key. A `check` is in none: where it runs is decided with the planner.
    pub shards: Vec<ShardSteps>,
    /// The named errors it can raise, in order of first use.
    pub errors: Vec<String>,
    /// What the command returns, in the order written.
    pub result: Vec<ResultItem>,
    /// The final read of the subject row, if the result needs it.
    pub result_fetch: Option<Fetch>,
    /// The rights the recipe role needs.
    pub grants: Grants,
    /// Tables whose rows it reads, sorted.
    pub reads: Vec<String>,
    /// Tables it writes, sorted.
    pub writes: Vec<String>,
    /// What one run may cost.
    pub bounds: Bounds,
}

impl Plan {
    /// The plan as JSON, in canonical form (the registry stores exactly this text).
    #[must_use]
    pub fn canonical_json(&self) -> String {
        self.to_json().canonical()
    }

    /// SHA-256 of the canonical JSON: 64 lower-case hex digits.
    #[must_use]
    pub fn content_hash(&self) -> String {
        sha256_hex(&self.canonical_json())
    }

    fn to_json(&self) -> Json {
        Json::obj([
            ("args", Json::Arr(self.args.iter().map(arg_json).collect())),
            (
                "bounds",
                Json::obj([
                    (
                        "rows",
                        Json::Int(i64::try_from(self.bounds.rows).unwrap_or(i64::MAX)),
                    ),
                    ("steps", Json::count(self.bounds.steps)),
                    (
                        "time_ms",
                        Json::Int(i64::try_from(self.bounds.time_ms).unwrap_or(i64::MAX)),
                    ),
                ]),
            ),
            ("counterparty", Json::opt_str(self.counterparty.as_deref())),
            ("errors", Json::strs(self.errors.iter().cloned())),
            ("format", Json::Int(PLAN_FORMAT)),
            ("grants", self.grants.to_json()),
            ("lane", Json::str(self.lane.as_str())),
            ("name", Json::str(&self.name)),
            ("reads", Json::strs(self.reads.iter().cloned())),
            (
                "result",
                Json::Arr(self.result.iter().map(result_json).collect()),
            ),
            (
                "result_fetch",
                self.result_fetch.as_ref().map_or(Json::Null, |f| {
                    Json::obj([
                        ("columns", out_json(&f.columns)),
                        ("sql", Json::str(&f.sql)),
                    ])
                }),
            ),
            (
                "shards",
                Json::Arr(
                    self.shards
                        .iter()
                        .map(|s| {
                            Json::obj([
                                ("of", Json::str(who_text(s.of))),
                                (
                                    "savepoint_before",
                                    s.steps.first().map_or(Json::Null, |n| Json::count(*n)),
                                ),
                                (
                                    "steps",
                                    Json::Arr(s.steps.iter().map(|n| Json::count(*n)).collect()),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "steps",
                Json::Arr(self.steps.iter().map(step_json).collect()),
            ),
            ("subject", Json::str(&self.subject)),
            ("writes", Json::strs(self.writes.iter().cloned())),
        ])
    }
}

fn who_text(of: Who) -> &'static str {
    match of {
        Who::Subject => "subject",
        Who::Counterparty => "counterparty",
    }
}

fn arg_json(a: &PlanArg) -> Json {
    Json::obj([
        ("max", Json::opt_str(a.max.as_deref())),
        ("min", Json::opt_str(a.min.as_deref())),
        ("name", Json::str(&a.name)),
        ("type", Json::str(&a.ty)),
    ])
}

fn out_json(columns: &[OutColumn]) -> Json {
    Json::Arr(
        columns
            .iter()
            .map(|c| {
                Json::obj([
                    ("as", Json::str(&c.ty)),
                    ("column", Json::str(&c.column)),
                    ("name", Json::str(&c.name)),
                ])
            })
            .collect(),
    )
}

fn params_json(params: &[Param]) -> Json {
    Json::Arr(
        params
            .iter()
            .map(|p| Json::obj([("as", Json::str(&p.ty)), ("expr", Json::str(&p.expr))]))
            .collect(),
    )
}

fn limit_json(l: Option<&Limit>) -> Json {
    l.map_or(Json::Null, |l| {
        Json::obj([
            ("error", Json::str(&l.error)),
            ("limit", Json::str(&l.limit)),
        ])
    })
}

fn result_json(r: &ResultItem) -> Json {
    match r.source {
        Source::Step(n) => Json::obj([
            ("from", Json::str("step")),
            ("name", Json::str(&r.name)),
            ("step", Json::count(n)),
        ]),
        Source::Fetch => Json::obj([("from", Json::str("fetch")), ("name", Json::str(&r.name))]),
    }
}

fn step_json(s: &PlanStep) -> Json {
    let mut fields: BTreeMap<String, Json> = BTreeMap::new();
    let mut put = |k: &str, v: Json| {
        fields.insert(k.to_string(), v);
    };
    put("label", Json::opt_str(s.label.as_deref()));
    put("n", Json::count(s.n));
    put("of", s.of.map_or(Json::Null, |w| Json::str(who_text(w))));
    match &s.plan {
        StepPlan::Ensure { sql } => {
            put("op", Json::str("ensure"));
            put("sql", Json::str(sql));
        }
        StepPlan::Read { sql, columns } => {
            put("op", Json::str("read"));
            put("sql", Json::str(sql));
            put("columns", out_json(columns));
        }
        StepPlan::Check { require, error } => {
            put("op", Json::str("check"));
            put("require", Json::str(require));
            put("error", Json::str(error));
        }
        StepPlan::Add {
            sql,
            params,
            column,
            ty,
            below,
            above,
            miss_sql,
        } => {
            put("op", Json::str("add"));
            put("sql", Json::str(sql));
            put("params", params_json(params));
            put("column", Json::str(column));
            put("as", Json::str(ty));
            put("below", limit_json(below.as_ref()));
            put("above", limit_json(above.as_ref()));
            put("miss_sql", Json::opt_str(miss_sql.as_deref()));
        }
        StepPlan::Set { sql, params } => {
            put("op", Json::str("set"));
            put("sql", Json::str(sql));
            put("params", params_json(params));
        }
        StepPlan::Insert {
            lock_sql,
            sql,
            params,
        } => {
            put("op", Json::str("insert"));
            put("lock_sql", Json::opt_str(lock_sql.as_deref()));
            put("sql", Json::str(sql));
            put("params", params_json(params));
        }
        StepPlan::List {
            sql,
            columns,
            into,
            limit,
        } => {
            put("op", Json::str("list"));
            put("sql", Json::str(sql));
            put("columns", out_json(columns));
            put("into", Json::str(into));
            put("limit", Json::Int(i64::from(*limit)));
        }
        StepPlan::ReadDerived {
            store,
            window,
            max_age_ms,
            limit,
        } => {
            put("op", Json::str("read_derived"));
            put("store", Json::str(store));
            put("window", Json::str(window));
            put(
                "max_age_ms",
                Json::Int(i64::try_from(*max_age_ms).unwrap_or(i64::MAX)),
            );
            put("limit", Json::Int(i64::from(*limit)));
        }
    }
    Json::Obj(fields)
}

// ---- SQL pieces. Every name here was checked by the schema; quoting is plain double quotes. ----

fn quote(name: &str) -> String {
    debug_assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    format!("\"{name}\"")
}

fn qualified(table: &str) -> String {
    format!("{APP_SCHEMA}.{}", quote(table))
}

/// A column type as the plan writes it: `i64`, `decimal(18,2)`, `text(64)`, `bool`, `timestamp`,
/// `bytes(16)`. Frozen: plans and their hashes contain it.
#[must_use]
pub fn type_text(ty: ColumnType) -> String {
    match ty {
        ColumnType::I64 => "i64".to_string(),
        ColumnType::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
        ColumnType::Text { max_len } => format!("text({max_len})"),
        ColumnType::Bool => "bool".to_string(),
        ColumnType::Timestamp => "timestamp".to_string(),
        ColumnType::Bytes { max_len } => format!("bytes({max_len})"),
    }
}

/// `$n` cast to the column's database type. A decimal is cast without a scale: the compiler
/// already proved it has no more digits than the column, so nothing is rounded here.
fn cast(ty: ColumnType, n: usize) -> String {
    match ty {
        ColumnType::I64 => format!("${n}::bigint"),
        ColumnType::Decimal { .. } => format!("${n}::numeric"),
        ColumnType::Text { .. } => format!("${n}::text"),
        ColumnType::Bool => format!("${n}::boolean"),
        ColumnType::Timestamp => {
            format!("('epoch'::timestamptz + ${n}::bigint * interval '1 microsecond')")
        }
        ColumnType::Bytes { .. } => format!("${n}::bytea"),
    }
}

/// How a column is read back: decimals as text, timestamps as whole microseconds.
fn select_item(c: &Column) -> String {
    let name = quote(&c.name);
    match c.ty {
        ColumnType::Decimal { .. } => format!("{name}::text"),
        ColumnType::Timestamp => format!("(extract(epoch from {name}) * 1000000)::bigint"),
        _ => name,
    }
}

/// The type a column comes back as.
fn out_type(ty: ColumnType) -> String {
    match ty {
        ColumnType::Timestamp => "timestamp".to_string(),
        other => type_text(other),
    }
}

/// The start value `ensure` writes into a non-nullable column.
fn start_literal(c: &Column) -> String {
    match c.ty {
        ColumnType::I64 | ColumnType::Decimal { .. } => {
            let lo = c.min.unwrap_or(i128::MIN);
            let hi = c.max.unwrap_or(i128::MAX);
            format_scaled(0i128.clamp(lo, hi), c.ty.scale())
        }
        ColumnType::Text { .. } => "''".to_string(),
        ColumnType::Bool => "false".to_string(),
        ColumnType::Timestamp => "'epoch'::timestamptz".to_string(),
        ColumnType::Bytes { .. } => "''::bytea".to_string(),
    }
}

// ---- building ----

fn inconsistent(step: Option<usize>, what: &str) -> Rejection {
    Rejection::new(
        Code::Type,
        step,
        format!("internal: the checked recipe and the schema disagree about {what}"),
    )
}

/// `(column, cast parameter)` pairs of a `set` or `insert`, in column order.
type Assignments = Vec<(String, String)>;

struct Builder<'a> {
    schema: &'a Schema,
    recipe: &'a Recipe,
    grants: Grants,
    reads: BTreeSet<String>,
    writes: BTreeSet<String>,
    /// Result names produced by a step, and the step.
    produced: BTreeMap<String, usize>,
    /// (key, table) pairs whose insert lock an earlier step already took.
    locked: BTreeSet<(&'static str, String)>,
    rows: u64,
}

impl<'a> Builder<'a> {
    fn key_table(&self, of: Who) -> &str {
        match of {
            Who::Subject => &self.recipe.subject,
            Who::Counterparty => self
                .recipe
                .counterparty
                .as_deref()
                .unwrap_or(&self.recipe.subject),
        }
    }

    fn table(&self, name: &str, n: usize) -> Result<&'a Table, Rejection> {
        self.schema
            .table(name)
            .ok_or_else(|| inconsistent(Some(n), &format!("table `{name}`")))
    }

    fn column<'t>(table: &'t Table, name: &str, n: usize) -> Result<&'t Column, Rejection> {
        table
            .column(name)
            .ok_or_else(|| inconsistent(Some(n), &format!("column `{name}`")))
    }

    fn out_columns(
        &self,
        table: &Table,
        columns: &[String],
        prefix: &str,
        n: usize,
    ) -> Result<(Vec<OutColumn>, Vec<String>), Rejection> {
        let mut out = Vec::new();
        let mut items = Vec::new();
        for name in columns {
            let c = Self::column(table, name, n)?;
            items.push(select_item(c));
            out.push(OutColumn {
                name: format!("{prefix}{name}"),
                column: name.clone(),
                ty: out_type(c.ty),
            });
        }
        Ok((out, items))
    }

    fn params(
        table: &Table,
        values: &[(String, crate::expr::Expr)],
        first: usize,
        n: usize,
    ) -> Result<(Vec<Param>, Assignments), Rejection> {
        let mut params = Vec::new();
        let mut targets = Vec::new();
        for (i, (column, expr)) in values.iter().enumerate() {
            let c = Self::column(table, column, n)?;
            params.push(Param {
                expr: expr.to_string(),
                ty: type_text(c.ty),
            });
            targets.push((column.clone(), cast(c.ty, first + i)));
        }
        Ok((params, targets))
    }

    fn step(&mut self, idx: usize, op: &Op, label: Option<String>) -> Result<PlanStep, Rejection> {
        let n = idx + 1;
        let (of, plan) = match op {
            Op::Ensure { of } => (Some(*of), self.ensure(*of, n)?),
            Op::Read { of, table, columns } => (Some(*of), self.read(*of, table, columns, idx)?),
            Op::Check { require, error } => (
                None,
                StepPlan::Check {
                    require: require.to_string(),
                    error: error.clone(),
                },
            ),
            Op::Add {
                of,
                table,
                column,
                by,
                below,
                above,
            } => (
                Some(*of),
                self.add(
                    *of,
                    table,
                    column,
                    &by.to_string(),
                    below.as_deref(),
                    above.as_deref(),
                    n,
                )?,
            ),
            Op::Set { of, table, values } => (Some(*of), self.set(*of, table, values, n)?),
            Op::Insert { of, table, values } => (Some(*of), self.insert(*of, table, values, n)?),
            Op::List {
                of,
                table,
                columns,
                limit,
                into,
            } => (Some(*of), self.list(*of, table, columns, *limit, into, n)?),
            Op::ReadDerived {
                store,
                window,
                max_age_ms,
                limit,
            } => {
                self.rows = self.rows.saturating_add(u64::from(*limit));
                (
                    None,
                    StepPlan::ReadDerived {
                        store: store.clone(),
                        window: window.clone(),
                        max_age_ms: *max_age_ms,
                        limit: *limit,
                    },
                )
            }
        };
        Ok(PlanStep { n, label, of, plan })
    }

    fn ensure(&mut self, of: Who, n: usize) -> Result<StepPlan, Rejection> {
        let name = self.key_table(of).to_string();
        let table = self.table(&name, n)?;
        let mut cols: Vec<&Column> = table.columns.iter().filter(|c| !c.nullable).collect();
        cols.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<String> = std::iter::once("\"id\"".to_string())
            .chain(cols.iter().map(|c| quote(&c.name)))
            .collect();
        let values: Vec<String> = std::iter::once("$1".to_string())
            .chain(cols.iter().map(|c| start_literal(c)))
            .collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT DO NOTHING",
            qualified(&name),
            names.join(", "),
            values.join(", ")
        );
        let written: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        self.grants.insert(
            &name,
            std::iter::once("id").chain(written.iter().map(String::as_str)),
        );
        self.writes.insert(name);
        self.rows += 1;
        Ok(StepPlan::Ensure { sql })
    }

    fn read(
        &mut self,
        of: Who,
        table: &str,
        columns: &[String],
        idx: usize,
    ) -> Result<StepPlan, Rejection> {
        let n = idx + 1;
        let t = self.table(table, n)?;
        let prefix = if of == Who::Counterparty { "to." } else { "" };
        let (out, items) = self.out_columns(t, columns, prefix, n)?;
        // A later write to the same row means the value read must not go stale.
        let written_later = self.recipe.steps[idx + 1..].iter().any(|s| {
            matches!(&s.op, Op::Add { of: o, table: tb, .. } | Op::Set { of: o, table: tb, .. }
                if *o == of && tb == table)
        });
        let sql = format!(
            "SELECT {} FROM {} WHERE \"id\" = $1{}",
            items.join(", "),
            qualified(table),
            if written_later { " FOR UPDATE" } else { "" }
        );
        for o in &out {
            self.produced.insert(o.name.clone(), n);
        }
        self.grants.select(
            table,
            std::iter::once("id").chain(columns.iter().map(String::as_str)),
        );
        self.reads.insert(table.to_string());
        self.rows += 1;
        Ok(StepPlan::Read { sql, columns: out })
    }

    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        _of: Who,
        table: &str,
        column: &str,
        by: &str,
        below: Option<&str>,
        above: Option<&str>,
        n: usize,
    ) -> Result<StepPlan, Rejection> {
        let t = self.table(table, n)?;
        let c = Self::column(t, column, n)?;
        let (q, scale) = (quote(column), c.ty.scale());
        let amount = cast(c.ty, 2);
        let below = below.zip(c.min).map(|(error, min)| Limit {
            error: error.to_string(),
            limit: format_scaled(min, scale),
        });
        let above = above.zip(c.max).map(|(error, max)| Limit {
            error: error.to_string(),
            limit: format_scaled(max, scale),
        });
        let mut wheres = vec!["\"id\" = $1".to_string()];
        if let Some(l) = &below {
            wheres.push(format!("{q} + {amount} >= {}", l.limit));
        }
        if let Some(l) = &above {
            wheres.push(format!("{q} + {amount} <= {}", l.limit));
        }
        let sql = format!(
            "UPDATE {} SET {q} = {q} + {amount} WHERE {} RETURNING {}",
            qualified(table),
            wheres.join(" AND "),
            select_item(c)
        );
        let miss_sql = (below.is_some() || above.is_some()).then(|| {
            format!(
                "SELECT {} FROM {} WHERE \"id\" = $1 FOR UPDATE",
                select_item(c),
                qualified(table)
            )
        });
        self.grants.select(table, ["id", column]);
        self.grants.update(table, [column]);
        self.reads.insert(table.to_string());
        self.writes.insert(table.to_string());
        self.rows += 1;
        Ok(StepPlan::Add {
            sql,
            params: vec![Param {
                expr: by.to_string(),
                ty: type_text(c.ty),
            }],
            column: column.to_string(),
            ty: type_text(c.ty),
            below,
            above,
            miss_sql,
        })
    }

    fn set(
        &mut self,
        _of: Who,
        table: &str,
        values: &[(String, crate::expr::Expr)],
        n: usize,
    ) -> Result<StepPlan, Rejection> {
        let t = self.table(table, n)?;
        let (params, targets) = Self::params(t, values, 2, n)?;
        let assignments: Vec<String> = targets
            .iter()
            .map(|(column, value)| format!("{} = {value}", quote(column)))
            .collect();
        let sql = format!(
            "UPDATE {} SET {} WHERE \"id\" = $1",
            qualified(table),
            assignments.join(", ")
        );
        self.grants.select(table, ["id"]);
        self.grants
            .update(table, values.iter().map(|(c, _)| c.as_str()));
        self.writes.insert(table.to_string());
        self.rows += 1;
        Ok(StepPlan::Set { sql, params })
    }

    fn insert(
        &mut self,
        of: Who,
        table: &str,
        values: &[(String, crate::expr::Expr)],
        n: usize,
    ) -> Result<StepPlan, Rejection> {
        let t = self.table(table, n)?;
        let (params, targets) = Self::params(t, values, 2, n)?;
        let key = match of {
            Who::Subject => "subject",
            Who::Counterparty => "counterparty",
        };
        let lock_sql = self.locked.insert((key, table.to_string())).then(|| {
            format!("SELECT pg_advisory_xact_lock(hashtext('{APP_SCHEMA}.{table}'), hashtext($1))")
        });
        let names: Vec<String> = ["\"owner_id\"".to_string(), "\"seq\"".to_string()]
            .into_iter()
            .chain(targets.iter().map(|(c, _)| quote(c)))
            .collect();
        let vals: Vec<String> = [
            "$1".to_string(),
            format!(
                "COALESCE((SELECT MAX(\"seq\") FROM {} WHERE \"owner_id\" = $1), 0) + 1",
                qualified(table)
            ),
        ]
        .into_iter()
        .chain(targets.iter().map(|(_, v)| v.clone()))
        .collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            qualified(table),
            names.join(", "),
            vals.join(", ")
        );
        self.grants.select(table, ["owner_id", "seq"]);
        self.grants.insert(
            table,
            ["owner_id", "seq"]
                .into_iter()
                .chain(values.iter().map(|(c, _)| c.as_str())),
        );
        self.writes.insert(table.to_string());
        self.rows += 1;
        Ok(StepPlan::Insert {
            lock_sql,
            sql,
            params,
        })
    }

    fn list(
        &mut self,
        of: Who,
        table: &str,
        columns: &[String],
        limit: u32,
        into: &str,
        n: usize,
    ) -> Result<StepPlan, Rejection> {
        let t = self.table(table, n)?;
        let prefix = if of == Who::Counterparty { "to." } else { "" };
        let (out, items) = self.out_columns(t, columns, "", n)?;
        let sql = format!(
            "SELECT {} FROM {} WHERE \"owner_id\" = $1 ORDER BY \"seq\" DESC LIMIT {limit}",
            items.join(", "),
            qualified(table)
        );
        self.produced.insert(format!("{prefix}{into}"), n);
        self.produced.insert(into.to_string(), n);
        self.grants.select(
            table,
            ["owner_id", "seq"]
                .into_iter()
                .chain(columns.iter().map(String::as_str)),
        );
        self.reads.insert(table.to_string());
        self.rows = self.rows.saturating_add(u64::from(limit));
        Ok(StepPlan::List {
            sql,
            columns: out,
            into: into.to_string(),
            limit,
        })
    }

    /// The result: values some step produced, and plain columns of the subject row.
    fn result(&mut self) -> Result<(Vec<ResultItem>, Option<Fetch>), Rejection> {
        let subject = self
            .table(&self.recipe.subject.clone(), 0)
            .map_err(|mut r| {
                r.step = None;
                r
            })?;
        let mut items = Vec::new();
        let mut fetched: Vec<&Column> = Vec::new();
        for entry in &self.recipe.result {
            if let Some(n) = self.produced.get(entry) {
                items.push(ResultItem {
                    name: entry.clone(),
                    source: Source::Step(*n),
                });
            } else if let Some(c) = subject.column(entry) {
                fetched.push(c);
                items.push(ResultItem {
                    name: entry.clone(),
                    source: Source::Fetch,
                });
            } else {
                return Err(inconsistent(None, &format!("result entry `{entry}`")));
            }
        }
        if fetched.is_empty() {
            return Ok((items, None));
        }
        let sql = format!(
            "SELECT {} FROM {} WHERE \"id\" = $1",
            fetched
                .iter()
                .map(|c| select_item(c))
                .collect::<Vec<_>>()
                .join(", "),
            qualified(&subject.name)
        );
        let columns = fetched
            .iter()
            .map(|c| OutColumn {
                name: c.name.clone(),
                column: c.name.clone(),
                ty: out_type(c.ty),
            })
            .collect();
        let subject_name = subject.name.clone();
        let names: Vec<String> = fetched.iter().map(|c| c.name.clone()).collect();
        self.grants.select(
            &subject_name,
            std::iter::once("id").chain(names.iter().map(String::as_str)),
        );
        self.reads.insert(subject_name);
        self.rows += 1;
        Ok((items, Some(Fetch { sql, columns })))
    }
}

/// Checks `recipe` against `schema` and compiles it.
///
/// # Errors
/// The first [`Rejection`] of the checks (see [`check`]).
pub fn compile(
    schema: &Schema,
    recipe: &Recipe,
    derived: &[DerivedStore],
    limits: &Limits,
) -> Result<Plan, Rejection> {
    let checked = check(schema, recipe, derived, limits)?;
    let mut b = Builder {
        schema,
        recipe,
        grants: Grants::default(),
        reads: BTreeSet::new(),
        writes: BTreeSet::new(),
        produced: BTreeMap::new(),
        locked: BTreeSet::new(),
        rows: 0,
    };
    let mut steps = Vec::new();
    for (idx, step) in recipe.steps.iter().enumerate() {
        steps.push(b.step(idx, &step.op, step.label.clone())?);
    }
    let (result, result_fetch) = b.result()?;

    let mut shards = Vec::new();
    for of in [Who::Subject, Who::Counterparty] {
        let numbers: Vec<usize> = steps
            .iter()
            .filter(|s| s.of == Some(of))
            .map(|s| s.n)
            .collect();
        if !numbers.is_empty() {
            shards.push(ShardSteps { of, steps: numbers });
        }
    }

    let args = recipe
        .args
        .iter()
        .map(|a| PlanArg {
            name: a.name.clone(),
            ty: type_text(a.ty),
            min: a.min.map(|v| format_scaled(v, a.ty.scale())),
            max: a.max.map(|v| format_scaled(v, a.ty.scale())),
        })
        .collect();

    Ok(Plan {
        name: recipe.name.clone(),
        subject: recipe.subject.clone(),
        counterparty: recipe.counterparty.clone(),
        args,
        lane: checked.lane,
        shards,
        errors: checked.errors,
        result,
        result_fetch,
        grants: b.grants,
        reads: b.reads.into_iter().collect(),
        writes: b.writes.into_iter().collect(),
        bounds: Bounds {
            steps: steps.len(),
            rows: b.rows,
            time_ms: RUN_TIME_LIMIT_MS,
        },
        steps,
    })
}
