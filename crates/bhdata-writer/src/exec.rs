//! One command in one transaction (spec `write-path.md`, sections 3, 4, 5 and 7).
//!
//! ```text
//! check shape, bounds and shard   (no database)
//! check the request's clock rule  (no database)
//! BEGIN
//!   claim_request                 new / same (replay the stored answer) / conflict
//!   SAVEPOINT
//!   the plan's steps, in order    a named error rolls back to the savepoint
//!   RELEASE
//!   the result, the change-log entries, finish_request
//! COMMIT
//! ```
//!
//! A command either commits whole, request record included, or leaves nothing: the record is
//! written in the same transaction as the data, so a crash can never leave one without the other.
//! A named error of the recipe is not a failure of the transaction: its record (`failed`) commits
//! and a retry gets the same answer. A deadlock or a serialization failure runs the command again
//! from `BEGIN`, up to `writer.max_attempts` times; every other database error rolls back, stores
//! nothing, and is answered by [`bhdata_pg::PgError::client_error`].
//!
//! The one thing this module does not do is queueing: callers hold the pool's connections only for
//! as long as [`Executor::run`] takes, and slice C puts the per-shard queue in front of it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bhdata_api::recipe::eval::{eval, Context, EvalError, Value};
use bhdata_api::recipe::expr::parse_expr;
use bhdata_api::recipe::model::Who;
use bhdata_api::recipe::plan::{Limit, Param, PlanArg, PlanStep, Source, StepPlan};
use bhdata_api::shard::{Command, Outcome, Retention, Status};
use bhdata_api::types::config::{Config, WriterConfig};
use bhdata_api::types::{Clock, Error, SubjectId, Timestamp};
use bhdata_changelog::entry::{child_row_key, subject_row_key, Collector};
use bhdata_pg::errors::OVERLOAD_RETRY_AFTER;
use bhdata_pg::pool::{pg_error, PoolError, Pools};
use bhdata_pg::PgError;
use deadpool_postgres::Transaction;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::Row;

use crate::convert::{bind, kind_of, read_value, Bound, Kind};
use crate::result::{done_result_items, failed_result, stored_outcome, Item};
use crate::retry::{after_failure, Next};
use crate::time_rule::check_request_time;

const CLAIM_SQL: &str = "SELECT out_outcome, out_status, out_error_kind, out_result \
    FROM bhdata_internal.claim_request($1, $2, $3, $4, $5, $6, \
    ('epoch'::timestamptz + $7::bigint * interval '1 millisecond'), $8)";
const APPEND_SQL: &str = "SELECT bhdata_internal.append_change($1, $2, $3, $4, $5, $6)";
const FINISH_SQL: &str = "SELECT bhdata_internal.finish_request($1, $2, $3, $4, $5)";
const SAVEPOINT: &str = "SAVEPOINT bh_steps";
const ROLLBACK_TO: &str = "ROLLBACK TO SAVEPOINT bh_steps";
const RELEASE: &str = "RELEASE SAVEPOINT bh_steps";

/// Whether this shard owns a subject. Stage 0 has one shard and owns everything; the planner's
/// shard map replaces it at M5. A command whose subject is not owned is `wrong_shard`.
pub type OwnsFn = Arc<dyn Fn(&SubjectId) -> bool + Send + Sync>;

/// What the executor needs from the config.
#[derive(Debug, Clone)]
pub struct ExecConfig {
    /// The write path's settings.
    pub writer: WriterConfig,
    /// How long a stored result can answer a retry (`requests.retry_window_secs`).
    pub retry_window: Duration,
    /// Most values in a result (`recipes.max_result_values`).
    pub max_result_values: usize,
    /// Largest result in bytes (`recipes.max_result_bytes`).
    pub max_result_bytes: usize,
}

impl ExecConfig {
    /// The settings of a loaded config.
    #[must_use]
    pub fn from_config(config: &Config) -> ExecConfig {
        ExecConfig {
            writer: config.writer.clone(),
            retry_window: Duration::from_secs(config.requests.retry_window_secs),
            max_result_values: config.recipes.max_result_values as usize,
            max_result_bytes: config.recipes.max_result_bytes as usize,
        }
    }
}

/// A command that ended: the outcome, and how long the commit took (`None` for a replay, which
/// writes nothing). The queue feeds the commit time to the latency watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// What the record holds.
    pub outcome: Outcome,
    /// The time `COMMIT` took.
    pub commit_time: Option<Duration>,
}

