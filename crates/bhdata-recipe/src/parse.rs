//! TOML text to a [`Recipe`] (spec `recipe-format.md`, section 2). Reads and checks only what the
//! text itself says: keys, field use per operation, names, expressions' syntax, argument types and
//! the configured limits. Whether the tables and columns exist, the types and the lane are the
//! compiler's job (slice B2). The first problem found is the rejection.

use std::collections::BTreeMap;

use bhdata_schema::model::ColumnType;
use bhdata_schema::parse::{parse_bound, parse_type};
use bhdata_schema::validate::valid_name;
use serde::Deserialize;

use crate::expr::{parse_expr, Expr, KEYWORDS};
use crate::model::{Arg, Op, Recipe, Step, Who};
use crate::reject::{Code, Rejection};

/// The longest `text` or `bytes` an argument may be, as for columns.
const MAX_TEXT_LEN: u32 = 4096;
const NAME_RULE: &str =
    "lower-case letters, digits and `_`, starting with a letter, at most 40 characters, not starting `bhdata_`";

/// What a recipe may be (spec section 2, "Limits"). They are config settings (`[recipes]`) and are
/// checked at registration; a registered version keeps working if a limit is lowered later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most steps in one recipe.
    pub max_steps: usize,
    /// Largest `limit` of a `list` or `read_derived`.
    pub max_list_limit: u32,
    /// Most values in the result.
    pub max_result_values: usize,
    /// Largest result, in bytes.
    pub max_result_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_steps: 16,
            max_list_limit: 100,
            max_result_values: 32,
            max_result_bytes: 64 * 1024,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    recipe: RawRecipe,
    #[serde(default)]
    step: Vec<RawStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecipe {
    name: String,
    subject: String,
    counterparty: Option<String>,
    #[serde(default)]
    args: BTreeMap<String, RawArg>,
    result: Option<RawResult>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArg {
    #[serde(rename = "type")]
    ty: String,
    precision: Option<u8>,
    scale: Option<u8>,
    max_len: Option<u32>,
    min: Option<toml::Value>,
    max: Option<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResult {
    #[serde(rename = "return", default)]
    ret: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    op: String,
    label: Option<String>,
    of: Option<String>,
    table: Option<String>,
    column: Option<String>,
    columns: Option<Vec<String>>,
    by: Option<String>,
    below: Option<String>,
    above: Option<String>,
    require: Option<String>,
    error: Option<String>,
    set: Option<BTreeMap<String, String>>,
    limit: Option<u32>,
    into: Option<String>,
    store: Option<String>,
    window: Option<String>,
    max_age_ms: Option<u64>,
}

fn reject(code: Code, step: Option<usize>, message: impl Into<String>) -> Rejection {
    Rejection::new(code, step, message)
}

/// Reads a recipe.
///
/// # Errors
/// The first [`Rejection`]: a TOML or key problem, a bad name, a bad expression, a field that does
/// not belong to its operation, or a limit exceeded.
pub fn parse_recipe(text: &str, limits: &Limits) -> Result<Recipe, Rejection> {
    let raw: RawFile = toml::from_str(text).map_err(|e| {
        let line = e.span().map_or(String::new(), |s| {
            format!("line {}: ", text[..s.start].matches('\n').count() + 1)
        });
        reject(Code::Parse, None, format!("{line}{}", e.message()))
    })?;

    let r = raw.recipe;
    name_ok(&r.name, "recipe name", None)?;
    name_ok(&r.subject, "`subject`", None)?;
    if let Some(cp) = &r.counterparty {
        name_ok(cp, "`counterparty`", None)?;
    }
    let has_counterparty = r.counterparty.is_some();
    let args = r
        .args
        .into_iter()
        .map(|(name, raw)| convert_arg(&name, raw, has_counterparty))
        .collect::<Result<Vec<_>, _>>()?;

    if raw.step.len() > limits.max_steps {
        return Err(reject(
            Code::TooBig,
            None,
            format!(
                "{} steps; at most {} are allowed",
                raw.step.len(),
                limits.max_steps
            ),
        ));
    }
    let steps = raw
        .step
        .into_iter()
        .enumerate()
        .map(|(i, s)| convert_step(i + 1, s, has_counterparty, limits))
        .collect::<Result<Vec<_>, _>>()?;

    let result = r.result.map(|x| x.ret).unwrap_or_default();
    check_result(&result, limits)?;
    Ok(Recipe {
        name: r.name,
        subject: r.subject,
        counterparty: r.counterparty,
        args,
        steps,
        result,
    })
}

fn name_ok(name: &str, what: &str, step: Option<usize>) -> Result<(), Rejection> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(reject(
            Code::Name,
            step,
            format!("{what} {name:?} must be {NAME_RULE}"),
        ))
    }
}

