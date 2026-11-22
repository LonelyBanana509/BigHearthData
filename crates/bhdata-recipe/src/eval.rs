//! The exact evaluator of expressions (spec `write-path.md`, decision 5, and `recipe-compiler.md`,
//! section 10). The writer runs a plan's [`Expr`]s here, in Rust, on exact whole numbers: no floats,
//! nothing wraps, and every overflow is an error instead of a wrong answer.
//!
//! It follows the types of `typing.rs` rule for rule, so a value that [`crate::typing::eval`]
//! accepted has, here, the type it was promised:
//!
//! - whole numbers stay whole (`I64`); a decimal is a scaled `i128` with its scale;
//! - `+` and `-` use the larger scale, `*` adds the scales, comparisons cross scales exactly;
//! - `/` exists only directly inside `round(x / y, scale, mode)` and is rounded once, from the exact
//!   quotient, never from an already rounded value;
//! - `and` and `or` stop at the left side when it decides the answer (so `b != 0 and ...` is a guard),
//!   and the compiler has already type-checked both sides.
//!
//! Evaluation is pure: the time `now` is given in the [`Context`], never read from a clock here.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use bhdata_schema::decimal::format_scaled;

use crate::expr::{BinOp, Expr, RoundMode, UnOp};
use crate::typing::Ty;

/// A value at run time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A 64-bit whole number.
    I64(i64),
    /// An exact decimal: `value` times 10 to the power of minus `scale`.
    Dec {
        /// The value times `10^scale`.
        value: i128,
        /// Digits after the point.
        scale: u8,
    },
    /// `true` or `false`.
    Bool(bool),
    /// Text.
    Text(String),
    /// Bytes.
    Bytes(Vec<u8>),
    /// A UTC time, in whole microseconds since 1970-01-01.
    Timestamp(i64),
}

impl Value {
    /// The type of this value, as `typing.rs` names it.
    #[must_use]
    pub fn ty(&self) -> Ty {
        match self {
            Value::I64(_) => Ty::I64,
            Value::Dec { scale, .. } => Ty::Dec(*scale),
            Value::Bool(_) => Ty::Bool,
            Value::Text(_) => Ty::Text,
            Value::Bytes(_) => Ty::Bytes,
            Value::Timestamp(_) => Ty::Timestamp,
        }
    }

    /// The exact text the value is sent to the database as: a decimal with exactly its scale, a
    /// timestamp as whole microseconds. Text and bytes are not turned into text here (the caller
    /// binds them as they are); they give `None`.
    #[must_use]
    pub fn number_text(&self) -> Option<String> {
        match self {
            Value::I64(n) | Value::Timestamp(n) => Some(n.to_string()),
            Value::Dec { value, scale } => Some(format_scaled(*value, *scale)),
            Value::Bool(b) => Some(b.to_string()),
            Value::Text(_) | Value::Bytes(_) => None,
        }
    }
}

/// What an expression is evaluated against.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// Arguments, then values read by earlier steps, by name.
    pub vars: BTreeMap<String, Value>,
    /// The time the command runs at, in whole microseconds.
    pub now_micros: i64,
}

/// Why an evaluation stopped. All of these are the writer's `internal` or `bad_arguments`, never a
/// recipe's named error: the compiler's bounds are meant to make `Overflow` unreachable for a
/// checked plan, and this is the second wall behind them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// A name with no value.
    Unknown(String),
    /// A value of the wrong type (the compiler should have refused the plan).
    Type(String),
    /// A result does not fit its type (`i64`, or 38 digits for a decimal).
    Overflow,
    /// A division by zero.
    DivisionByZero,
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::Unknown(name) => write!(f, "`{name}` has no value"),
            EvalError::Type(what) => write!(f, "wrong type: {what}"),
            EvalError::Overflow => f.write_str("a number does not fit its type"),
            EvalError::DivisionByZero => f.write_str("division by zero"),
        }
    }
}

impl std::error::Error for EvalError {}

/// The largest absolute value of a decimal: 38 digits (the same cap as the schema's `precision`).
const MAX_DIGITS: u8 = 38;

fn pow10(n: u32) -> Option<i128> {
    10i128.checked_pow(n)
}

