//! The model to PostgreSQL statements, always in the `app` schema (never `sql`).
//!
//! Every name was checked by [`crate::validate`] (lower-case letters, digits and `_`), so quoting
//! is plain double quotes and no value from a user ever becomes SQL text except those checked
//! names and checked numbers. Statements carry no trailing `;`.
//!
//! The `CHECK`s here are the database's own enforcement of the schema's bounds; the recipe
//! compiler's checks come on top of them, not instead.

use crate::decimal::format_scaled;
use crate::diff::Change;
use crate::model::{Column, ColumnType, Index, Schema, Table, TableKind};

/// The schema the data tables live in.
pub const APP_SCHEMA: &str = "app";

/// The id rule of `bhdata-types`: 1 to 128 characters from `A-Z a-z 0-9 . _ : -`.
const ID_CHECK: &str = "~ '^[A-Za-z0-9._:-]{1,128}$'";

fn quote(name: &str) -> String {
    debug_assert!(name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'));
    format!("\"{name}\"")
}

fn qualified(table: &str) -> String {
    format!("{APP_SCHEMA}.{}", quote(table))
}

fn sql_type(ty: ColumnType) -> String {
    match ty {
        ColumnType::I64 => "bigint".to_string(),
        ColumnType::Decimal { precision, scale } => format!("numeric({precision},{scale})"),
        ColumnType::Text { .. } => "text".to_string(),
        ColumnType::Bool => "boolean".to_string(),
        ColumnType::Timestamp => "timestamptz".to_string(),
        ColumnType::Bytes { .. } => "bytea".to_string(),
    }
}

/// The column's definition: name, type, `NOT NULL` unless nullable, and its checks.
fn column_definition(column: &Column) -> String {
    let name = quote(&column.name);
    let mut sql = format!("{name} {}", sql_type(column.ty));
    if !column.nullable {
        sql.push_str(" NOT NULL");
    }
    match column.ty {
        ColumnType::Text { max_len } => {
            sql.push_str(&format!(" CHECK (char_length({name}) <= {max_len})"))
        }
        ColumnType::Bytes { max_len } => {
            sql.push_str(&format!(" CHECK (octet_length({name}) <= {max_len})"))
        }
        _ => {}
    }
    let scale = column.ty.scale();
    if let Some(min) = column.min {
        sql.push_str(&format!(" CHECK ({name} >= {})", format_scaled(min, scale)));
    }
    if let Some(max) = column.max {
        sql.push_str(&format!(" CHECK ({name} <= {})", format_scaled(max, scale)));
    }
    sql
}

/// The name of an index: the table and a frozen 64-bit FNV-1a hash of its column list, so the
/// name is stable, never too long (at most 60 bytes), and the same everywhere.
#[must_use]
pub fn index_name(table: &str, index: &Index) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in index.columns.join(",").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("ix_{table}_{hash:016x}")
}

/// `CREATE INDEX` for one index.
#[must_use]
pub fn create_index(table: &str, index: &Index) -> String {
    let columns: Vec<String> = index.columns.iter().map(|c| quote(c)).collect();
    format!(
        "CREATE INDEX {} ON {} ({})",
        quote(&index_name(table, index)),
        qualified(table),
        columns.join(", ")
    )
}

/// `ALTER TABLE ... ADD COLUMN` for a nullable column added later.
#[must_use]
pub fn add_column(table: &str, column: &Column) -> String {
    format!(
        "ALTER TABLE {} ADD COLUMN {}",
        qualified(table),
        column_definition(column)
    )
}

/// `CREATE TABLE` for one table, then its indexes.
#[must_use]
pub fn create_table(table: &Table) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    match &table.kind {
        TableKind::Subject => {
            lines.push(format!("\"id\" text PRIMARY KEY CHECK (\"id\" {ID_CHECK})"))
        }
        TableKind::Child { owner } => {
            lines.push(format!(
                "\"owner_id\" text NOT NULL REFERENCES {} (\"id\")",
                qualified(owner)
            ));
            lines.push("\"seq\" bigint NOT NULL CHECK (\"seq\" >= 1)".to_string());
        }
        TableKind::Global { .. } => {}
    }
    lines.extend(table.columns.iter().map(column_definition));
    match &table.kind {
        TableKind::Subject => {}
        TableKind::Child { .. } => lines.push("PRIMARY KEY (\"owner_id\", \"seq\")".to_string()),
        TableKind::Global { key } => lines.push(format!("PRIMARY KEY ({})", quote(key))),
    }
    let mut statements = vec![format!(
        "CREATE TABLE {} (\n  {}\n)",
        qualified(&table.name),
        lines.join(",\n  ")
    )];
    statements.extend(table.indexes.iter().map(|i| create_index(&table.name, i)));
    statements
}

/// Everything for a whole schema: subject tables first, then children (which refer to them),
/// then global tables.
#[must_use]
pub fn create_schema(schema: &Schema) -> Vec<String> {
    ordered(&schema.tables).flat_map(create_table).collect()
}

