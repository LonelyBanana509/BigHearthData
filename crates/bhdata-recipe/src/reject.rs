//! Why a recipe is refused: a stable code, the step, and a plain sentence (spec
//! `recipe-compiler.md`, section 2). Nothing is ever "fixed" for the author.

use std::fmt;

/// The stable rejection codes. Their text never changes: clients and tests match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// Not valid TOML, an unknown key, a wrong value type, or a bad expression.
    Parse,
    /// A name breaks the rules.
    Name,
    /// It names a table the schema lacks, writes a global table, or names the `sql` schema.
    UnknownTable,
    /// It names a column the schema lacks.
    UnknownColumn,
    /// A step reaches rows other than by the subject's (or counterparty's) id.
    NotByKey,
    /// An expression's type is wrong, or something is used before it exists.
    Type,
    /// A `decimal` result is not explicitly rounded.
    Rounding,
    /// A written value can leave a column's bounds with no named error for that side.
    Bounds,
    /// Over a limit: steps, list size, result size, expression size.
    TooBig,
    /// A named error is malformed, or reuses a built-in name with another meaning.
    ErrorName,
    /// The lane cannot be decided.
    Lane,
    /// A derived-store read names an undeclared store, window or staleness.
    Derived,
}

impl Code {
    /// The code's text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Parse => "parse",
            Code::Name => "name",
            Code::UnknownTable => "unknown_table",
            Code::UnknownColumn => "unknown_column",
            Code::NotByKey => "not_by_key",
            Code::Type => "type",
            Code::Rounding => "rounding",
            Code::Bounds => "bounds",
            Code::TooBig => "too_big",
            Code::ErrorName => "error_name",
            Code::Lane => "lane",
            Code::Derived => "derived",
        }
    }
}

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.render())]
pub struct Rejection {
    /// The code.
    pub code: Code,
    /// The step (counting from 1), or `None` for the recipe as a whole.
    pub step: Option<usize>,
    /// A short plain sentence.
    pub message: String,
}

impl Rejection {
    pub(crate) fn new(code: Code, step: Option<usize>, message: impl Into<String>) -> Rejection {
        Rejection {
            code,
            step,
            message: message.into(),
        }
    }

    fn render(&self) -> String {
        match self.step {
            Some(n) => format!("[{}] step {n}: {}", self.code, self.message),
            None => format!("[{}] {}", self.code, self.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_texts_are_frozen() {
        let all = [
            (Code::Parse, "parse"),
            (Code::Name, "name"),
            (Code::UnknownTable, "unknown_table"),
            (Code::UnknownColumn, "unknown_column"),
            (Code::NotByKey, "not_by_key"),
            (Code::Type, "type"),
            (Code::Rounding, "rounding"),
            (Code::Bounds, "bounds"),
            (Code::TooBig, "too_big"),
            (Code::ErrorName, "error_name"),
            (Code::Lane, "lane"),
            (Code::Derived, "derived"),
        ];
        for (code, text) in all {
            assert_eq!(code.as_str(), text);
        }
    }

    #[test]
    fn a_rejection_names_its_code_and_step() {
        let r = Rejection::new(Code::Type, Some(3), "amount is text");
        assert_eq!(r.to_string(), "[type] step 3: amount is text");
        let r = Rejection::new(Code::Name, None, "bad name");
        assert_eq!(r.to_string(), "[name] bad name");
    }
}
