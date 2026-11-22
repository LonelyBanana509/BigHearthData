//! Two schemas to the list of changes between them. Additive changes can be applied directly;
//! everything else is flagged destructive and left for an explicit admin action (spec
//! `recipe-format.md`, section 1). A rename cannot be told from a drop and an add, so it shows
//! as both.

use crate::ddl::ordered;
use crate::model::{Column, Index, Schema, Table};

/// One difference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A new table (with its indexes).
    AddTable(Table),
    /// A new nullable column on an existing table.
    AddColumn {
        /// The table.
        table: String,
        /// The column.
        column: Column,
    },
    /// A new index on an existing table.
    AddIndex {
        /// The table.
        table: String,
        /// The index.
        index: Index,
    },
    /// A new required column: existing rows would have no value for it.
    AddRequiredColumn {
        /// The table.
        table: String,
        /// The column.
        column: Column,
    },
    /// A table that is gone.
    DropTable {
        /// The table.
        table: String,
    },
    /// A column that is gone.
    DropColumn {
        /// The table.
        table: String,
        /// The column's name.
        column: String,
    },
    /// An index that is gone.
    DropIndex {
        /// The table.
        table: String,
        /// The index.
        index: Index,
    },
    /// A column whose type, bounds or nullability changed.
    ChangeColumn {
        /// The table.
        table: String,
        /// How it was.
        from: Column,
        /// How it is now.
        to: Column,
    },
    /// A table whose kind, owner or key changed.
    ChangeTable {
        /// The table.
        table: String,
    },
}

impl Change {
    /// Whether an admin must apply this on purpose.
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        !matches!(
            self,
            Change::AddTable(_) | Change::AddColumn { .. } | Change::AddIndex { .. }
        )
    }

    /// One line for a person.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Change::AddTable(t) => format!("add table {}", t.name),
            Change::AddColumn { table, column } => format!("add column {table}.{}", column.name),
            Change::AddIndex { table, .. } => format!("add an index on {table}"),
            Change::AddRequiredColumn { table, column } => {
                format!(
                    "add required column {table}.{} (existing rows have no value)",
                    column.name
                )
            }
            Change::DropTable { table } => format!("drop table {table}"),
            Change::DropColumn { table, column } => format!("drop column {table}.{column}"),
            Change::DropIndex { table, .. } => format!("drop an index on {table}"),
            Change::ChangeColumn { table, to, .. } => format!("change column {table}.{}", to.name),
            Change::ChangeTable { table } => format!("change the kind, owner or key of {table}"),
        }
    }
}

/// What changed between two schemas.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SchemaDiff {
    /// The changes: additions in creation order, then the rest.
    pub changes: Vec<Change>,
}

impl SchemaDiff {
    /// Nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The changes that can be applied without an admin's say.
    pub fn additive(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(|c| !c.is_destructive())
    }

    /// The changes that need an explicit admin action.
    pub fn destructive(&self) -> impl Iterator<Item = &Change> {
        self.changes.iter().filter(|c| c.is_destructive())
    }
}

/// The changes that turn `old` into `new`. Both are assumed valid.
#[must_use]
pub fn diff(old: &Schema, new: &Schema) -> SchemaDiff {
    let mut changes = Vec::new();
    for table in ordered(&new.tables) {
        match old.table(&table.name) {
            None => changes.push(Change::AddTable(table.clone())),
            Some(before) => diff_table(before, table, &mut changes),
        }
    }
    for table in &old.tables {
        if new.table(&table.name).is_none() {
            changes.push(Change::DropTable {
                table: table.name.clone(),
            });
        }
    }
    SchemaDiff { changes }
}

fn diff_table(before: &Table, after: &Table, out: &mut Vec<Change>) {
    let name = &after.name;
    if before.kind != after.kind {
        out.push(Change::ChangeTable {
            table: name.clone(),
        });
        return;
    }
    for column in &after.columns {
        match before.column(&column.name) {
            None if column.nullable => out.push(Change::AddColumn {
                table: name.clone(),
                column: column.clone(),
            }),
            None => out.push(Change::AddRequiredColumn {
                table: name.clone(),
                column: column.clone(),
            }),
            Some(was) if was != column => out.push(Change::ChangeColumn {
                table: name.clone(),
                from: was.clone(),
                to: column.clone(),
            }),
            Some(_) => {}
        }
    }
    for index in &after.indexes {
        if !before.indexes.contains(index) {
            out.push(Change::AddIndex {
                table: name.clone(),
                index: index.clone(),
            });
        }
    }
    for column in &before.columns {
        if after.column(&column.name).is_none() {
            out.push(Change::DropColumn {
                table: name.clone(),
                column: column.name.clone(),
            });
        }
    }
    for index in &before.indexes {
        if !after.indexes.contains(index) {
            out.push(Change::DropIndex {
                table: name.clone(),
                index: index.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_schema;

    fn schema(extra_users: &str, extra_tables: &str) -> Schema {
        parse_schema(&format!(
            "[[table]]\nname = \"users\"\nkind = \"subject\"\ncolumns = [{{ name = \"balance\", type = \"i64\" }}{extra_users}]\n{extra_tables}"
        ))
        .unwrap()
    }

    #[test]
    fn an_unchanged_schema_has_no_changes() {
        assert!(diff(&schema("", ""), &schema("", "")).is_empty());
    }

    #[test]
    fn additions_are_additive_and_come_in_creation_order() {
        let old = schema("", "");
        let new = schema(
            ", { name = \"note\", type = \"text\", max_len = 9, nullable = true }",
            "[[table]]\nname = \"e\"\nkind = \"child\"\nowner = \"users\"\n[[table]]\nname = \"z\"\nkind = \"subject\"\n",
        );
        let d = diff(&old, &new);
        let text: Vec<String> = d.changes.iter().map(Change::describe).collect();
        assert_eq!(
            text,
            ["add column users.note", "add table z", "add table e"]
        );
        assert!(d.destructive().next().is_none());
        assert_eq!(d.additive().count(), 3);
    }

    #[test]
    fn everything_else_is_flagged_destructive() {
        let old = schema(
            ", { name = \"gone\", type = \"bool\" }",
            "[[table]]\nname = \"old\"\nkind = \"subject\"\n",
        );
        let new = schema(", { name = \"extra\", type = \"bool\" }", "");
        let text: Vec<String> = diff(&old, &new)
            .destructive()
            .map(Change::describe)
            .collect();
        assert_eq!(
            text,
            [
                "add required column users.extra (existing rows have no value)",
                "drop column users.gone",
                "drop table old",
            ]
        );
    }

    #[test]
    fn a_changed_column_a_changed_kind_and_a_dropped_index_are_destructive() {
        let old = parse_schema("[[table]]\nname = \"u\"\nkind = \"subject\"\ncolumns = [{ name = \"a\", type = \"i64\", max = 5 }]\nindexes = [{ columns = [\"a\"] }]").unwrap();
        let new = parse_schema("[[table]]\nname = \"u\"\nkind = \"subject\"\ncolumns = [{ name = \"a\", type = \"i64\", max = 9 }]").unwrap();
        let text: Vec<String> = diff(&old, &new)
            .changes
            .iter()
            .map(Change::describe)
            .collect();
        assert_eq!(text, ["change column u.a", "drop an index on u"]);

        let other = parse_schema("[[table]]\nname = \"u\"\nkind = \"global\"\nkey = \"a\"\ncolumns = [{ name = \"a\", type = \"i64\" }]").unwrap();
        let d = diff(&old, &other);
        assert_eq!(d.changes, [Change::ChangeTable { table: "u".into() }]);
    }
}
