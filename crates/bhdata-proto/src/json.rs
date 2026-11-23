//! The exact JSON parser (wire-protocol spec, decision 4). Lane P2.
//!
//! JSON is parsed once, exactly: an integer is never passed through a float. The parser refuses, and
//! the connection is closed on: invalid UTF-8, nesting deeper than the limit, an object with too many
//! keys, a string over the limit, **duplicate keys, floating-point numbers, and integers outside the
//! 64-bit range**. Unknown fields are the message layer's business (`message.rs`).
//!
//! The parser recurses at most [`MAX_RECURSION`] levels whatever the configured depth is.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::frame::Limits;

/// Hard cap on recursion, so a configured depth can never be a stack overflow.
pub const MAX_RECURSION: u32 = 128;

/// A JSON value as the protocol allows it: no floats, integers are `i64`, objects are sorted by key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A whole number in the 64-bit range.
    Int(i64),
    /// Text.
    Str(String),
    /// A list, in order.
    Arr(Vec<Value>),
    /// An object. Keys are unique (the parser refuses duplicates) and sorted.
    Obj(BTreeMap<String, Value>),
}

impl Value {
    /// A string value.
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }

    /// An object from pairs. A repeated key keeps the last value (this is for building, not parsing).
    pub fn obj<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Value)>) -> Value {
        Value::Obj(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }
}

/// What was wrong with the JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonErrorKind {
    /// The bytes are not UTF-8.
    NotUtf8,
    /// Not JSON (a missing comma, a bad literal, an empty input, a leading zero, ...).
    Syntax,
    /// Arrays and objects nested deeper than `max_depth`.
    TooDeep,
    /// More keys in one object than `max_keys`.
    TooManyKeys,
    /// A string (or key) longer than `max_string`.
    StringTooLong,
    /// A key repeated within one object.
    DuplicateKey,
    /// A number with a fraction or an exponent.
    Float,
    /// A whole number outside the 64-bit range.
    IntOutOfRange,
    /// A bad escape, or a `\u` surrogate with no partner.
    BadEscape,
    /// A raw control character (below U+0020) inside a string.
    ControlCharacter,
    /// Something after the one value.
    TrailingData,
}

/// A JSON refusal and the byte where it was noticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("bad JSON at byte {offset}: {kind:?}")]
pub struct JsonError {
    /// What.
    pub kind: JsonErrorKind,
    /// Where, as a byte offset into the input.
    pub offset: usize,
}

/// Parses one JSON value that fills all of `bytes`, under `limits` (`max_frame` is the frame
/// reader's and is not checked here).
///
/// # Errors
/// [`JsonError`], see [`JsonErrorKind`].
pub fn parse(bytes: &[u8], limits: &Limits) -> Result<Value, JsonError> {
    let text = std::str::from_utf8(bytes).map_err(|e| JsonError {
        kind: JsonErrorKind::NotUtf8,
        offset: e.valid_up_to(),
    })?;
    let mut p = Parser {
        src: text.as_bytes(),
        text,
        pos: 0,
        max_depth: limits.max_depth.min(MAX_RECURSION),
        max_keys: limits.max_keys,
        max_string: limits.max_string,
    };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos != p.src.len() {
        return Err(p.err(JsonErrorKind::TrailingData));
    }
    Ok(v)
}

struct Parser<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    max_depth: u32,
    max_keys: u32,
    max_string: u32,
}