/// Tables in creation order: subjects, children, globals; the file's order within each.
pub(crate) fn ordered(tables: &[Table]) -> impl Iterator<Item = &Table> {
    let rank = |t: &Table| match t.kind {
        TableKind::Subject => 0,
        TableKind::Child { .. } => 1,
        TableKind::Global { .. } => 2,
    };
    (0..3).flat_map(move |r| tables.iter().filter(move |t| rank(t) == r))
}

/// The statements for one change; `None` for a destructive change, which only an admin applies.
#[must_use]
pub fn statements_for(change: &Change) -> Option<Vec<String>> {
    match change {
        Change::AddTable(table) => Some(create_table(table)),
        Change::AddColumn { table, column } => Some(vec![add_column(table, column)]),
        Change::AddIndex { table, index } => Some(vec![create_index(table, index)]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_schema;

    const SCHEMA: &str = r#"
[[table]]
name = "shop"
kind = "global"
key = "item"
columns = [{ name = "item", type = "text", max_len = 32 }, { name = "price", type = "decimal", precision = 18, scale = 2, min = "0" }]

[[table]]
name = "entries"
kind = "child"
owner = "users"
columns = [{ name = "delta", type = "i64" }, { name = "at", type = "timestamp" }]
indexes = [{ columns = ["owner_id", "at"] }]

[[table]]
name = "users"
kind = "subject"
columns = [
  { name = "balance", type = "i64", min = 0, max = 1_000_000_000 },
  { name = "name", type = "text", max_len = 64, nullable = true },
]
"#;

    #[test]
    fn a_schema_becomes_tables_in_dependency_order() {
        let sql = create_schema(&parse_schema(SCHEMA).unwrap());
        let tables: Vec<&String> = sql
            .iter()
            .filter(|s| s.starts_with("CREATE TABLE"))
            .collect();
        assert_eq!(tables.len(), 3);
        assert!(tables[0].starts_with("CREATE TABLE app.\"users\""));
        assert!(tables[1].starts_with("CREATE TABLE app.\"entries\""));
        assert!(tables[2].starts_with("CREATE TABLE app.\"shop\""));
        // The child's index follows the child's table.
        assert!(sql[2].starts_with("CREATE INDEX \"ix_entries_"));
    }

    #[test]
    fn the_exact_statements_for_each_kind() {
        let s = parse_schema(SCHEMA).unwrap();
        assert_eq!(
            create_table(s.table("users").unwrap())[0],
            "CREATE TABLE app.\"users\" (\n  \
             \"id\" text PRIMARY KEY CHECK (\"id\" ~ '^[A-Za-z0-9._:-]{1,128}$'),\n  \
             \"balance\" bigint NOT NULL CHECK (\"balance\" >= 0) CHECK (\"balance\" <= 1000000000),\n  \
             \"name\" text CHECK (char_length(\"name\") <= 64)\n)"
        );
        assert_eq!(
            create_table(s.table("entries").unwrap())[0],
            "CREATE TABLE app.\"entries\" (\n  \
             \"owner_id\" text NOT NULL REFERENCES app.\"users\" (\"id\"),\n  \
             \"seq\" bigint NOT NULL CHECK (\"seq\" >= 1),\n  \
             \"delta\" bigint NOT NULL,\n  \
             \"at\" timestamptz NOT NULL,\n  \
             PRIMARY KEY (\"owner_id\", \"seq\")\n)"
        );
        assert_eq!(
            create_table(s.table("shop").unwrap())[0],
            "CREATE TABLE app.\"shop\" (\n  \
             \"item\" text NOT NULL CHECK (char_length(\"item\") <= 32),\n  \
             \"price\" numeric(18,2) NOT NULL CHECK (\"price\" >= 0.00),\n  \
             PRIMARY KEY (\"item\")\n)"
        );
    }

    #[test]
    fn index_names_are_frozen_short_and_depend_only_on_the_columns() {
        let ix = Index {
            columns: vec!["owner_id".into(), "at".into()],
        };
        // Known answer: changing this would rename every index already made.
        assert_eq!(index_name("entries", &ix), "ix_entries_9b957ad5dfc3f65d");
        let long = "t".repeat(40);
        assert!(index_name(&long, &ix).len() <= 60);
        assert_ne!(
            index_name("entries", &ix),
            index_name(
                "entries",
                &Index {
                    columns: vec!["at".into(), "owner_id".into()]
                }
            )
        );
    }

    #[test]
    fn only_additive_changes_have_statements() {
        let col = Column {
            name: "note".into(),
            ty: ColumnType::Text { max_len: 10 },
            nullable: true,
            min: None,
            max: None,
        };
        let add = Change::AddColumn {
            table: "users".into(),
            column: col.clone(),
        };
        assert_eq!(
            statements_for(&add).unwrap(),
            ["ALTER TABLE app.\"users\" ADD COLUMN \"note\" text CHECK (char_length(\"note\") <= 10)"]
        );
        assert!(statements_for(&Change::DropColumn {
            table: "users".into(),
            column: "note".into()
        })
        .is_none());
    }
}