fn fits_decimal(v: i128) -> bool {
    pow10(u32::from(MAX_DIGITS)).is_some_and(|top| v.unsigned_abs() < top.unsigned_abs())
}

fn dec(value: i128, scale: u8) -> Result<Value, EvalError> {
    if fits_decimal(value) {
        Ok(Value::Dec { value, scale })
    } else {
        Err(EvalError::Overflow)
    }
}

fn whole(value: i128) -> Result<Value, EvalError> {
    i64::try_from(value)
        .map(Value::I64)
        .map_err(|_| EvalError::Overflow)
}

/// `(value, scale)` of a number, or a type error naming `what`.
fn number(v: &Value, what: &str) -> Result<(i128, u8), EvalError> {
    match v {
        Value::I64(n) => Ok((i128::from(*n), 0)),
        Value::Dec { value, scale } => Ok((*value, *scale)),
        other => Err(EvalError::Type(format!(
            "{what} needs a number, not {}",
            other.ty().name()
        ))),
    }
}

fn boolean(v: &Value, what: &str) -> Result<bool, EvalError> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(EvalError::Type(format!(
            "{what} needs a bool, not {}",
            other.ty().name()
        ))),
    }
}

/// Brings two numbers to the larger of their scales. Exact; `Overflow` if a result leaves `i128`.
fn align(a: i128, sa: u8, b: i128, sb: u8) -> Result<(i128, i128, u8), EvalError> {
    let scale = sa.max(sb);
    let up = |v: i128, from: u8| -> Result<i128, EvalError> {
        let f = pow10(u32::from(scale - from)).ok_or(EvalError::Overflow)?;
        v.checked_mul(f).ok_or(EvalError::Overflow)
    };
    Ok((up(a, sa)?, up(b, sb)?, scale))
}

/// Divides `n` by `d` (not zero) and settles the rest as `mode` says. Exact in `i128`.
fn divide_rounded(n: i128, d: i128, mode: RoundMode) -> Result<i128, EvalError> {
    if d == 0 {
        return Err(EvalError::DivisionByZero);
    }
    let q = n.checked_div(d).ok_or(EvalError::Overflow)?;
    let r = n.checked_rem(d).ok_or(EvalError::Overflow)?;
    if r == 0 {
        return Ok(q);
    }
    // |r| < |d|, so `2 * |r|` fits a `u128`.
    let twice = r.unsigned_abs() * 2;
    let den = d.unsigned_abs();
    let away = match mode {
        RoundMode::Down => false,
        RoundMode::HalfUp => twice >= den,
        RoundMode::HalfEven => twice > den || (twice == den && q % 2 != 0),
    };
    if !away {
        return Ok(q);
    }
    // Away from zero: the quotient's sign is the product of the signs (q may be 0).
    let negative = (n < 0) != (d < 0);
    if negative {
        q.checked_sub(1).ok_or(EvalError::Overflow)
    } else {
        q.checked_add(1).ok_or(EvalError::Overflow)
    }
}

/// The value of `round(...)`: scale 0 gives a whole number, anything else a decimal.
fn rounded(value: i128, scale: u8) -> Result<Value, EvalError> {
    if scale == 0 {
        whole(value)
    } else {
        dec(value, scale)
    }
}

