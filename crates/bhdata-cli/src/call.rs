//! `bhdatactl call`: runs one recipe once, as the local admin (spec `write-path.md`, section 11).
//!
//! ```text
//! bhdatactl call <recipe file> --schema <schema file> --subject <id> [--counterparty <id>]
//!                [--arg name=value]... [--request-id <id>] [--config <absolute path>]
//! ```
//!
//! It is a dev command, not a client interface: it makes the by-hand checks and the clean-VM run
//! possible before the gateway exists (M5). The recipe and the schema are read from files and
//! compiled on the spot; the schema must already be applied and the recipe registered (`bhdatactl
//! schema apply`, `bhdatactl recipe register`), because that is what gives the recipe role its
//! rights. The command goes through the same executor the server's queue uses. Run it as the
//! service user, like the other commands. Without `--request-id` it makes a fresh one and prints
//! it, so a retry can reuse it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use bhdata_api::recipe::eval::Value;
use bhdata_api::recipe::plan::PlanArg;
use bhdata_api::shard::{Command, Outcome, Retention, Status};
use bhdata_api::types::config::PoolConfig;
use bhdata_api::types::{Clock, Config, LoginId, RecipeVersion, RequestId, SubjectId, SystemClock};
use bhdata_engine::host::{self, SERVICE_USER};
use bhdata_pg::pool::Pools;
use bhdata_recipe::{compile, parse_recipe, Limits};
use bhdata_schema::parse_schema;
use bhdata_writer::exec::{ExecConfig, Executor};
use bhdata_writer::startup::verify_synchronous_commit;

/// The login the command is run under.
const LOCAL_ADMIN: &str = "local-admin";

/// A `call` command line, understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallArgs {
    /// The recipe file.
    pub recipe_file: PathBuf,
    /// The schema file the recipe is compiled against.
    pub schema_file: PathBuf,
    /// The subject's id.
    pub subject: String,
    /// The counterparty's id, for a recipe that has one.
    pub counterparty: Option<String>,
    /// `--arg name=value`, in the order given.
    pub args: Vec<(String, String)>,
    /// The request id, if the caller chose one.
    pub request_id: Option<String>,
    /// `--config <path>` or nothing, for the engine's config reader.
    pub rest: Vec<OsString>,
}

