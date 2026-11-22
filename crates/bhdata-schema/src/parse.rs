//! The TOML schema file to the model (spec `recipe-format.md`, section 1).

use serde::Deserialize;

use crate::decimal::parse_scaled;
use crate::model::{Column, ColumnType, Index, Schema, Table, TableKind};
use crate::validate::validate;
use crate::SchemaError;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    #[serde(default)]
    table: Vec<RawTable>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    name: String,
    kind: String,
    owner: Option<String>,
    key: Option<String>,
    #[serde(default)]
    columns: Vec<RawColumn>,
    #[serde(default)]
    indexes: Vec<RawIndex>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawColumn {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    nullable: bool,
    max_len: Option<u32>,
    precision: Option<u8>,
    scale: Option<u8>,
    min: Option<toml::Value>,
    max: Option<toml::Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIndex {
    columns: Vec<String>,
}

/// Reads and checks a schema file.
///
/// # Errors
/// [`SchemaError`] for text that is not a valid schema file, or a schema that breaks a rule.
pub fn parse_schema(text: &str) -> Result<Schema, SchemaError> {
    let raw: RawSchema = toml::from_str(text).map_err(|e| SchemaError::new("", e.to_string()))?;
    let mut tables = Vec::with_capacity(raw.table.len());
    for table in raw.table {
        tables.push(convert_table(table)?);
    }
    let schema = Schema { tables };
    validate(&schema)?;
    Ok(schema)
}

fn convert_table(raw: RawTable) -> Result<Table, SchemaError> {
    let path = format!("table {}", raw.name);
    let kind = match (raw.kind.as_str(), raw.owner, raw.key) {
        ("subject", None, None) => TableKind::Subject,
        ("child", Some(owner), None) => TableKind::Child { owner },
        ("global", None, Some(key)) => TableKind::Global { key },
        ("subject", ..) => {
            return Err(SchemaError::new(
                path,
                "a subject table takes no `owner` or `key`",
            ))
        }
        ("child", ..) => {
            return Err(SchemaError::new(
                path,
                "a child table needs `owner` and takes no `key`",
            ))
        }
        ("global", ..) => {
            return Err(SchemaError::new(
                path,
                "a global table needs `key` and takes no `owner`",
            ))
        }
        (other, ..) => {
            return Err(SchemaError::new(
                path,
                format!("unknown kind {other:?} (use subject, child or global)"),
            ))
        }
    };
    let mut columns = Vec::with_capacity(raw.columns.len());
    for column in raw.columns {
        columns.push(convert_column(&path, column)?);
    }
    let indexes = raw
        .indexes
        .into_iter()
        .map(|i| Index { columns: i.columns })
        .collect();
    Ok(Table {
        name: raw.name,
        kind,
        columns,
        indexes,
    })
}

fn convert_column(table_path: &str, raw: RawColumn) -> Result<Column, SchemaError> {
    let path = format!("{table_path}, column {}", raw.name);
    let only = |what: &str, ok: bool, present: bool| {
        if present && !ok {
            Err(SchemaError::new(
                &path,
                format!("`{what}` does not apply to type {}", raw.ty),
            ))
        } else {
            Ok(())
        }
    };
    let ty = parse_type(&raw.ty, raw.precision, raw.scale, raw.max_len)
        .map_err(|m| SchemaError::new(&path, m))?;
    only(
        "max_len",
        matches!(ty, ColumnType::Text { .. } | ColumnType::Bytes { .. }),
        raw.max_len.is_some(),
    )?;
    only(
        "precision",
        matches!(ty, ColumnType::Decimal { .. }),
        raw.precision.is_some(),
    )?;
    only(
        "scale",
        matches!(ty, ColumnType::Decimal { .. }),
        raw.scale.is_some(),
    )?;
    only("min", ty.is_numeric(), raw.min.is_some())?;
    only("max", ty.is_numeric(), raw.max.is_some())?;
    let bound = |value: Option<toml::Value>| -> Result<Option<i128>, SchemaError> {
        value
            .map(|v| parse_bound(ty, v))
            .transpose()
            .map_err(|m| SchemaError::new(&path, m))
    };
    Ok(Column {
        name: raw.name.clone(),
        ty,
        nullable: raw.nullable,
        min: bound(raw.min)?,
        max: bound(raw.max)?,
    })
}

/// A column or argument type from its words: `i64`, `decimal` (with `precision` and `scale`),
/// `text` or `bytes` (with `max_len`), `bool`, `timestamp`. Shared with `bhdata-recipe`.
///
/// # Errors
/// A plain sentence naming what is missing or unknown.
pub fn parse_type(
    name: &str,
    precision: Option<u8>,
    scale: Option<u8>,
    max_len: Option<u32>,
) -> Result<ColumnType, String> {
    Ok(match name {
        "i64" => ColumnType::I64,
        "decimal" => match (precision, scale) {
            (Some(precision), Some(scale)) => ColumnType::Decimal { precision, scale },
            _ => return Err("a decimal needs `precision` and `scale`".to_string()),
        },
        "text" | "bytes" => {
            let Some(max_len) = max_len else {
                return Err(format!("a {name} column needs `max_len`"));
            };
            if name == "text" {
                ColumnType::Text { max_len }
            } else {
                ColumnType::Bytes { max_len }
            }
        }
        "bool" => ColumnType::Bool,
        "timestamp" => ColumnType::Timestamp,
        other => {
            return Err(format!(
                "unknown type {other:?} (use i64, decimal, text, bool, timestamp or bytes)"
            ))
        }
    })
}

/// A `min` or `max` from TOML, scaled for the type: a whole number for `i64`, a string such as
/// `"0.05"` for `decimal`. Shared with `bhdata-recipe`.
///
/// # Errors
/// A plain sentence.
pub fn parse_bound(ty: ColumnType, value: toml::Value) -> Result<i128, String> {
    match (ty, value) {
        (ColumnType::I64, toml::Value::Integer(n)) => Ok(i128::from(n)),
        (ColumnType::I64, _) => Err("an i64 bound is a whole number".to_string()),
        (ColumnType::Decimal { precision, scale }, toml::Value::String(text)) => {
            parse_scaled(&text, precision, scale)
        }
        (ColumnType::Decimal { .. }, _) => {
            Err("a decimal bound is written as a string, like \"0.05\"".to_string())
        }
        _ => Err("`min` and `max` apply to i64 and decimal only".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const GOOD: &str = r#"
[[table]]
name = "users"
kind = "subject"
columns = [
  { name = "balance", type = "i64", min = 0, max = 1_000_000_000 },
  { name = "rate", type = "decimal", precision = 9, scale = 6, min = "0", max = "1" },
  { name = "name", type = "text", max_len = 64, nullable = true },
]

[[table]]
name = "entries"
kind = "child"
owner = "users"
columns = [{ name = "delta", type = "i64" }, { name = "at", type = "timestamp" }]
indexes = [{ columns = ["at"] }]

[[table]]
name = "shop"
kind = "global"
key = "item"
columns = [{ name = "item", type = "text", max_len = 32 }, { name = "price", type = "decimal", precision = 18, scale = 2 }]
"#;

    #[test]
    fn the_example_schema_parses_into_the_model() {
        let s = parse_schema(GOOD).unwrap();
        assert_eq!(s.tables.len(), 3);
        let users = s.table("users").unwrap();
        assert_eq!(users.kind, TableKind::Subject);
        let balance = users.column("balance").unwrap();
        assert_eq!((balance.min, balance.max), (Some(0), Some(1_000_000_000)));
        assert!(!balance.nullable);
        let rate = users.column("rate").unwrap();
        assert_eq!((rate.min, rate.max), (Some(0), Some(1_000_000)));
        assert!(users.column("name").unwrap().nullable);
        assert_eq!(
            s.table("entries").unwrap().kind,
            TableKind::Child {
                owner: "users".into()
            }
        );
        assert_eq!(
            s.table("shop").unwrap().kind,
            TableKind::Global { key: "item".into() }
        );
    }

    #[test]
    fn bad_files_are_refused_with_a_place_and_a_reason() {
        for (text, path, fragment) in [
            ("[[table]]\nname = \"t\"\nkind = \"subject\"\nbogus = 1", "", "bogus"),
            ("[[table]]\nname = \"t\"\nkind = \"thing\"", "table t", "unknown kind"),
            ("[[table]]\nname = \"t\"\nkind = \"child\"", "table t", "needs `owner`"),
            ("[[table]]\nname = \"t\"\nkind = \"global\"", "table t", "needs `key`"),
            ("[[table]]\nname = \"t\"\nkind = \"subject\"\ncolumns = [{ name = \"a\", type = \"float\" }]", "table t, column a", "unknown type"),
            ("[[table]]\nname = \"t\"\nkind = \"subject\"\ncolumns = [{ name = \"a\", type = \"text\" }]", "table t, column a", "max_len"),
            ("[[table]]\nname = \"t\"\nkind = \"subject\"\ncolumns = [{ name = \"a\", type = \"decimal\", precision = 5 }]", "table t, column a", "precision"),
            ("[[table]]\nname = \"t\"\nkind = \"subject\"\ncolumns = [{ name = \"a\", type = \"bool\", min = 1 }]", "table t, column a", "`min` does not apply"),
            ("[[table]]\nname = \"t\"\nkind = \"subject\"\ncolumns = [{ name = \"a\", type = \"decimal\", precision = 5, scale = 2, min = 1.5 }]", "table t, column a", "string"),
            ("[[table]]\nname = \"t\"\nkind = \"subject\"\ncolumns = [{ name = \"a\", type = \"decimal\", precision = 5, scale = 2, min = \"1.234\" }]", "table t, column a", "digits after the point"),
            ("[[table]]\nname = \"t\"\nkind = \"subject\"\ncolumns = [{ name = \"a\", type = \"i64\", min = \"0\" }]", "table t, column a", "whole number"),
        ] {
            let e = parse_schema(text).unwrap_err();
            assert_eq!(e.path, path, "{text}");
            assert!(e.message.contains(fragment), "{text}: {e}");
        }
    }
}
