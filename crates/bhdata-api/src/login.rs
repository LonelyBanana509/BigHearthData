//! The login seam (wire-protocol spec, sections 2, 6, 7 and 10): what the gateway reads about a
//! login, and what it reports for the audit log. `bhdata-control` implements it from the `logins`
//! table; the gateway only calls it. No implementation here.

use std::collections::BTreeSet;
use std::future::Future;
use std::net::IpAddr;

use bhdata_types::{Error, LoginId, Timestamp};

/// The four permissions of a login (design section 10). At M5 only `execute` has any effect on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Permissions {
    /// May run recipes.
    pub execute: bool,
    /// May register recipes (CLI only until M8).
    pub register: bool,
    /// May use the admin operations (CLI only until M8).
    pub admin: bool,
    /// May use SQL mode (M12).
    pub sql: bool,
}

/// Which recipes an `execute` login may run. The default is none.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RecipeAllow {
    /// No recipe.
    #[default]
    None,
    /// Every recipe (`*`). Such a login gets `unknown_recipe` for an unknown name.
    All,
    /// Exactly these names. For such a login an unknown and a not-allowed recipe both answer `unauthorized`.
    Names(BTreeSet<String>),
}

impl RecipeAllow {
    /// `true` if `recipe` is on the list.
    #[must_use]
    pub fn permits(&self, recipe: &str) -> bool {
        match self {
            RecipeAllow::None => false,
            RecipeAllow::All => true,
            RecipeAllow::Names(names) => names.contains(recipe),
        }
    }
}

/// A SCRAM-SHA-256 verifier (RFC 5802): never the password.
#[derive(Clone, PartialEq, Eq)]
pub struct ScramVerifier {
    /// The salt.
    pub salt: Vec<u8>,
    /// The PBKDF2 iteration count.
    pub iterations: u32,
    /// `H(HMAC(SaltedPassword, "Client Key"))`.
    pub stored_key: [u8; 32],
    /// `HMAC(SaltedPassword, "Server Key")`.
    pub server_key: [u8; 32],
}

impl std::fmt::Debug for ScramVerifier {
    // The keys never reach a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScramVerifier")
            .field("iterations", &self.iterations)
            .finish_non_exhaustive()
    }
}

/// One row of the `logins` table, as the gateway needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRecord {
    /// The login id (the name the client logs in with).
    pub login_id: LoginId,
    /// The password verifier.
    pub verifier: ScramVerifier,
    /// What the login may do.
    pub permissions: Permissions,
    /// Which recipes it may run.
    pub recipes: RecipeAllow,
    /// Requests per second, or `None` for `gateway.rate_per_sec`.
    pub rate_per_sec: Option<u32>,
    /// Burst, or `None` for `gateway.rate_burst`.
    pub rate_burst: Option<u32>,
    /// Most connections, or `None` for `gateway.max_connections_per_login`.
    pub max_connections: Option<u32>,
    /// A disabled login fails like a wrong password.
    pub disabled: bool,
}

/// What the gateway reports for the audit log (spec, section 7). Login creation, disabling and
/// permission or limit changes are written by the control plane itself, not reported here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEvent {
    /// A login succeeded.
    LoginSucceeded {
        /// When.
        at: Timestamp,
        /// Who.
        login_id: LoginId,
        /// From where.
        source: IpAddr,
    },
    /// A login failed. `name_exists` is for the log only; the client never learns it.
    LoginFailed {
        /// When.
        at: Timestamp,
        /// The name that was tried, as sent (it may not be a valid login id).
        name: String,
        /// Whether a login of that name exists.
        name_exists: bool,
        /// From where.
        source: IpAddr,
    },
    /// A name was locked. Stored before the `locked_out` answer is sent.
    LockedOut {
        /// When.
        at: Timestamp,
        /// The name.
        name: String,
    },
    /// A lock ended.
    LockoutReleased {
        /// When.
        at: Timestamp,
        /// The name.
        name: String,
    },
}

/// The gateway's view of the control plane's logins and audit log.
pub trait LoginStore: Send + Sync {
    /// The login of that name, or `None`. `name` is whatever the client sent, so it is not a `LoginId` yet.
    fn find(&self, name: &str) -> impl Future<Output = Result<Option<LoginRecord>, Error>> + Send;

    /// Records an event. Best effort and never delays a reply, except `LockedOut`, which the caller
    /// awaits before it answers (spec, section 7).
    fn audit(&self, event: AuditEvent) -> impl Future<Output = Result<(), Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_login_may_do_nothing() {
        assert_eq!(
            Permissions::default(),
            Permissions {
                execute: false,
                register: false,
                admin: false,
                sql: false
            }
        );
        assert!(!RecipeAllow::default().permits("anything"));
    }

    #[test]
    fn the_allow_list_is_exact() {
        let names = RecipeAllow::Names(["spend".to_string()].into_iter().collect());
        assert!(names.permits("spend"));
        assert!(!names.permits("spend2"));
        assert!(!names.permits("Spend"));
        assert!(RecipeAllow::All.permits("whatever"));
    }

    #[test]
    fn a_verifier_never_prints_its_keys() {
        let v = ScramVerifier {
            salt: vec![1, 2, 3],
            iterations: 4096,
            stored_key: [7; 32],
            server_key: [9; 32],
        };
        let text = format!("{v:?}");
        assert!(text.contains("4096"));
        assert!(
            !text.contains("stored_key") && !text.contains("server_key") && !text.contains("salt")
        );
    }
}