impl Parser<'_> {
    fn err(&self, kind: JsonErrorKind) -> JsonError {
        JsonError {
            kind,
            offset: self.pos,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), JsonError> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(JsonErrorKind::Syntax))
        }
    }

    fn rest_starts_with(&self, word: &[u8]) -> bool {
        self.src
            .get(self.pos..)
            .is_some_and(|r| r.starts_with(word))
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value, JsonError> {
        if self.rest_starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err(JsonErrorKind::Syntax))
        }
    }

    fn value(&mut self, depth: u32) -> Result<Value, JsonError> {
        match self.peek() {
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => self.string().map(Value::Str),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err(JsonErrorKind::Syntax)),
        }
    }

    fn array(&mut self, depth: u32) -> Result<Value, JsonError> {
        if depth > self.max_depth {
            return Err(self.err(JsonErrorKind::TooDeep));
        }
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Arr(items));
                }
                _ => return Err(self.err(JsonErrorKind::Syntax)),
            }
        }
    }

    fn object(&mut self, depth: u32) -> Result<Value, JsonError> {
        if depth > self.max_depth {
            return Err(self.err(JsonErrorKind::TooDeep));
        }
        self.pos += 1; // {
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Obj(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err(JsonErrorKind::Syntax));
            }
            let key_at = self.pos;
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let v = self.value(depth)?;
            if map.len() >= self.max_keys as usize {
                return Err(JsonError {
                    kind: JsonErrorKind::TooManyKeys,
                    offset: key_at,
                });
            }
            if map.insert(key, v).is_some() {
                return Err(JsonError {
                    kind: JsonErrorKind::DuplicateKey,
                    offset: key_at,
                });
            }
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Obj(map));
                }
                _ => return Err(self.err(JsonErrorKind::Syntax)),
            }
        }
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err(JsonErrorKind::Syntax)); // a leading zero
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.err(JsonErrorKind::Syntax)),
        }
        let int_end = self.pos;
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            return Err(self.err(JsonErrorKind::Float));
        }
        let digits = self.text.get(start..int_end).unwrap_or("");
        digits
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| JsonError {
                kind: JsonErrorKind::IntOutOfRange,
                offset: start,
            })
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let end = self.pos + 4;
        let digits = self
            .text
            .get(self.pos..end)
            .ok_or_else(|| self.err(JsonErrorKind::BadEscape))?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(self.err(JsonErrorKind::BadEscape));
        }
        let n = u32::from_str_radix(digits, 16).map_err(|_| self.err(JsonErrorKind::BadEscape))?;
        self.pos = end;
        Ok(n)
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.pos += 1; // opening quote
        let max = self.max_string as usize;
        let mut out = String::new();
        loop {
            let Some(b) = self.peek() else {
                return Err(self.err(JsonErrorKind::Syntax)); // no closing quote
            };
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let Some(esc) = self.peek() else {
                        return Err(self.err(JsonErrorKind::BadEscape));
                    };
                    self.pos += 1;
                    let ch = match esc {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let first = self.hex4()?;
                            match first {
                                0xD800..=0xDBFF => {
                                    if !self.rest_starts_with(b"\\u") {
                                        return Err(self.err(JsonErrorKind::BadEscape));
                                    }
                                    self.pos += 2;
                                    let second = self.hex4()?;
                                    if !(0xDC00..=0xDFFF).contains(&second) {
                                        return Err(self.err(JsonErrorKind::BadEscape));
                                    }
                                    let cp = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                                    char::from_u32(cp)
                                        .ok_or_else(|| self.err(JsonErrorKind::BadEscape))?
                                }
                                0xDC00..=0xDFFF => return Err(self.err(JsonErrorKind::BadEscape)),
                                cp => char::from_u32(cp)
                                    .ok_or_else(|| self.err(JsonErrorKind::BadEscape))?,
                            }
                        }
                        _ => {
                            self.pos -= 1;
                            return Err(self.err(JsonErrorKind::BadEscape));
                        }
                    };
                    if out.len() + ch.len_utf8() > max {
                        return Err(self.err(JsonErrorKind::StringTooLong));
                    }
                    out.push(ch);
                }
                0x00..=0x1F => return Err(self.err(JsonErrorKind::ControlCharacter)),
                _ => {
                    // Copy the plain run up to the next quote, backslash or control byte in one go.
                    let start = self.pos;
                    while let Some(c) = self.peek() {
                        if c == b'"' || c == b'\\' || c < 0x20 {
                            break;
                        }
                        self.pos += 1;
                    }
                    let run = self.text.get(start..self.pos).unwrap_or("");
                    if out.len() + run.len() > max {
                        return Err(JsonError {
                            kind: JsonErrorKind::StringTooLong,
                            offset: start,
                        });
                    }
                    out.push_str(run);
                }
            }
        }
    }
}

/// Writes `v` as compact JSON with keys in sorted order (the order the type keeps them in). Text is
/// written as UTF-8; only the characters that must be escaped are.
#[must_use]
pub fn to_string(v: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, v);
    out
}

