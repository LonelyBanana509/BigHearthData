//! Types and value ranges of expressions (spec `recipe-compiler.md`, section 2: `type`,
//! `rounding`, `bounds`). Every expression gets a type and, for numbers, a range it provably stays
//! in, from the arguments' bounds and the columns' bounds. A range is exact arithmetic on scaled
//! whole numbers; `None` means unbounded on that side. Nothing here ever rounds: only `round` can
//! change a number's scale downward.

use std::collections::BTreeMap;

use bhdata_schema::model::ColumnType;

use crate::expr::{BinOp, Expr, UnOp};
use crate::reject::Code;

/// The type of a value in an expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// 64-bit whole number.
    I64,
    /// Exact decimal with this many digits after the point.
    Dec(u8),
    /// `true` or `false`.
    Bool,
    /// Text.
    Text,
    /// Bytes.
    Bytes,
    /// A UTC time.
    Timestamp,
}

impl Ty {
    /// Whether arithmetic applies.
    #[must_use]
    pub fn numeric(self) -> bool {
        matches!(self, Ty::I64 | Ty::Dec(_))
    }

    /// Digits after the point (0 for everything but a decimal).
    #[must_use]
    pub fn scale(self) -> u8 {
        match self {
            Ty::Dec(s) => s,
            _ => 0,
        }
    }

    /// The type of a column of this type.
    #[must_use]
    pub fn of_column(ty: ColumnType) -> Ty {
        match ty {
            ColumnType::I64 => Ty::I64,
            ColumnType::Decimal { scale, .. } => Ty::Dec(scale),
            ColumnType::Text { .. } => Ty::Text,
            ColumnType::Bytes { .. } => Ty::Bytes,
            ColumnType::Bool => Ty::Bool,
            ColumnType::Timestamp => Ty::Timestamp,
        }
    }

    /// How a type is named in messages.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Ty::I64 => "i64".into(),
            Ty::Dec(s) => format!("decimal with {s} digits after the point"),
            Ty::Bool => "bool".into(),
            Ty::Text => "text".into(),
            Ty::Bytes => "bytes".into(),
            Ty::Timestamp => "timestamp".into(),
        }
    }
}

/// The values a number can take, scaled by `10^scale` of its type; `None` is unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Range {
    /// Smallest value.
    pub lo: Option<i128>,
    /// Largest value.
    pub hi: Option<i128>,
}

fn pow10(n: u8) -> Option<i128> {
    10i128.checked_pow(u32::from(n))
}

fn floor_div(v: i128, d: i128) -> i128 {
    v.div_euclid(d)
}

fn ceil_div(v: i128, d: i128) -> i128 {
    -((-v).div_euclid(d))
}

impl Range {
    /// Exactly `v`.
    #[must_use]
    pub fn point(v: i128) -> Range {
        Range {
            lo: Some(v),
            hi: Some(v),
        }
    }

    /// Both ends known.
    #[must_use]
    pub fn between(lo: i128, hi: i128) -> Range {
        Range {
            lo: Some(lo),
            hi: Some(hi),
        }
    }

    /// The same values at another scale. Growing the scale is exact; shrinking it widens the
    /// range outward, so the result always contains every rounded value.
    #[must_use]
    pub fn shift(self, from: u8, to: u8) -> Range {
        if to >= from {
            let Some(f) = pow10(to - from) else {
                return Range::default();
            };
            Range {
                lo: self.lo.and_then(|v| v.checked_mul(f)),
                hi: self.hi.and_then(|v| v.checked_mul(f)),
            }
        } else {
            let Some(f) = pow10(from - to) else {
                return Range::default();
            };
            Range {
                lo: self.lo.map(|v| floor_div(v, f)),
                hi: self.hi.map(|v| ceil_div(v, f)),
            }
        }
    }

    fn add(self, o: Range) -> Range {
        Range {
            lo: self.lo.zip(o.lo).and_then(|(a, b)| a.checked_add(b)),
            hi: self.hi.zip(o.hi).and_then(|(a, b)| a.checked_add(b)),
        }
    }

