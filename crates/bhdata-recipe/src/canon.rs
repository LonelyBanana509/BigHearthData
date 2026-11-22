//! Canonical JSON and the content hash (spec `recipe-compiler.md`, section 1).
//!
//! "Canonical" means the same value always gives the same bytes: object keys are sorted, there is
//! no whitespace, numbers are plain integers, and strings are escaped one way only (`"`, `\`, and
//! control characters as `\u00xx`; every other character is written as it is, in UTF-8). A plan's
//! hash is the SHA-256 of its canonical JSON, written as 64 lower-case hex digits.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// A JSON value, built in code and written in canonical form. Objects are sorted by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A whole number.
    Int(i64),
    /// Text.
    Str(String),
    /// A list, in order.
    Arr(Vec<Json>),
    /// An object, sorted by key.
    Obj(BTreeMap<String, Json>),
}

impl Json {
    /// An object from `(key, value)` pairs.
    #[must_use]
    pub fn obj<const N: usize>(fields: [(&str, Json); N]) -> Json {
        Json::Obj(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    /// Text.
    #[must_use]
    pub fn str(text: impl Into<String>) -> Json {
        Json::Str(text.into())
    }

    /// Text, or `null` when there is none.
    #[must_use]
    pub fn opt_str(text: Option<&str>) -> Json {
        text.map_or(Json::Null, Json::str)
    }

    /// A whole number from a count; saturates at `i64::MAX`.
    #[must_use]
    pub fn count(n: usize) -> Json {
        Json::Int(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// A list of texts.
    #[must_use]
    pub fn strs<I, S>(items: I) -> Json
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Json::Arr(items.into_iter().map(Json::str).collect())
    }

    /// The canonical text.
    #[must_use]
    pub fn canonical(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Json::Str(s) => write_string(s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Obj(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// SHA-256 of `text`, as 64 lower-case hex digits.
#[must_use]
pub fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_sorted_and_nothing_is_padded() {
        let j = Json::obj([
            ("b", Json::Int(2)),
            (
                "a",
                Json::Arr(vec![Json::Null, Json::Bool(true), Json::str("x")]),
            ),
        ]);
        assert_eq!(j.canonical(), r#"{"a":[null,true,"x"],"b":2}"#);
    }

    #[test]
    fn insertion_order_never_matters() {
        let one = Json::obj([
            ("x", Json::Int(1)),
            ("y", Json::Int(2)),
            ("z", Json::Int(3)),
        ]);
        let two = Json::obj([
            ("z", Json::Int(3)),
            ("x", Json::Int(1)),
            ("y", Json::Int(2)),
        ]);
        assert_eq!(one.canonical(), two.canonical());
    }

    #[test]
    fn strings_are_escaped_one_way() {
        let j = Json::str("a\"b\\c\nd\u{1}é");
        assert_eq!(j.canonical(), "\"a\\\"b\\\\c\\u000ad\\u0001é\"");
        assert_eq!(Json::Int(-7).canonical(), "-7");
        assert_eq!(Json::count(usize::MAX).canonical(), i64::MAX.to_string());
    }

    #[test]
    fn the_hash_is_sha256_in_lower_case_hex() {
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
