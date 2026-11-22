//! The schema as data. Built by [`crate::parse_schema`] or by code (the kinds), and checked by
//! [`crate::validate`].

/// The most tables in a schema.
pub const MAX_TABLES: usize = 200;
/// The most declared columns in a table.
pub const MAX_COLUMNS: usize = 100;
/// The longest table, column and recipe name.
pub const MAX_NAME_LEN: usize = 40;
/// The longest `text` or `bytes` column.
pub const MAX_TEXT_LEN: u32 = 4096;
/// The most columns in one index.
pub const MAX_INDEX_COLUMNS: usize = 4;
/// The most digits in a `decimal`.
pub const MAX_PRECISION: u8 = 38;

/// A whole schema.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    /// The tables, in the order the file gives them.
    pub tables: Vec<Table>,
}

impl Schema {
    /// The table with this name.
    #[must_use]
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }
}

/// One table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Lower-case name.
    pub name: String,
    /// What the table is.
    pub kind: TableKind,
    /// The declared columns; the implicit ones (see [`TableKind::implicit_columns`]) are not here.
    pub columns: Vec<Column>,
    /// Extra indexes.
    pub indexes: Vec<Index>,
}

impl Table {
    /// The declared column with this name.
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }
}

/// What a table is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableKind {
    /// Owns rows and is sharded by its implicit `id`.
    Subject,
    /// Follows the row of a subject table, so a subject's data stays together.
    Child {
        /// The subject table that owns these rows.
        owner: String,
    },
    /// Small and read-mostly; copied to every shard; never written by recipes.
    Global {
        /// The primary key column.
        key: String,
    },
}

impl TableKind {
    /// The columns every table of this kind has without declaring them.
    #[must_use]
    pub fn implicit_columns(&self) -> &'static [&'static str] {
        match self {
            TableKind::Subject => &["id"],
            TableKind::Child { .. } => &["owner_id", "seq"],
            TableKind::Global { .. } => &[],
        }
    }
}

/// One declared column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Lower-case name.
    pub name: String,
    /// The type.
    pub ty: ColumnType,
    /// Whether the value may be missing. Columns are required unless this is set.
    pub nullable: bool,
    /// Lowest allowed value, scaled by `10^scale` (scale is 0 for `i64`).
    pub min: Option<i128>,
    /// Highest allowed value, scaled the same way.
    pub max: Option<i128>,
}

/// A column type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    /// 64-bit whole number.
    I64,
    /// Exact decimal.
    Decimal {
        /// Digits in all, 1 to 38.
        precision: u8,
        /// Digits after the point, at most `precision`.
        scale: u8,
    },
    /// UTF-8 text.
    Text {
        /// Longest length in characters.
        max_len: u32,
    },
    /// True or false.
    Bool,
    /// UTC time, microseconds.
    Timestamp,
    /// Raw bytes.
    Bytes {
        /// Longest length in bytes.
        max_len: u32,
    },
}

impl ColumnType {
    /// The scale that `min` and `max` are stored in.
    #[must_use]
    pub fn scale(self) -> u8 {
        match self {
            ColumnType::Decimal { scale, .. } => scale,
            _ => 0,
        }
    }

    /// Whether `min` and `max` apply.
    #[must_use]
    pub fn is_numeric(self) -> bool {
        matches!(self, ColumnType::I64 | ColumnType::Decimal { .. })
    }
}

/// An extra index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// Declared or implicit columns, in order.
    pub columns: Vec<String>,
}