    fn neg(self) -> Range {
        Range {
            lo: self.hi.and_then(i128::checked_neg),
            hi: self.lo.and_then(i128::checked_neg),
        }
    }

    fn mul(self, o: Range) -> Range {
        let (Some(a), Some(b), Some(c), Some(d)) = (self.lo, self.hi, o.lo, o.hi) else {
            return Range::default();
        };
        let corners: Option<Vec<i128>> = [(a, c), (a, d), (b, c), (b, d)]
            .into_iter()
            .map(|(x, y)| x.checked_mul(y))
            .collect();
        match corners {
            Some(v) => Range {
                lo: v.iter().copied().min(),
                hi: v.iter().copied().max(),
            },
            None => Range::default(),
        }
    }

    fn abs(self) -> Range {
        match (self.lo, self.hi) {
            (Some(lo), _) if lo >= 0 => self,
            (_, Some(hi)) if hi <= 0 => self.neg(),
            (Some(lo), Some(hi)) => Range {
                lo: Some(0),
                hi: lo.checked_neg().map(|n| n.max(hi)),
            },
            _ => Range {
                lo: Some(0),
                hi: None,
            },
        }
    }

    fn min_with(self, o: Range) -> Range {
        Range {
            lo: self.lo.zip(o.lo).map(|(a, b)| a.min(b)),
            hi: match (self.hi, o.hi) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        }
    }

    fn max_with(self, o: Range) -> Range {
        Range {
            lo: match (self.lo, o.lo) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (a, b) => a.or(b),
            },
            hi: self.hi.zip(o.hi).map(|(a, b)| a.max(b)),
        }
    }
}

/// A typed expression: its type and the range of its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Typed {
    /// The type.
    pub ty: Ty,
    /// The range, for numbers.
    pub range: Range,
}

impl Typed {
    /// A non-number of this type.
    #[must_use]
    pub fn plain(ty: Ty) -> Typed {
        Typed {
            ty,
            range: Range::default(),
        }
    }

    /// A column's values: its declared bounds, or what its type can hold.
    #[must_use]
    pub fn of_column(ty: ColumnType, min: Option<i128>, max: Option<i128>) -> Typed {
        let (lo, hi) = match ty {
            ColumnType::I64 => (i128::from(i64::MIN), i128::from(i64::MAX)),
            ColumnType::Decimal { precision, .. } => {
                let top = pow10(precision).map_or(i128::MAX, |p| p - 1);
                (-top, top)
            }
            _ => return Typed::plain(Ty::of_column(ty)),
        };
        Typed {
            ty: Ty::of_column(ty),
            range: Range::between(min.map_or(lo, |m| m.max(lo)), max.map_or(hi, |m| m.min(hi))),
        }
    }
}

/// The names an expression may use, with their types: arguments, then values read by earlier steps.
pub type Env = BTreeMap<String, Typed>;

/// Why an expression is refused: a code and a sentence (the caller adds the step).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fail {
    /// `type`, `rounding`.
    pub code: Code,
    /// What is wrong.
    pub message: String,
}

fn fail<T>(code: Code, message: impl Into<String>) -> Result<T, Fail> {
    Err(Fail {
        code,
        message: message.into(),
    })
}

fn number(t: Typed, what: &str) -> Result<Typed, Fail> {
    if t.ty.numeric() {
        Ok(t)
    } else {
        fail(
            Code::Type,
            format!("{what} needs a number, not {}", t.ty.name()),
        )
    }
}

