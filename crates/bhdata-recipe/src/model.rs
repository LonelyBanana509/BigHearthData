//! A recipe as data (spec `recipe-format.md`, section 2). Produced by [`crate::parse`]; the
//! compiler (slice B2) checks it against a schema. Maps are kept sorted by name, so the order the
//! author wrote keys in never matters.

use bhdata_schema::model::ColumnType;

use crate::expr::Expr;

/// Whose row a step reaches: the request's subject, or the recipe's counterparty (argument `to`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    /// The subject named by the request.
    Subject,
    /// The second subject, passed as argument `to`.
    Counterparty,
}

/// An argument of the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arg {
    /// Its name.
    pub name: String,
    /// Its type.
    pub ty: ColumnType,
    /// Smallest allowed value, scaled for the type.
    pub min: Option<i128>,
    /// Largest allowed value, scaled for the type.
    pub max: Option<i128>,
}

/// One step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The author's label, reported with any error from this step.
    pub label: Option<String>,
    /// What it does.
    pub op: Op,
}

/// The whole list of operations. Nothing else exists: no joins, no scans, no raw SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Create the row if it is missing.
    Ensure {
        /// Whose row.
        of: Who,
    },
    /// Read one row by key into names (`balance`, or `to.balance` for the counterparty).
    Read {
        /// Whose row.
        of: Who,
        /// Its table.
        table: String,
        /// The columns.
        columns: Vec<String>,
    },
    /// Continue only if `require` is true; otherwise fail with the named error.
    Check {
        /// The condition.
        require: Expr,
        /// The error.
        error: String,
    },
    /// `column = column + by`, within the column's bounds.
    Add {
        /// Whose row.
        of: Who,
        /// Its table.
        table: String,
        /// The column.
        column: String,
        /// The amount.
        by: Expr,
        /// Named error when the result is under `min`.
        below: Option<String>,
        /// Named error when the result is over `max`.
        above: Option<String>,
    },
    /// Replace columns of the row: `(column, value)` sorted by column.
    Set {
        /// Whose row.
        of: Who,
        /// Its table.
        table: String,
        /// The new values.
        values: Vec<(String, Expr)>,
    },
    /// Add a row to a child table of the owner.
    Insert {
        /// The owner.
        of: Who,
        /// The child table.
        table: String,
        /// The column values, sorted by column.
        values: Vec<(String, Expr)>,
    },
    /// Read the newest rows of a child table into a named list.
    List {
        /// The owner.
        of: Who,
        /// The child table.
        table: String,
        /// The columns.
        columns: Vec<String>,
        /// At most this many rows.
        limit: u32,
        /// The list's name in the result.
        into: String,
    },
    /// Read a derived store (the all-shards lane).
    ReadDerived {
        /// The store.
        store: String,
        /// Its window.
        window: String,
        /// The stalest result accepted.
        max_age_ms: u64,
        /// At most this many entries.
        limit: u32,
    },
}

/// A parsed recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    /// Its name.
    pub name: String,
    /// The table whose row the command owns.
    pub subject: String,
    /// The second subject's table, if any.
    pub counterparty: Option<String>,
    /// The arguments, sorted by name.
    pub args: Vec<Arg>,
    /// The steps, in order.
    pub steps: Vec<Step>,
    /// What the command returns: columns, read names and list names.
    pub result: Vec<String>,
}
