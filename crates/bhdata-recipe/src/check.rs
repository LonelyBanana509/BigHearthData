//! The compiler's checks (spec `recipe-compiler.md`, sections 2 and 3). Input: a schema and a parsed
//! recipe; output: the lane and the named errors, or the first [`Rejection`]. Pure.
//!
//! Order: tables, then columns, then keys (each over the whole recipe); then step by step the
//! types, rounding and bounds of every expression; then the result, the error names, and last the
//! lane and derived reads. The first failure is the rejection; nothing is ever fixed for the author.

use std::collections::{BTreeMap, BTreeSet};

use bhdata_schema::model::{Column, ColumnType, Schema, Table, TableKind};
use bhdata_types::error::ErrorKind;

use crate::model::{Op, Recipe, Who};
use crate::parse::Limits;
use crate::reject::{Code, Rejection};
use crate::typing::{eval, Env, Fail, Ty, Typed};

/// How many shards a command touches (spec section 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Every step is keyed by the subject's id.
    OneShard,
    /// A counterparty is declared and used: steps are split per key.
    TwoShards,
    /// Only a `read_derived`.
    AllShards,
}

impl Lane {
    /// The name used in plans. Frozen: plans and their hashes contain it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::OneShard => "one_shard",
            Lane::TwoShards => "two_shards",
            Lane::AllShards => "all_shards",
        }
    }
}

/// What a derived store promises (the real declarations arrive with the derived stores).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedStore {
    /// The store's name.
    pub name: String,
    /// Its declared windows.
    pub windows: Vec<String>,
    /// The freshest result it can promise, in milliseconds.
    pub freshest_ms: u64,
}

/// What the checks learned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// The lane.
    pub lane: Lane,
    /// The named errors the recipe can raise, in order of first use.
    pub errors: Vec<String>,
}

fn reject(code: Code, step: Option<usize>, message: impl Into<String>) -> Rejection {
    Rejection::new(code, step, message)
}

fn at(step: usize) -> impl Fn(Fail) -> Rejection {
    move |f| reject(f.code, Some(step), f.message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    Below,
    Above,
    Check,
}

fn who(op: &Op) -> Option<Who> {
    match op {
        Op::Ensure { of }
        | Op::Read { of, .. }
        | Op::Add { of, .. }
        | Op::Set { of, .. }
        | Op::Insert { of, .. }
        | Op::List { of, .. } => Some(*of),
        Op::Check { .. } | Op::ReadDerived { .. } => None,
    }
}

fn table_of(op: &Op) -> Option<&str> {
    match op {
        Op::Read { table, .. }
        | Op::Add { table, .. }
        | Op::Set { table, .. }
        | Op::Insert { table, .. }
        | Op::List { table, .. } => Some(table),
        _ => None,
    }
}

fn columns_of(op: &Op) -> Vec<&str> {
    match op {
        Op::Read { columns, .. } | Op::List { columns, .. } => {
            columns.iter().map(String::as_str).collect()
        }
        Op::Add { column, .. } => vec![column],
        Op::Set { values, .. } | Op::Insert { values, .. } => {
            values.iter().map(|(c, _)| c.as_str()).collect()
        }
        _ => Vec::new(),
    }
}

fn owner_table(recipe: &Recipe, of: Who) -> &str {
    match of {
        Who::Subject => &recipe.subject,
        Who::Counterparty => recipe.counterparty.as_deref().unwrap_or(&recipe.subject),
    }
}

fn size_of(ty: ColumnType) -> usize {
    match ty {
        ColumnType::I64 | ColumnType::Timestamp => 8,
        ColumnType::Decimal { .. } => 16,
        ColumnType::Bool => 1,
        ColumnType::Text { max_len } => max_len as usize * 4,
        ColumnType::Bytes { max_len } => max_len as usize,
    }
}

/// Runs every check.
///
/// # Errors
/// The first [`Rejection`], by the order in the module description.
pub fn check(
    schema: &Schema,
    recipe: &Recipe,
    derived: &[DerivedStore],
    limits: &Limits,
) -> Result<Checked, Rejection> {
    tables(schema, recipe)?;
    columns(schema, recipe)?;
    keys(schema, recipe)?;
    let walked = walk(schema, recipe)?;
    result(schema, recipe, &walked, limits)?;
    let errors = error_names(&walked.errors)?;
    let lane = lane(recipe, derived)?;
    Ok(Checked { lane, errors })
}

fn table<'a>(schema: &'a Schema, name: &str, step: Option<usize>) -> Result<&'a Table, Rejection> {
    schema.table(name).ok_or_else(|| {
        reject(
            Code::UnknownTable,
            step,
            format!("the schema has no table `{name}`"),
        )
    })
}

