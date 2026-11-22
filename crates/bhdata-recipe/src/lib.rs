//! Recipes: the format, the parser and (slice B2) the compiler (specs `docs/specs/recipe-format.md`
//! and `recipe-compiler.md`). Everything is pure: text in, data out; no database, clock or
//! randomness.
//!
//! * [`typing`]: types and value ranges of expressions.
//! * [`check`]: the compiler's checks and the lane.
//! * [`plan`]: a checked recipe as a plan: statements, grants, canonical JSON and hash.
//! * [`canon`]: canonical JSON and the SHA-256 content hash.
//! * [`reject`]: the stable rejection codes and the [`Rejection`] a recipe is refused with.
//! * [`expr`]: the small expression language of steps.
//! * [`model`]: a recipe as data.
//! * [`parse`]: TOML text to a [`model::Recipe`], with the limits it is checked against.

pub mod canon;
pub mod check;
pub mod expr;
pub mod model;
pub mod parse;
pub mod plan;
pub mod reject;
pub mod typing;

pub use check::{check, Checked, DerivedStore, Lane};
pub use parse::{parse_recipe, Limits};
pub use plan::{compile, Plan};
pub use reject::{Code, Rejection};