/// Evaluates `e` against `ctx`.
///
/// # Errors
/// [`EvalError`]; see its variants. Never panics, whatever the input.
pub fn eval(e: &Expr, ctx: &Context) -> Result<Value, EvalError> {
    match e {
        Expr::Num { value, scale } => {
            if *scale == 0 {
                whole(*value)
            } else {
                dec(*value, *scale)
            }
        }
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Now => Ok(Value::Timestamp(ctx.now_micros)),
        Expr::Var(name) => ctx
            .vars
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError::Unknown(name.clone())),
        Expr::Unary(UnOp::Neg, x) => {
            let v = eval(x, ctx)?;
            let (n, s) = number(&v, "`-`")?;
            let n = n.checked_neg().ok_or(EvalError::Overflow)?;
            if v.ty() == Ty::I64 {
                whole(n)
            } else {
                dec(n, s)
            }
        }
        Expr::Unary(UnOp::Not, x) => Ok(Value::Bool(!boolean(&eval(x, ctx)?, "`not`")?)),
        Expr::Binary(op, a, b) => binary(*op, a, b, ctx),
        Expr::Round { value, scale, mode } => round(value, *scale, *mode, ctx),
        Expr::MinMax { max, args } => {
            let (a, b) = (eval(&args.0, ctx)?, eval(&args.1, ctx)?);
            let (x, sa) = number(&a, "`min`/`max`")?;
            let (y, sb) = number(&b, "`min`/`max`")?;
            let (x, y, scale) = align(x, sa, y, sb)?;
            let pick = if *max { x.max(y) } else { x.min(y) };
            if a.ty() == Ty::I64 && b.ty() == Ty::I64 {
                whole(pick)
            } else {
                dec(pick, scale)
            }
        }
        Expr::Abs(x) => {
            let v = eval(x, ctx)?;
            let (n, s) = number(&v, "`abs`")?;
            let n = n.checked_abs().ok_or(EvalError::Overflow)?;
            if v.ty() == Ty::I64 {
                whole(n)
            } else {
                dec(n, s)
            }
        }
    }
}

fn round(value: &Expr, scale: u8, mode: RoundMode, ctx: &Context) -> Result<Value, EvalError> {
    if scale > MAX_DIGITS {
        return Err(EvalError::Type("`round` keeps at most 38 digits".into()));
    }
    if let Expr::Binary(BinOp::Div, a, b) = value {
        // a / b where a = A / 10^sa and b = B / 10^sb, wanted at `scale` digits:
        //   result = A * 10^(sb + scale) / (B * 10^sa), settled once.
        let (va, vb) = (eval(a, ctx)?, eval(b, ctx)?);
        let (na, sa) = number(&va, "`/`")?;
        let (nb, sb) = number(&vb, "`/`")?;
        if nb == 0 {
            return Err(EvalError::DivisionByZero);
        }
        let up = pow10(u32::from(sb) + u32::from(scale)).ok_or(EvalError::Overflow)?;
        let down = pow10(u32::from(sa)).ok_or(EvalError::Overflow)?;
        let top = na.checked_mul(up).ok_or(EvalError::Overflow)?;
        let bottom = nb.checked_mul(down).ok_or(EvalError::Overflow)?;
        return rounded(divide_rounded(top, bottom, mode)?, scale);
    }
    let v = eval(value, ctx)?;
    let (n, s) = number(&v, "`round`")?;
    let settled = if scale >= s {
        let f = pow10(u32::from(scale - s)).ok_or(EvalError::Overflow)?;
        n.checked_mul(f).ok_or(EvalError::Overflow)?
    } else {
        let f = pow10(u32::from(s - scale)).ok_or(EvalError::Overflow)?;
        divide_rounded(n, f, mode)?
    };
    rounded(settled, scale)
}

