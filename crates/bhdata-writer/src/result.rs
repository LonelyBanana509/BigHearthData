//! The stored result of a command (spec `write-path.md`, section 7): canonical JSON, keys sorted,
//! built from the plan's result values.
//!
//! `done`: `{"values":{name:value,...}}`. `failed`: `{"error":"<name>"}`. Decimals are text,
//! timestamps whole microseconds, bytes base64. A replay returns the stored text exactly; nothing
//! here is ever run again to rebuild it.

use bhdata_api::recipe::canon::Json;
use bhdata_api::recipe::eval::Value;
use bhdata_api::shard::{Outcome, Status};
use bhdata_api::types::Error;

/// One value as JSON.
#[must_use]
pub fn value_json(v: &Value) -> Json {
    match v {
        Value::I64(n) | Value::Timestamp(n) => Json::Int(*n),
        Value::Bool(b) => Json::Bool(*b),
        Value::Text(s) => Json::str(s),
        Value::Bytes(b) => Json::str(base64(b)),
        Value::Dec { .. } => Json::str(v.number_text().unwrap_or_default()),
    }
}

/// One entry of a result: a single value, or the rows of a `list` step (each row is its columns in
/// order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// A value of a `read`, or of the final read of the subject row.
    One(Value),
    /// The rows of a `list`.
    Rows(Vec<Vec<(String, Value)>>),
}

fn item_json(item: &Item) -> Json {
    match item {
        Item::One(v) => value_json(v),
        Item::Rows(rows) => Json::Arr(
            rows.iter()
                .map(|row| {
                    Json::Obj(
                        row.iter()
                            .map(|(name, v)| (name.clone(), value_json(v)))
                            .collect(),
                    )
                })
                .collect(),
        ),
    }
}

/// The result of a command that is `done`, from values and lists.
///
/// # Errors
/// `internal` if the plan produced more values or more bytes than the recipe limits allow (the
/// compiler checks the count; this is the second wall, and the bytes are only known now).
pub fn done_result_items(
    items: &[(String, Item)],
    max_values: usize,
    max_bytes: usize,
) -> Result<String, Error> {
    if items.len() > max_values {
        return Err(Error::Internal(format!(
            "a result has {} values, more than the limit of {max_values}",
            items.len()
        )));
    }
    let map = Json::Obj(
        items
            .iter()
            .map(|(name, item)| (name.clone(), item_json(item)))
            .collect(),
    );
    let text = Json::obj([("values", map)]).canonical();
    if text.len() > max_bytes {
        return Err(Error::Internal(format!(
            "a result of {} bytes is over the limit of {max_bytes}",
            text.len()
        )));
    }
    Ok(text)
}

/// The result of a command that is `done`, from single values only.
///
/// # Errors
/// As [`done_result_items`].
pub fn done_result(
    values: &[(String, Value)],
    max_values: usize,
    max_bytes: usize,
) -> Result<String, Error> {
    let items: Vec<(String, Item)> = values
        .iter()
        .map(|(n, v)| (n.clone(), Item::One(v.clone())))
        .collect();
    done_result_items(&items, max_values, max_bytes)
}

/// The result of a command that ended in a named error.
#[must_use]
pub fn failed_result(error: &str) -> String {
    Json::obj([("error", Json::str(error))]).canonical()
}

