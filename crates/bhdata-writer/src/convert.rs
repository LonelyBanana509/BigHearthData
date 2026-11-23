//! Values to and from the database (spec `write-path.md`, section 3, and the plan's statement
//! shapes in `recipe-compiler.md`, section 10).
//!
//! A plan's types are text: `i64`, `decimal(18,2)`, `text(64)`, `bool`, `timestamp`, `bytes(16)`.
//! Every statement is prepared with explicit parameter types, so a decimal travels as text (the
//! statement casts it with `::numeric`) and a timestamp as whole microseconds (`::bigint`); no float
//! is involved anywhere.

use bhdata_api::recipe::eval::Value;
use bhdata_api::types::Error;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::Row;

/// A plan type, parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `i64`.
    I64,
    /// `decimal(p,s)`, with its scale.
    Dec(u8),
    /// `bool`.
    Bool,
    /// `text(n)`.
    Text,
    /// `bytes(n)`.
    Bytes,
    /// `timestamp`.
    Timestamp,
}

/// Parses a plan type.
///
/// # Errors
/// `internal`: the registry holds a plan this release cannot read.
pub fn kind_of(ty: &str) -> Result<Kind, Error> {
    let bad = || {
        Error::Internal(format!(
            "the plan has a type this release does not know: `{ty}`"
        ))
    };
    match ty {
        "i64" => Ok(Kind::I64),
        "bool" => Ok(Kind::Bool),
        "timestamp" => Ok(Kind::Timestamp),
        _ if ty.starts_with("text(") => Ok(Kind::Text),
        _ if ty.starts_with("bytes(") => Ok(Kind::Bytes),
        _ => {
            let scale = ty
                .strip_prefix("decimal(")
                .and_then(|r| r.strip_suffix(')'))
                .and_then(|r| r.split_once(','))
                .and_then(|(_, s)| s.trim().parse::<u8>().ok())
                .ok_or_else(bad)?;
            Ok(Kind::Dec(scale))
        }
    }
}

/// The type a value of this kind is sent as.
#[must_use]
pub fn wire_type(kind: Kind) -> Type {
    match kind {
        Kind::I64 | Kind::Timestamp => Type::INT8,
        Kind::Dec(_) | Kind::Text => Type::TEXT,
        Kind::Bool => Type::BOOL,
        Kind::Bytes => Type::BYTEA,
    }
}

/// A value ready to send, with the type to prepare the statement with.
pub struct Bound {
    /// The parameter type.
    pub ty: Type,
    /// The value.
    pub value: Box<dyn ToSql + Sync + Send>,
}

/// Turns an evaluated value into the parameter of type `ty` (the target column's type). A whole
/// number may go into a decimal and a decimal into a wider one; anything else that does not match is
/// a plan the compiler should have refused.
///
/// # Errors
/// `internal` when the value does not fit the type.
pub fn bind(v: &Value, ty: &str) -> Result<Bound, Error> {
    let kind = kind_of(ty)?;
    let mismatch = || {
        Error::Internal(format!(
            "a {} value cannot be sent as `{ty}`",
            v.ty().name()
        ))
    };
    let value: Box<dyn ToSql + Sync + Send> = match (kind, v) {
        (Kind::I64, Value::I64(n)) | (Kind::Timestamp, Value::Timestamp(n)) => Box::new(*n),
        (Kind::Bool, Value::Bool(b)) => Box::new(*b),
        (Kind::Text, Value::Text(t)) => Box::new(t.clone()),
        (Kind::Bytes, Value::Bytes(b)) => Box::new(b.clone()),
        (Kind::Dec(target), Value::I64(n)) => Box::new(widen(i128::from(*n), 0, target)?),
        (Kind::Dec(target), Value::Dec { value, scale }) => {
            Box::new(widen(*value, *scale, target)?)
        }
        _ => return Err(mismatch()),
    };
    Ok(Bound {
        ty: wire_type(kind),
        value,
    })
}

/// The exact text of `value` (scaled by `10^from`) with exactly `to` digits after the point.
fn widen(value: i128, from: u8, to: u8) -> Result<String, Error> {
    let too_wide = || Error::Internal("a decimal has more digits than its column".to_string());
    if from > to {
        return Err(too_wide());
    }
    let factor = 10i128
        .checked_pow(u32::from(to - from))
        .ok_or_else(too_wide)?;
    let scaled = value.checked_mul(factor).ok_or_else(too_wide)?;
    Ok(Value::Dec {
        value: scaled,
        scale: to,
    }
    .number_text()
    .unwrap_or_default())
}