fn tables(schema: &Schema, recipe: &Recipe) -> Result<(), Rejection> {
    for (what, name) in [
        ("subject", Some(&recipe.subject)),
        ("counterparty", recipe.counterparty.as_ref()),
    ] {
        if let Some(name) = name {
            if !matches!(table(schema, name, None)?.kind, TableKind::Subject) {
                return Err(reject(
                    Code::UnknownTable,
                    None,
                    format!("the {what} `{name}` is not a subject table"),
                ));
            }
        }
    }
    for (i, step) in recipe.steps.iter().enumerate() {
        let Some(name) = table_of(&step.op) else {
            continue;
        };
        let t = table(schema, name, Some(i + 1))?;
        let writes = matches!(step.op, Op::Add { .. } | Op::Set { .. } | Op::Insert { .. });
        if writes && matches!(t.kind, TableKind::Global { .. }) {
            return Err(reject(
                Code::UnknownTable,
                Some(i + 1),
                format!("`{name}` is a global table; recipes never write it"),
            ));
        }
    }
    Ok(())
}

fn columns(schema: &Schema, recipe: &Recipe) -> Result<(), Rejection> {
    for (i, step) in recipe.steps.iter().enumerate() {
        let Some(name) = table_of(&step.op) else {
            continue;
        };
        let t = table(schema, name, Some(i + 1))?;
        for col in columns_of(&step.op) {
            if t.column(col).is_none() {
                let why = if t.kind.implicit_columns().contains(&col) {
                    "is implicit: it is never named in a step"
                } else {
                    "does not exist"
                };
                return Err(reject(
                    Code::UnknownColumn,
                    Some(i + 1),
                    format!("column `{col}` of `{name}` {why}"),
                ));
            }
        }
    }
    Ok(())
}

fn keys(schema: &Schema, recipe: &Recipe) -> Result<(), Rejection> {
    for (i, step) in recipe.steps.iter().enumerate() {
        let (Some(name), Some(of)) = (table_of(&step.op), who(&step.op)) else {
            continue;
        };
        let t = table(schema, name, Some(i + 1))?;
        let owner = owner_table(recipe, of);
        let by_key = match (&step.op, &t.kind) {
            (Op::Insert { .. } | Op::List { .. }, TableKind::Child { owner: o }) => o == owner,
            (Op::Insert { .. } | Op::List { .. }, _) => false,
            _ => name == owner,
        };
        if !by_key {
            return Err(reject(
                Code::NotByKey,
                Some(i + 1),
                format!("`{name}` is not reached by the id of a `{owner}` row: steps touch only the subject's (or counterparty's) row and its child rows"),
            ));
        }
    }
    Ok(())
}

struct Walked {
    errors: Vec<(String, Use, usize)>,
    reads: BTreeSet<String>,
    lists: BTreeMap<String, usize>,
    sizes: BTreeMap<String, usize>,
}

fn fit(value: Ty, column: ColumnType, name: &str) -> Result<(), Fail> {
    let fail = |code, message: String| Err(Fail { code, message });
    match (value, column) {
        (Ty::I64, ColumnType::I64 | ColumnType::Decimal { .. })
        | (Ty::Bool, ColumnType::Bool)
        | (Ty::Text, ColumnType::Text { .. })
        | (Ty::Bytes, ColumnType::Bytes { .. })
        | (Ty::Timestamp, ColumnType::Timestamp) => Ok(()),
        (Ty::Dec(s), ColumnType::Decimal { scale, .. }) if s <= scale => Ok(()),
        (Ty::Dec(s), ColumnType::Decimal { scale, .. }) => fail(
            Code::Rounding,
            format!("the value has {s} digits after the point but `{name}` keeps {scale}: wrap it in round(..., {scale}, mode)"),
        ),
        (Ty::Dec(_), ColumnType::I64) => fail(
            Code::Rounding,
            format!("a decimal cannot be stored in the i64 column `{name}`: wrap it in round(..., 0, mode)"),
        ),
        (v, _) => fail(Code::Type, format!("`{name}` cannot hold {}", v.name())),
    }
}

