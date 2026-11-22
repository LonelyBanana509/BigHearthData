//! The rules a schema must keep (spec `recipe-format.md`, section 1). The first break is reported.

use std::collections::HashSet;

use crate::model::{
    ColumnType, Schema, Table, TableKind, MAX_COLUMNS, MAX_INDEX_COLUMNS, MAX_NAME_LEN,
    MAX_PRECISION, MAX_TABLES, MAX_TEXT_LEN,
};
use crate::SchemaError;

/// Whether `name` is a good table, column or recipe name: lower-case letters, digits and `_`,
/// starting with a letter, at most 40 characters, and not reserved (`bhdata_` prefix).
#[must_use]
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with("bhdata_")
}

/// Checks a whole schema.
///
/// # Errors
/// The first rule that is broken, with where.
pub fn validate(schema: &Schema) -> Result<(), SchemaError> {
    if schema.tables.len() > MAX_TABLES {
        return Err(SchemaError::new(
            "",
            format!("more than {MAX_TABLES} tables"),
        ));
    }
    let mut seen = HashSet::new();
    for table in &schema.tables {
        let path = format!("table {}", table.name);
        if !valid_name(&table.name) {
            return Err(SchemaError::new(path, name_rule()));
        }
        if !seen.insert(table.name.as_str()) {
            return Err(SchemaError::new(path, "declared twice"));
        }
        check_table(schema, table, &path)?;
    }
    Ok(())
}

fn name_rule() -> String {
    format!(
        "a name is lower-case letters, digits and `_`, starts with a letter, has at most \
         {MAX_NAME_LEN} characters and does not start with `bhdata_`"
    )
}

fn check_table(schema: &Schema, table: &Table, path: &str) -> Result<(), SchemaError> {
    if table.columns.len() > MAX_COLUMNS {
        return Err(SchemaError::new(
            path,
            format!("more than {MAX_COLUMNS} columns"),
        ));
    }
    let implicit = table.kind.implicit_columns();
    let mut names = HashSet::new();
    for column in &table.columns {
        let path = format!("{path}, column {}", column.name);
        if !valid_name(&column.name) {
            return Err(SchemaError::new(path, name_rule()));
        }
        if implicit.contains(&column.name.as_str()) {
            return Err(SchemaError::new(
                path,
                "this name is a column every such table has already",
            ));
        }
        if !names.insert(column.name.as_str()) {
            return Err(SchemaError::new(path, "declared twice"));
        }
        check_column(column, &path)?;
    }
    match &table.kind {
        TableKind::Subject => {}
        TableKind::Child { owner } => match schema.table(owner) {
            Some(o) if o.kind == TableKind::Subject => {}
            Some(_) => {
                return Err(SchemaError::new(
                    path,
                    format!("owner {owner} is not a subject table"),
                ))
            }
            None => {
                return Err(SchemaError::new(
                    path,
                    format!("owner {owner} is not declared"),
                ))
            }
        },
        TableKind::Global { key } => {
            let Some(column) = table.column(key) else {
                return Err(SchemaError::new(
                    path,
                    format!("key {key} is not a declared column"),
                ));
            };
            if column.nullable || !matches!(column.ty, ColumnType::I64 | ColumnType::Text { .. }) {
                return Err(SchemaError::new(
                    path,
                    format!("key {key} must be a required i64 or text column"),
                ));
            }
        }
    }
    let mut indexes = HashSet::new();
    for (n, index) in table.indexes.iter().enumerate() {
        let path = format!("{path}, index {}", n + 1);
        if index.columns.is_empty() || index.columns.len() > MAX_INDEX_COLUMNS {
            return Err(SchemaError::new(
                path,
                format!("an index has 1 to {MAX_INDEX_COLUMNS} columns"),
            ));
        }
        let mut within = HashSet::new();
        for name in &index.columns {
            if table.column(name).is_none() && !implicit.contains(&name.as_str()) {
                return Err(SchemaError::new(
                    path,
                    format!("column {name} does not exist"),
                ));
            }
            if !within.insert(name.as_str()) {
                return Err(SchemaError::new(
                    path,
                    format!("column {name} is listed twice"),
                ));
            }
        }
        if !indexes.insert(&index.columns) {
            return Err(SchemaError::new(path, "the same index is declared twice"));
        }
    }
    Ok(())
}

