//! Recipes: the format, the parser and (slice B2) the compiler (specs `docs/specs/recipe-format.md`
//! and `recipe-compiler.md`). Everything is pure: text in, data out; no database, clock or
//! randomness.
//!
//! * [`reject`]: the stable rejection codes and the [`Rejection`] a recipe is refused with.
//! * [`expr`]: the small expression language of steps.
//! * [`model`]: a recipe as data.
//! * [`parse`]: TOML text to a [`model::Recipe`], with the limits it is checked against.

pub mod expr;
pub mod model;
pub mod parse;
pub mod reject;

pub use parse::{parse_recipe, Limits};
pub use reject::{Code, Rejection};