/// Rebuilds an [`Outcome`] from what a record holds (`status`, `error_kind`, `result`). A replay
/// never recomputes the result: `result` is returned as stored.
///
/// # Errors
/// `internal` if the record is not one the writer could have stored (a `running` record, a
/// `failed` one with no name, or a `done` one with a name).
pub fn stored_outcome(
    status: &str,
    error_kind: Option<&str>,
    result: String,
    replayed: bool,
) -> Result<Outcome, Error> {
    let status = match (status, error_kind) {
        ("done", None) => Status::Done,
        ("failed", Some(name)) if !name.is_empty() => Status::Failed {
            error: name.to_string(),
        },
        (other, kind) => {
            return Err(Error::Internal(format!(
                "a stored request has status `{other}` and error {kind:?}"
            )))
        }
    };
    Ok(Outcome {
        status,
        result,
        replayed,
    })
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                let idx = usize::try_from((n >> (18 - 6 * i)) & 63).unwrap_or(0);
                out.push(char::from(B64[idx]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[(&str, Value)]) -> Vec<(String, Value)> {
        items
            .iter()
            .map(|(k, x)| ((*k).to_string(), x.clone()))
            .collect()
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input:?}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn a_done_result_is_canonical_with_sorted_keys() {
        let values = v(&[
            (
                "balance",
                Value::Dec {
                    value: 12_345,
                    scale: 2,
                },
            ),
            ("at", Value::Timestamp(1_700_000_000_123_456)),
            ("count", Value::I64(-3)),
            ("ok", Value::Bool(true)),
            ("name", Value::Text("a\"b".into())),
            ("raw", Value::Bytes(b"foo".to_vec())),
        ]);
        assert_eq!(
            done_result(&values, 32, 65_536).unwrap(),
            r#"{"values":{"at":1700000000123456,"balance":"123.45","count":-3,"name":"a\"b","ok":true,"raw":"Zm9v"}}"#
        );
        assert_eq!(done_result(&[], 32, 65_536).unwrap(), r#"{"values":{}}"#);
    }

    #[test]
    fn a_decimal_keeps_its_scale_in_the_text() {
        let values = v(&[(
            "p",
            Value::Dec {
                value: 100,
                scale: 2,
            },
        )]);
        assert_eq!(
            done_result(&values, 32, 65_536).unwrap(),
            r#"{"values":{"p":"1.00"}}"#
        );
    }

    #[test]
    fn the_result_limits_are_second_walls() {
        let values = v(&[("a", Value::I64(1)), ("b", Value::I64(2))]);
        assert!(matches!(
            done_result(&values, 1, 65_536),
            Err(Error::Internal(_))
        ));
        assert!(matches!(
            done_result(&values, 32, 10),
            Err(Error::Internal(_))
        ));
        assert!(done_result(&values, 2, 64).is_ok());
    }

    #[test]
    fn a_list_is_an_array_of_objects_in_row_order() {
        let rows = vec![
            vec![
                ("at".to_string(), Value::Timestamp(5)),
                ("delta".to_string(), Value::I64(-2)),
            ],
            vec![
                ("at".to_string(), Value::Timestamp(4)),
                ("delta".to_string(), Value::I64(3)),
            ],
        ];
        let items = vec![
            ("recent".to_string(), Item::Rows(rows)),
            ("n".to_string(), Item::One(Value::I64(1))),
        ];
        assert_eq!(
            done_result_items(&items, 32, 65_536).unwrap(),
            r#"{"values":{"n":1,"recent":[{"at":5,"delta":-2},{"at":4,"delta":3}]}}"#
        );
        let empty = vec![("recent".to_string(), Item::Rows(vec![]))];
        assert_eq!(
            done_result_items(&empty, 32, 65_536).unwrap(),
            r#"{"values":{"recent":[]}}"#
        );
    }

    #[test]
    fn a_failed_result_is_the_name() {
        assert_eq!(
            failed_result("insufficient_funds"),
            r#"{"error":"insufficient_funds"}"#
        );
    }

    #[test]
    fn a_stored_record_becomes_an_outcome_and_the_bytes_are_kept() {
        let text = r#"{"values":{"a":1}}"#.to_string();
        let o = stored_outcome("done", None, text.clone(), true).unwrap();
        assert_eq!(
            o,
            Outcome {
                status: Status::Done,
                result: text,
                replayed: true
            }
        );
        let o =
            stored_outcome("failed", Some("overflow"), failed_result("overflow"), false).unwrap();
        assert_eq!(
            o.status,
            Status::Failed {
                error: "overflow".into()
            }
        );
        for bad in [
            ("running", None),
            ("failed", None),
            ("failed", Some("")),
            ("done", Some("overflow")),
            ("weird", None),
        ] {
            assert!(
                matches!(
                    stored_outcome(bad.0, bad.1, String::new(), true),
                    Err(Error::Internal(_))
                ),
                "{bad:?}"
            );
        }
    }
}