/// Why an attempt stopped.
enum Fail {
    /// The database refused; the retry rule decides.
    Pg(PgError),
    /// Final: this is the reply, nothing was stored.
    Err(Error),
}

impl From<tokio_postgres::Error> for Fail {
    fn from(e: tokio_postgres::Error) -> Fail {
        Fail::Pg(pg_error(&e))
    }
}

impl From<PoolError> for Fail {
    fn from(e: PoolError) -> Fail {
        Fail::Err(e.client_error())
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Fail {
        Fail::Err(e)
    }
}

fn internal(what: impl Into<String>) -> Error {
    Error::Internal(what.into())
}

fn eval_error(e: &EvalError) -> Error {
    internal(format!("evaluating the plan: {e}"))
}

/// Runs commands on the recipe pool.
pub struct Executor {
    pools: Arc<Pools>,
    clock: Arc<dyn Clock>,
    cfg: ExecConfig,
    owns: OwnsFn,
}

/// What a run builds up while its steps go.
struct Run {
    ctx: Context,
    lists: BTreeMap<String, Vec<Vec<(String, Value)>>>,
    collector: Collector,
}

enum Ended {
    Done,
    Named(String),
}

impl Executor {
    /// An executor that owns every subject.
    #[must_use]
    pub fn new(pools: Arc<Pools>, clock: Arc<dyn Clock>, cfg: ExecConfig) -> Executor {
        Executor {
            pools,
            clock,
            cfg,
            owns: Arc::new(|_| true),
        }
    }

    /// Replaces the ownership rule.
    #[must_use]
    pub fn with_owner_check(mut self, owns: OwnsFn) -> Executor {
        self.owns = owns;
        self
    }

    /// Runs one command to its final outcome, or says why it could not. An `Err` stored nothing,
    /// so the caller may retry it with the same request id.
    ///
    /// # Errors
    /// `bad_arguments`, `wrong_shard`, `request_expired`, `request_id_conflict`, `overloaded`,
    /// `internal`.
    pub async fn run(&self, cmd: &Command) -> Result<Finished, Error> {
        self.precheck(cmd)?;
        let hash = hex_decode(&cmd.payload_hash());
        let mut attempt = 1u32;
        loop {
            let now = self.clock.now();
            check_request_time(
                now,
                cmd.requested_at,
                self.cfg.retry_window,
                self.cfg.writer.request_time_tolerance(),
            )?;
            match self.attempt(cmd, &hash, now).await {
                Ok(done) => return Ok(done),
                Err(Fail::Err(e)) => return Err(e),
                Err(Fail::Pg(e)) => {
                    let jitter = u64::from(self.clock.monotonic().subsec_nanos());
                    match after_failure(&e, attempt, self.cfg.writer.max_attempts, jitter) {
                        Next::Retry { pause } => {
                            tokio::time::sleep(pause).await;
                            attempt += 1;
                        }
                        Next::GiveUp => return Err(e.client_error(true)),
                    }
                }
            }
        }
    }

    /// Step 0, the parts that need no database.
    fn precheck(&self, cmd: &Command) -> Result<(), Error> {
        cmd.check_shape()?;
        match (&cmd.plan.counterparty, &cmd.counterparty) {
            (Some(_), None) => {
                return Err(Error::BadArguments(
                    "this recipe needs a counterparty".into(),
                ))
            }
            (None, Some(_)) => {
                return Err(Error::BadArguments(
                    "this recipe has no counterparty".into(),
                ))
            }
            _ => {}
        }
        check_bounds(&cmd.plan.args, &cmd.args)?;
        let owned =
            (self.owns)(&cmd.subject) && cmd.counterparty.as_ref().is_none_or(|c| (self.owns)(c));
        if owned {
            Ok(())
        } else {
            Err(Error::WrongShard)
        }
    }