fn error_name_ok(name: &str, what: &str, step: usize) -> Result<(), Rejection> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(reject(
            Code::ErrorName,
            Some(step),
            format!("{what} {name:?} must be lower-case letters and `_`, 1 to 40 characters"),
        ))
    }
}

fn convert_arg(name: &str, raw: RawArg, has_counterparty: bool) -> Result<Arg, Rejection> {
    let at = |m: String| reject(Code::Parse, None, format!("argument {name}: {m}"));
    if !valid_name(name) || KEYWORDS.contains(&name) {
        return Err(reject(
            Code::Name,
            None,
            format!("argument name {name:?} must be {NAME_RULE} and not a word of the expression language"),
        ));
    }
    if has_counterparty && name == "to" {
        return Err(reject(
            Code::Name,
            None,
            "argument `to` is reserved: it carries the counterparty's id",
        ));
    }
    let ty = parse_type(&raw.ty, raw.precision, raw.scale, raw.max_len).map_err(at)?;
    let applies = |field: &str, present: bool, ok: bool| {
        if present && !ok {
            Err(at(format!("`{field}` does not apply to type {}", raw.ty)))
        } else {
            Ok(())
        }
    };
    applies(
        "max_len",
        raw.max_len.is_some(),
        matches!(ty, ColumnType::Text { .. } | ColumnType::Bytes { .. }),
    )?;
    applies(
        "precision",
        raw.precision.is_some(),
        matches!(ty, ColumnType::Decimal { .. }),
    )?;
    applies(
        "scale",
        raw.scale.is_some(),
        matches!(ty, ColumnType::Decimal { .. }),
    )?;
    match ty {
        ColumnType::Decimal { precision, scale }
            if !(1..=38).contains(&precision) || scale > precision =>
        {
            return Err(at(
                "a decimal needs 1 to 38 digits and a scale of at most `precision`".into(),
            ));
        }
        ColumnType::Text { max_len } | ColumnType::Bytes { max_len }
            if !(1..=MAX_TEXT_LEN).contains(&max_len) =>
        {
            return Err(at(format!("`max_len` is 1 to {MAX_TEXT_LEN}")));
        }
        _ => {}
    }
    let bound = |v: Option<toml::Value>| v.map(|v| parse_bound(ty, v)).transpose().map_err(at);
    let (min, max) = (bound(raw.min)?, bound(raw.max)?);
    if let (Some(lo), Some(hi)) = (min, max) {
        if lo > hi {
            return Err(reject(
                Code::Parse,
                None,
                format!("argument {name}: `min` is above `max`"),
            ));
        }
    }
    Ok(Arg {
        name: name.to_string(),
        ty,
        min,
        max,
    })
}

/// Which fields an operation needs and which it may also have.
fn fields_of(op: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    Some(match op {
        "ensure" => (&[], &["of"]),
        "read" => (&["table", "columns"], &["of"]),
        "check" => (&["require", "error"], &[]),
        "add" => (&["table", "column", "by"], &["of", "below", "above"]),
        "set" | "insert" => (&["table", "set"], &["of"]),
        "list" => (&["table", "columns", "limit", "into"], &["of"]),
        "read_derived" => (&["store", "window", "max_age_ms", "limit"], &[]),
        _ => return None,
    })
}

