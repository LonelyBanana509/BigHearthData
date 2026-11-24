//! The thin handler (wire-protocol spec, section 1): what the gateway calls for each request.
//!
//! It does what `bhdatactl call` does, for a client instead of a file: find the registered plan,
//! convert the JSON arguments by the plan's argument types, build a [`Command`] and give it to the
//! write queue. The planner replaces it in M6; the wire does not change.
//!
//! Plans come from a [`PlanSource`]. The real one ([`PgPlans`]) rebuilds a plan from the registry
//! and keeps it for a second, so a recipe registered a moment ago is seen without a query per
//! request.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bhdata_api::handler::{Call, Caller, Reply, RequestHandler};
use bhdata_api::recipe::canon::Json;
use bhdata_api::recipe::eval::{Value, WireArg};
use bhdata_api::shard::{Command, Retention, ShardBackend};
use bhdata_api::types::{Clock, Error, RecipeVersion};
use bhdata_control::error::ControlError;
use bhdata_control::registry;
use bhdata_pg::pool::Pools;
use bhdata_recipe::Plan;

/// How long a plan found in the registry is trusted before it is looked up again.
pub const PLAN_TTL: Duration = Duration::from_secs(1);

/// A plan and the version it is.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The version that will run.
    pub version: RecipeVersion,
    /// The compiled recipe.
    pub plan: Arc<Plan>,
}

/// Where plans come from.
pub trait PlanSource: Send + Sync + 'static {
    /// The plan of `name` at `version`, or of the newest active version when `version` is `None`.
    ///
    /// # Errors
    /// `unknown_recipe` when there is no such version (or it is retired); `internal` when the
    /// registry cannot be read or no longer rebuilds.
    fn plan(
        &self,
        name: &str,
        version: Option<RecipeVersion>,
    ) -> impl Future<Output = Result<Entry, Error>> + Send;
}

type CacheKey = (String, Option<u32>);

/// The registry, through the admin pool, with a one second cache.
pub struct PgPlans {
    pools: Arc<Pools>,
    clock: Arc<dyn Clock>,
    cache: Mutex<HashMap<CacheKey, (Duration, Entry)>>,
}

impl PgPlans {
    /// Plans read through `pools` (only the admin pool is used).
    #[must_use]
    pub fn new(pools: Arc<Pools>, clock: Arc<dyn Clock>) -> Self {
        PgPlans {
            pools,
            clock,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, key: &CacheKey) -> Option<Entry> {
        let now = self.clock.monotonic();
        let cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        match cache.get(key) {
            Some((at, entry)) if now.saturating_sub(*at) < PLAN_TTL => Some(entry.clone()),
            _ => None,
        }
    }

    fn remember(&self, key: CacheKey, entry: &Entry) {
        let now = self.clock.monotonic();
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        // Entries older than the TTL are dead weight; dropping them here keeps the map to the
        // recipes asked for in the last second.
        cache.retain(|_, (at, _)| now.saturating_sub(*at) < PLAN_TTL);
        cache.insert(key, (now, entry.clone()));
    }
}

fn internal(what: &str, e: &impl std::fmt::Display) -> Error {
    Error::Internal(format!("{what}: {e}"))
}

impl PlanSource for PgPlans {
    async fn plan(&self, name: &str, version: Option<RecipeVersion>) -> Result<Entry, Error> {
        let key = (name.to_string(), version.map(RecipeVersion::get));
        if let Some(entry) = self.cached(&key) {
            return Ok(entry);
        }
        let conn = self
            .pools
            .admin()
            .await
            .map_err(|e| internal("plan source", &e))?;
        let loaded = match registry::load_plan(&**conn, name, key.1).await {
            Ok(loaded) => loaded,
            Err(ControlError::UnknownRecipe { .. }) => return Err(Error::UnknownRecipe),
            Err(e) => return Err(internal("plan source", &e)),
        };
        // A retired version takes no new requests (plan section 6, retire rules).
        if loaded.status != "active" {
            return Err(Error::UnknownRecipe);
        }
        let entry = Entry {
            version: RecipeVersion::new(loaded.version),
            plan: Arc::new(loaded.plan),
        };
        self.remember(key, &entry);
        Ok(entry)
    }
}

/// The wire form of one argument, for [`Value::from_wire_arg`].
fn wire_arg(json: &Json) -> WireArg<'_> {
    match json {
        Json::Int(n) => WireArg::Int(*n),
        Json::Str(s) => WireArg::Str(s),
        Json::Bool(b) => WireArg::Bool(*b),
        Json::Null => WireArg::Other("null"),
        Json::Arr(_) => WireArg::Other("a list"),
        Json::Obj(_) => WireArg::Other("an object"),
    }
}

