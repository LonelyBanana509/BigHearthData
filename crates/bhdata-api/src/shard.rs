//! The shard seam, write side (spec `write-path.md`, sections 3, 4 and 9): what a command is, what
//! comes back, and the one function that names a payload. No implementation, no database.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use bhdata_recipe::canon::{sha256_hex, Json};
use bhdata_recipe::eval::Value;
use bhdata_recipe::plan::PlanArg;
use bhdata_recipe::Plan;
use bhdata_types::{Error, LoginId, RecipeVersion, RequestId, SubjectId, Timestamp};

/// How long the record of a command is kept (write-path spec, section 9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Retention {
    /// Until the retry window and the grace have passed (the normal case).
    #[default]
    Window,
    /// Never compacted: import, batch runs. Only code that builds a [`Command`] itself may ask.
    Permanent,
}

/// One command for one shard, built by the caller (planner, tests, `bhdatactl call`).
#[derive(Debug, Clone)]
pub struct Command {
    /// Who sent it. With `request_id`, the request key.
    pub login_id: LoginId,
    /// The client's id for this request.
    pub request_id: RequestId,
    /// When the client created the request (section 4).
    pub requested_at: Timestamp,
    /// The compiled recipe.
    pub plan: Arc<Plan>,
    /// The version of the recipe that runs.
    pub version: RecipeVersion,
    /// `true` if the caller named the version, `false` if it asked for the latest.
    pub version_named: bool,
    /// The shard key of the plan's subject.
    pub subject: SubjectId,
    /// The shard key of the counterparty, if the plan has one.
    pub counterparty: Option<SubjectId>,
    /// One typed value per plan argument, by name.
    pub args: BTreeMap<String, Value>,
    /// How long the record is kept.
    pub retention: Retention,
}

/// How a finished command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Applied.
    Done,
    /// A named error of the recipe (`insufficient_funds`, `overflow`, or a name the recipe defined).
    /// Nothing was applied.
    Failed {
        /// The name.
        error: String,
    },
}

/// The final outcome of a command: what the stored record holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Done or failed.
    pub status: Status,
    /// The stored result, canonical JSON, exactly as stored (a replay returns these bytes).
    pub result: String,
    /// `true` if this is the stored answer of an earlier identical request.
    pub replayed: bool,
}

/// The write side of a shard. Reads (M6) and status (M8) join this trait with their milestones.
pub trait ShardBackend: Send + Sync {
    /// Runs one command to its final outcome, or says why it could not (`overloaded`,
    /// `request_expired`, `request_id_conflict`, `bad_arguments`, `internal`, ...). A command that
    /// ends in an `Err` stored nothing, so the caller may retry it with the same request id.
    fn execute(&self, command: Command) -> impl Future<Output = Result<Outcome, Error>> + Send;
}

/// One argument, in the form that goes into the payload hash: its type and an exact text. A decimal
/// is normalised (`1.50` and `1.5` are one value), a timestamp is whole microseconds.
fn value_json(v: &Value) -> Json {
    let (t, text) = match v {
        Value::I64(n) => ("i64", n.to_string()),
        Value::Dec { .. } => ("dec", normalise(&v.number_text().unwrap_or_default())),
        Value::Bool(b) => ("bool", b.to_string()),
        Value::Text(s) => ("text", s.clone()),
        Value::Bytes(b) => ("bytes", hex(b)),
        Value::Timestamp(micros) => ("timestamp", micros.to_string()),
    };
    Json::obj([("t", Json::str(t)), ("v", Json::str(text))])
}

