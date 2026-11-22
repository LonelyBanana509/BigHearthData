//! Why a control-plane call failed, and the error a client would see (spec `recipe-compiler.md`,
//! section 5: "a rejection is `bad_arguments` with the code and sentence; an unknown name or
//! version is `unknown_recipe`").

use bhdata_pg::pool::PoolError;
use bhdata_pg::PgError;
use bhdata_recipe::Rejection;
use bhdata_types::error::Error;

/// A failed registry call.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    /// The schema file is not a valid schema.
    #[error("the schema file is refused: {0}")]
    Schema(String),
    /// The recipe was refused by the parser or the compiler.
    #[error("the recipe is refused: {0}")]
    Rejected(Rejection),
    /// A recipe was registered before any schema was applied.
    #[error("no schema has been applied yet; run `bhdatactl schema apply <file>` first")]
    NoSchema,
    /// The new schema needs changes only an explicit admin action may make. Nothing was applied.
    #[error(
        "the schema change needs an explicit admin action, which this release does not have yet; \
         nothing was applied: {}",
        .0.join("; ")
    )]
    Destructive(Vec<String>),
    /// No such recipe, or no such version of it.
    #[error("{}", unknown_recipe_text(.name, *.version))]
    UnknownRecipe {
        /// The name asked for.
        name: String,
        /// The version asked for; `None` when the latest active one was.
        version: Option<u32>,
    },
    /// The server refused a step.
    #[error("registry step {step}: {detail}")]
    Pg {
        /// Which step.
        step: &'static str,
        /// What kind of failure, and whether it is worth another try.
        error: PgError,
        /// The server's text.
        detail: String,
    },
    /// The admin pool could not hand out a connection.
    #[error(transparent)]
    Pool(#[from] PoolError),
    /// What the registry holds does not fit what this release computes from it.
    #[error("the registry's data is inconsistent: {0}")]
    Inconsistent(String),
    /// The private runtime could not be built.
    #[error("could not start the runtime: {0}")]
    Runtime(std::io::Error),
}

fn unknown_recipe_text(name: &str, version: Option<u32>) -> String {
    match version {
        Some(v) => format!("there is no recipe `{name}` version {v}"),
        None => format!("there is no active recipe `{name}`"),
    }
}

impl ControlError {
    /// The error a client would see: a refusal of what was asked is `bad_arguments`, an unknown
    /// recipe is `unknown_recipe`, and everything else is `internal`.
    #[must_use]
    pub fn client_error(&self) -> Error {
        match self {
            ControlError::Schema(_)
            | ControlError::Rejected(_)
            | ControlError::NoSchema
            | ControlError::Destructive(_) => Error::BadArguments(self.to_string()),
            ControlError::UnknownRecipe { .. } => Error::UnknownRecipe,
            ControlError::Pg { .. }
            | ControlError::Pool(_)
            | ControlError::Inconsistent(_)
            | ControlError::Runtime(_) => Error::Internal(self.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bhdata_recipe::{parse_recipe, Limits};
    use bhdata_types::error::ErrorKind;

    #[test]
    fn what_was_asked_wrongly_is_bad_arguments_and_the_text_keeps_the_code() {
        let rejection = parse_recipe("not toml [", &Limits::default()).unwrap_err();
        let e = ControlError::Rejected(rejection);
        assert_eq!(e.client_error().kind(), ErrorKind::BadArguments);
        assert!(e.to_string().contains("[parse]"), "{e}");
        for e in [
            ControlError::Schema("x".into()),
            ControlError::NoSchema,
            ControlError::Destructive(vec!["drop table a".into(), "drop column b.c".into()]),
        ] {
            assert_eq!(e.client_error().kind(), ErrorKind::BadArguments, "{e}");
        }
        let text = ControlError::Destructive(vec!["drop table a".into(), "drop column b.c".into()])
            .to_string();
        assert!(text.contains("drop table a; drop column b.c"), "{text}");
        assert!(text.contains("nothing was applied"), "{text}");
    }

    #[test]
    fn an_unknown_name_or_version_is_unknown_recipe() {
        let with = ControlError::UnknownRecipe {
            name: "spend".into(),
            version: Some(7),
        };
        let without = ControlError::UnknownRecipe {
            name: "spend".into(),
            version: None,
        };
        assert_eq!(with.client_error().kind(), ErrorKind::UnknownRecipe);
        assert_eq!(without.client_error().kind(), ErrorKind::UnknownRecipe);
        assert!(with.to_string().contains("spend") && with.to_string().contains('7'));
        assert!(without.to_string().contains("active"));
    }

    #[test]
    fn nothing_internal_reaches_a_client_as_anything_but_internal() {
        let e = ControlError::Inconsistent("x".into());
        assert_eq!(e.client_error().kind(), ErrorKind::Internal);
    }
}