/// The type and range of `e`.
///
/// # Errors
/// [`Code::Type`] for a wrong or unknown type or name, [`Code::Rounding`] for `/` outside `round`.
pub fn eval(e: &Expr, env: &Env) -> Result<Typed, Fail> {
    match e {
        Expr::Num { value, scale } => {
            if *scale == 0 {
                if i64::try_from(*value).is_err() {
                    return fail(Code::Type, format!("{value} does not fit an i64"));
                }
                Ok(Typed {
                    ty: Ty::I64,
                    range: Range::point(*value),
                })
            } else {
                Ok(Typed {
                    ty: Ty::Dec(*scale),
                    range: Range::point(*value),
                })
            }
        }
        Expr::Bool(_) => Ok(Typed::plain(Ty::Bool)),
        Expr::Now => Ok(Typed::plain(Ty::Timestamp)),
        Expr::Var(name) => env.get(name).copied().map_or_else(
            || {
                fail(
                    Code::Type,
                    format!("`{name}` is not an argument or a value read by an earlier step"),
                )
            },
            Ok,
        ),
        Expr::Unary(UnOp::Neg, x) => {
            let t = number(eval(x, env)?, "`-`")?;
            Ok(Typed {
                ty: t.ty,
                range: t.range.neg(),
            })
        }
        Expr::Unary(UnOp::Not, x) => bool_of(eval(x, env)?, "`not`").map(Typed::plain),
        Expr::Binary(op, a, b) => binary(*op, a, b, env),
        Expr::Round { value, scale, .. } => {
            let (from, range) = match &**value {
                Expr::Binary(BinOp::Div, a, b) => {
                    let (ta, tb) = (number(eval(a, env)?, "`/`")?, number(eval(b, env)?, "`/`")?);
                    if matches!(**b, Expr::Num { value: 0, .. }) {
                        return fail(Code::Type, "division by zero");
                    }
                    (ta.ty.scale().max(tb.ty.scale()), Range::default())
                }
                other => {
                    let t = number(eval(other, env)?, "`round`")?;
                    (t.ty.scale(), t.range)
                }
            };
            let ty = if *scale == 0 {
                Ty::I64
            } else {
                Ty::Dec(*scale)
            };
            Ok(Typed {
                ty,
                range: range.shift(from, *scale),
            })
        }
        Expr::MinMax { max, args } => {
            let (a, b) = (
                number(eval(&args.0, env)?, "`min`/`max`")?,
                number(eval(&args.1, env)?, "`min`/`max`")?,
            );
            let scale = a.ty.scale().max(b.ty.scale());
            let ty = if a.ty == Ty::I64 && b.ty == Ty::I64 {
                Ty::I64
            } else {
                Ty::Dec(scale)
            };
            let (ra, rb) = (
                a.range.shift(a.ty.scale(), scale),
                b.range.shift(b.ty.scale(), scale),
            );
            Ok(Typed {
                ty,
                range: if *max {
                    ra.max_with(rb)
                } else {
                    ra.min_with(rb)
                },
            })
        }
        Expr::Abs(x) => {
            let t = number(eval(x, env)?, "`abs`")?;
            Ok(Typed {
                ty: t.ty,
                range: t.range.abs(),
            })
        }
    }
}

fn bool_of(t: Typed, what: &str) -> Result<Ty, Fail> {
    if t.ty == Ty::Bool {
        Ok(Ty::Bool)
    } else {
        fail(
            Code::Type,
            format!("{what} needs a bool, not {}", t.ty.name()),
        )
    }
}

