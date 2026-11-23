//! What the writer checks before it takes a command (spec `write-path.md`, decision 14).
//!
//! A reply sent after a commit that was not synced would break "full durability by default", so the
//! writer refuses to start when the server's `synchronous_commit` does not match the configured
//! durability: `on` for full, `off` for relaxed (as `bhdata-engine` writes it).

use bhdata_api::types::Durability;
use bhdata_pg::pool::{describe, Pools};

/// The `synchronous_commit` value the configured durability requires.
#[must_use]
pub fn expected_synchronous_commit(durability: Durability) -> &'static str {
    match durability {
        Durability::Full => "on",
        Durability::Relaxed => "off",
    }
}

/// Whether the setting the server reports is the one the durability needs.
///
/// # Errors
/// A sentence naming both values.
pub fn check_synchronous_commit(actual: &str, durability: Durability) -> Result<(), String> {
    let want = expected_synchronous_commit(durability);
    if actual == want {
        Ok(())
    } else {
        Err(format!(
            "the server has synchronous_commit = {actual}, but durability {durability:?} needs {want}; \
             the writer will not start"
        ))
    }
}

/// Reads `synchronous_commit` as the recipe role sees it (the role the writers run as) and checks it.
///
/// # Errors
/// The pool or driver error, or the mismatch, as a sentence.
pub async fn verify_synchronous_commit(
    pools: &Pools,
    durability: Durability,
) -> Result<(), String> {
    let client = pools.recipe().await.map_err(|e| e.to_string())?;
    let row = client
        .query_one("SHOW synchronous_commit", &[])
        .await
        .map_err(|e| describe(&e))?;
    let actual: String = row.get(0);
    check_synchronous_commit(&actual, durability)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_needs_on_and_relaxed_needs_off() {
        assert_eq!(expected_synchronous_commit(Durability::Full), "on");
        assert_eq!(expected_synchronous_commit(Durability::Relaxed), "off");
        assert!(check_synchronous_commit("on", Durability::Full).is_ok());
        assert!(check_synchronous_commit("off", Durability::Relaxed).is_ok());
    }

    #[test]
    fn any_other_pairing_is_refused_and_names_both_values() {
        for (actual, d) in [
            ("off", Durability::Full),
            ("local", Durability::Full),
            ("remote_apply", Durability::Full),
            ("on", Durability::Relaxed),
        ] {
            let e = check_synchronous_commit(actual, d).expect_err(actual);
            assert!(e.contains(actual) && e.contains("will not start"), "{e}");
        }
    }
}