fn walk(schema: &Schema, recipe: &Recipe) -> Result<Walked, Rejection> {
    let mut env = Env::new();
    let mut arg_names = BTreeSet::new();
    for a in &recipe.args {
        env.insert(a.name.clone(), Typed::of_column(a.ty, a.min, a.max));
        arg_names.insert(a.name.clone());
    }
    if recipe.counterparty.is_some() {
        env.insert("to".into(), Typed::plain(Ty::Text));
        arg_names.insert("to".into());
    }
    let mut w = Walked {
        errors: Vec::new(),
        reads: BTreeSet::new(),
        lists: BTreeMap::new(),
        sizes: BTreeMap::new(),
    };

    for (i, step) in recipe.steps.iter().enumerate() {
        let n = i + 1;
        let t = table_of(&step.op).and_then(|name| schema.table(name));
        let col = |name: &str| -> Result<&Column, Rejection> {
            t.and_then(|t| t.column(name)).ok_or_else(|| {
                reject(
                    Code::UnknownColumn,
                    Some(n),
                    format!("column `{name}` does not exist"),
                )
            })
        };
        match &step.op {
            Op::Ensure { .. } | Op::ReadDerived { .. } => {}
            Op::Read { of, columns, .. } => {
                for name in columns {
                    let c = col(name)?;
                    let key = if *of == Who::Counterparty {
                        format!("to.{name}")
                    } else {
                        name.clone()
                    };
                    if arg_names.contains(&key) {
                        return Err(reject(
                            Code::Type,
                            Some(n),
                            format!("`{key}` is already the name of an argument"),
                        ));
                    }
                    env.insert(key.clone(), Typed::of_column(c.ty, c.min, c.max));
                    w.sizes.insert(key.clone(), size_of(c.ty));
                    w.reads.insert(key);
                }
            }
            Op::Check { require, error } => {
                let v = eval(require, &env).map_err(at(n))?;
                if v.ty != Ty::Bool {
                    return Err(reject(
                        Code::Type,
                        Some(n),
                        format!("`require` must be true or false, not {}", v.ty.name()),
                    ));
                }
                w.errors.push((error.clone(), Use::Check, n));
            }
            Op::Add {
                column,
                by,
                below,
                above,
                ..
            } => {
                let c = col(column)?;
                if !c.ty.is_numeric() {
                    return Err(reject(
                        Code::Type,
                        Some(n),
                        format!("`add` needs an i64 or decimal column, and `{column}` is not"),
                    ));
                }
                let v = eval(by, &env).map_err(at(n))?;
                if !v.ty.numeric() {
                    return Err(reject(
                        Code::Type,
                        Some(n),
                        format!("`by` needs a number, not {}", v.ty.name()),
                    ));
                }
                fit(v.ty, c.ty, column).map_err(at(n))?;
                // The column is already inside its bounds, so only a side the amount can push
                // across needs a named error.
                let can_fall = v.range.lo.is_none_or(|lo| lo < 0);
                let can_rise = v.range.hi.is_none_or(|hi| hi > 0);
                if c.min.is_some() && can_fall && below.is_none() {
                    return Err(reject(
                        Code::Bounds,
                        Some(n),
                        format!("`{column}` can fall under its `min`: name an error with `below`"),
                    ));
                }
                if c.max.is_some() && can_rise && above.is_none() {
                    return Err(reject(
                        Code::Bounds,
                        Some(n),
                        format!("`{column}` can rise over its `max`: name an error with `above`"),
                    ));
                }
                if let Some(e) = below {
                    w.errors.push((e.clone(), Use::Below, n));
                }
                if let Some(e) = above {
                    w.errors.push((e.clone(), Use::Above, n));
                }
            }
            Op::Set { values, .. } | Op::Insert { values, .. } => {
                for (name, e) in values {
                    let c = col(name)?;
                    let v = eval(e, &env).map_err(at(n))?;
                    fit(v.ty, c.ty, name).map_err(at(n))?;
                    if c.ty.is_numeric() {
                        let r = v.range.shift(v.ty.scale(), c.ty.scale());
                        let low_ok = c.min.is_none_or(|m| r.lo.is_some_and(|l| l >= m));
                        let high_ok = c.max.is_none_or(|m| r.hi.is_some_and(|h| h <= m));
                        if !low_ok || !high_ok {
                            return Err(reject(
                                Code::Bounds,
                                Some(n),
                                format!("the value for `{name}` can leave the column's `min`/`max`, and `set` and `insert` have no named error for that"),
                            ));
                        }
                    }
                }
                if let (Op::Insert { .. }, Some(t)) = (&step.op, t) {
                    if let Some(missing) = t
                        .columns
                        .iter()
                        .find(|c| !c.nullable && !values.iter().any(|(v, _)| *v == c.name))
                    {
                        return Err(reject(
                            Code::Type,
                            Some(n),
                            format!(
                                "column `{}` is required (not nullable) and has no default",
                                missing.name
                            ),
                        ));
                    }
                }
            }
            Op::List {
                columns,
                limit,
                into,
                ..
            } => {
                if arg_names.contains(into) || w.reads.contains(into) || w.lists.contains_key(into)
                {
                    return Err(reject(
                        Code::Type,
                        Some(n),
                        format!("`{into}` is already used as a name"),
                    ));
                }
                let row: usize = columns
                    .iter()
                    .filter_map(|c| col(c).ok())
                    .map(|c| size_of(c.ty))
                    .sum();
                w.sizes.insert(into.clone(), row * *limit as usize);
                w.lists.insert(into.clone(), *limit as usize);
            }
        }
    }
    Ok(w)
}

fn result(schema: &Schema, recipe: &Recipe, w: &Walked, limits: &Limits) -> Result<(), Rejection> {
    let subject = table(schema, &recipe.subject, None)?;
    let mut bytes = 0usize;
    for entry in &recipe.result {
        if let Some(size) = w.sizes.get(entry) {
            bytes = bytes.saturating_add(*size);
        } else if let Some(c) = subject.column(entry) {
            bytes = bytes.saturating_add(size_of(c.ty));
        } else {
            return Err(reject(
                Code::UnknownColumn,
                None,
                format!("result entry `{entry}` is not a column of the subject row, a value read by a step or a list"),
            ));
        }
    }
    if bytes > limits.max_result_bytes {
        return Err(reject(
            Code::TooBig,
            None,
            format!(
                "the result can reach {bytes} bytes; at most {} are allowed",
                limits.max_result_bytes
            ),
        ));
    }
    Ok(())
}