    async fn attempt(&self, cmd: &Command, hash: &[u8], now: Timestamp) -> Result<Finished, Fail> {
        let plan = &cmd.plan;
        let mut conn = self.pools.recipe().await?;
        let started = self.clock.monotonic();
        let tx = conn.transaction().await?;

        let requested_ms = i64::try_from(cmd.requested_at.as_unix_millis()).unwrap_or(i64::MAX);
        let permanent = matches!(cmd.retention, Retention::Permanent);
        let version = i32::try_from(cmd.version.get()).unwrap_or(i32::MAX);
        let claim = tx
            .query_one(
                CLAIM_SQL,
                &[
                    &cmd.login_id.as_str(),
                    &cmd.request_id.as_str(),
                    &hash,
                    &plan.name,
                    &version,
                    &cmd.subject.as_str(),
                    &requested_ms,
                    &permanent,
                ],
            )
            .await?;
        let claimed: String = claim.get(0);
        match claimed.as_str() {
            "new" => {}
            "same" => {
                let status: Option<String> = claim.get(1);
                let kind: Option<String> = claim.get(2);
                let result: Option<Vec<u8>> = claim.get(3);
                let result = String::from_utf8(result.unwrap_or_default())
                    .map_err(|_| internal("a stored result is not text"))?;
                let outcome = stored_outcome(
                    status.as_deref().unwrap_or(""),
                    kind.as_deref(),
                    result,
                    true,
                )?;
                tx.commit().await?;
                return Ok(Finished {
                    outcome,
                    commit_time: None,
                });
            }
            "conflict" => {
                tx.rollback().await?;
                return Err(Fail::Err(Error::RequestIdConflict));
            }
            other => return Err(internal(format!("claim_request answered `{other}`")).into()),
        }

        tx.batch_execute(SAVEPOINT).await?;
        let mut run = Run {
            ctx: Context {
                vars: cmd
                    .args
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                now_micros: i64::try_from(now.as_unix_millis())
                    .unwrap_or(i64::MAX)
                    .saturating_mul(1_000),
            },
            lists: BTreeMap::new(),
            collector: Collector::new(),
        };

        let login = cmd.login_id.as_str();
        let request = cmd.request_id.as_str();
        let (status, error_kind, result) = match self.steps(&tx, cmd, &mut run, started).await? {
            Ended::Named(name) => {
                tx.batch_execute(ROLLBACK_TO).await?;
                let text = failed_result(&name);
                ("failed", Some(name), text)
            }
            Ended::Done => {
                tx.batch_execute(RELEASE).await?;
                let items = self.result_items(&tx, cmd, &run).await?;
                let text = done_result_items(
                    &items,
                    self.cfg.max_result_values,
                    self.cfg.max_result_bytes,
                )?;
                for e in run.collector.entries() {
                    tx.execute(
                        APPEND_SQL,
                        &[
                            &login,
                            &request,
                            &e.subject.as_str(),
                            &e.table.as_str(),
                            &e.op.as_str(),
                            &e.row_key.as_str(),
                        ],
                    )
                    .await?;
                }
                ("done", None, text)
            }
        };
        tx.execute(
            FINISH_SQL,
            &[&login, &request, &status, &error_kind, &result.as_bytes()],
        )
        .await?;

        let before = self.clock.monotonic();
        tx.commit().await?;
        let commit_time = self.clock.monotonic().saturating_sub(before);
        let outcome = Outcome {
            status: match error_kind {
                None => Status::Done,
                Some(error) => Status::Failed { error },
            },
            result,
            replayed: false,
        };
        Ok(Finished {
            outcome,
            commit_time: Some(commit_time),
        })
    }

    async fn steps(
        &self,
        tx: &Transaction<'_>,
        cmd: &Command,
        run: &mut Run,
        started: Duration,
    ) -> Result<Ended, Fail> {
        let budget = Duration::from_millis(cmd.plan.bounds.time_ms);
        for step in &cmd.plan.steps {
            if self.clock.monotonic().saturating_sub(started) > budget {
                return Err(Fail::Err(Error::Overloaded {
                    retry_after: OVERLOAD_RETRY_AFTER,
                }));
            }
            if let Some(name) = step_run(tx, cmd, step, run).await? {
                return Ok(Ended::Named(name));
            }
        }
        Ok(Ended::Done)
    }