/// Reads column `idx` of a row as a value of plan type `ty`. A `NULL` is an error: recipes read
/// only what they have written.
///
/// # Errors
/// `internal` for a `NULL`, a wrong type or an unreadable decimal.
pub fn read_value(row: &Row, idx: usize, ty: &str) -> Result<Value, Error> {
    let kind = kind_of(ty)?;
    let null = || {
        Error::Internal(format!(
            "column {idx} is NULL where the plan expects `{ty}`"
        ))
    };
    let wrong = |e: tokio_postgres::Error| Error::Internal(format!("column {idx}: {e}"));
    Ok(match kind {
        Kind::I64 => Value::I64(
            row.try_get::<_, Option<i64>>(idx)
                .map_err(wrong)?
                .ok_or_else(null)?,
        ),
        Kind::Timestamp => Value::Timestamp(
            row.try_get::<_, Option<i64>>(idx)
                .map_err(wrong)?
                .ok_or_else(null)?,
        ),
        Kind::Bool => Value::Bool(
            row.try_get::<_, Option<bool>>(idx)
                .map_err(wrong)?
                .ok_or_else(null)?,
        ),
        Kind::Text => Value::Text(
            row.try_get::<_, Option<String>>(idx)
                .map_err(wrong)?
                .ok_or_else(null)?,
        ),
        Kind::Bytes => Value::Bytes(
            row.try_get::<_, Option<Vec<u8>>>(idx)
                .map_err(wrong)?
                .ok_or_else(null)?,
        ),
        Kind::Dec(scale) => {
            let text = row
                .try_get::<_, Option<String>>(idx)
                .map_err(wrong)?
                .ok_or_else(null)?;
            Value::dec_from_text(&text, scale)
                .map_err(|e| Error::Internal(format!("column {idx}: {e}")))?
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_types_are_parsed_and_unknown_ones_are_internal() {
        assert_eq!(kind_of("i64").unwrap(), Kind::I64);
        assert_eq!(kind_of("decimal(18,2)").unwrap(), Kind::Dec(2));
        assert_eq!(kind_of("decimal(38, 0)").unwrap(), Kind::Dec(0));
        assert_eq!(kind_of("text(64)").unwrap(), Kind::Text);
        assert_eq!(kind_of("bytes(16)").unwrap(), Kind::Bytes);
        assert_eq!(kind_of("bool").unwrap(), Kind::Bool);
        assert_eq!(kind_of("timestamp").unwrap(), Kind::Timestamp);
        for bad in ["", "float", "decimal(18)", "decimal(a,b)"] {
            assert!(matches!(kind_of(bad), Err(Error::Internal(_))), "{bad}");
        }
    }

    #[test]
    fn decimals_are_sent_as_text_with_the_columns_scale() {
        assert_eq!(widen(150, 2, 2).unwrap(), "1.50");
        assert_eq!(widen(15, 1, 3).unwrap(), "1.500");
        assert_eq!(widen(-7, 0, 2).unwrap(), "-7.00");
        assert_eq!(widen(0, 0, 2).unwrap(), "0.00");
        // More digits than the column holds is a refused plan, never a rounding.
        assert!(widen(150, 2, 1).is_err());
        assert!(widen(i128::MAX, 0, 5).is_err());
    }

    #[test]
    fn a_value_goes_only_into_a_type_it_fits() {
        assert!(bind(&Value::I64(1), "i64").is_ok());
        assert!(bind(&Value::I64(1), "decimal(18,2)").is_ok());
        assert!(bind(&Value::Timestamp(1), "timestamp").is_ok());
        assert!(bind(&Value::Text("a".into()), "text(4)").is_ok());
        assert!(bind(&Value::Bool(true), "bool").is_ok());
        for (v, ty) in [
            (Value::Text("a".into()), "i64"),
            (Value::I64(1), "timestamp"),
            (Value::Bool(true), "text(4)"),
            (Value::Dec { value: 1, scale: 3 }, "decimal(18,2)"),
        ] {
            assert!(bind(&v, ty).is_err(), "{ty}");
        }
    }

    #[test]
    fn the_wire_types_match_what_the_statements_cast() {
        assert_eq!(wire_type(Kind::I64), Type::INT8);
        assert_eq!(wire_type(Kind::Timestamp), Type::INT8);
        assert_eq!(wire_type(Kind::Dec(2)), Type::TEXT);
        assert_eq!(wire_type(Kind::Bytes), Type::BYTEA);
    }
}