fn write_value(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Str(s) => write_string(out, s),
        Value::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Obj(map) => {
            out.push('{');
            for (i, (k, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k);
                out.push(':');
                write_value(out, item);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn limits() -> Limits {
        Limits {
            max_frame: 1_024,
            max_depth: 8,
            max_keys: 4,
            max_string: 16,
        }
    }

    fn kind(text: &str) -> JsonErrorKind {
        parse(text.as_bytes(), &limits()).unwrap_err().kind
    }

    fn ok(text: &str) -> Value {
        parse(text.as_bytes(), &limits()).unwrap()
    }

    #[test]
    fn plain_values_parse() {
        assert_eq!(ok("null"), Value::Null);
        assert_eq!(ok(" true "), Value::Bool(true));
        assert_eq!(ok("false"), Value::Bool(false));
        assert_eq!(ok("0"), Value::Int(0));
        assert_eq!(ok("-0"), Value::Int(0));
        assert_eq!(ok("-12"), Value::Int(-12));
        assert_eq!(ok("\"hi\""), Value::str("hi"));
        assert_eq!(ok("[]"), Value::Arr(vec![]));
        assert_eq!(ok("{}"), Value::Obj(BTreeMap::new()));
        assert_eq!(
            ok("{\"b\": [1, 2], \"a\": {\"c\": null}}"),
            Value::obj([
                ("a", Value::obj([("c", Value::Null)])),
                ("b", Value::Arr(vec![Value::Int(1), Value::Int(2)])),
            ])
        );
    }

    #[test]
    fn integers_are_exact_at_the_edges_of_i64() {
        assert_eq!(ok("9223372036854775807"), Value::Int(i64::MAX));
        assert_eq!(ok("-9223372036854775808"), Value::Int(i64::MIN));
        assert_eq!(kind("9223372036854775808"), JsonErrorKind::IntOutOfRange);
        assert_eq!(kind("-9223372036854775809"), JsonErrorKind::IntOutOfRange);
        assert_eq!(
            kind("123456789012345678901234567890"),
            JsonErrorKind::IntOutOfRange
        );
    }

    #[test]
    fn floats_are_refused_in_every_spelling() {
        for text in [
            "1.5",
            "1.0",
            "0.1",
            "1e3",
            "1E3",
            "1e-3",
            "-0.0",
            "{\"a\": 2.5}",
            "[1, 2.0]",
        ] {
            assert_eq!(kind(text), JsonErrorKind::Float, "{text}");
        }
    }

    #[test]
    fn malformed_numbers_are_syntax_errors() {
        for text in ["01", "-", "+1", "--1", ".5", "-a", "00"] {
            assert_eq!(kind(text), JsonErrorKind::Syntax, "{text}");
        }
        assert_eq!(kind("1."), JsonErrorKind::Float);
        assert_eq!(kind("0x10"), JsonErrorKind::TrailingData);
    }

    #[test]
    fn duplicate_keys_are_refused_even_with_equal_values() {
        assert_eq!(kind("{\"a\": 1, \"a\": 1}"), JsonErrorKind::DuplicateKey);
        assert_eq!(
            kind("{\"a\": 1, \"b\": 2, \"a\": 3}"),
            JsonErrorKind::DuplicateKey
        );
        assert!(parse(b"{\"a\": {\"a\": 1}}", &limits()).is_ok());
        assert_eq!(
            kind("{\"a\": 1, \"\\u0061\": 2}"),
            JsonErrorKind::DuplicateKey
        );
    }

    #[test]
    fn nesting_is_limited_and_the_limit_is_exact() {
        let eight = format!("{}1{}", "[".repeat(8), "]".repeat(8));
        let nine = format!("{}1{}", "[".repeat(9), "]".repeat(9));
        assert!(parse(eight.as_bytes(), &limits()).is_ok());
        assert_eq!(kind(&nine), JsonErrorKind::TooDeep);
        let objs = format!("{}1{}", "{\"a\":".repeat(9), "}".repeat(9));
        assert_eq!(kind(&objs), JsonErrorKind::TooDeep);
    }

    #[test]
    fn a_very_deep_input_is_refused_without_overflowing_the_stack() {
        let deep = "[".repeat(100_000);
        let big = Limits {
            max_depth: u32::MAX,
            ..limits()
        };
        assert_eq!(
            parse(deep.as_bytes(), &big).unwrap_err().kind,
            JsonErrorKind::TooDeep
        );
    }

    #[test]
    fn keys_per_object_and_string_length_are_limited() {
        assert!(parse(b"{\"a\":1,\"b\":2,\"c\":3,\"d\":4}", &limits()).is_ok());
        assert_eq!(
            kind("{\"a\":1,\"b\":2,\"c\":3,\"d\":4,\"e\":5}"),
            JsonErrorKind::TooManyKeys
        );
        assert!(parse(format!("\"{}\"", "x".repeat(16)).as_bytes(), &limits()).is_ok());
        assert_eq!(
            kind(&format!("\"{}\"", "x".repeat(17))),
            JsonErrorKind::StringTooLong
        );
        assert_eq!(
            kind(&format!("{{\"{}\":1}}", "k".repeat(17))),
            JsonErrorKind::StringTooLong
        );
        assert_eq!(
            kind(&format!("\"{}\"", "\\n".repeat(17))),
            JsonErrorKind::StringTooLong
        );
        assert!(parse(format!("\"{}\"", "é".repeat(8)).as_bytes(), &limits()).is_ok());
        assert_eq!(
            kind(&format!("\"{}\"", "é".repeat(9))),
            JsonErrorKind::StringTooLong
        );
    }

    #[test]
    fn invalid_utf8_is_refused_with_its_position() {
        let e = parse(b"\"ab\xFFcd\"", &limits()).unwrap_err();
        assert_eq!(e.kind, JsonErrorKind::NotUtf8);
        assert_eq!(e.offset, 3);
        assert_eq!(
            parse(b"\"\xC3\"", &limits()).unwrap_err().kind,
            JsonErrorKind::NotUtf8
        );
        assert_eq!(
            parse(b"\"\xC0\xAF\"", &limits()).unwrap_err().kind,
            JsonErrorKind::NotUtf8
        );
        assert_eq!(
            parse(b"\"\xED\xA0\x80\"", &limits()).unwrap_err().kind,
            JsonErrorKind::NotUtf8
        );
    }

    #[test]
    fn escapes_decode_and_bad_ones_are_refused() {
        assert_eq!(
            ok(r#""a\"b\\c\/d\b\f\n\r\t""#),
            Value::str("a\"b\\c/d\u{8}\u{c}\n\r\t")
        );
        assert_eq!(ok(r#""\u0041""#), Value::str("A"));
        assert_eq!(ok(r#""\u00e9""#), Value::str("é"));
        assert_eq!(ok(r#""\ud83d\ude00""#), Value::str("😀"));
        for bad in [
            r#""\ud83d""#,
            r#""\ud83dx""#,
            r#""\ude00""#,
            r#""\ud83d\u0041""#,
            r#""\x41""#,
            r#""\u12""#,
            r#""\u12G4""#,
            "\"\\",
        ] {
            assert_eq!(kind(bad), JsonErrorKind::BadEscape, "{bad}");
        }
    }

    #[test]
    fn raw_control_characters_in_strings_are_refused() {
        assert_eq!(kind("\"a\nb\""), JsonErrorKind::ControlCharacter);
        assert_eq!(kind("\"a\tb\""), JsonErrorKind::ControlCharacter);
        assert_eq!(kind("\"a\u{1}b\""), JsonErrorKind::ControlCharacter);
        assert!(parse("\"a\u{7f}b\"".as_bytes(), &limits()).is_ok());
    }

    #[test]
    fn syntax_errors_and_trailing_data_are_refused() {
        for text in [
            "",
            "   ",
            "{",
            "[",
            "[1,",
            "[1 2]",
            "[1,]",
            "{\"a\"}",
            "{\"a\":}",
            "{\"a\":1,}",
            "{a:1}",
            "{'a':1}",
            "tru",
            "nul",
            "True",
            "\"abc",
            "{\"a\" 1}",
            "[,1]",
            "}",
            "]",
            ",",
        ] {
            assert_eq!(kind(text), JsonErrorKind::Syntax, "{text:?}");
        }
        assert_eq!(kind("1 2"), JsonErrorKind::TrailingData);
        assert_eq!(kind("{} {}"), JsonErrorKind::TrailingData);
        assert_eq!(kind("null x"), JsonErrorKind::TrailingData);
        assert_eq!(kind("[]]"), JsonErrorKind::TrailingData);
        assert_eq!(ok("\t\r\n [ ] \n"), Value::Arr(vec![]));
        assert_eq!(kind("\u{a0}1"), JsonErrorKind::Syntax);
        assert_eq!(kind("\u{feff}1"), JsonErrorKind::Syntax);
    }

    #[test]
    fn writing_escapes_only_what_must_be() {
        let v = Value::obj([
            ("k\"ey", Value::str("a\\b\n\u{1}é😀")),
            ("n", Value::Int(-5)),
            ("l", Value::Arr(vec![Value::Null, Value::Bool(true)])),
        ]);
        assert_eq!(
            to_string(&v),
            "{\"k\\\"ey\":\"a\\\\b\\n\\u0001é😀\",\"l\":[null,true],\"n\":-5}"
        );
    }

    fn arb_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(Value::Int),
            "\\PC{0,5}".prop_map(Value::Str),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..4).prop_map(Value::Arr),
                proptest::collection::btree_map("[a-z]{1,3}", inner, 0..4).prop_map(Value::Obj),
            ]
        })
    }

    proptest! {
        #[test]
        fn what_is_written_parses_back_to_the_same_value(v in arb_value()) {
            let text = to_string(&v);
            let back = parse(text.as_bytes(), &Limits::trusted()).unwrap();
            prop_assert_eq!(back, v);
        }

        #[test]
        fn no_input_makes_the_parser_panic(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            let _ = parse(&bytes, &limits());
            let _ = parse(&bytes, &Limits::trusted());
        }

        #[test]
        fn text_that_looks_like_json_never_panics(text in "[\\[\\]{}:,\"\\\\0-9a-fx.eE+\\- \\n]{0,60}") {
            let _ = parse(text.as_bytes(), &limits());
        }
    }
}