    /// The result values, in the order the plan writes them: from a `read`, a `list`, or the one
    /// final read of the subject row.
    async fn result_items(
        &self,
        tx: &Transaction<'_>,
        cmd: &Command,
        run: &Run,
    ) -> Result<Vec<(String, Item)>, Fail> {
        let plan = &cmd.plan;
        let mut fetched: BTreeMap<String, Value> = BTreeMap::new();
        if let Some(fetch) = &plan.result_fetch {
            if plan.result.iter().any(|r| r.source == Source::Fetch) {
                let rows = query(tx, &fetch.sql, &cmd.subject, &[]).await?;
                let row = rows.first().ok_or_else(|| missing(false))?;
                for (i, c) in fetch.columns.iter().enumerate() {
                    fetched.insert(c.name.clone(), read_value(row, i, &c.ty)?);
                }
            }
        }
        let mut items = Vec::with_capacity(plan.result.len());
        for r in &plan.result {
            let item = match r.source {
                Source::Fetch => fetched.get(&r.name).cloned().map(Item::One),
                Source::Step(_) => match run.lists.get(&r.name) {
                    Some(rows) => Some(Item::Rows(rows.clone())),
                    None => run.ctx.vars.get(&r.name).cloned().map(Item::One),
                },
            };
            let item = item.ok_or_else(|| {
                internal(format!("the result value `{}` was never produced", r.name))
            })?;
            items.push((r.name.clone(), item));
        }
        Ok(items)
    }
}

/// A subject or counterparty row that a step needed and that is not there: the recipe had no
/// `ensure`, and the id is new. Rolled back; the client can act on the text.
fn missing(counterparty: bool) -> Error {
    Error::BadArguments(format!(
        "the {} does not exist",
        if counterparty {
            "counterparty"
        } else {
            "subject"
        }
    ))
}

/// Runs one step. `Some(name)` is the recipe's named error.
async fn step_run(
    tx: &Transaction<'_>,
    cmd: &Command,
    step: &PlanStep,
    run: &mut Run,
) -> Result<Option<String>, Fail> {
    let plan = &cmd.plan;
    let is_counterparty = step.of == Some(Who::Counterparty);
    let owner: &SubjectId = if is_counterparty {
        cmd.counterparty
            .as_ref()
            .ok_or_else(|| internal("a counterparty step without a counterparty"))?
    } else {
        &cmd.subject
    };
    let table: &str = if is_counterparty {
        plan.counterparty.as_deref().unwrap_or_default()
    } else {
        &plan.subject
    };
    let who_missing = || Fail::Err(missing(is_counterparty));

    match &step.plan {
        StepPlan::Ensure { sql } => {
            let n = execute(tx, sql, owner, &[]).await?;
            run.collector.note(
                table,
                &subject_row_key(owner.as_str()),
                owner.as_str(),
                true,
                n,
            );
        }
        StepPlan::Read { sql, columns } => {
            let rows = query(tx, sql, owner, &[]).await?;
            let row = rows.first().ok_or_else(who_missing)?;
            for (i, c) in columns.iter().enumerate() {
                run.ctx
                    .vars
                    .insert(c.name.clone(), read_value(row, i, &c.ty)?);
            }
        }
        StepPlan::Check { require, error } => {
            let holds = match eval_text(require, &run.ctx)? {
                Value::Bool(b) => b,
                other => {
                    return Err(internal(format!(
                        "a check gave a {} value, not a bool",
                        other.ty().name()
                    ))
                    .into())
                }
            };
            if !holds {
                return Ok(Some(error.clone()));
            }
        }
        StepPlan::Add {
            sql,
            params,
            ty,
            below,
            above,
            miss_sql,
            ..
        } => {
            let values = eval_params(params, &run.ctx)?;
            let bound = bind_all(&values, params)?;
            let mut missed_once = false;
            loop {
                if !query(tx, sql, owner, &bound).await?.is_empty() {
                    run.collector.note(
                        table,
                        &subject_row_key(owner.as_str()),
                        owner.as_str(),
                        false,
                        1,
                    );
                    return Ok(None);
                }
                // No row matched: the row is missing, or a bound was crossed. Lock and read it,
                // and decide by exact comparison.
                let Some(miss) = miss_sql else {
                    return Err(who_missing());
                };
                if missed_once {
                    return Err(internal("a row kept changing under an `add`").into());
                }
                missed_once = true;
                let rows = query(tx, miss, owner, &[]).await?;
                let row = rows.first().ok_or_else(who_missing)?;
                let current = read_value(row, 0, ty)?;
                let by = values
                    .first()
                    .ok_or_else(|| internal("an `add` without an amount"))?;
                if let Some(name) = verdict(&current, by, below.as_ref(), above.as_ref())? {
                    return Ok(Some(name));
                }
            }
        }
        StepPlan::Set { sql, params } => {
            let bound = bind_all(&eval_params(params, &run.ctx)?, params)?;
            if execute(tx, sql, owner, &bound).await? == 0 {
                return Err(who_missing());
            }
            run.collector.note(
                table,
                &subject_row_key(owner.as_str()),
                owner.as_str(),
                false,
                1,
            );
        }
        StepPlan::Insert {
            lock_sql,
            sql,
            params,
        } => {
            let child = insert_table(sql).ok_or_else(|| {
                internal("the plan's insert statement is not in the shape this release writes")
            })?;
            let bound = bind_all(&eval_params(params, &run.ctx)?, params)?;
            if let Some(lock) = lock_sql {
                execute(tx, lock, owner, &[]).await?;
            }
            execute(tx, sql, owner, &bound).await?;
            // The advisory lock is held to the end of the transaction, so the highest `seq` is ours.
            let seq_sql =
                format!("SELECT MAX(\"seq\") FROM app.\"{child}\" WHERE \"owner_id\" = $1");
            let rows = query(tx, &seq_sql, owner, &[]).await?;
            let seq: Option<i64> = rows
                .first()
                .ok_or_else(|| internal("no row after an insert"))?
                .try_get(0)
                .map_err(|e| internal(format!("reading the new seq: {e}")))?;
            let seq = seq.ok_or_else(|| internal("no seq after an insert"))?;
            run.collector.note(
                child,
                &child_row_key(owner.as_str(), seq),
                owner.as_str(),
                true,
                1,
            );
        }
        StepPlan::List {
            sql, columns, into, ..
        } => {
            let rows = query(tx, sql, owner, &[]).await?;
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut one = Vec::with_capacity(columns.len());
                for (i, c) in columns.iter().enumerate() {
                    one.push((c.name.clone(), read_value(row, i, &c.ty)?));
                }
                out.push(one);
            }
            run.lists.insert(into.clone(), out);
        }
        StepPlan::ReadDerived { .. } => {
            return Err(internal("a derived read is not a write and cannot run here").into())
        }
    }
    Ok(None)
}