fn convert_step(
    n: usize,
    raw: RawStep,
    has_counterparty: bool,
    limits: &Limits,
) -> Result<Step, Rejection> {
    let bad = |m: String| reject(Code::Parse, Some(n), m);
    let Some((required, optional)) = fields_of(&raw.op) else {
        return Err(bad(format!(
            "unknown op {:?} (use ensure, read, check, add, set, insert, list or read_derived)",
            raw.op
        )));
    };
    let present = [
        ("of", raw.of.is_some()),
        ("table", raw.table.is_some()),
        ("column", raw.column.is_some()),
        ("columns", raw.columns.is_some()),
        ("by", raw.by.is_some()),
        ("below", raw.below.is_some()),
        ("above", raw.above.is_some()),
        ("require", raw.require.is_some()),
        ("error", raw.error.is_some()),
        ("set", raw.set.is_some()),
        ("limit", raw.limit.is_some()),
        ("into", raw.into.is_some()),
        ("store", raw.store.is_some()),
        ("window", raw.window.is_some()),
        ("max_age_ms", raw.max_age_ms.is_some()),
    ];
    for (field, here) in present {
        if here && !required.contains(&field) && !optional.contains(&field) {
            return Err(bad(format!("`{field}` does not apply to op `{}`", raw.op)));
        }
        if !here && required.contains(&field) {
            return Err(bad(format!("op `{}` needs `{field}`", raw.op)));
        }
    }
    if let Some(label) = &raw.label {
        name_ok(label, "`label`", Some(n))?;
    }

    let of = match raw.of.as_deref() {
        None | Some("subject") => Who::Subject,
        Some("counterparty") if has_counterparty => Who::Counterparty,
        Some("counterparty") => {
            return Err(bad(
                "`of = \"counterparty\"` needs `counterparty` in [recipe]".into(),
            ))
        }
        Some(other) => {
            return Err(bad(format!(
                "`of` is \"subject\" or \"counterparty\", not {other:?}"
            )))
        }
    };
    let table = |t: Option<String>| -> Result<String, Rejection> {
        let t = t.unwrap_or_default();
        name_ok(&t, "table name", Some(n))?;
        Ok(t)
    };
    let columns = |c: Option<Vec<String>>| -> Result<Vec<String>, Rejection> {
        let c = c.unwrap_or_default();
        if c.is_empty() {
            return Err(bad("`columns` needs at least one column".into()));
        }
        for (i, name) in c.iter().enumerate() {
            name_ok(name, "column name", Some(n))?;
            if c[..i].contains(name) {
                return Err(bad(format!("column {name:?} is listed twice")));
            }
        }
        Ok(c)
    };
    let expr = |what: &str, text: Option<String>| -> Result<Expr, Rejection> {
        parse_expr(&text.unwrap_or_default()).map_err(|m| bad(format!("`{what}`: {m}")))
    };
    let values = |m: Option<BTreeMap<String, String>>| -> Result<Vec<(String, Expr)>, Rejection> {
        let m = m.unwrap_or_default();
        if m.is_empty() {
            return Err(bad("`set` needs at least one column".into()));
        }
        m.into_iter()
            .map(|(col, text)| {
                name_ok(&col, "column name", Some(n))?;
                let e = expr(&format!("set.{col}"), Some(text))?;
                Ok((col, e))
            })
            .collect()
    };
    let limit = |l: Option<u32>| -> Result<u32, Rejection> {
        let l = l.unwrap_or(0);
        if l == 0 || l > limits.max_list_limit {
            return Err(reject(
                Code::TooBig,
                Some(n),
                format!("`limit` is 1 to {}", limits.max_list_limit),
            ));
        }
        Ok(l)
    };

    let op = match raw.op.as_str() {
        "ensure" => Op::Ensure { of },
        "read" => Op::Read {
            of,
            table: table(raw.table)?,
            columns: columns(raw.columns)?,
        },
        "check" => {
            let error = raw.error.unwrap_or_default();
            error_name_ok(&error, "error name", n)?;
            Op::Check {
                require: expr("require", raw.require)?,
                error,
            }
        }
        "add" => {
            for (what, v) in [("`below`", &raw.below), ("`above`", &raw.above)] {
                if let Some(v) = v {
                    error_name_ok(v, what, n)?;
                }
            }
            Op::Add {
                of,
                table: table(raw.table)?,
                column: {
                    let c = raw.column.unwrap_or_default();
                    name_ok(&c, "column name", Some(n))?;
                    c
                },
                by: expr("by", raw.by)?,
                below: raw.below,
                above: raw.above,
            }
        }
        "set" => Op::Set {
            of,
            table: table(raw.table)?,
            values: values(raw.set)?,
        },
        "insert" => Op::Insert {
            of,
            table: table(raw.table)?,
            values: values(raw.set)?,
        },
        "list" => {
            let into = raw.into.unwrap_or_default();
            name_ok(&into, "`into`", Some(n))?;
            Op::List {
                of,
                table: table(raw.table)?,
                columns: columns(raw.columns)?,
                limit: limit(raw.limit)?,
                into,
            }
        }
        _ => {
            let (store, window) = (
                raw.store.unwrap_or_default(),
                raw.window.unwrap_or_default(),
            );
            name_ok(&store, "`store`", Some(n))?;
            name_ok(&window, "`window`", Some(n))?;
            Op::ReadDerived {
                store,
                window,
                max_age_ms: raw.max_age_ms.unwrap_or(0),
                limit: limit(raw.limit)?,
            }
        }
    };
    Ok(Step {
        label: raw.label,
        op,
    })
}