/// The request's arguments as typed values, by the plan's argument types. An argument the plan does
/// not have is refused here; a missing one is left to the writer's own check, as `bhdatactl call` does.
///
/// # Errors
/// `bad_arguments` naming the argument.
pub fn convert_args(plan: &Plan, args: &Json) -> Result<BTreeMap<String, Value>, Error> {
    let Json::Obj(map) = args else {
        return Err(Error::BadArguments(
            "the arguments must be an object".to_string(),
        ));
    };
    let mut out = BTreeMap::new();
    for (name, json) in map {
        let arg = plan.args.iter().find(|a| &a.name == name).ok_or_else(|| {
            Error::BadArguments(format!("`{name}` is not an argument of this recipe"))
        })?;
        out.insert(
            name.clone(),
            Value::from_wire_arg(name, &arg.ty, wire_arg(json))?,
        );
    }
    Ok(out)
}

/// The handler: plans in, commands out to the write queue.
pub struct ThinHandler<P, B> {
    plans: P,
    backend: B,
}

impl<P, B> ThinHandler<P, B> {
    /// A handler that finds plans in `plans` and runs commands on `backend`.
    #[must_use]
    pub fn new(plans: P, backend: B) -> Self {
        ThinHandler { plans, backend }
    }
}

impl<P: PlanSource, B: ShardBackend + 'static> RequestHandler for ThinHandler<P, B> {
    async fn handle(&self, caller: Caller, call: Call) -> Result<Reply, Error> {
        let entry = self.plans.plan(&call.recipe, call.version).await?;
        let args = convert_args(&entry.plan, &call.args)?;
        let command = Command {
            login_id: caller.login_id,
            request_id: call.request_id,
            requested_at: call.created_at,
            plan: entry.plan,
            version: entry.version,
            version_named: call.version.is_some(),
            subject: call.subject,
            counterparty: call.counterparty,
            args,
            retention: Retention::Window,
        };
        self.backend.execute(command).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bhdata_api::shard::{Outcome, Status};
    use bhdata_api::types::{LoginId, RequestId, SubjectId, Timestamp};
    use bhdata_recipe::{compile, parse_recipe, Limits};
    use bhdata_schema::parse_schema;

    use super::*;

    const SCHEMA: &str = include_str!("../../bhdata-recipe/tests/data/schema.toml");
    const SPEND: &str = include_str!("../../bhdata-recipe/tests/data/spend.toml");

    fn plan() -> Plan {
        let limits = Limits::from(&bhdata_api::types::Config::default().recipes);
        let schema = parse_schema(SCHEMA).unwrap();
        let recipe = parse_recipe(SPEND, &limits).unwrap();
        compile(&schema, &recipe, &[], &limits).unwrap()
    }

    struct Fixed(Entry);

    impl PlanSource for Fixed {
        async fn plan(&self, name: &str, version: Option<RecipeVersion>) -> Result<Entry, Error> {
            match (name, version.map(RecipeVersion::get)) {
                ("spend", None | Some(1)) => Ok(self.0.clone()),
                _ => Err(Error::UnknownRecipe),
            }
        }
    }

    /// Records what it was given, and answers `done`.
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<Command>>,
        calls: AtomicUsize,
    }

    /// The backend the handler holds; the `Recorder` stays with the test.
    struct Spy(Arc<Recorder>);

    impl ShardBackend for Spy {
        async fn execute(&self, command: Command) -> Result<Outcome, Error> {
            self.0.calls.fetch_add(1, Ordering::SeqCst);
            self.0.seen.lock().unwrap().push(command);
            Ok(Outcome {
                status: Status::Done,
                result: "{}".to_string(),
                replayed: false,
            })
        }
    }

    fn handler() -> (ThinHandler<Fixed, Spy>, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        let entry = Entry {
            version: RecipeVersion::new(1),
            plan: Arc::new(plan()),
        };
        (ThinHandler::new(Fixed(entry), Spy(Arc::clone(&rec))), rec)
    }

    fn caller() -> Caller {
        Caller {
            login_id: LoginId::new("alice").unwrap(),
        }
    }

    fn call(version: Option<u32>, args: Json) -> Call {
        Call {
            request_id: RequestId::new("req-1").unwrap(),
            created_at: Timestamp::from_unix_millis(1_700_000_000_000),
            recipe: "spend".to_string(),
            version: version.map(RecipeVersion::new),
            subject: SubjectId::new("u1").unwrap(),
            counterparty: None,
            args,
            map_version: None,
        }
    }

    fn amount(n: i64) -> Json {
        Json::obj([("amount", Json::Int(n))])
    }

    #[tokio::test]
    async fn a_request_becomes_a_command_with_the_callers_login_and_the_clients_time() {
        let (h, rec) = handler();
        h.handle(caller(), call(None, amount(5))).await.unwrap();
        let seen = rec.seen.lock().unwrap();
        let c = &seen[0];
        assert_eq!(c.login_id.as_str(), "alice");
        assert_eq!(c.request_id.as_str(), "req-1");
        assert_eq!(
            c.requested_at,
            Timestamp::from_unix_millis(1_700_000_000_000)
        );
        assert_eq!(c.subject.as_str(), "u1");
        assert_eq!(c.args["amount"], Value::I64(5));
        assert_eq!(c.retention, Retention::Window);
        assert_eq!(c.version, RecipeVersion::new(1));
        assert!(!c.version_named, "the caller asked for the latest");
    }

    #[tokio::test]
    async fn naming_the_version_is_recorded() {
        let (h, rec) = handler();
        h.handle(caller(), call(Some(1), amount(5))).await.unwrap();
        assert!(rec.seen.lock().unwrap()[0].version_named);
    }

    #[tokio::test]
    async fn an_unknown_recipe_or_version_is_refused_and_nothing_is_queued() {
        let (h, rec) = handler();
        let e = h
            .handle(caller(), call(Some(9), amount(5)))
            .await
            .unwrap_err();
        assert_eq!(e, Error::UnknownRecipe);
        let mut other = call(None, amount(5));
        other.recipe = "nope".to_string();
        assert_eq!(
            h.handle(caller(), other).await.unwrap_err(),
            Error::UnknownRecipe
        );
        assert_eq!(rec.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bad_arguments_are_named_and_nothing_is_queued() {
        let (h, rec) = handler();
        for (args, word) in [
            (Json::Int(1), "object"),
            (Json::obj([("nope", Json::Int(1))]), "`nope`"),
            (Json::obj([("amount", Json::Str("5".into()))]), "`amount`"),
            (Json::obj([("amount", Json::Null)]), "null"),
            (Json::obj([("amount", Json::Bool(true))]), "`amount`"),
            (Json::obj([("amount", Json::Arr(Vec::new()))]), "a list"),
        ] {
            let e = h.handle(caller(), call(None, args)).await.unwrap_err();
            let Error::BadArguments(text) = e else {
                panic!("{e:?}")
            };
            assert!(text.contains(word), "{word}: {text}");
        }
        assert_eq!(rec.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_missing_argument_is_left_to_the_writer() {
        let (h, rec) = handler();
        h.handle(caller(), call(None, Json::Obj(BTreeMap::new())))
            .await
            .unwrap();
        assert!(rec.seen.lock().unwrap()[0].args.is_empty());
    }

    #[tokio::test]
    async fn what_the_queue_refuses_with_comes_back_unchanged() {
        struct Full;
        impl ShardBackend for Full {
            async fn execute(&self, _c: Command) -> Result<Outcome, Error> {
                Err(Error::Overloaded {
                    retry_after: Duration::from_millis(250),
                })
            }
        }
        let entry = Entry {
            version: RecipeVersion::new(1),
            plan: Arc::new(plan()),
        };
        let h = ThinHandler::new(Fixed(entry), Full);
        let e = h.handle(caller(), call(None, amount(5))).await.unwrap_err();
        assert_eq!(
            e,
            Error::Overloaded {
                retry_after: Duration::from_millis(250)
            }
        );
    }
}