/// The child table of an `INSERT INTO app."<table>" ...` statement. The plan does not carry the
/// table by itself (and its text is frozen), but the compiler writes the statement in exactly this
/// shape, and anything else is refused.
fn insert_table(sql: &str) -> Option<&str> {
    let rest = sql.strip_prefix("INSERT INTO app.\"")?;
    let end = rest.find('"')?;
    let name = &rest[..end];
    (!name.is_empty()).then_some(name)
}

fn eval_text(text: &str, ctx: &Context) -> Result<Value, Error> {
    let expr = parse_expr(text).map_err(|m| internal(format!("plan expression `{text}`: {m}")))?;
    eval(&expr, ctx).map_err(|e| eval_error(&e))
}

fn eval_params(params: &[Param], ctx: &Context) -> Result<Vec<Value>, Error> {
    params.iter().map(|p| eval_text(&p.expr, ctx)).collect()
}

fn bind_all(values: &[Value], params: &[Param]) -> Result<Vec<Bound>, Error> {
    values
        .iter()
        .zip(params)
        .map(|(v, p)| bind(v, &p.ty))
        .collect()
}

async fn prepared(
    tx: &Transaction<'_>,
    sql: &str,
    bound: &[Bound],
) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
    let mut types: Vec<Type> = vec![Type::TEXT];
    types.extend(bound.iter().map(|b| b.ty.clone()));
    tx.prepare_typed_cached(sql, &types).await
}

fn args<'a>(owner: &'a String, bound: &'a [Bound]) -> Vec<&'a (dyn ToSql + Sync)> {
    let mut out: Vec<&(dyn ToSql + Sync)> = vec![owner];
    for b in bound {
        out.push(&*b.value);
    }
    out
}

async fn query(
    tx: &Transaction<'_>,
    sql: &str,
    owner: &SubjectId,
    bound: &[Bound],
) -> Result<Vec<Row>, tokio_postgres::Error> {
    let stmt = prepared(tx, sql, bound).await?;
    let owner = owner.as_str().to_string();
    tx.query(&stmt, &args(&owner, bound)).await
}

async fn execute(
    tx: &Transaction<'_>,
    sql: &str,
    owner: &SubjectId,
    bound: &[Bound],
) -> Result<u64, tokio_postgres::Error> {
    let stmt = prepared(tx, sql, bound).await?;
    let owner = owner.as_str().to_string();
    tx.execute(&stmt, &args(&owner, bound)).await
}