fn binary(op: BinOp, a: &Expr, b: &Expr, ctx: &Context) -> Result<Value, EvalError> {
    match op {
        BinOp::And | BinOp::Or => {
            let left = boolean(&eval(a, ctx)?, "`and`/`or`")?;
            // Stop when the left side decides the answer.
            if (op == BinOp::And && !left) || (op == BinOp::Or && left) {
                return Ok(Value::Bool(left));
            }
            Ok(Value::Bool(boolean(&eval(b, ctx)?, "`and`/`or`")?))
        }
        BinOp::Div => Err(EvalError::Type(
            "`/` is only allowed directly inside `round`".into(),
        )),
        _ => {
            let (va, vb) = (eval(a, ctx)?, eval(b, ctx)?);
            match op {
                BinOp::Eq | BinOp::Ne => {
                    let equal = compare(&va, &vb)? == Ordering::Equal;
                    Ok(Value::Bool(equal == (op == BinOp::Eq)))
                }
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    let ord = order(&va, &vb)?;
                    Ok(Value::Bool(match op {
                        BinOp::Lt => ord == Ordering::Less,
                        BinOp::Le => ord != Ordering::Greater,
                        BinOp::Gt => ord == Ordering::Greater,
                        _ => ord != Ordering::Less,
                    }))
                }
                BinOp::Add | BinOp::Sub => {
                    let (x, sa) = number(&va, "`+`/`-`")?;
                    let (y, sb) = number(&vb, "`+`/`-`")?;
                    let (x, y, scale) = align(x, sa, y, sb)?;
                    let r = if op == BinOp::Add {
                        x.checked_add(y)
                    } else {
                        x.checked_sub(y)
                    }
                    .ok_or(EvalError::Overflow)?;
                    if va.ty() == Ty::I64 && vb.ty() == Ty::I64 {
                        whole(r)
                    } else {
                        dec(r, scale)
                    }
                }
                _ => {
                    let (x, sa) = number(&va, "`*`")?;
                    let (y, sb) = number(&vb, "`*`")?;
                    let scale = sa + sb;
                    if scale > MAX_DIGITS {
                        return Err(EvalError::Type(
                            "a product would have more than 38 digits after the point".into(),
                        ));
                    }
                    let r = x.checked_mul(y).ok_or(EvalError::Overflow)?;
                    if va.ty() == Ty::I64 && vb.ty() == Ty::I64 {
                        whole(r)
                    } else {
                        dec(r, scale)
                    }
                }
            }
        }
    }
}

/// `==` and `!=`: numbers across scales, otherwise the same type on both sides.
fn compare(a: &Value, b: &Value) -> Result<Ordering, EvalError> {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => Ok(x.cmp(y)),
        (Value::Text(x), Value::Text(y)) => Ok(x.cmp(y)),
        (Value::Bytes(x), Value::Bytes(y)) => Ok(x.cmp(y)),
        (Value::Timestamp(x), Value::Timestamp(y)) => Ok(x.cmp(y)),
        _ => numbers(a, b, "cannot compare"),
    }
}

/// `<`, `<=`, `>`, `>=`: numbers or timestamps only.
fn order(a: &Value, b: &Value) -> Result<Ordering, EvalError> {
    match (a, b) {
        (Value::Timestamp(x), Value::Timestamp(y)) => Ok(x.cmp(y)),
        _ => numbers(a, b, "cannot order"),
    }
}

