//! The permission and recipe allow-list check (spec, decision 10). Lane S.
//!
//! Pure functions, no state. The session calls [`check_execute`] first in the order of decision 9
//! (permission, then rate limit, then shed), and passes whatever error the handler returns through
//! [`present_error`] before it reaches the client.

use bhdata_api::login::{Permissions, RecipeAllow};
use bhdata_api::types::Error;

/// What a request asks to do. At M5 only `Execute` has any effect on this wire; the others are
/// here so that the later milestones (M8 `register` and `admin`, M12 `sql`) add no new check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Run a recipe.
    Execute,
    /// Register a recipe (CLI only until M8).
    Register,
    /// Admin operations (CLI only until M8).
    Admin,
    /// SQL mode (M12).
    Sql,
}

/// `true` if the login holds the permission for `op`.
#[must_use]
pub fn has_permission(permissions: Permissions, op: Operation) -> bool {
    match op {
        Operation::Execute => permissions.execute,
        Operation::Register => permissions.register,
        Operation::Admin => permissions.admin,
        Operation::Sql => permissions.sql,
    }
}

/// The permission check for one `request` message.
///
/// `unauthorized` if the login lacks `execute`, or if the recipe is not on its allow-list. The two
/// answers are the same on purpose. A login with `*` passes here and gets `unknown_recipe` later, from
/// the handler, if the name is unknown.
///
/// # Errors
/// [`Error::Unauthorized`].
pub fn check_execute(
    permissions: Permissions,
    allow: &RecipeAllow,
    recipe: &str,
) -> Result<(), Error> {
    if !has_permission(permissions, Operation::Execute) || !allow.permits(recipe) {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

/// Hides which recipes exist from a restricted login. For a login whose allow-list is not `*`, an
/// `unknown_recipe` from the handler becomes `unauthorized`, the same answer as a recipe that is
/// known but not allowed (an admin may have granted a name that was never registered or was
/// retired). Every other error, and every error for a `*` login, passes through unchanged.
#[must_use]
pub fn present_error(allow: &RecipeAllow, error: Error) -> Error {
    match (allow, error) {
        (RecipeAllow::All, error) => error,
        (_, Error::UnknownRecipe) => Error::Unauthorized,
        (_, error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn can_execute() -> Permissions {
        Permissions {
            execute: true,
            ..Permissions::default()
        }
    }

    fn names(list: &[&str]) -> RecipeAllow {
        RecipeAllow::Names(list.iter().map(|s| (*s).to_string()).collect())
    }

    #[test]
    fn a_login_without_execute_is_unauthorized_even_with_a_full_allow_list() {
        let r = check_execute(Permissions::default(), &RecipeAllow::All, "spend");
        assert_eq!(r, Err(Error::Unauthorized));
    }

    #[test]
    fn the_default_allow_list_allows_nothing() {
        let r = check_execute(can_execute(), &RecipeAllow::default(), "spend");
        assert_eq!(r, Err(Error::Unauthorized));
    }

    #[test]
    fn a_recipe_outside_the_list_is_unauthorized() {
        let allow = names(&["spend"]);
        assert_eq!(check_execute(can_execute(), &allow, "spend"), Ok(()));
        assert_eq!(
            check_execute(can_execute(), &allow, "give"),
            Err(Error::Unauthorized)
        );
    }

    #[test]
    fn star_passes_the_check_whatever_the_name() {
        assert_eq!(
            check_execute(can_execute(), &RecipeAllow::All, "no_such_recipe"),
            Ok(())
        );
    }

    #[test]
    fn a_restricted_login_cannot_tell_unknown_from_not_allowed() {
        // Not allowed (caught by the check) and unknown-but-allowed (caught by the handler) look alike.
        let allow = names(&["retired"]);
        let not_allowed = check_execute(can_execute(), &allow, "other").unwrap_err();
        let unknown = present_error(&allow, Error::UnknownRecipe);
        assert_eq!(not_allowed, unknown);
        assert_eq!(
            present_error(&RecipeAllow::None, Error::UnknownRecipe),
            Error::Unauthorized
        );
    }

    #[test]
    fn a_star_login_gets_unknown_recipe() {
        assert_eq!(
            present_error(&RecipeAllow::All, Error::UnknownRecipe),
            Error::UnknownRecipe
        );
    }

    #[test]
    fn other_errors_pass_through_for_every_login() {
        for allow in [RecipeAllow::All, RecipeAllow::None, names(&["x"])] {
            assert_eq!(
                present_error(&allow, Error::RequestIdConflict),
                Error::RequestIdConflict
            );
            assert_eq!(
                present_error(&allow, Error::BadArguments("amount".into())),
                Error::BadArguments("amount".into())
            );
        }
    }

    #[test]
    fn only_execute_is_a_permission_the_default_login_lacks_and_each_flag_is_separate() {
        let p = Permissions {
            execute: false,
            register: true,
            admin: false,
            sql: true,
        };
        assert!(!has_permission(p, Operation::Execute));
        assert!(has_permission(p, Operation::Register));
        assert!(!has_permission(p, Operation::Admin));
        assert!(has_permission(p, Operation::Sql));
    }
}