/// After an `add` matched no row: which named error does the exact arithmetic give? `None` when
/// neither bound is crossed any more (another command changed the row meanwhile).
fn verdict(
    current: &Value,
    by: &Value,
    below: Option<&Limit>,
    above: Option<&Limit>,
) -> Result<Option<String>, Error> {
    if let Some(l) = below {
        if crossed(current, by, &l.limit, "(a + b) < c", true)? {
            return Ok(Some(l.error.clone()));
        }
    }
    if let Some(l) = above {
        if crossed(current, by, &l.limit, "(a + b) > c", false)? {
            return Ok(Some(l.error.clone()));
        }
    }
    Ok(None)
}

fn negative(v: &Value) -> bool {
    match v {
        Value::I64(n) => *n < 0,
        Value::Dec { value, .. } => *value < 0,
        _ => false,
    }
}

/// Evaluates a comparison over named values (`a`, `b`, `c`).
fn holds(test: &str, vars: [(&str, &Value); 3]) -> Result<bool, EvalError> {
    let ctx = Context {
        vars: vars
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        now_micros: 0,
    };
    let expr = parse_expr(test).map_err(|m| EvalError::Type(format!("`{test}`: {m}")))?;
    match eval(&expr, &ctx)? {
        Value::Bool(b) => Ok(b),
        other => Err(EvalError::Type(format!(
            "`{test}` gave a {} value",
            other.ty().name()
        ))),
    }
}

fn crossed(
    current: &Value,
    by: &Value,
    limit: &str,
    test: &str,
    is_below: bool,
) -> Result<bool, Error> {
    let c = eval_text(limit, &Context::default())?;
    match holds(test, [("a", current), ("b", by), ("c", &c)]) {
        Ok(b) => Ok(b),
        // The sum does not even fit the type: it went past the end the amount points to.
        Err(EvalError::Overflow) => Ok(negative(by) == is_below),
        Err(e) => Err(eval_error(&e)),
    }
}

/// Step 0, bounds: each argument inside the plan's `min` and `max` (compared exactly), and a text
/// or bytes argument inside its length.
fn check_bounds(plan_args: &[PlanArg], given: &BTreeMap<String, Value>) -> Result<(), Error> {
    for a in plan_args {
        let Some(v) = given.get(&a.name) else {
            continue; // `check_shape` has already reported a missing argument
        };
        for (limit, test, what) in [(&a.min, "a < c", "at least"), (&a.max, "a > c", "at most")] {
            let Some(text) = limit else { continue };
            let c = eval_text(text, &Context::default())?;
            let outside = holds(test, [("a", v), ("b", v), ("c", &c)])
                .map_err(|e| internal(format!("bounds of `{}`: {e}", a.name)))?;
            if outside {
                return Err(Error::BadArguments(format!(
                    "argument `{}` must be {what} {text}",
                    a.name
                )));
            }
        }
        let max_len =
            a.ty.split_once('(')
                .and_then(|(_, r)| r.strip_suffix(')'))
                .and_then(|n| n.parse::<usize>().ok());
        let len = match (kind_of(&a.ty)?, v, max_len) {
            (Kind::Text, Value::Text(t), Some(n)) => Some((t.chars().count(), n, "characters")),
            (Kind::Bytes, Value::Bytes(b), Some(n)) => Some((b.len(), n, "bytes")),
            _ => None,
        };
        if let Some((have, most, unit)) = len {
            if have > most {
                return Err(Error::BadArguments(format!(
                    "argument `{}` has {have} {unit}, at most {most} are allowed",
                    a.name
                )));
            }
        }
    }
    Ok(())
}