fn check_result(result: &[String], limits: &Limits) -> Result<(), Rejection> {
    if result.len() > limits.max_result_values {
        return Err(reject(
            Code::TooBig,
            None,
            format!(
                "{} result values; at most {} are allowed",
                result.len(),
                limits.max_result_values
            ),
        ));
    }
    for (i, entry) in result.iter().enumerate() {
        let parts: Vec<&str> = entry.split('.').collect();
        if parts.len() > 2 || !parts.iter().all(|p| valid_name(p)) {
            return Err(reject(
                Code::Name,
                None,
                format!("result entry {entry:?} must be a name, or `to.` and a name"),
            ));
        }
        if result[..i].contains(entry) {
            return Err(reject(
                Code::Parse,
                None,
                format!("result entry {entry:?} is listed twice"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::BinOp;

    const SPEND: &str = r#"
[recipe]
name    = "spend"
subject = "users"
args    = { amount = { type = "i64", min = 1, max = 1_000_000 } }

[[step]]
op = "ensure"

[[step]]
op     = "add"
table  = "users"
column = "balance"
by     = "-amount"
below  = "insufficient_funds"
above  = "overflow"

[[step]]
op    = "insert"
table = "entries"
set   = { delta = "-amount", at = "now" }

[recipe.result]
return = ["balance"]
"#;

    const GIVE: &str = r#"
[recipe]
name = "give"
subject = "users"
counterparty = "users"
args = { amount = { type = "i64", min = 1, max = 1_000_000 } }

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

[recipe.result]
return = ["balance", "to.balance"]
"#;

    fn parse(text: &str) -> Result<Recipe, Rejection> {
        parse_recipe(text, &Limits::default())
    }

    fn with_step(step: &str) -> String {
        format!("[recipe]\nname = \"r\"\nsubject = \"users\"\n\n[[step]]\n{step}\n")
    }

    #[test]
    fn spend_is_read_as_written() {
        let r = parse(SPEND).unwrap();
        assert_eq!(
            (r.name.as_str(), r.subject.as_str(), r.counterparty),
            ("spend", "users", None)
        );
        assert_eq!(r.args.len(), 1);
        assert_eq!(
            (
                r.args[0].name.as_str(),
                r.args[0].ty,
                r.args[0].min,
                r.args[0].max
            ),
            ("amount", ColumnType::I64, Some(1), Some(1_000_000))
        );
        assert_eq!(r.steps.len(), 3);
        assert!(matches!(&r.steps[0].op, Op::Ensure { of: Who::Subject }));
        match &r.steps[1].op {
            Op::Add {
                table,
                column,
                by,
                below,
                above,
                ..
            } => {
                assert_eq!((table.as_str(), column.as_str()), ("users", "balance"));
                assert_eq!(by.to_string(), "(-amount)");
                assert_eq!(
                    (below.as_deref(), above.as_deref()),
                    (Some("insufficient_funds"), Some("overflow"))
                );
            }
            other => panic!("{other:?}"),
        }
        match &r.steps[2].op {
            Op::Insert { table, values, .. } => {
                assert_eq!(table, "entries");
                let names: Vec<&str> = values.iter().map(|(c, _)| c.as_str()).collect();
                assert_eq!(names, ["at", "delta"]); // sorted, not as written
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(r.result, ["balance"]);
    }

    #[test]
    fn interest_reads_a_decimal_argument_with_exact_bounds() {
        let r = parse(
            r#"
[recipe]
name = "add_interest"
subject = "users"
args = { rate = { type = "decimal", precision = 9, scale = 6, min = "0", max = "1" } }

[[step]]
op = "read"
table = "users"
columns = ["balance"]

[[step]]
op = "add"
label = "interest"
table = "users"
column = "balance"
by = "round(balance * rate, 2, half_even)"
above = "overflow"
"#,
        )
        .unwrap();
        assert_eq!(
            r.args[0].ty,
            ColumnType::Decimal {
                precision: 9,
                scale: 6
            }
        );
        assert_eq!((r.args[0].min, r.args[0].max), (Some(0), Some(1_000_000)));
        assert_eq!(r.steps[1].label.as_deref(), Some("interest"));
        match &r.steps[1].op {
            Op::Add { by, below, .. } => {
                assert!(matches!(by, Expr::Round { .. }));
                assert_eq!(*below, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn give_names_a_counterparty_and_reads_its_row() {
        let r = parse(GIVE).unwrap();
        assert_eq!(r.counterparty.as_deref(), Some("users"));
        let whose: Vec<Who> = r
            .steps
            .iter()
            .map(|s| match &s.op {
                Op::Ensure { of } | Op::Read { of, .. } | Op::Add { of, .. } => *of,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            whose,
            [
                Who::Subject,
                Who::Counterparty,
                Who::Counterparty,
                Who::Subject,
                Who::Counterparty
            ]
        );
        assert_eq!(r.result, ["balance", "to.balance"]);
    }

    #[test]
    fn check_list_set_and_read_derived_are_read() {
        let r = parse(
            r#"
[recipe]
name = "mix"
subject = "users"

[[step]]
op = "read"
table = "users"
columns = ["balance"]
[[step]]
op = "check"
require = "balance >= 10 and not (balance == 99)"
error = "too_poor"
[[step]]
op = "set"
table = "users"
set = { name = "now" }
[[step]]
op = "list"
table = "entries"
columns = ["delta", "at"]
limit = 20
into = "recent"
"#,
        )
        .unwrap();
        match &r.steps[1].op {
            Op::Check { require, error } => {
                assert!(matches!(require, Expr::Binary(BinOp::And, _, _)));
                assert_eq!(error, "too_poor");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&r.steps[2].op, Op::Set { values, .. } if values.len() == 1));
        assert!(matches!(&r.steps[3].op, Op::List { limit: 20, into, .. } if into == "recent"));

        let d = parse(
            r#"
[recipe]
name = "top"
subject = "users"
[[step]]
op = "read_derived"
store = "leaderboard"
window = "weekly"
max_age_ms = 5000
limit = 50
"#,
        )
        .unwrap();
        assert!(
            matches!(&d.steps[0].op, Op::ReadDerived { store, window, max_age_ms: 5000, limit: 50 } if store == "leaderboard" && window == "weekly")
        );
    }

    #[test]
    fn the_order_of_keys_never_changes_the_recipe() {
        let a = with_step(
            "op = \"insert\"\ntable = \"entries\"\nset = { delta = \"1\", at = \"now\" }",
        );
        let b = with_step(
            "set = { at = \"now\", delta = \"1\" }\ntable = \"entries\"\nop = \"insert\"",
        );
        assert_eq!(parse(&a).unwrap(), parse(&b).unwrap());
    }

    fn rejected(text: &str, code: Code, step: Option<usize>, fragment: &str) {
        let e = parse(text).unwrap_err();
        assert_eq!((e.code, e.step), (code, step), "{text}\n{e}");
        assert!(e.message.contains(fragment), "{text}\n{e}");
    }

    #[test]
    fn toml_and_key_problems_are_parse_rejections_with_a_line() {
        rejected("this is not toml", Code::Parse, None, "line 1");
        rejected(
            "[recipe]\nname = \"r\"\nsubject = \"u\"\ncolour = \"x\"\n",
            Code::Parse,
            None,
            "unknown field",
        );
        rejected(
            "[recipe]\nname = \"r\"\n",
            Code::Parse,
            None,
            "missing field",
        );
        rejected(
            &with_step("op = \"ensure\"\nwhat = 1"),
            Code::Parse,
            None,
            "unknown field",
        );
        rejected(
            &with_step("op = \"add\"\nlimit = \"x\""),
            Code::Parse,
            None,
            "line",
        );
    }

    #[test]
    fn steps_use_the_fields_of_their_operation_and_no_others() {
        rejected(
            &with_step("op = \"fly\""),
            Code::Parse,
            Some(1),
            "unknown op",
        );
        rejected(
            &with_step("op = \"ensure\"\nby = \"1\""),
            Code::Parse,
            Some(1),
            "`by` does not apply to op `ensure`",
        );
        rejected(
            &with_step("op = \"add\"\ntable = \"users\"\nby = \"1\""),
            Code::Parse,
            Some(1),
            "needs `column`",
        );
        rejected(
            &with_step("op = \"check\"\nrequire = \"true\""),
            Code::Parse,
            Some(1),
            "needs `error`",
        );
        rejected(
            &with_step("op = \"set\"\ntable = \"users\"\nset = {}"),
            Code::Parse,
            Some(1),
            "at least one column",
        );
        rejected(
            &with_step("op = \"read\"\ntable = \"users\"\ncolumns = []"),
            Code::Parse,
            Some(1),
            "at least one column",
        );
        rejected(
            &with_step("op = \"read\"\ntable = \"users\"\ncolumns = [\"a\", \"a\"]"),
            Code::Parse,
            Some(1),
            "twice",
        );
        rejected(
            &with_step("op = \"ensure\"\nof = \"counterparty\""),
            Code::Parse,
            Some(1),
            "needs `counterparty`",
        );
        rejected(
            &with_step("op = \"ensure\"\nof = \"them\""),
            Code::Parse,
            Some(1),
            "\"subject\" or \"counterparty\"",
        );
    }

    #[test]
    fn bad_expressions_name_the_step_the_field_and_the_place() {
        rejected(
            &with_step("op = \"add\"\ntable = \"users\"\ncolumn = \"balance\"\nby = \"1 +\""),
            Code::Parse,
            Some(1),
            "`by`: at",
        );
        rejected(
            &with_step("op = \"check\"\nrequire = \"a $ b\"\nerror = \"nope\""),
            Code::Parse,
            Some(1),
            "`require`",
        );
        rejected(
            &with_step("op = \"insert\"\ntable = \"entries\"\nset = { delta = \"lower(a)\" }"),
            Code::Parse,
            Some(1),
            "`set.delta`",
        );
    }

    #[test]
    fn names_follow_the_rules_and_error_names_have_their_own() {
        rejected(
            "[recipe]\nname = \"Spend\"\nsubject = \"users\"\n",
            Code::Name,
            None,
            "recipe name",
        );
        rejected(
            "[recipe]\nname = \"bhdata_x\"\nsubject = \"users\"\n",
            Code::Name,
            None,
            "recipe name",
        );
        rejected(
            "[recipe]\nname = \"r\"\nsubject = \"Users\"\n",
            Code::Name,
            None,
            "`subject`",
        );
        rejected(
            &with_step("op = \"read\"\ntable = \"Users\"\ncolumns = [\"a\"]"),
            Code::Name,
            Some(1),
            "table name",
        );
        rejected(
            &with_step("op = \"read\"\ntable = \"users\"\ncolumns = [\"A\"]"),
            Code::Name,
            Some(1),
            "column name",
        );
        rejected(
            &with_step("op = \"ensure\"\nlabel = \"Bad Label\""),
            Code::Name,
            Some(1),
            "`label`",
        );
        rejected(
            &with_step("op = \"check\"\nrequire = \"true\"\nerror = \"Bad1\""),
            Code::ErrorName,
            Some(1),
            "error name",
        );
        rejected(
            &with_step(
                "op = \"add\"\ntable = \"u\"\ncolumn = \"b\"\nby = \"1\"\nbelow = \"no good\"",
            ),
            Code::ErrorName,
            Some(1),
            "`below`",
        );
        let long = "a".repeat(41);
        rejected(
            &with_step(&format!(
                "op = \"check\"\nrequire = \"true\"\nerror = \"{long}\""
            )),
            Code::ErrorName,
            Some(1),
            "1 to 40",
        );
    }

    #[test]
    fn arguments_are_checked_like_columns() {
        let arg =
            |a: &str| format!("[recipe]\nname = \"r\"\nsubject = \"users\"\nargs = {{ {a} }}\n");
        rejected(
            &arg("now = { type = \"i64\" }"),
            Code::Name,
            None,
            "argument name",
        );
        rejected(
            &arg("Amount = { type = \"i64\" }"),
            Code::Name,
            None,
            "argument name",
        );
        rejected(
            &arg("x = { type = \"float\" }"),
            Code::Parse,
            None,
            "unknown type",
        );
        rejected(
            &arg("x = { type = \"decimal\", precision = 9 }"),
            Code::Parse,
            None,
            "precision",
        );
        rejected(
            &arg("x = { type = \"decimal\", precision = 40, scale = 2 }"),
            Code::Parse,
            None,
            "1 to 38",
        );
        rejected(
            &arg("x = { type = \"decimal\", precision = 4, scale = 5 }"),
            Code::Parse,
            None,
            "1 to 38",
        );
        rejected(
            &arg("x = { type = \"text\" }"),
            Code::Parse,
            None,
            "max_len",
        );
        rejected(
            &arg("x = { type = \"text\", max_len = 5000 }"),
            Code::Parse,
            None,
            "1 to 4096",
        );
        rejected(
            &arg("x = { type = \"i64\", max_len = 5 }"),
            Code::Parse,
            None,
            "does not apply",
        );
        rejected(
            &arg("x = { type = \"i64\", min = \"1\" }"),
            Code::Parse,
            None,
            "whole number",
        );
        rejected(
            &arg("x = { type = \"decimal\", precision = 5, scale = 2, min = 1 }"),
            Code::Parse,
            None,
            "string",
        );
        rejected(
            &arg("x = { type = \"i64\", min = 5, max = 1 }"),
            Code::Parse,
            None,
            "above `max`",
        );
        rejected(
            &arg("x = { type = \"bool\", min = 1 }"),
            Code::Parse,
            None,
            "i64 and decimal only",
        );
        rejected(
            "[recipe]\nname = \"r\"\nsubject = \"u\"\ncounterparty = \"u\"\nargs = { to = { type = \"text\", max_len = 8 } }\n",
            Code::Name,
            None,
            "reserved",
        );
    }

    #[test]
    fn limits_are_checked_and_can_be_changed() {
        let many = format!(
            "[recipe]\nname = \"r\"\nsubject = \"users\"\n{}",
            "[[step]]\nop = \"ensure\"\n".repeat(17)
        );
        rejected(&many, Code::TooBig, None, "17 steps");
        let ok = format!(
            "[recipe]\nname = \"r\"\nsubject = \"users\"\n{}",
            "[[step]]\nop = \"ensure\"\n".repeat(16)
        );
        assert!(parse(&ok).is_ok());
        let tight = Limits {
            max_steps: 3,
            ..Limits::default()
        };
        assert_eq!(parse_recipe(&ok, &tight).unwrap_err().code, Code::TooBig);

        for l in [0, 101] {
            rejected(
                &with_step(&format!("op = \"list\"\ntable = \"entries\"\ncolumns = [\"a\"]\nlimit = {l}\ninto = \"x\"")),
                Code::TooBig,
                Some(1),
                "1 to 100",
            );
        }
        rejected(
            &with_step(
                "op = \"read_derived\"\nstore = \"s\"\nwindow = \"w\"\nmax_age_ms = 1\nlimit = 101",
            ),
            Code::TooBig,
            Some(1),
            "1 to 100",
        );
        let names: Vec<String> = (0..33).map(|i| format!("\"c{i}\"")).collect();
        let big = format!(
            "[recipe]\nname = \"r\"\nsubject = \"users\"\n[recipe.result]\nreturn = [{}]\n",
            names.join(", ")
        );
        rejected(&big, Code::TooBig, None, "33 result values");
    }

    #[test]
    fn result_entries_are_names_once() {
        let r = |list: &str| {
            format!(
                "[recipe]\nname = \"r\"\nsubject = \"users\"\n[recipe.result]\nreturn = {list}\n"
            )
        };
        rejected(&r("[\"a.b.c\"]"), Code::Name, None, "result entry");
        rejected(&r("[\"A\"]"), Code::Name, None, "result entry");
        rejected(&r("[\"a\", \"a\"]"), Code::Parse, None, "twice");
        assert_eq!(
            parse(&r("[\"a\", \"to.b\"]")).unwrap().result,
            ["a", "to.b"]
        );
    }

    #[test]
    fn a_recipe_with_no_steps_and_no_result_is_still_a_recipe_for_the_compiler_to_judge() {
        let r = parse("[recipe]\nname = \"empty\"\nsubject = \"users\"\n").unwrap();
        assert!(r.steps.is_empty() && r.result.is_empty() && r.args.is_empty());
    }
}