fn check_column(column: &crate::model::Column, path: &str) -> Result<(), SchemaError> {
    match column.ty {
        ColumnType::Text { max_len } | ColumnType::Bytes { max_len } => {
            if max_len == 0 || max_len > MAX_TEXT_LEN {
                return Err(SchemaError::new(
                    path,
                    format!("`max_len` is 1 to {MAX_TEXT_LEN}"),
                ));
            }
        }
        ColumnType::Decimal { precision, scale } => {
            if precision == 0 || precision > MAX_PRECISION || scale > precision {
                return Err(SchemaError::new(
                    path,
                    format!(
                        "`precision` is 1 to {MAX_PRECISION} and `scale` is at most `precision`"
                    ),
                ));
            }
        }
        ColumnType::I64 | ColumnType::Bool | ColumnType::Timestamp => {}
    }
    if !column.ty.is_numeric() && (column.min.is_some() || column.max.is_some()) {
        return Err(SchemaError::new(
            path,
            "`min` and `max` apply to i64 and decimal only",
        ));
    }
    let limit = match column.ty {
        ColumnType::I64 => None,
        ColumnType::Decimal { precision, .. } => Some(10_i128.pow(u32::from(precision))),
        _ => None,
    };
    for value in [column.min, column.max].into_iter().flatten() {
        let fits = match limit {
            Some(limit) => value.abs() < limit,
            None => i64::try_from(value).is_ok(),
        };
        if !fits {
            return Err(SchemaError::new(
                path,
                "a bound does not fit the column's type",
            ));
        }
    }
    if let (Some(min), Some(max)) = (column.min, column.max) {
        if min > max {
            return Err(SchemaError::new(path, "`min` is above `max`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_schema;

    fn refuse(text: &str) -> SchemaError {
        parse_schema(text).unwrap_err()
    }

    const USERS: &str = "[[table]]\nname = \"users\"\nkind = \"subject\"\n";

    #[test]
    fn names_follow_the_rules() {
        for good in ["users", "a", "a_1", &"a".repeat(40)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "Users",
            "1a",
            "_a",
            "a-b",
            "a b",
            "bhdata_x",
            &"a".repeat(41),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn every_rule_names_its_place() {
        for (text, path, fragment) in [
            ("[[table]]\nname = \"Bad\"\nkind = \"subject\"", "table Bad", "lower-case"),
            (&format!("{USERS}{USERS}"), "table users", "twice"),
            (&format!("{USERS}columns = [{{ name = \"id\", type = \"i64\" }}]"), "table users, column id", "already"),
            (&format!("{USERS}columns = [{{ name = \"a\", type = \"i64\" }}, {{ name = \"a\", type = \"i64\" }}]"), "table users, column a", "twice"),
            (&format!("{USERS}columns = [{{ name = \"a\", type = \"text\", max_len = 5000 }}]"), "table users, column a", "max_len"),
            (&format!("{USERS}columns = [{{ name = \"a\", type = \"decimal\", precision = 39, scale = 2 }}]"), "table users, column a", "precision"),
            (&format!("{USERS}columns = [{{ name = \"a\", type = \"decimal\", precision = 4, scale = 5 }}]"), "table users, column a", "scale"),
            (&format!("{USERS}columns = [{{ name = \"a\", type = \"i64\", min = 5, max = 1 }}]"), "table users, column a", "above"),
            ("[[table]]\nname = \"c\"\nkind = \"child\"\nowner = \"nope\"", "table c", "not declared"),
            (&format!("{USERS}[[table]]\nname = \"g\"\nkind = \"global\"\nkey = \"k\"\ncolumns = [{{ name = \"k\", type = \"i64\" }}]\n[[table]]\nname = \"c\"\nkind = \"child\"\nowner = \"g\""), "table c", "not a subject"),
            ("[[table]]\nname = \"g\"\nkind = \"global\"\nkey = \"k\"", "table g", "not a declared column"),
            ("[[table]]\nname = \"g\"\nkind = \"global\"\nkey = \"k\"\ncolumns = [{ name = \"k\", type = \"i64\", nullable = true }]", "table g", "required"),
            ("[[table]]\nname = \"g\"\nkind = \"global\"\nkey = \"k\"\ncolumns = [{ name = \"k\", type = \"bool\" }]", "table g", "i64 or text"),
            (&format!("{USERS}indexes = [{{ columns = [] }}]"), "table users, index 1", "1 to"),
            (&format!("{USERS}indexes = [{{ columns = [\"zzz\"] }}]"), "table users, index 1", "does not exist"),
            (&format!("{USERS}indexes = [{{ columns = [\"id\", \"id\"] }}]"), "table users, index 1", "twice"),
            (&format!("{USERS}indexes = [{{ columns = [\"id\"] }}, {{ columns = [\"id\"] }}]"), "table users, index 2", "twice"),
        ] {
            let e = refuse(text);
            assert_eq!(e.path, path, "{text}");
            assert!(e.message.contains(fragment), "{text}: {e}");
        }
    }

    #[test]
    fn a_bound_that_does_not_fit_a_model_built_in_code_is_refused() {
        use crate::model::Column;
        let mut s = parse_schema(&format!(
            "{USERS}columns = [{{ name = \"a\", type = \"i64\" }}]"
        ))
        .unwrap();
        s.tables[0].columns[0] = Column {
            name: "a".into(),
            ty: ColumnType::Decimal {
                precision: 3,
                scale: 0,
            },
            nullable: false,
            min: Some(1000),
            max: None,
        };
        assert!(validate(&s).unwrap_err().message.contains("fit"));
    }

    #[test]
    fn implicit_columns_can_be_indexed() {
        let s = parse_schema(
            "[[table]]\nname = \"u\"\nkind = \"subject\"\n[[table]]\nname = \"e\"\nkind = \"child\"\nowner = \"u\"\ncolumns = [{ name = \"at\", type = \"timestamp\" }]\nindexes = [{ columns = [\"owner_id\", \"at\"] }]",
        );
        assert!(s.is_ok(), "{s:?}");
    }
}
