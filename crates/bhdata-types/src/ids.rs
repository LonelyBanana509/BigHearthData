//! Identifiers, and the deterministic derivation of generated ids (design section 6).
//!
//! # Id text
//!
//! Request, login, subject and transfer ids are short text: 1 to 128 characters (64 for a login)
//! from `A-Z a-z 0-9 . _ : -`. That is enough for a UUID, a Discord snowflake, or an id from some
//! other outside service, and it keeps ids safe to put in logs, URLs and file names.
//!
//! # Derived ids
//!
//! A generated subject id is a hash of the login id, the request id, the table and the position of
//! the create within the recipe. So:
//!
//! * a retried create produces the same id (it routes to the same shard and finds its stored result);
//! * two logins using the same request id never collide;
//! * two creates in one request get different ids.
//!
//! The derivation is **frozen**: ids derived today are stored in databases, so changing the hash,
//! the domain tags or the field order would orphan them. A known-answer test guards this. A change
//! needs a new domain tag (`v2`), never an edit of `v1`.
//!
//! The hash is SHA-256 over length-prefixed fields (so `("ab", "c")` and `("a", "bc")` differ),
//! cut to 128 bits and written as 32 lowercase hex characters.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Longest id text, in characters, for every id except a login id.
pub const MAX_ID_LEN: usize = 128;

/// Longest login id, in characters.
pub const MAX_LOGIN_LEN: usize = 64;

const SUBJECT_DOMAIN: &[u8] = b"bhdata/id/v1/subject";
const TRANSFER_DOMAIN: &[u8] = b"bhdata/id/v1/transfer";

fn validate_id(what: &str, value: &str, max: usize) -> Result<()> {
    let ok_len = !value.is_empty() && value.len() <= max;
    let ok_chars = value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    if ok_len && ok_chars {
        Ok(())
    } else {
        Err(Error::BadArguments(format!(
            "{what} must be 1 to {max} characters from A-Z a-z 0-9 . _ : -"
        )))
    }
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $what:literal, $max:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Longest allowed text for this id.
            pub const MAX_LEN: usize = $max;

            /// Checks `value` and wraps it. Fails with `bad_arguments` if it is empty, too long,
            /// or has a character outside `A-Z a-z 0-9 . _ : -`.
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                validate_id($what, &value, Self::MAX_LEN)?;
                Ok(Self(value))
            }

            /// The id as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(s: &str) -> Result<Self> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;

            fn try_from(value: String) -> Result<Self> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id! {
    /// Chosen by the client for each request. Unique per login, never across logins.
    RequestId, "request id", MAX_ID_LEN
}

string_id! {
    /// Names a login. Together with the request id it identifies one request.
    LoginId, "login id", MAX_LOGIN_LEN
}

string_id! {
    /// The one value the server guarantees unique: either the app's own id for a subject, or an id
    /// derived with [`derive_subject_id`].
    SubjectId, "subject id", MAX_ID_LEN
}

string_id! {
    /// Identifies a cross-shard transaction; a `pending` reply carries it so the client can poll.
    TransferId, "transfer id", MAX_ID_LEN
}

/// Hashes `fields`, each preceded by its length, under a domain tag; keeps the first 128 bits.
fn hash_fields(domain: &[u8], fields: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    for field in std::iter::once(&domain).chain(fields.iter()) {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(HEX[usize::from(byte >> 4)]));
        text.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    text
}

/// Derives the id of a created subject from the login, the request, the table, and the position of
/// the create within the recipe (0 for the first create, 1 for the second, and so on).
///
/// Deterministic and frozen (see the module docs). The result is 32 lowercase hex characters.
#[must_use]
pub fn derive_subject_id(
    login: &LoginId,
    request: &RequestId,
    table: &str,
    position: u32,
) -> SubjectId {
    let digest = hash_fields(
        SUBJECT_DOMAIN,
        &[
            login.as_str().as_bytes(),
            request.as_str().as_bytes(),
            table.as_bytes(),
            &position.to_be_bytes(),
        ],
    );
    SubjectId(to_hex(&digest))
}