fn numbers(a: &Value, b: &Value, what: &str) -> Result<Ordering, EvalError> {
    let wrong = || EvalError::Type(format!("{what} {} with {}", a.ty().name(), b.ty().name()));
    let (x, sa) = number(a, what).map_err(|_| wrong())?;
    let (y, sb) = number(b, what).map_err(|_| wrong())?;
    let (x, y, _) = align(x, sa, y, sb)?;
    Ok(x.cmp(&y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::parse_expr;
    use proptest::prelude::*;

    fn run(text: &str, args: &[(&str, Value)]) -> Result<Value, EvalError> {
        let ctx = Context {
            vars: args
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            now_micros: 1_700_000_000_000_000,
        };
        eval(&parse_expr(text).unwrap(), &ctx)
    }

    fn d(value: i128, scale: u8) -> Value {
        Value::Dec { value, scale }
    }

    #[test]
    fn whole_numbers_stay_whole_and_decimals_keep_their_scale() {
        assert_eq!(run("7", &[]), Ok(Value::I64(7)));
        assert_eq!(run("1 + 2 * 3", &[]), Ok(Value::I64(7)));
        assert_eq!(run("1.50", &[]), Ok(d(150, 2)));
        assert_eq!(run("1.5 + 2", &[]), Ok(d(35, 1)));
        assert_eq!(run("1.50 + 0.5", &[]), Ok(d(200, 2)));
        assert_eq!(run("0.5 * 0.25", &[]), Ok(d(125, 3)));
        assert_eq!(run("-3 + 1", &[]), Ok(Value::I64(-2)));
        assert_eq!(run("abs(-2.5)", &[]), Ok(d(25, 1)));
        assert_eq!(run("min(3, 2.50)", &[]), Ok(d(250, 2)));
        assert_eq!(run("max(3, 4)", &[]), Ok(Value::I64(4)));
        assert_eq!(run("now", &[]), Ok(Value::Timestamp(1_700_000_000_000_000)));
    }

    #[test]
    fn values_are_sent_to_the_database_as_exact_text() {
        assert_eq!(d(150, 2).number_text().as_deref(), Some("1.50"));
        assert_eq!(d(-5, 3).number_text().as_deref(), Some("-0.005"));
        assert_eq!(Value::I64(-7).number_text().as_deref(), Some("-7"));
        assert_eq!(Value::Timestamp(12).number_text().as_deref(), Some("12"));
        assert_eq!(Value::Text("x".into()).number_text(), None);
    }

    #[test]
    fn rounding_modes_on_both_signs() {
        // (value as scaled text, expected at scale 0) for half_even, half_up, down.
        for (x, even, up, down) in [
            ("0.5", 0, 1, 0),
            ("1.5", 2, 2, 1),
            ("2.5", 2, 3, 2),
            ("-0.5", 0, -1, 0),
            ("-1.5", -2, -2, -1),
            ("-2.5", -2, -3, -2),
            ("2.4", 2, 2, 2),
            ("2.6", 3, 3, 2),
            ("-2.6", -3, -3, -2),
        ] {
            for (mode, want) in [("half_even", even), ("half_up", up), ("down", down)] {
                assert_eq!(
                    run(&format!("round({x}, 0, {mode})"), &[]),
                    Ok(Value::I64(want)),
                    "{x} {mode}"
                );
            }
        }
        assert_eq!(run("round(1.005, 2, half_up)", &[]), Ok(d(101, 2)));
        assert_eq!(run("round(1.005, 2, half_even)", &[]), Ok(d(100, 2)));
        assert_eq!(run("round(1.5, 3, down)", &[]), Ok(d(1500, 3)));
    }

    #[test]
    fn a_division_is_rounded_once_from_exact_numbers() {
        assert_eq!(run("round(1 / 3, 4, half_even)", &[]), Ok(d(3333, 4)));
        assert_eq!(run("round(2 / 3, 4, half_even)", &[]), Ok(d(6667, 4)));
        assert_eq!(run("round(-2 / 3, 0, half_up)", &[]), Ok(Value::I64(-1)));
        assert_eq!(run("round(1 / 2, 0, half_even)", &[]), Ok(Value::I64(0)));
        assert_eq!(run("round(3 / 2, 0, half_even)", &[]), Ok(Value::I64(2)));
        // Scales on both sides: 1.50 / 0.25 = 6.
        assert_eq!(run("round(1.50 / 0.25, 0, down)", &[]), Ok(Value::I64(6)));
        assert_eq!(run("round(1.00 / 3, 2, down)", &[]), Ok(d(33, 2)));
        assert_eq!(
            run(
                "round(a / b, 2, half_even)",
                &[("a", Value::I64(10)), ("b", d(30, 1))]
            ),
            Ok(d(333, 2))
        );
    }

    #[test]
    fn the_starter_interest_expression_rounds_half_even() {
        // round(balance * rate, 2, half_even) with a whole balance and a 6-digit rate; 12.345 is an exact tie, so the even 12.34 wins.
        let args = [("balance", Value::I64(1_000)), ("rate", d(12_345, 6))];
        assert_eq!(
            run("round(balance * rate, 2, half_even)", &args),
            Ok(d(1_234, 2))
        );
        let args = [("balance", Value::I64(1)), ("rate", d(5_000, 6))];
        assert_eq!(
            run("round(balance * rate, 2, half_even)", &args),
            Ok(d(0, 2))
        );
    }

    #[test]
    fn comparisons_cross_scales_and_types_are_checked() {
        assert_eq!(run("1 == 1.00", &[]), Ok(Value::Bool(true)));
        assert_eq!(run("1.5 > 1", &[]), Ok(Value::Bool(true)));
        assert_eq!(run("2 <= 1.99", &[]), Ok(Value::Bool(false)));
        assert_eq!(run("1 != 2", &[]), Ok(Value::Bool(true)));
        assert_eq!(run("now >= now", &[]), Ok(Value::Bool(true)));
        assert_eq!(
            run(
                "a == b",
                &[
                    ("a", Value::Text("x".into())),
                    ("b", Value::Text("x".into()))
                ]
            ),
            Ok(Value::Bool(true))
        );
        assert!(matches!(
            run("a == 1", &[("a", Value::Text("x".into()))]),
            Err(EvalError::Type(_))
        ));
        assert!(matches!(
            run(
                "a < b",
                &[
                    ("a", Value::Text("x".into())),
                    ("b", Value::Text("y".into()))
                ]
            ),
            Err(EvalError::Type(_))
        ));
        assert!(matches!(run("true + 1", &[]), Err(EvalError::Type(_))));
        assert!(matches!(run("1 and true", &[]), Err(EvalError::Type(_))));
    }

    #[test]
    fn and_or_stop_at_the_left_side() {
        let zero = [("b", Value::I64(0))];
        assert_eq!(
            run("b != 0 and round(1 / b, 0, down) == 1", &zero),
            Ok(Value::Bool(false))
        );
        assert_eq!(
            run("b == 0 or round(1 / b, 0, down) == 1", &zero),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            run("b == 0 and round(1 / b, 0, down) == 1", &zero),
            Err(EvalError::DivisionByZero)
        );
        assert_eq!(run("not false", &[]), Ok(Value::Bool(true)));
    }

    #[test]
    fn nothing_wraps_and_zero_is_not_a_divisor() {
        let big = [("n", Value::I64(i64::MAX))];
        assert_eq!(run("n + 1", &big), Err(EvalError::Overflow));
        assert_eq!(run("n * 2", &big), Err(EvalError::Overflow));
        assert_eq!(run("-n - 2", &big), Err(EvalError::Overflow));
        assert_eq!(
            run("abs(n)", &[("n", Value::I64(i64::MIN))]),
            Err(EvalError::Overflow)
        );
        assert_eq!(
            run("-n", &[("n", Value::I64(i64::MIN))]),
            Err(EvalError::Overflow)
        );
        assert_eq!(
            run(
                "round(a / b, 0, down)",
                &[("a", Value::I64(1)), ("b", Value::I64(0))]
            ),
            Err(EvalError::DivisionByZero)
        );
        // i64::MIN / -1 does not fit an i64.
        assert_eq!(
            run(
                "round(a / b, 0, down)",
                &[("a", Value::I64(i64::MIN)), ("b", Value::I64(-1))]
            ),
            Err(EvalError::Overflow)
        );
        // A decimal past 38 digits.
        let huge = d(10i128.pow(37) * 9, 0);
        assert_eq!(run("a + a", &[("a", huge)]), Err(EvalError::Overflow));
        assert_eq!(run("x", &[]), Err(EvalError::Unknown("x".into())));
    }

    #[test]
    fn a_product_with_too_many_decimals_is_a_type_error() {
        let a = d(1, 30);
        assert!(matches!(run("a * a", &[("a", a)]), Err(EvalError::Type(_))));
    }

    #[test]
    fn the_frozen_starter_plans_evaluate() {
        let args = [("amount", Value::I64(250)), ("balance", Value::I64(1_000))];
        assert_eq!(run("balance - amount", &args), Ok(Value::I64(750)));
        assert_eq!(run("balance + amount", &args), Ok(Value::I64(1_250)));
        assert_eq!(run("balance >= amount", &args), Ok(Value::Bool(true)));
        assert_eq!(run("-amount", &args), Ok(Value::I64(-250)));
    }

    proptest! {
        /// Half-up on a division equals the plain formula on whole numbers.
        #[test]
        fn half_up_division_matches_the_plain_formula(
            n in -1_000_000i64..1_000_000,
            dv in prop_oneof![-1_000i64..-1, 1i64..1_000],
        ) {
            let got = run("round(a / b, 0, half_up)", &[("a", Value::I64(n)), ("b", Value::I64(dv))]).unwrap();
            let (n, dv) = (i128::from(n), i128::from(dv));
            // Reference: floor((2|n| + |d|) / (2|d|)) with the sign of the quotient.
            let mag = (2 * n.abs() + dv.abs()) / (2 * dv.abs());
            let want = if (n < 0) != (dv < 0) { -mag } else { mag };
            prop_assert_eq!(got, Value::I64(i64::try_from(want).unwrap()));
        }

        /// Half-even equals half-up except on exact ties, where it picks the even neighbour.
        #[test]
        fn half_even_is_half_up_except_on_exact_ties(
            n in -100_000i64..100_000,
            dv in prop_oneof![-100i64..-1, 1i64..100],
        ) {
            let args = [("a", Value::I64(n)), ("b", Value::I64(dv))];
            let up = run("round(a / b, 0, half_up)", &args).unwrap();
            let even = run("round(a / b, 0, half_even)", &args).unwrap();
            let (Value::I64(u), Value::I64(e)) = (up, even) else { panic!() };
            let r = (i128::from(n) % i128::from(dv)).abs();
            let tie = r != 0 && 2 * r == i128::from(dv).abs();
            if tie {
                prop_assert!(e % 2 == 0 && (u - e).abs() <= 1);
            } else {
                prop_assert_eq!(u, e);
            }
        }

        /// `down` never moves away from zero: |down| <= |half_even| <= |down| + 1.
        #[test]
        fn down_is_truncation(
            n in -100_000i64..100_000,
            dv in prop_oneof![-100i64..-1, 1i64..100],
        ) {
            let args = [("a", Value::I64(n)), ("b", Value::I64(dv))];
            let Value::I64(t) = run("round(a / b, 0, down)", &args).unwrap() else { panic!() };
            prop_assert_eq!(i128::from(t), i128::from(n) / i128::from(dv));
        }

        /// Addition and subtraction undo each other exactly, across scales.
        #[test]
        fn add_then_subtract_returns_the_start(
            a in -10_000_000_000i64..10_000_000_000,
            b in -10_000_000_000i64..10_000_000_000,
            sa in 0u8..6,
            sb in 0u8..6,
        ) {
            let env = [("a", d(i128::from(a), sa)), ("b", d(i128::from(b), sb))];
            let Value::Dec { value, scale } = run("(a + b) - b", &env).unwrap() else { panic!() };
            // The same number as `a`, possibly written at a larger scale.
            let (x, y, _) = align(value, scale, i128::from(a), sa).unwrap();
            prop_assert_eq!(x, y);
        }

        /// Multiplication adds the scales and loses nothing.
        #[test]
        fn product_is_exact(
            a in -1_000_000i64..1_000_000,
            b in -1_000_000i64..1_000_000,
            sa in 0u8..6,
            sb in 0u8..6,
        ) {
            let env = [("a", d(i128::from(a), sa)), ("b", d(i128::from(b), sb))];
            let got = run("a * b", &env).unwrap();
            prop_assert_eq!(got, d(i128::from(a) * i128::from(b), sa + sb));
        }

        /// Comparisons agree with the ordering of the exact numbers.
        #[test]
        fn comparison_matches_cross_multiplication(
            a in -1_000_000i64..1_000_000,
            b in -1_000_000i64..1_000_000,
            sa in 0u8..5,
            sb in 0u8..5,
        ) {
            let env = [("a", d(i128::from(a), sa)), ("b", d(i128::from(b), sb))];
            let lt = run("a < b", &env).unwrap();
            let lhs = i128::from(a) * 10i128.pow(u32::from(sb));
            let rhs = i128::from(b) * 10i128.pow(u32::from(sa));
            prop_assert_eq!(lt, Value::Bool(lhs < rhs));
        }

        /// No input makes the evaluator panic.
        #[test]
        fn evaluating_never_panics(
            a in any::<i64>(),
            b in any::<i64>(),
            ea in 0u8..39,
            eb in 0u8..39,
            pick in 0usize..9,
        ) {
            let texts = [
                "a + b", "a - b", "a * b", "-a", "abs(a)", "round(a / b, 2, half_even)",
                "round(a * b, 0, half_up)", "min(a, b)", "a == b",
            ];
            let env = [("a", d(i128::from(a) * 1_000_000_000_000, ea)), ("b", d(i128::from(b), eb))];
            let _ = run(texts[pick], &env);
            let env = [("a", Value::I64(a)), ("b", Value::I64(b))];
            let _ = run(texts[pick], &env);
        }
    }
}