/// The payload hash is 64 lower-case hex digits that this release made itself.
fn hex_decode(hex: &str) -> Vec<u8> {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|p| u8::from_str_radix(p, 16).ok())
                .unwrap_or(0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(name: &str, ty: &str, min: Option<&str>, max: Option<&str>) -> PlanArg {
        PlanArg {
            name: name.into(),
            ty: ty.into(),
            min: min.map(str::to_string),
            max: max.map(str::to_string),
        }
    }

    fn given(items: &[(&str, Value)]) -> BTreeMap<String, Value> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn dec(value: i128, scale: u8) -> Value {
        Value::Dec { value, scale }
    }

    #[test]
    fn the_table_of_an_insert_is_read_from_the_statement_the_compiler_writes() {
        let sql = "INSERT INTO app.\"entries\" (\"owner_id\", \"seq\") VALUES ($1, 1)";
        assert_eq!(insert_table(sql), Some("entries"));
        for bad in [
            "",
            "INSERT INTO app.\"\" (",
            "INSERT INTO other.\"t\" (",
            "UPDATE app.\"t\"",
        ] {
            assert_eq!(insert_table(bad), None, "{bad}");
        }
    }

    #[test]
    fn arguments_outside_min_and_max_are_bad_arguments_naming_the_argument() {
        let plan = [arg("amount", "i64", Some("1"), Some("1000000"))];
        assert!(check_bounds(&plan, &given(&[("amount", Value::I64(1))])).is_ok());
        assert!(check_bounds(&plan, &given(&[("amount", Value::I64(1_000_000))])).is_ok());
        for bad in [0, -5, 1_000_001] {
            let e = check_bounds(&plan, &given(&[("amount", Value::I64(bad))])).unwrap_err();
            assert!(
                matches!(&e, Error::BadArguments(m) if m.contains("`amount`")),
                "{bad}: {e}"
            );
        }
    }

    #[test]
    fn decimal_bounds_compare_across_scales_exactly() {
        let plan = [arg("rate", "decimal(18,2)", Some("0.01"), Some("5"))];
        assert!(check_bounds(&plan, &given(&[("rate", dec(1, 2))])).is_ok());
        assert!(check_bounds(&plan, &given(&[("rate", dec(500, 2))])).is_ok());
        assert!(check_bounds(&plan, &given(&[("rate", dec(0, 2))])).is_err());
        assert!(check_bounds(&plan, &given(&[("rate", dec(501, 2))])).is_err());
    }

    #[test]
    fn text_and_bytes_are_limited_by_length() {
        let t = [arg("who", "text(3)", None, None)];
        assert!(check_bounds(&t, &given(&[("who", Value::Text("héé".into()))])).is_ok());
        assert!(check_bounds(&t, &given(&[("who", Value::Text("abcd".into()))])).is_err());
        let b = [arg("raw", "bytes(2)", None, None)];
        assert!(check_bounds(&b, &given(&[("raw", Value::Bytes(vec![1, 2]))])).is_ok());
        assert!(check_bounds(&b, &given(&[("raw", Value::Bytes(vec![1, 2, 3]))])).is_err());
    }

    fn limit(error: &str, text: &str) -> Limit {
        Limit {
            error: error.into(),
            limit: text.into(),
        }
    }

    #[test]
    fn the_named_error_is_picked_by_exact_comparison() {
        let below = limit("insufficient_funds", "0");
        let above = limit("overflow", "1000000000");
        let v = |cur, by| {
            verdict(
                &Value::I64(cur),
                &Value::I64(by),
                Some(&below),
                Some(&above),
            )
            .unwrap()
        };
        assert_eq!(v(5, -6).as_deref(), Some("insufficient_funds"));
        assert_eq!(v(5, -5), None);
        assert_eq!(v(999_999_999, 2).as_deref(), Some("overflow"));
        assert_eq!(v(999_999_999, 1), None);
        // Past the end of i64 itself, in the direction the amount points.
        assert_eq!(v(i64::MAX, 1).as_deref(), Some("overflow"));
        assert_eq!(v(i64::MIN, -1).as_deref(), Some("insufficient_funds"));
    }

    #[test]
    fn a_missing_bound_is_never_crossed() {
        let r = verdict(&Value::I64(0), &Value::I64(-1), None, None).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn decimals_cross_a_bound_exactly() {
        let below = limit("insufficient_funds", "0.00");
        let r = verdict(&dec(100, 2), &dec(-101, 2), Some(&below), None).unwrap();
        assert_eq!(r.as_deref(), Some("insufficient_funds"));
        let r = verdict(&dec(100, 2), &dec(-100, 2), Some(&below), None).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn the_hash_text_becomes_its_bytes() {
        assert_eq!(hex_decode("00ff10"), vec![0, 255, 16]);
        assert_eq!(hex_decode("").len(), 0);
    }

    #[test]
    fn a_subject_that_is_not_there_is_bad_arguments() {
        assert!(matches!(missing(false), Error::BadArguments(m) if m.contains("subject")));
        assert!(matches!(missing(true), Error::BadArguments(m) if m.contains("counterparty")));
    }
}