fn error_names(used: &[(String, Use, usize)]) -> Result<Vec<String>, Rejection> {
    let mut out: Vec<String> = Vec::new();
    for (name, how, step) in used {
        if let Some(kind) = ErrorKind::from_name(name) {
            let (ok, allowed) = match kind {
                ErrorKind::Overflow => (
                    *how == Use::Above,
                    "only as `above`, where it means the result went over `max`",
                ),
                ErrorKind::InsufficientFunds => (
                    matches!(how, Use::Below | Use::Check),
                    "only as `below` or in a `check`, where it means not enough",
                ),
                _ => (false, "recipes name their own errors instead"),
            };
            if !ok {
                return Err(reject(
                    Code::ErrorName,
                    Some(*step),
                    format!("`{name}` is a built-in error name: {allowed}"),
                ));
            }
        }
        if !out.contains(name) {
            out.push(name.clone());
        }
    }
    Ok(out)
}

fn lane(recipe: &Recipe, derived: &[DerivedStore]) -> Result<Lane, Rejection> {
    if recipe.steps.is_empty() {
        return Err(reject(Code::Lane, None, "a recipe needs at least one step"));
    }
    if let Some((
        i,
        Op::ReadDerived {
            store,
            window,
            max_age_ms,
            ..
        },
    )) = recipe
        .steps
        .iter()
        .enumerate()
        .map(|(i, s)| (i, &s.op))
        .find(|(_, op)| matches!(op, Op::ReadDerived { .. }))
    {
        if recipe.steps.len() != 1 || recipe.counterparty.is_some() {
            return Err(reject(
                Code::Lane,
                Some(i + 1),
                "a `read_derived` is the whole recipe: no other step and no counterparty",
            ));
        }
        let Some(s) = derived.iter().find(|s| s.name == *store) else {
            return Err(reject(
                Code::Derived,
                Some(1),
                format!("no derived store `{store}` is declared"),
            ));
        };
        if !s.windows.contains(window) {
            return Err(reject(
                Code::Derived,
                Some(1),
                format!("store `{store}` has no window `{window}`"),
            ));
        }
        if *max_age_ms < s.freshest_ms {
            return Err(reject(
                Code::Derived,
                Some(1),
                format!(
                    "store `{store}` cannot promise results fresher than {} ms",
                    s.freshest_ms
                ),
            ));
        }
        return Ok(Lane::AllShards);
    }
    if recipe.counterparty.is_none() {
        return Ok(Lane::OneShard);
    }
    if recipe
        .steps
        .iter()
        .any(|s| who(&s.op) == Some(Who::Counterparty))
    {
        Ok(Lane::TwoShards)
    } else {
        Err(reject(
            Code::Lane,
            None,
            "a counterparty is declared and no step uses it",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_recipe;
    use bhdata_schema::parse_schema;

    const SCHEMA: &str = r#"
[[table]]
name = "users"
kind = "subject"
columns = [
  { name = "balance", type = "i64", min = 0, max = 1_000_000_000 },
  { name = "points", type = "i64" },
  { name = "interest", type = "decimal", precision = 18, scale = 2, min = "0" },
  { name = "name", type = "text", max_len = 64, nullable = true },
]

[[table]]
name = "entries"
kind = "child"
owner = "users"
columns = [{ name = "delta", type = "i64" }, { name = "at", type = "timestamp" }, { name = "note", type = "text", max_len = 100, nullable = true }]

[[table]]
name = "teams"
kind = "subject"
columns = [{ name = "size", type = "i64" }]

[[table]]
name = "notes"
kind = "child"
owner = "teams"
columns = [{ name = "n", type = "i64" }]

[[table]]
name = "shop"
kind = "global"
key = "item"
columns = [{ name = "item", type = "text", max_len = 40 }, { name = "price", type = "i64" }]
"#;

    fn schema() -> Schema {
        parse_schema(SCHEMA).unwrap()
    }

    fn stores() -> Vec<DerivedStore> {
        vec![DerivedStore {
            name: "leaderboard".into(),
            windows: vec!["weekly".into(), "all_time".into()],
            freshest_ms: 1000,
        }]
    }

    /// A recipe over `users` with the given arguments, steps and result, checked with `limits`.
    fn run(head: &str, steps: &str, result: &str, limits: &Limits) -> Result<Checked, Rejection> {
        let text = format!("{head}\n{steps}\n[recipe.result]\nreturn = [{result}]\n");
        let recipe = parse_recipe(&text, limits).unwrap();
        check(&schema(), &recipe, &stores(), limits)
    }

    const HEAD: &str = "[recipe]\nname = \"r\"\nsubject = \"users\"\nargs = { amount = { type = \"i64\", min = 1, max = 1_000_000 }, rate = { type = \"decimal\", precision = 9, scale = 6, min = \"0\", max = \"1\" } }";

    fn go(steps: &str) -> Result<Checked, Rejection> {
        run(HEAD, steps, "\"balance\"", &Limits::default())
    }

    fn code(steps: &str) -> (Code, Option<usize>) {
        let e = go(steps).unwrap_err();
        (e.code, e.step)
    }

    const SPEND: &str = r#"
[[step]]
op = "ensure"
[[step]]
op = "add"
table = "users"
column = "balance"
by = "-amount"
below = "insufficient_funds"
above = "overflow"
[[step]]
op = "insert"
table = "entries"
set = { delta = "-amount", at = "now" }
"#;

    #[test]
    fn a_plain_spend_is_one_shard_and_lists_its_errors_in_order() {
        let c = go(SPEND).unwrap();
        assert_eq!(c.lane, Lane::OneShard);
        assert_eq!(c.errors, ["insufficient_funds", "overflow"]);
    }

    #[test]
    fn a_give_with_a_counterparty_is_two_shards() {
        let head = "[recipe]\nname = \"give\"\nsubject = \"users\"\ncounterparty = \"users\"\nargs = { amount = { type = \"i64\", min = 1, max = 1_000_000 } }";
        let steps = r#"
[[step]]
op = "ensure"
[[step]]
op = "ensure"
of = "counterparty"
[[step]]
op = "read"
of = "counterparty"
table = "users"
columns = ["balance"]
[[step]]
op = "add"
table = "users"
column = "balance"
by = "-amount"
below = "insufficient_funds"
[[step]]
op = "add"
of = "counterparty"
table = "users"
column = "balance"
by = "amount"
above = "overflow"
"#;
        let c = run(
            head,
            steps,
            "\"balance\", \"to.balance\"",
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(c.lane, Lane::TwoShards);
        assert_eq!(c.errors, ["insufficient_funds", "overflow"]);
    }

    // ---- unknown_table, unknown_column ----

    #[test]
    fn unknown_tables_and_the_wrong_kinds_of_table_are_refused() {
        let add = |table: &str| {
            format!("[[step]]\nop = \"add\"\ntable = \"{table}\"\ncolumn = \"balance\"\nby = \"1\"\nbelow = \"x\"\nabove = \"y\"")
        };
        assert_eq!(code(&add("nope")), (Code::UnknownTable, Some(1)));
        // A global table is never written.
        let set = "[[step]]\nop = \"set\"\ntable = \"shop\"\nset = { price = \"1\" }";
        assert_eq!(code(set), (Code::UnknownTable, Some(1)));
        // The subject must be a subject table.
        let text = "[recipe]\nname = \"r\"\nsubject = \"entries\"\n";
        let recipe = parse_recipe(text, &Limits::default()).unwrap();
        let e = check(&schema(), &recipe, &[], &Limits::default()).unwrap_err();
        assert_eq!((e.code, e.step), (Code::UnknownTable, None));
        let text = "[recipe]\nname = \"r\"\nsubject = \"ghosts\"\n";
        let recipe = parse_recipe(text, &Limits::default()).unwrap();
        assert_eq!(
            check(&schema(), &recipe, &[], &Limits::default())
                .unwrap_err()
                .code,
            Code::UnknownTable
        );
    }

    #[test]
    fn unknown_and_implicit_columns_are_refused_with_the_step() {
        let add = |col: &str| {
            format!("[[step]]\nop = \"ensure\"\n[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"{col}\"\nby = \"1\"")
        };
        assert_eq!(code(&add("zzz")), (Code::UnknownColumn, Some(2)));
        let e = go(&add("id")).unwrap_err();
        assert!(e.message.contains("implicit"), "{e}");
        let read = "[[step]]\nop = \"read\"\ntable = \"users\"\ncolumns = [\"nope\"]";
        assert_eq!(code(read), (Code::UnknownColumn, Some(1)));
        let set = "[[step]]\nop = \"insert\"\ntable = \"entries\"\nset = { delta = \"1\", at = \"now\", owner_id = \"1\" }";
        assert_eq!(code(set), (Code::UnknownColumn, Some(1)));
    }

    #[test]
    fn a_result_entry_must_exist() {
        let e = run(HEAD, SPEND, "\"balance\", \"ghost\"", &Limits::default()).unwrap_err();
        assert_eq!((e.code, e.step), (Code::UnknownColumn, None));
        assert!(e.message.contains("ghost"));
    }

    // ---- not_by_key ----

    #[test]
    fn rows_are_reached_only_by_the_subjects_key() {
        // A subject table other than the recipe's.
        let other = "[[step]]\nop = \"read\"\ntable = \"teams\"\ncolumns = [\"size\"]";
        assert_eq!(code(other), (Code::NotByKey, Some(1)));
        // A global table is not reached by the subject's id.
        let global = "[[step]]\nop = \"read\"\ntable = \"shop\"\ncolumns = [\"price\"]";
        assert_eq!(code(global), (Code::NotByKey, Some(1)));
        // A child of someone else.
        let child = "[[step]]\nop = \"insert\"\ntable = \"notes\"\nset = { n = \"1\" }";
        let e = go(child).unwrap_err();
        assert_eq!((e.code, e.step), (Code::NotByKey, Some(1)));
        // A child table is written by insert, never updated by key.
        let add_child =
            "[[step]]\nop = \"add\"\ntable = \"entries\"\ncolumn = \"delta\"\nby = \"1\"";
        assert_eq!(code(add_child), (Code::NotByKey, Some(1)));
        // Insert into the subject table itself.
        let insert_subject =
            "[[step]]\nop = \"insert\"\ntable = \"users\"\nset = { points = \"1\" }";
        assert_eq!(code(insert_subject), (Code::NotByKey, Some(1)));
    }

    // ---- type ----

    #[test]
    fn expression_types_are_checked_in_every_step_kind() {
        let bad_check = "[[step]]\nop = \"check\"\nrequire = \"amount + 1\"\nerror = \"nope\"";
        assert_eq!(code(bad_check), (Code::Type, Some(1)));
        let unknown_name = "[[step]]\nop = \"check\"\nrequire = \"ghost > 1\"\nerror = \"nope\"";
        assert_eq!(code(unknown_name), (Code::Type, Some(1)));
        // A value is usable only after the read that brings it in.
        let early = "[[step]]\nop = \"check\"\nrequire = \"balance > 1\"\nerror = \"nope\"\n[[step]]\nop = \"read\"\ntable = \"users\"\ncolumns = [\"balance\"]";
        assert_eq!(code(early), (Code::Type, Some(1)));
        let late = "[[step]]\nop = \"read\"\ntable = \"users\"\ncolumns = [\"balance\"]\n[[step]]\nop = \"check\"\nrequire = \"balance > 1\"\nerror = \"nope\"";
        assert!(go(late).is_ok());
        let add_text = "[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"name\"\nby = \"1\"";
        assert_eq!(code(add_text), (Code::Type, Some(1)));
        let by_bool =
            "[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"points\"\nby = \"true\"";
        assert_eq!(code(by_bool), (Code::Type, Some(1)));
        let set_text = "[[step]]\nop = \"set\"\ntable = \"users\"\nset = { name = \"amount\" }";
        assert_eq!(code(set_text), (Code::Type, Some(1)));
    }

    #[test]
    fn an_insert_must_give_every_required_column() {
        let missing = "[[step]]\nop = \"insert\"\ntable = \"entries\"\nset = { delta = \"1\" }";
        let e = go(missing).unwrap_err();
        assert_eq!((e.code, e.step), (Code::Type, Some(1)));
        assert!(e.message.contains("`at`"), "{e}");
        // Nullable columns may be left out.
        let ok =
            "[[step]]\nop = \"insert\"\ntable = \"entries\"\nset = { delta = \"1\", at = \"now\" }";
        assert!(go(ok).is_ok());
    }

    #[test]
    fn a_name_is_never_used_twice() {
        let clash = "[[step]]\nop = \"read\"\ntable = \"users\"\ncolumns = [\"points\"]\n[[step]]\nop = \"list\"\ntable = \"entries\"\ncolumns = [\"delta\"]\nlimit = 5\ninto = \"points\"";
        assert_eq!(code(clash), (Code::Type, Some(2)));
        let arg = "[[step]]\nop = \"list\"\ntable = \"entries\"\ncolumns = [\"delta\"]\nlimit = 5\ninto = \"amount\"";
        assert_eq!(code(arg), (Code::Type, Some(1)));
        let twice = format!("{arg_a}\n{arg_a}", arg_a = "[[step]]\nop = \"list\"\ntable = \"entries\"\ncolumns = [\"delta\"]\nlimit = 5\ninto = \"recent\"");
        assert_eq!(code(&twice), (Code::Type, Some(2)));
        // A read may not shadow an argument.
        let head =
            "[recipe]\nname = \"r\"\nsubject = \"users\"\nargs = { points = { type = \"i64\" } }";
        let read = "[[step]]\nop = \"read\"\ntable = \"users\"\ncolumns = [\"points\"]";
        let e = run(head, read, "", &Limits::default()).unwrap_err();
        assert_eq!((e.code, e.step), (Code::Type, Some(1)));
    }

    // ---- rounding ----

    #[test]
    fn a_decimal_never_loses_digits_without_round() {
        // rate has 6 digits; interest keeps 2.
        let too_fine = "[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"interest\"\nby = \"rate\"\nbelow = \"x\"\nabove = \"y\"";
        let e = go(too_fine).unwrap_err();
        assert_eq!((e.code, e.step), (Code::Rounding, Some(1)));
        assert!(e.message.contains("round(..., 2, mode)"), "{e}");
        // Into a whole-number column.
        let into_i64 = "[[step]]\nop = \"set\"\ntable = \"users\"\nset = { points = \"rate\" }";
        assert_eq!(code(into_i64), (Code::Rounding, Some(1)));
        // `/` outside `round`.
        let div = "[[step]]\nop = \"set\"\ntable = \"users\"\nset = { points = \"amount / 2\" }";
        assert_eq!(code(div), (Code::Rounding, Some(1)));
        // Wrapped, it is fine.
        let ok = "[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"interest\"\nby = \"round(amount * rate, 2, half_even)\"\nbelow = \"x\"\nabove = \"y\"";
        assert!(go(ok).is_ok());
        let ok_div = "[[step]]\nop = \"set\"\ntable = \"users\"\nset = { points = \"round(amount / 3, 0, down)\" }";
        assert!(go(ok_div).is_ok());
    }

    // ---- bounds ----

    #[test]
    fn an_add_that_can_cross_a_bound_needs_the_named_error_for_that_side() {
        let add = |by: &str, extra: &str| {
            format!("[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"balance\"\nby = \"{by}\"\n{extra}")
        };
        // Can fall: needs `below`.
        let e = go(&add("-amount", "above = \"overflow\"")).unwrap_err();
        assert_eq!((e.code, e.step), (Code::Bounds, Some(1)));
        assert!(e.message.contains("below"), "{e}");
        // Can rise: needs `above`.
        let e = go(&add("amount", "below = \"insufficient_funds\"")).unwrap_err();
        assert_eq!((e.code, e.step), (Code::Bounds, Some(1)));
        assert!(e.message.contains("above"), "{e}");
        // Only the side the amount can push across is required.
        assert!(go(&add("amount", "above = \"overflow\"")).is_ok());
        assert!(go(&add("-amount", "below = \"insufficient_funds\"")).is_ok());
        // An amount that can go either way needs both.
        assert!(go(&add(
            "amount - 500",
            "below = \"insufficient_funds\"\nabove = \"overflow\""
        ))
        .is_ok());
        // A column with no bound needs neither.
        let points =
            "[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"points\"\nby = \"-amount\"";
        assert!(go(points).is_ok());
    }

    #[test]
    fn set_and_insert_have_no_named_error_so_the_value_must_stay_inside() {
        let set = |e: &str| {
            format!("[[step]]\nop = \"set\"\ntable = \"users\"\nset = {{ balance = \"{e}\" }}")
        };
        assert!(go(&set("5")).is_ok());
        assert!(go(&set("amount")).is_ok());
        assert_eq!(code(&set("-1")), (Code::Bounds, Some(1)));
        assert_eq!(code(&set("1000000001")), (Code::Bounds, Some(1)));
        assert_eq!(code(&set("amount - 5")), (Code::Bounds, Some(1)));
        assert_eq!(
            code(&set("amount * amount * 5000")),
            (Code::Bounds, Some(1))
        );
        // Decimal bounds are compared at the column's scale.
        let dec = |e: &str| {
            format!("[[step]]\nop = \"set\"\ntable = \"users\"\nset = {{ interest = \"{e}\" }}")
        };
        assert!(go(&dec("0.50")).is_ok());
        assert_eq!(code(&dec("-0.01")), (Code::Bounds, Some(1)));
    }

    // ---- too_big ----

    #[test]
    fn a_result_over_the_byte_limit_is_too_big() {
        let list = "[[step]]\nop = \"list\"\ntable = \"entries\"\ncolumns = [\"delta\", \"at\", \"note\"]\nlimit = 100\ninto = \"recent\"";
        // 100 rows of (8 + 8 + 400) bytes.
        let limits = Limits {
            max_result_bytes: 40_000,
            ..Limits::default()
        };
        let e = run(HEAD, list, "\"recent\"", &limits).unwrap_err();
        assert_eq!((e.code, e.step), (Code::TooBig, None));
        assert!(e.message.contains("41600"), "{e}");
        let limits = Limits {
            max_result_bytes: 41_600,
            ..Limits::default()
        };
        assert!(run(HEAD, list, "\"recent\"", &limits).is_ok());
    }

    // ---- error_name ----

    #[test]
    fn built_in_error_names_keep_their_meaning() {
        let chk = |name: &str| {
            format!("[[step]]\nop = \"check\"\nrequire = \"amount > 5\"\nerror = \"{name}\"")
        };
        assert!(go(&chk("insufficient_funds")).is_ok());
        assert!(go(&chk("out_of_stock")).is_ok());
        for bad in [
            "overflow",
            "already_exists",
            "unauthorized",
            "internal",
            "bad_arguments",
        ] {
            let e = go(&chk(bad)).unwrap_err();
            assert_eq!((e.code, e.step), (Code::ErrorName, Some(1)), "{bad}");
        }
        let add = |below: &str, above: &str| {
            format!("[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"balance\"\nby = \"amount - 500\"\nbelow = \"{below}\"\nabove = \"{above}\"")
        };
        assert!(go(&add("insufficient_funds", "overflow")).is_ok());
        // `overflow` below, or `insufficient_funds` above, would say the opposite of the truth.
        assert_eq!(code(&add("overflow", "overflow")).0, Code::ErrorName);
        assert_eq!(
            code(&add("insufficient_funds", "insufficient_funds")).0,
            Code::ErrorName
        );
    }

    #[test]
    fn the_error_list_has_each_name_once_in_order_of_first_use() {
        let steps = "[[step]]\nop = \"check\"\nrequire = \"amount > 5\"\nerror = \"too_small\"\n[[step]]\nop = \"check\"\nrequire = \"amount > 6\"\nerror = \"too_small\"\n[[step]]\nop = \"check\"\nrequire = \"amount > 7\"\nerror = \"insufficient_funds\"";
        assert_eq!(
            go(steps).unwrap().errors,
            ["too_small", "insufficient_funds"]
        );
    }

    // ---- lane, derived ----

    #[test]
    fn a_recipe_with_no_steps_has_no_lane() {
        let e = run(HEAD, "", "", &Limits::default()).unwrap_err();
        assert_eq!((e.code, e.step), (Code::Lane, None));
    }

    #[test]
    fn a_counterparty_nobody_uses_is_a_lane_error() {
        let head = "[recipe]\nname = \"r\"\nsubject = \"users\"\ncounterparty = \"users\"";
        let e = run(head, "[[step]]\nop = \"ensure\"", "", &Limits::default()).unwrap_err();
        assert_eq!((e.code, e.step), (Code::Lane, None));
    }

    #[test]
    fn the_argument_named_to_is_taken_when_a_counterparty_is_declared() {
        let head = "[recipe]\nname = \"r\"\nsubject = \"users\"\ncounterparty = \"users\"";
        let steps = "[[step]]\nop = \"read\"\nof = \"counterparty\"\ntable = \"users\"\ncolumns = [\"points\"]\n[[step]]\nop = \"check\"\nrequire = \"to.points > 1\"\nerror = \"poor\"";
        assert_eq!(
            run(head, steps, "\"to.points\"", &Limits::default())
                .unwrap()
                .lane,
            Lane::TwoShards
        );
        // `to.points` is only known after the read.
        let steps = "[[step]]\nop = \"check\"\nrequire = \"to.points > 1\"\nerror = \"poor\"\n[[step]]\nop = \"read\"\nof = \"counterparty\"\ntable = \"users\"\ncolumns = [\"points\"]";
        assert_eq!(
            run(head, steps, "", &Limits::default()).unwrap_err().code,
            Code::Type
        );
    }

    const DERIVED: &str = "[[step]]\nop = \"read_derived\"\nstore = \"leaderboard\"\nwindow = \"weekly\"\nmax_age_ms = 5000\nlimit = 50";

    #[test]
    fn a_read_derived_alone_is_the_all_shards_lane() {
        assert_eq!(
            run(
                "[recipe]\nname = \"top\"\nsubject = \"users\"",
                DERIVED,
                "",
                &Limits::default()
            )
            .unwrap()
            .lane,
            Lane::AllShards
        );
    }

    #[test]
    fn a_read_derived_must_be_the_whole_recipe() {
        let head = "[recipe]\nname = \"top\"\nsubject = \"users\"";
        let two = format!("[[step]]\nop = \"ensure\"\n{DERIVED}");
        let e = run(head, &two, "", &Limits::default()).unwrap_err();
        assert_eq!((e.code, e.step), (Code::Lane, Some(2)));
        let head = "[recipe]\nname = \"top\"\nsubject = \"users\"\ncounterparty = \"users\"";
        assert_eq!(
            run(head, DERIVED, "", &Limits::default()).unwrap_err().code,
            Code::Lane
        );
    }

    #[test]
    fn a_read_derived_must_match_a_declared_store_window_and_freshness() {
        let head = "[recipe]\nname = \"top\"\nsubject = \"users\"";
        let with = |store: &str, window: &str, age: u64| {
            format!("[[step]]\nop = \"read_derived\"\nstore = \"{store}\"\nwindow = \"{window}\"\nmax_age_ms = {age}\nlimit = 10")
        };
        for (steps, why) in [
            (with("nope", "weekly", 5000), "no derived store"),
            (with("leaderboard", "daily", 5000), "no window"),
            (with("leaderboard", "weekly", 999), "fresher than 1000 ms"),
        ] {
            let e = run(head, &steps, "", &Limits::default()).unwrap_err();
            assert_eq!((e.code, e.step), (Code::Derived, Some(1)), "{why}");
            assert!(e.message.contains(why), "{e}");
        }
        // Exactly the freshest promise is allowed.
        assert!(run(
            head,
            &with("leaderboard", "all_time", 1000),
            "",
            &Limits::default()
        )
        .is_ok());
    }

    // ---- order of checks ----

    #[test]
    fn the_first_failure_in_the_documented_order_is_the_one_reported() {
        // A bad table at step 3 is reported before a bad type at step 1 (tables come first).
        let steps = "[[step]]\nop = \"check\"\nrequire = \"amount + 1\"\nerror = \"e\"\n[[step]]\nop = \"ensure\"\n[[step]]\nop = \"read\"\ntable = \"nope\"\ncolumns = [\"x\"]";
        assert_eq!(code(steps), (Code::UnknownTable, Some(3)));
        // A bad column is reported before a not-by-key table.
        let steps = "[[step]]\nop = \"read\"\ntable = \"teams\"\ncolumns = [\"size\"]\n[[step]]\nop = \"read\"\ntable = \"users\"\ncolumns = [\"ghost\"]";
        assert_eq!(code(steps), (Code::UnknownColumn, Some(2)));
    }

    #[test]
    fn checking_is_deterministic() {
        let a = go(SPEND);
        let b = go(SPEND);
        assert_eq!(a, b);
    }
}