/// Reads what follows `call` (the recipe file first).
///
/// # Errors
/// A sentence saying what is wrong.
pub fn parse(args: &[OsString]) -> Result<CallArgs, String> {
    let mut it = args.iter();
    let recipe_file = match it.next() {
        Some(f) if !f.to_string_lossy().starts_with("--") => PathBuf::from(f),
        _ => return Err("a recipe file is missing".to_string()),
    };
    let (mut schema_file, mut subject, mut counterparty, mut request_id) = (None, None, None, None);
    let mut pairs = Vec::new();
    let mut rest = Vec::new();
    while let Some(flag) = it.next() {
        let name = flag.to_string_lossy().into_owned();
        let mut value = |what: &str| -> Result<String, String> {
            it.next()
                .map(|v| v.to_string_lossy().into_owned())
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match name.as_str() {
            "--schema" => schema_file = Some(PathBuf::from(value("--schema")?)),
            "--subject" => subject = Some(value("--subject")?),
            "--counterparty" => counterparty = Some(value("--counterparty")?),
            "--request-id" => request_id = Some(value("--request-id")?),
            "--arg" => {
                let text = value("--arg")?;
                let (k, v) = text
                    .split_once('=')
                    .ok_or_else(|| format!("--arg wants name=value, not `{text}`"))?;
                pairs.push((k.to_string(), v.to_string()));
            }
            "--config" => {
                rest.push(OsString::from("--config"));
                rest.push(OsString::from(value("--config")?));
            }
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    Ok(CallArgs {
        recipe_file,
        schema_file: schema_file.ok_or("--schema <schema file> is missing")?,
        subject: subject.ok_or("--subject <id> is missing")?,
        counterparty,
        args: pairs,
        request_id,
        rest,
    })
}

/// One argument's value, read by the type the plan gives it (`i64`, `decimal(18,2)`, `text(64)`,
/// `bool`, `timestamp` as whole microseconds, `bytes(n)` as hex).
///
/// # Errors
/// A sentence naming the argument.
pub fn value_of(arg: &PlanArg, text: &str) -> Result<Value, String> {
    let bad = |what: &str| format!("argument `{}` must be {what}, not `{text}`", arg.name);
    let ty = arg.ty.as_str();
    if ty == "i64" {
        return text
            .parse()
            .map(Value::I64)
            .map_err(|_| bad("a whole number"));
    }
    if ty == "bool" {
        return match text {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(bad("true or false")),
        };
    }
    if ty == "timestamp" {
        return text
            .parse()
            .map(Value::Timestamp)
            .map_err(|_| bad("whole microseconds since 1970"));
    }
    if ty.starts_with("text(") {
        return Ok(Value::Text(text.to_string()));
    }
    if ty.starts_with("bytes(") {
        return unhex(text)
            .map(Value::Bytes)
            .ok_or_else(|| bad("hex bytes"));
    }
    if let Some(scale) = ty
        .strip_prefix("decimal(")
        .and_then(|r| r.strip_suffix(')'))
        .and_then(|r| r.split_once(','))
        .and_then(|(_, s)| s.trim().parse::<u8>().ok())
    {
        return Value::dec_from_text(text, scale)
            .map_err(|_| bad(&format!("a decimal with {scale} digits after the point")));
    }
    Err(format!(
        "argument `{}` has the type `{ty}`, which this command cannot read",
        arg.name
    ))
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// All the `--arg` pairs as typed values; a name the plan does not have is an error here, a
/// missing one is left for the writer's own check.
///
/// # Errors
/// A sentence naming the argument.
pub fn values(
    plan_args: &[PlanArg],
    pairs: &[(String, String)],
) -> Result<BTreeMap<String, Value>, String> {
    let mut out = BTreeMap::new();
    for (name, text) in pairs {
        let arg = plan_args
            .iter()
            .find(|a| &a.name == name)
            .ok_or_else(|| format!("`{name}` is not an argument of this recipe"))?;
        if out.insert(name.clone(), value_of(arg, text)?).is_some() {
            return Err(format!("argument `{name}` is given twice"));
        }
    }
    Ok(out)
}

/// The outcome as one line of JSON. The stored result is already canonical JSON and goes in as is.
#[must_use]
pub fn render(request_id: &str, outcome: &Outcome) -> String {
    let status = match &outcome.status {
        Status::Done => "\"done\"".to_string(),
        Status::Failed { error } => format!("\"failed\",\"error\":\"{error}\""),
    };
    format!(
        "{{\"request_id\":\"{request_id}\",\"status\":{status},\"replayed\":{},\"result\":{}}}",
        outcome.replayed, outcome.result
    )
}

fn read_file(path: &std::path::Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Runs the command; the lines to print, or why it failed.
///
/// # Errors
/// A sentence: a file, the config, the compile, the connection, or the command's own error.
pub fn run(call: &CallArgs) -> Result<Vec<String>, String> {
    let path = host::config_path_from_args(&call.rest)?;
    let config: Config = host::load_config(&path).map_err(|e| e.to_string())?;
    let spec = host::spec_from(&config, SERVICE_USER.to_string()).map_err(|e| e.to_string())?;
    let target = host::target_of(&spec);

    let limits = Limits::from(&config.recipes);
    let schema = parse_schema(&read_file(&call.schema_file)?).map_err(|e| e.to_string())?;
    let recipe =
        parse_recipe(&read_file(&call.recipe_file)?, &limits).map_err(|e| e.to_string())?;
    let plan = compile(&schema, &recipe, &[], &limits).map_err(|e| e.to_string())?;

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let request_id = match &call.request_id {
        Some(id) => id.clone(),
        None => format!(
            "call-{}-{}",
            clock.now().as_unix_millis(),
            std::process::id()
        ),
    };
    let command = Command {
        login_id: LoginId::new(LOCAL_ADMIN).map_err(|e| e.to_string())?,
        request_id: RequestId::new(request_id.clone()).map_err(|e| e.to_string())?,
        requested_at: clock.now(),
        version: RecipeVersion::new(1),
        version_named: false,
        subject: SubjectId::new(call.subject.clone()).map_err(|e| e.to_string())?,
        counterparty: call
            .counterparty
            .as_ref()
            .map(|c| SubjectId::new(c.clone()).map_err(|e| e.to_string()))
            .transpose()?,
        args: values(&plan.args, &call.args)?,
        retention: Retention::Window,
        plan: Arc::new(plan),
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?;
    let one_writer = PoolConfig {
        recipe_writers: 1,
        readers: 0,
        sql_sessions: 0,
        admin: 0,
        reserved_backup: 0,
    };
    let finished = runtime.block_on(async {
        let pools = Arc::new(
            Pools::connect(&target, &one_writer)
                .await
                .map_err(|e| format!("cannot connect: {e}"))?,
        );
        verify_synchronous_commit(&pools, config.durability).await?;
        let exec = Executor::new(pools, clock, ExecConfig::from_config(&config));
        exec.run(&command).await.map_err(|e| e.to_string())
    })?;
    Ok(vec![render(&request_id, &finished.outcome)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn plan_arg(name: &str, ty: &str) -> PlanArg {
        PlanArg {
            name: name.into(),
            ty: ty.into(),
            min: None,
            max: None,
        }
    }

    #[test]
    fn a_full_command_line_is_read() {
        let c = parse(&args(&[
            "spend.toml",
            "--schema",
            "schema.toml",
            "--subject",
            "u1",
            "--counterparty",
            "u2",
            "--arg",
            "amount=5",
            "--arg",
            "note=hi=there",
            "--request-id",
            "r1",
            "--config",
            "/etc/x.toml",
        ]))
        .expect("parses");
        assert_eq!(c.recipe_file, PathBuf::from("spend.toml"));
        assert_eq!(c.schema_file, PathBuf::from("schema.toml"));
        assert_eq!(
            (c.subject.as_str(), c.counterparty.as_deref()),
            ("u1", Some("u2"))
        );
        assert_eq!(
            c.args,
            [
                ("amount".into(), "5".into()),
                ("note".into(), "hi=there".into())
            ]
        );
        assert_eq!(c.request_id.as_deref(), Some("r1"));
        assert_eq!(c.rest, args(&["--config", "/etc/x.toml"]));
    }

    #[test]
    fn what_is_missing_or_unknown_is_named() {
        for (bad, word) in [
            (args(&[]), "recipe file"),
            (args(&["--schema", "s"]), "recipe file"),
            (args(&["r.toml", "--subject", "u"]), "--schema"),
            (args(&["r.toml", "--schema", "s"]), "--subject"),
            (
                args(&["r.toml", "--schema", "s", "--subject"]),
                "needs a value",
            ),
            (
                args(&["r.toml", "--schema", "s", "--subject", "u", "--arg", "x"]),
                "name=value",
            ),
            (
                args(&["r.toml", "--schema", "s", "--subject", "u", "--nope"]),
                "--nope",
            ),
        ] {
            let e = parse(&bad).expect_err(word);
            assert!(e.contains(word), "{bad:?}: {e}");
        }
    }

    #[test]
    fn values_follow_the_plans_types() {
        assert_eq!(value_of(&plan_arg("a", "i64"), "-7"), Ok(Value::I64(-7)));
        assert_eq!(
            value_of(&plan_arg("a", "bool"), "true"),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            value_of(&plan_arg("a", "decimal(18,2)"), "1.5"),
            Ok(Value::Dec {
                value: 150,
                scale: 2
            })
        );
        assert_eq!(
            value_of(&plan_arg("a", "text(8)"), "hi"),
            Ok(Value::Text("hi".into()))
        );
        assert_eq!(
            value_of(&plan_arg("a", "bytes(4)"), "0aff"),
            Ok(Value::Bytes(vec![10, 255]))
        );
        assert_eq!(
            value_of(&plan_arg("a", "timestamp"), "12"),
            Ok(Value::Timestamp(12))
        );
        for (ty, text) in [
            ("i64", "x"),
            ("i64", "1.5"),
            ("bool", "yes"),
            ("bytes(4)", "0a f"),
            ("bytes(4)", "zz"),
            ("decimal(18,2)", "abc"),
        ] {
            let e = value_of(&plan_arg("a", ty), text).expect_err(text);
            assert!(e.contains("`a`"), "{e}");
        }
    }

    #[test]
    fn an_unknown_or_repeated_argument_is_an_error_and_a_missing_one_is_left_to_the_writer() {
        let plan = [plan_arg("amount", "i64")];
        let one = [("amount".to_string(), "5".to_string())];
        assert_eq!(values(&plan, &one).expect("ok")["amount"], Value::I64(5));
        assert!(values(&plan, &[]).expect("none given").is_empty());
        let e = values(&plan, &[("nope".into(), "1".into())]).expect_err("unknown");
        assert!(e.contains("`nope`"));
        let twice = [one[0].clone(), one[0].clone()];
        assert!(values(&plan, &twice).expect_err("twice").contains("twice"));
    }

    #[test]
    fn the_outcome_is_one_line_of_json_with_the_stored_result_as_is() {
        let done = Outcome {
            status: Status::Done,
            result: "{\"values\":{\"balance\":\"95\"}}".into(),
            replayed: false,
        };
        assert_eq!(
            render("r1", &done),
            "{\"request_id\":\"r1\",\"status\":\"done\",\"replayed\":false,\"result\":{\"values\":{\"balance\":\"95\"}}}"
        );
        let failed = Outcome {
            status: Status::Failed {
                error: "insufficient_funds".into(),
            },
            result: "{\"error\":\"insufficient_funds\"}".into(),
            replayed: true,
        };
        assert_eq!(
            render("r2", &failed),
            "{\"request_id\":\"r2\",\"status\":\"failed\",\"error\":\"insufficient_funds\",\"replayed\":true,\"result\":{\"error\":\"insufficient_funds\"}}"
        );
    }
}