/// Derives the id of the cross-shard transaction that one request starts.
///
/// A retried request gets the same transfer id, which is what makes the saga steps idempotent.
/// Deterministic and frozen, like [`derive_subject_id`]. The result is 32 lowercase hex characters.
#[must_use]
pub fn derive_transfer_id(login: &LoginId, request: &RequestId) -> TransferId {
    let digest = hash_fields(
        TRANSFER_DOMAIN,
        &[login.as_str().as_bytes(), request.as_str().as_bytes()],
    );
    TransferId(to_hex(&digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn login(s: &str) -> LoginId {
        LoginId::new(s).unwrap()
    }

    fn request(s: &str) -> RequestId {
        RequestId::new(s).unwrap()
    }

    #[test]
    fn accepts_ordinary_ids() {
        for ok in [
            "a",
            "req-1",
            "A.b_c:d-9",
            "123456789012345678",
            "0f8fad5b-d9cb-469f-a165-70867728950e",
        ] {
            assert!(RequestId::new(ok).is_ok(), "{ok}");
        }
        assert!(RequestId::new("x".repeat(MAX_ID_LEN)).is_ok());
        assert!(LoginId::new("x".repeat(MAX_LOGIN_LEN)).is_ok());
    }

    #[test]
    fn rejects_bad_ids_with_bad_arguments() {
        let too_long = "x".repeat(MAX_ID_LEN + 1);
        for bad in [
            "",
            "has space",
            "slash/",
            "quote\"",
            "tab\t",
            "nul\0",
            "é",
            "emoji😀",
            too_long.as_str(),
        ] {
            let err = RequestId::new(bad).unwrap_err();
            assert!(matches!(err, Error::BadArguments(_)), "{bad:?}");
        }
        assert!(LoginId::new("x".repeat(MAX_LOGIN_LEN + 1)).is_err());
    }

    #[test]
    fn display_parse_and_conversions_agree() {
        let id: SubjectId = "user-42".parse().unwrap();
        assert_eq!(id.to_string(), "user-42");
        assert_eq!(id.as_str(), "user-42");
        assert_eq!(String::from(id.clone()), "user-42");
        assert_eq!(SubjectId::try_from("user-42".to_string()).unwrap(), id);
        assert!("not ok".parse::<SubjectId>().is_err());
    }

    #[test]
    fn serde_uses_plain_strings_and_validates_on_the_way_in() {
        let id = RequestId::new("r1").unwrap();
        assert_eq!(toml_value_text(&id), "\"r1\"");
        let back: RequestId = toml::from_str::<Wrapper>("id = \"r1\"").unwrap().id;
        assert_eq!(back, id);
        assert!(toml::from_str::<Wrapper>("id = \"bad id\"").is_err());
    }

    #[derive(Serialize, Deserialize)]
    struct Wrapper {
        id: RequestId,
    }

    /// Serializes the id inside a one-field TOML table and returns just the quoted value.
    fn toml_value_text(id: &RequestId) -> String {
        let text = toml::to_string(&Wrapper { id: id.clone() }).unwrap();
        // `id = "r1"\n`
        text.trim_end().trim_start_matches("id = ").to_string()
    }

    /// Known answers, computed independently with Python's hashlib (see the module docs: these
    /// values must never change).
    #[test]
    fn derivation_is_frozen() {
        let subject = derive_subject_id(&login("shop-bot"), &request("req-0001"), "users", 0);
        assert_eq!(subject.as_str(), KNOWN_SUBJECT);
        let transfer = derive_transfer_id(&login("shop-bot"), &request("req-0001"));
        assert_eq!(transfer.as_str(), KNOWN_TRANSFER);
    }

    const KNOWN_SUBJECT: &str = "295a056f9a9dc8407994c6fbfed14d22";
    const KNOWN_TRANSFER: &str = "0c17669f8e9607e4cdf12163ec8e4389";

    #[test]
    fn derived_ids_are_32_lowercase_hex_and_valid_ids() {
        let id = derive_subject_id(&login("l"), &request("r"), "t", 0);
        assert_eq!(id.as_str().len(), 32);
        assert!(id
            .as_str()
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert!(SubjectId::new(id.as_str()).is_ok());
    }

    #[test]
    fn a_retried_create_gets_the_same_id() {
        let a = derive_subject_id(&login("bot"), &request("r1"), "users", 0);
        let b = derive_subject_id(&login("bot"), &request("r1"), "users", 0);
        assert_eq!(a, b);
    }

    #[test]
    fn two_logins_with_the_same_request_id_do_not_collide() {
        let a = derive_subject_id(&login("alice"), &request("r1"), "users", 0);
        let b = derive_subject_id(&login("bob"), &request("r1"), "users", 0);
        assert_ne!(a, b);
    }

    #[test]
    fn two_creates_in_one_request_differ_by_position_and_table() {
        let first = derive_subject_id(&login("bot"), &request("r1"), "users", 0);
        let second = derive_subject_id(&login("bot"), &request("r1"), "users", 1);
        let other_table = derive_subject_id(&login("bot"), &request("r1"), "items", 0);
        assert_ne!(first, second);
        assert_ne!(first, other_table);
    }

    #[test]
    fn field_boundaries_cannot_be_shifted() {
        // Without length prefixes these two would hash the same bytes.
        let a = derive_subject_id(&login("ab"), &request("c"), "t", 0);
        let b = derive_subject_id(&login("a"), &request("bc"), "t", 0);
        assert_ne!(a, b);
        let c = derive_subject_id(&login("l"), &request("r"), "ab", 0);
        let d = derive_subject_id(&login("l"), &request("r"), "a", 0);
        assert_ne!(c, d);
    }

    #[test]
    fn subject_and_transfer_ids_for_the_same_request_differ() {
        let subject = derive_subject_id(&login("bot"), &request("r1"), "", 0);
        let transfer = derive_transfer_id(&login("bot"), &request("r1"));
        assert_ne!(subject.as_str(), transfer.as_str());
    }

    proptest! {
        #[test]
        fn different_inputs_give_different_ids(
            l1 in "[A-Za-z0-9._:-]{1,16}", r1 in "[A-Za-z0-9._:-]{1,16}", t1 in "[a-z_]{0,8}", p1 in 0u32..4,
            l2 in "[A-Za-z0-9._:-]{1,16}", r2 in "[A-Za-z0-9._:-]{1,16}", t2 in "[a-z_]{0,8}", p2 in 0u32..4,
        ) {
            prop_assume!((&l1, &r1, &t1, p1) != (&l2, &r2, &t2, p2));
            let a = derive_subject_id(&login(&l1), &request(&r1), &t1, p1);
            let b = derive_subject_id(&login(&l2), &request(&r2), &t2, p2);
            prop_assert_ne!(a, b);
        }

        #[test]
        fn derivation_is_a_pure_function(l in "[A-Za-z0-9._:-]{1,16}", r in "[A-Za-z0-9._:-]{1,16}", t in "[a-z_]{0,8}", p in any::<u32>()) {
            let a = derive_subject_id(&login(&l), &request(&r), &t, p);
            let b = derive_subject_id(&login(&l), &request(&r), &t, p);
            prop_assert_eq!(a, b);
        }

        #[test]
        fn valid_text_always_parses_and_prints_back(s in "[A-Za-z0-9._:-]{1,128}") {
            let id = RequestId::new(s.clone()).unwrap();
            prop_assert_eq!(id.to_string(), s);
        }
    }
}