/// Removes trailing zeros after the point, and the point itself if nothing is left; `-0` is `0`.
fn normalise(decimal: &str) -> String {
    let text = if decimal.contains('.') {
        decimal.trim_end_matches('0').trim_end_matches('.')
    } else {
        decimal
    };
    if text == "-0" {
        "0".to_string()
    } else {
        text.to_string()
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

impl Command {
    /// SHA-256 of the canonical JSON of `{recipe, version, subject, counterparty, args}` (section 4).
    /// The version is in it only if the caller named one. `requested_at` and `retention` are not.
    #[must_use]
    pub fn payload_hash(&self) -> String {
        payload_hash(
            &self.plan.name,
            self.version_named.then_some(self.version),
            &self.subject,
            self.counterparty.as_ref(),
            &self.args,
        )
    }

    /// Checks what needs no database (section 3, step 0, first and last parts): every argument is
    /// present and has the plan's type, there are no extra ones, and the plan has a write.
    ///
    /// Bounds are checked by the writer against the plan's `min` and `max` as text; that part
    /// lives next to the plan's types in the writer.
    ///
    /// # Errors
    /// `bad_arguments` for a missing, extra or wrongly typed argument; `internal` for a plan that is
    /// a read.
    pub fn check_shape(&self) -> Result<(), Error> {
        check_args(
            &self.plan.name,
            &self.plan.args,
            !self.plan.writes.is_empty(),
            &self.args,
        )
    }
}

/// [`Command::check_shape`] on its parts, so it can be tested without compiling a plan.
fn check_args(
    recipe: &str,
    plan_args: &[PlanArg],
    writes: bool,
    args: &BTreeMap<String, Value>,
) -> Result<(), Error> {
    if !writes {
        return Err(Error::Internal(format!(
            "recipe `{recipe}` has no write and cannot be executed as a command"
        )));
    }
    for a in plan_args {
        match args.get(&a.name) {
            None => {
                return Err(Error::BadArguments(format!(
                    "argument `{}` is missing",
                    a.name
                )))
            }
            Some(v) if !type_matches(&a.ty, v) => {
                return Err(Error::BadArguments(format!(
                    "argument `{}` must be {}, not {}",
                    a.name,
                    a.ty,
                    v.ty().name()
                )))
            }
            Some(_) => {}
        }
    }
    if let Some(extra) = args
        .keys()
        .find(|k| !plan_args.iter().any(|a| &a.name == *k))
    {
        return Err(Error::BadArguments(format!(
            "`{extra}` is not an argument of `{recipe}`"
        )));
    }
    Ok(())
}

/// Whether `v` is a value of the plan's type text (`i64`, `decimal(18,2)`, `text(64)`, ...).
fn type_matches(ty: &str, v: &Value) -> bool {
    match v {
        Value::I64(_) => ty == "i64",
        Value::Dec { scale, .. } => ty
            .strip_prefix("decimal(")
            .and_then(|r| r.strip_suffix(')'))
            .and_then(|r| r.split_once(','))
            .and_then(|(_, s)| s.trim().parse::<u8>().ok())
            .is_some_and(|s| s == *scale),
        Value::Bool(_) => ty == "bool",
        Value::Text(_) => ty.starts_with("text("),
        Value::Bytes(_) => ty.starts_with("bytes("),
        Value::Timestamp(_) => ty == "timestamp",
    }
}

/// The payload hash of section 4, as a pure function so that the planner, the writer and the tests
/// use one copy.
#[must_use]
pub fn payload_hash(
    recipe: &str,
    version: Option<RecipeVersion>,
    subject: &SubjectId,
    counterparty: Option<&SubjectId>,
    args: &BTreeMap<String, Value>,
) -> String {
    let args = Json::Obj(
        args.iter()
            .map(|(name, v)| (name.clone(), value_json(v)))
            .collect(),
    );
    let doc = Json::obj([
        ("args", args),
        (
            "counterparty",
            Json::opt_str(counterparty.map(SubjectId::as_str)),
        ),
        ("recipe", Json::str(recipe)),
        ("subject", Json::str(subject.as_str())),
        (
            "version",
            version.map_or(Json::Null, |v| Json::Int(i64::from(v.get()))),
        ),
    ]);
    sha256_hex(&doc.canonical())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> SubjectId {
        SubjectId::new(s).unwrap()
    }

    fn args(items: &[(&str, Value)]) -> BTreeMap<String, Value> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn hash(version: Option<u32>, a: &BTreeMap<String, Value>) -> String {
        payload_hash(
            "spend",
            version.map(RecipeVersion::new),
            &id("alice"),
            None,
            a,
        )
    }

    fn dec(value: i128, scale: u8) -> Value {
        Value::Dec { value, scale }
    }

    #[test]
    fn the_hash_is_sha256_hex_and_stable() {
        let h = hash(None, &args(&[("amount", Value::I64(5))]));
        assert_eq!(h.len(), 64);
        assert_eq!(h, hash(None, &args(&[("amount", Value::I64(5))])));
        // Frozen: a change here splits request ids that are in flight across an upgrade.
        assert_eq!(
            payload_hash("r", None, &id("s"), None, &BTreeMap::new()),
            sha256_hex(
                r#"{"args":{},"counterparty":null,"recipe":"r","subject":"s","version":null}"#
            )
        );
    }

    #[test]
    fn anything_that_changes_the_meaning_changes_the_hash() {
        let base = hash(None, &args(&[("amount", Value::I64(5))]));
        assert_ne!(base, hash(None, &args(&[("amount", Value::I64(6))])));
        assert_ne!(base, hash(None, &args(&[("other", Value::I64(5))])));
        assert_ne!(base, hash(Some(1), &args(&[("amount", Value::I64(5))])));
        assert_ne!(
            hash(Some(1), &args(&[("amount", Value::I64(5))])),
            hash(Some(2), &args(&[("amount", Value::I64(5))]))
        );
        let a = args(&[("amount", Value::I64(5))]);
        assert_ne!(base, payload_hash("spend", None, &id("bob"), None, &a));
        assert_ne!(
            base,
            payload_hash("spend", None, &id("alice"), Some(&id("bob")), &a)
        );
        assert_ne!(base, payload_hash("give", None, &id("alice"), None, &a));
        // Text "5" is not the number 5.
        assert_ne!(
            base,
            hash(None, &args(&[("amount", Value::Text("5".into()))]))
        );
    }

    #[test]
    fn decimals_are_normalised_and_argument_order_does_not_matter() {
        assert_eq!(
            hash(None, &args(&[("p", dec(150, 2))])),
            hash(None, &args(&[("p", dec(15, 1))]))
        );
        assert_eq!(
            hash(None, &args(&[("p", dec(200, 2))])),
            hash(None, &args(&[("p", dec(2, 0))]))
        );
        assert_eq!(
            hash(None, &args(&[("p", dec(0, 3))])),
            hash(None, &args(&[("p", dec(0, 0))]))
        );
        assert_ne!(
            hash(None, &args(&[("p", dec(150, 2))])),
            hash(None, &args(&[("p", dec(151, 2))]))
        );
        let ab = args(&[("a", Value::I64(1)), ("b", Value::I64(2))]);
        let ba = args(&[("b", Value::I64(2)), ("a", Value::I64(1))]);
        assert_eq!(hash(None, &ab), hash(None, &ba));
    }

    fn plan_args() -> Vec<PlanArg> {
        [
            ("amount", "i64"),
            ("price", "decimal(18,2)"),
            ("note", "text(64)"),
        ]
        .into_iter()
        .map(|(n, t)| PlanArg {
            name: n.into(),
            ty: t.into(),
            min: None,
            max: None,
        })
        .collect()
    }

    fn good() -> BTreeMap<String, Value> {
        args(&[
            ("amount", Value::I64(1)),
            ("price", dec(150, 2)),
            ("note", Value::Text("x".into())),
        ])
    }

    #[test]
    fn a_well_formed_command_passes_the_shape_check() {
        assert_eq!(check_args("r", &plan_args(), true, &good()), Ok(()));
    }

    #[test]
    fn missing_extra_and_wrongly_typed_arguments_are_bad_arguments() {
        let mut a = good();
        a.remove("note");
        assert!(matches!(
            check_args("r", &plan_args(), true, &a),
            Err(Error::BadArguments(m)) if m.contains("`note` is missing")
        ));
        let mut a = good();
        a.insert("extra".into(), Value::I64(1));
        assert!(matches!(
            check_args("r", &plan_args(), true, &a),
            Err(Error::BadArguments(m)) if m.contains("`extra`")
        ));
        for (name, wrong) in [
            ("amount", dec(1, 0)),
            ("price", dec(15, 1)),
            ("price", Value::I64(1)),
            ("note", Value::Bytes(vec![1])),
        ] {
            let mut a = good();
            a.insert(name.into(), wrong);
            assert!(
                matches!(
                    check_args("r", &plan_args(), true, &a),
                    Err(Error::BadArguments(_))
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn a_plan_without_a_write_is_internal_not_bad_arguments() {
        assert!(matches!(
            check_args("r", &plan_args(), false, &good()),
            Err(Error::Internal(_))
        ));
    }

    #[test]
    fn retention_defaults_to_the_window() {
        assert_eq!(Retention::default(), Retention::Window);
    }
}