fn binary(op: BinOp, a: &Expr, b: &Expr, env: &Env) -> Result<Typed, Fail> {
    if op == BinOp::Div {
        return fail(
            Code::Rounding,
            "`/` is only allowed directly inside `round(..., scale, mode)`",
        );
    }
    let (ta, tb) = (eval(a, env)?, eval(b, env)?);
    match op {
        BinOp::And | BinOp::Or => {
            bool_of(ta, "`and`/`or`")?;
            bool_of(tb, "`and`/`or`")?;
            Ok(Typed::plain(Ty::Bool))
        }
        BinOp::Eq | BinOp::Ne => {
            let same = (ta.ty.numeric() && tb.ty.numeric()) || ta.ty == tb.ty;
            if same {
                Ok(Typed::plain(Ty::Bool))
            } else {
                fail(
                    Code::Type,
                    format!("cannot compare {} with {}", ta.ty.name(), tb.ty.name()),
                )
            }
        }
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let ok = (ta.ty.numeric() && tb.ty.numeric())
                || (ta.ty == Ty::Timestamp && tb.ty == Ty::Timestamp);
            if ok {
                Ok(Typed::plain(Ty::Bool))
            } else {
                fail(
                    Code::Type,
                    format!("cannot order {} against {}", ta.ty.name(), tb.ty.name()),
                )
            }
        }
        BinOp::Add | BinOp::Sub => {
            let (ta, tb) = (number(ta, "`+`/`-`")?, number(tb, "`+`/`-`")?);
            let scale = ta.ty.scale().max(tb.ty.scale());
            let ty = if ta.ty == Ty::I64 && tb.ty == Ty::I64 {
                Ty::I64
            } else {
                Ty::Dec(scale)
            };
            let (ra, rb) = (
                ta.range.shift(ta.ty.scale(), scale),
                tb.range.shift(tb.ty.scale(), scale),
            );
            let range = if op == BinOp::Add {
                ra.add(rb)
            } else {
                ra.add(rb.neg())
            };
            Ok(Typed { ty, range })
        }
        _ => {
            let (ta, tb) = (number(ta, "`*`")?, number(tb, "`*`")?);
            let scale = ta.ty.scale() + tb.ty.scale();
            if scale > 38 {
                return fail(
                    Code::Type,
                    "a product would have more than 38 digits after the point",
                );
            }
            let ty = if ta.ty == Ty::I64 && tb.ty == Ty::I64 {
                Ty::I64
            } else {
                Ty::Dec(scale)
            };
            Ok(Typed {
                ty,
                range: ta.range.mul(tb.range),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::parse_expr;

    fn env() -> Env {
        let mut e = Env::new();
        e.insert(
            "amount".into(),
            Typed {
                ty: Ty::I64,
                range: Range::between(1, 1_000_000),
            },
        );
        e.insert(
            "balance".into(),
            Typed {
                ty: Ty::I64,
                range: Range::between(0, 1_000_000_000),
            },
        );
        e.insert(
            "rate".into(),
            Typed {
                ty: Ty::Dec(6),
                range: Range::between(0, 1_000_000),
            },
        );
        e.insert(
            "price".into(),
            Typed {
                ty: Ty::Dec(2),
                range: Range::between(-500, 500),
            },
        );
        e.insert("name".into(), Typed::plain(Ty::Text));
        e
    }

    fn ty(text: &str) -> Result<Typed, Fail> {
        eval(&parse_expr(text).unwrap(), &env())
    }

    fn ok(text: &str) -> Typed {
        ty(text).unwrap()
    }

    #[test]
    fn literals_arguments_and_now() {
        assert_eq!(
            ok("7"),
            Typed {
                ty: Ty::I64,
                range: Range::point(7)
            }
        );
        assert_eq!(
            ok("1.50"),
            Typed {
                ty: Ty::Dec(2),
                range: Range::point(150)
            }
        );
        assert_eq!(ok("now").ty, Ty::Timestamp);
        assert_eq!(ok("true").ty, Ty::Bool);
        assert_eq!(ok("amount").range, Range::between(1, 1_000_000));
        assert_eq!(ty("nope").unwrap_err().code, Code::Type);
        assert_eq!(ty("9223372036854775808").unwrap_err().code, Code::Type);
    }

    #[test]
    fn arithmetic_tracks_scale_and_range_exactly() {
        assert_eq!(ok("-amount").range, Range::between(-1_000_000, -1));
        assert_eq!(
            ok("balance + amount").range,
            Range::between(1, 1_001_000_000)
        );
        assert_eq!(
            ok("balance - amount").range,
            Range::between(-1_000_000, 999_999_999)
        );
        // i64 + decimal: the whole number is brought to the decimal's scale first.
        let t = ok("amount + price");
        assert_eq!(
            (t.ty, t.range),
            (Ty::Dec(2), Range::between(100 - 500, 100_000_000 + 500))
        );
        // Products add their scales.
        let t = ok("balance * rate");
        assert_eq!(t.ty, Ty::Dec(6));
        assert_eq!(t.range, Range::between(0, 1_000_000_000 * 1_000_000));
        let t = ok("price * price");
        assert_eq!(
            (t.ty, t.range),
            (Ty::Dec(4), Range::between(-250_000, 250_000))
        );
        assert_eq!(ok("abs(price)").range, Range::between(0, 500));
        assert_eq!(ok("min(amount, 10)").range, Range::between(1, 10));
        assert_eq!(ok("max(amount, 10)").range, Range::between(10, 1_000_000));
    }

    #[test]
    fn round_is_the_only_way_down_in_scale_and_it_widens_outward() {
        let t = ok("round(balance * rate, 2, half_even)");
        assert_eq!(t.ty, Ty::Dec(2));
        assert_eq!(t.range, Range::between(0, 1_000_000_000 * 100));
        let t = ok("round(price, 0, down)");
        assert_eq!((t.ty, t.range), (Ty::I64, Range::between(-5, 5)));
        let t = ok("round(amount / 3, 2, half_up)");
        assert_eq!((t.ty, t.range), (Ty::Dec(2), Range::default()));
        assert_eq!(ok("round(amount, 2, down)").ty, Ty::Dec(2));
    }

    #[test]
    fn division_only_inside_round_and_never_by_a_zero_literal() {
        assert_eq!(ty("amount / 2").unwrap_err().code, Code::Rounding);
        assert_eq!(
            ty("round(amount / 2 + 1, 0, down)").unwrap_err().code,
            Code::Rounding
        );
        assert_eq!(
            ty("round(amount / 0, 0, down)").unwrap_err().code,
            Code::Type
        );
        assert_eq!(ty("round(name / 2, 0, down)").unwrap_err().code, Code::Type);
    }

    #[test]
    fn comparisons_and_logic_need_matching_types() {
        for text in [
            "amount >= 1 and balance < 5",
            "name == name",
            "now < now",
            "amount == price",
            "not (amount == 1)",
        ] {
            assert_eq!(ok(text).ty, Ty::Bool, "{text}");
        }
        for text in [
            "name < name",
            "amount == name",
            "amount and true",
            "not amount",
            "now + 1",
            "name * 2",
            "-name",
            "abs(true)",
            "round(name, 0, down)",
            "now == 1",
        ] {
            assert_eq!(ty(text).unwrap_err().code, Code::Type, "{text}");
        }
    }

    #[test]
    fn a_range_that_overflows_becomes_unbounded_not_wrong() {
        let mut e = Env::new();
        e.insert(
            "big".into(),
            Typed {
                ty: Ty::I64,
                range: Range::between(i128::MAX - 1, i128::MAX),
            },
        );
        let t = eval(&parse_expr("big + big").unwrap(), &e).unwrap();
        assert_eq!(t.range, Range::default());
        let t = eval(&parse_expr("big * big").unwrap(), &e).unwrap();
        assert_eq!(t.range, Range::default());
    }

    #[test]
    fn column_values_are_bounded_by_their_bounds_or_their_type() {
        let t = Typed::of_column(ColumnType::I64, Some(0), Some(100));
        assert_eq!(t.range, Range::between(0, 100));
        let t = Typed::of_column(
            ColumnType::Decimal {
                precision: 4,
                scale: 2,
            },
            None,
            None,
        );
        assert_eq!((t.ty, t.range), (Ty::Dec(2), Range::between(-9999, 9999)));
        assert_eq!(
            Typed::of_column(ColumnType::I64, None, None).range,
            Range::between(i128::from(i64::MIN), i128::from(i64::MAX))
        );
        assert_eq!(
            Typed::of_column(ColumnType::Bool, None, None),
            Typed::plain(Ty::Bool)
        );
    }

    #[test]
    fn shifting_to_a_smaller_scale_keeps_every_rounded_value_inside() {
        // 1.25 and -1.25 at scale 2 rounded to scale 0 can be 1, 2, -1 or -2.
        let r = Range::between(-125, 125).shift(2, 0);
        assert_eq!(r, Range::between(-2, 2));
        assert_eq!(Range::between(0, 99).shift(2, 0), Range::between(0, 1));
    }
}
