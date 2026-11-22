//! Plans and their hashes: the five frozen starter recipes, determinism, and what must and must
//! not change a hash. The golden files live in `tests/data/`; run with `BHDATA_BLESS=1` to rewrite
//! them after a change that is meant to alter a plan (and read the diff: a plan never changes by
//! accident).

use std::fs;
use std::path::PathBuf;

use bhdata_recipe::plan::{type_text, Grants, Source, StepPlan, RECIPE_ROLE};
use bhdata_recipe::{compile, parse_recipe, Code, DerivedStore, Lane, Limits, Plan, Rejection};
use bhdata_schema::model::{ColumnType, Schema};
use bhdata_schema::parse_schema;

fn data(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(file)
}

fn read(file: &str) -> String {
    fs::read_to_string(data(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
}

fn schema() -> Schema {
    parse_schema(&read("schema.toml")).expect("schema")
}

fn compile_text(schema: &Schema, text: &str) -> Result<Plan, Rejection> {
    let limits = Limits::default();
    let recipe = parse_recipe(text, &limits)?;
    compile(schema, &recipe, &[], &limits)
}

fn starter(name: &str) -> Plan {
    compile_text(&schema(), &read(&format!("{name}.toml"))).expect("starter recipe compiles")
}

const STARTERS: [&str; 5] = ["spend", "give", "daily", "add_interest", "history"];

#[test]
fn the_starter_plans_and_hashes_are_frozen() {
    let bless = std::env::var_os("BHDATA_BLESS").is_some();
    for name in STARTERS {
        let plan = starter(name);
        let (json, hash) = (
            format!("{}\n", plan.canonical_json()),
            format!("{}\n", plan.content_hash()),
        );
        if bless {
            fs::write(data(&format!("{name}.plan.json")), &json).expect("write golden plan");
            fs::write(data(&format!("{name}.hash")), &hash).expect("write golden hash");
            continue;
        }
        assert_eq!(
            json,
            read(&format!("{name}.plan.json")),
            "plan of {name} changed"
        );
        assert_eq!(
            hash,
            read(&format!("{name}.hash")),
            "hash of {name} changed"
        );
    }
}

#[test]
fn the_hashes_are_sha256_hex_and_all_different() {
    let hashes: Vec<String> = STARTERS.iter().map(|n| starter(n).content_hash()).collect();
    for h in &hashes {
        assert_eq!(h.len(), 64);
        assert!(h
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }
    let mut unique = hashes.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), STARTERS.len());
}

#[test]
fn compiling_twice_gives_the_same_plan_and_the_same_bytes() {
    for name in STARTERS {
        let (a, b) = (starter(name), starter(name));
        assert_eq!(a, b);
        assert_eq!(a.canonical_json(), b.canonical_json());
    }
}

// ---- what a plan contains ----

fn step_sql(plan: &Plan, n: usize) -> String {
    match &plan.steps[n - 1].plan {
        StepPlan::Ensure { sql }
        | StepPlan::Read { sql, .. }
        | StepPlan::Add { sql, .. }
        | StepPlan::Set { sql, .. }
        | StepPlan::Insert { sql, .. }
        | StepPlan::List { sql, .. } => sql.clone(),
        other => panic!("step {n} has no statement: {other:?}"),
    }
}

/// A recipe over the starter schema: `head` is the `[recipe]` table's extra lines.
fn recipe(head: &str, steps: &str, result: &str) -> String {
    format!(
        "[recipe]\nname = \"r\"\nsubject = \"users\"\n{head}\n{steps}\n[recipe.result]\nreturn = [{result}]\n"
    )
}

const ARG: &str = "args = { amount = { type = \"i64\", min = 1, max = 1_000_000 } }";

fn insert_locks(p: &Plan) -> Vec<bool> {
    p.steps
        .iter()
        .filter_map(|st| match &st.plan {
            StepPlan::Insert { lock_sql, .. } => Some(lock_sql.is_some()),
            _ => None,
        })
        .collect()
}

#[test]
fn ensure_writes_the_value_nearest_to_zero_and_leaves_nullable_columns_out() {
    let schema = parse_schema(
        r#"
[[table]]
name = "things"
kind = "subject"
columns = [
  { name = "level", type = "i64", min = 1, max = 10 },
  { name = "debt", type = "decimal", precision = 10, scale = 2, max = "-5.00" },
  { name = "free", type = "i64" },
  { name = "label", type = "text", max_len = 10 },
  { name = "flag", type = "bool" },
  { name = "seen", type = "timestamp" },
  { name = "blob", type = "bytes", max_len = 4 },
  { name = "note", type = "text", max_len = 10, nullable = true },
]
"#,
    )
    .expect("schema");
    let text = "[recipe]\nname = \"t\"\nsubject = \"things\"\n[[step]]\nop = \"ensure\"\n[recipe.result]\nreturn = []\n";
    let plan = compile_text(&schema, text).expect("compiles");
    assert_eq!(
        step_sql(&plan, 1),
        "INSERT INTO app.\"things\" (\"id\", \"blob\", \"debt\", \"flag\", \"free\", \"label\", \"level\", \"seen\") \
         VALUES ($1, ''::bytea, -5.00, false, 0, '', 1, 'epoch'::timestamptz) ON CONFLICT DO NOTHING"
    );
}

#[test]
fn add_enforces_only_the_sides_that_have_a_named_error() {
    let s = schema();
    let both = compile_text(&s, &read("spend.toml")).expect("spend");
    let StepPlan::Add {
        sql,
        below,
        above,
        miss_sql,
        ..
    } = &both.steps[1].plan
    else {
        panic!()
    };
    assert!(sql.contains(">= 0") && sql.contains("<= 1000000000"));
    assert_eq!(
        (
            below.as_ref().map(|l| l.limit.as_str()),
            above.as_ref().map(|l| l.limit.as_str())
        ),
        (Some("0"), Some("1000000000"))
    );
    assert!(miss_sql
        .as_deref()
        .is_some_and(|m| m.ends_with("FOR UPDATE")));

    let give = starter("give");
    let (
        StepPlan::Add {
            sql: low,
            above: a1,
            ..
        },
        StepPlan::Add {
            sql: high,
            below: b2,
            ..
        },
    ) = (&give.steps[3].plan, &give.steps[4].plan)
    else {
        panic!()
    };
    assert!(low.contains(">= 0") && !low.contains("<="), "{low}");
    assert!(
        high.contains("<= 1000000000") && !high.contains(">="),
        "{high}"
    );
    assert!(a1.is_none() && b2.is_none());

    // A column with no bounds and an amount that cannot go negative: a plain update, no miss path.
    let steps = "[[step]]\nop = \"ensure\"\n[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"last_claim\"\nby = \"amount\"";
    let plain = compile_text(&s, &recipe(ARG, steps, "\"last_claim\"")).expect("plain add");
    let StepPlan::Add {
        sql,
        miss_sql,
        below,
        above,
        ..
    } = &plain.steps[1].plan
    else {
        panic!()
    };
    assert_eq!(
        sql,
        "UPDATE app.\"users\" SET \"last_claim\" = \"last_claim\" + $2::bigint WHERE \"id\" = $1 RETURNING \"last_claim\""
    );
    assert!(miss_sql.is_none() && below.is_none() && above.is_none());
}

#[test]
fn decimal_arguments_and_bounds_are_written_with_their_scale() {
    let p = starter("add_interest");
    assert_eq!(p.args[0].ty, "decimal(9,6)");
    assert_eq!(
        (p.args[0].min.as_deref(), p.args[0].max.as_deref()),
        (Some("0.000000"), Some("1.000000"))
    );
    let StepPlan::Add { params, ty, .. } = &p.steps[1].plan else {
        panic!()
    };
    assert_eq!(ty, "decimal(18,2)");
    assert_eq!(params[0].expr, "round((interest * rate), 2, half_even)");
    let StepPlan::Read { sql, columns } = &p.steps[0].plan else {
        panic!()
    };
    assert!(sql.contains("\"interest\"::text"), "{sql}");
    assert_eq!(columns[0].ty, "decimal(18,2)");
}

#[test]
fn a_read_locks_its_row_only_when_the_recipe_writes_that_row_later() {
    assert!(step_sql(&starter("daily"), 2).ends_with(" FOR UPDATE"));
    assert!(step_sql(&starter("add_interest"), 1).ends_with(" FOR UPDATE"));
    assert!(
        step_sql(&starter("give"), 3).ends_with(" FOR UPDATE"),
        "the counterparty's balance is written later"
    );

    let s = schema();
    let only_read = "[[step]]\nop = \"read\"\ntable = \"users\"\ncolumns = [\"balance\"]";
    let p = compile_text(&s, &recipe("", only_read, "\"balance\"")).expect("read only");
    assert!(!step_sql(&p, 1).contains("FOR UPDATE"));

    // The counterparty's row is read, but only the subject's row is written.
    let steps = "[[step]]\nop = \"ensure\"\n[[step]]\nop = \"read\"\nof = \"counterparty\"\ntable = \"users\"\ncolumns = [\"balance\"]\n\
                 [[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"balance\"\nby = \"amount\"\nabove = \"overflow\"";
    let head = format!("counterparty = \"users\"\n{ARG}");
    let p = compile_text(&s, &recipe(&head, steps, "\"to.balance\"")).expect("cross read");
    assert!(!step_sql(&p, 2).contains("FOR UPDATE"));
}

#[test]
fn an_insert_takes_its_lock_once_per_owner_and_table() {
    let s = schema();
    let ins =
        "[[step]]\nop = \"insert\"\ntable = \"entries\"\nset = { delta = \"1\", at = \"now\" }";
    let two = format!("[[step]]\nop = \"ensure\"\n{ins}\n{ins}");
    let p = compile_text(&s, &recipe("", &two, "")).expect("two inserts");
    assert_eq!(insert_locks(&p), [true, false]);

    // The counterparty's rows are another owner, so they take their own lock.
    let cp = ins.replace("op = \"insert\"", "op = \"insert\"\nof = \"counterparty\"");
    let head = format!("counterparty = \"users\"\n{ARG}");
    let three = format!(
        "[[step]]\nop = \"ensure\"\n[[step]]\nop = \"ensure\"\nof = \"counterparty\"\n{ins}\n{cp}"
    );
    let p = compile_text(&s, &recipe(&head, &three, "")).expect("cross inserts");
    assert_eq!(insert_locks(&p), [true, true]);
}

#[test]
fn grants_are_named_columns_exactly() {
    let p = starter("spend");
    assert_eq!(
        p.grants.statements(RECIPE_ROLE),
        [
            "GRANT SELECT (\"owner_id\", \"seq\") ON TABLE app.\"entries\" TO \"bhdata_recipe\"",
            "GRANT INSERT (\"at\", \"delta\", \"owner_id\", \"seq\") ON TABLE app.\"entries\" TO \"bhdata_recipe\"",
            "GRANT SELECT (\"balance\", \"id\") ON TABLE app.\"users\" TO \"bhdata_recipe\"",
            "GRANT INSERT (\"balance\", \"id\", \"interest\", \"last_claim\") ON TABLE app.\"users\" TO \"bhdata_recipe\"",
            "GRANT UPDATE (\"balance\") ON TABLE app.\"users\" TO \"bhdata_recipe\"",
        ]
    );
    // Never a right on a column the recipes do not use, and never on a whole table.
    let mut all = Grants::default();
    for name in STARTERS {
        all.merge(&starter(name).grants);
    }
    let text = all.statements(RECIPE_ROLE).join("\n");
    assert!(!text.contains("\"name\""), "{text}");
    assert!(!text.contains("DELETE") && !text.contains("TRUNCATE"));
    assert!(
        all.statements(RECIPE_ROLE)
            .iter()
            .all(|s| s.contains(" (\"")),
        "every right names columns"
    );
    assert!(all.tables["entries"].update.is_empty());
}

#[test]
fn step_lists_per_key_keep_their_order_and_a_check_belongs_to_none() {
    let give = starter("give");
    let lists: Vec<(String, Vec<usize>)> = give
        .shards
        .iter()
        .map(|s| (format!("{:?}", s.of), s.steps.clone()))
        .collect();
    assert_eq!(
        lists,
        [
            ("Subject".to_string(), vec![1, 4, 6]),
            ("Counterparty".to_string(), vec![2, 3, 5])
        ]
    );
    assert_eq!(give.lane, Lane::TwoShards);

    let daily = starter("daily");
    assert_eq!(daily.shards.len(), 1);
    assert_eq!(daily.shards[0].steps, [1, 2, 4, 5], "step 3 is a check");
    assert!(daily.steps[2].of.is_none());
    assert_eq!(daily.steps[2].label.as_deref(), Some("once_a_day"));
}

#[test]
fn results_come_from_the_step_that_read_them_or_from_a_final_read() {
    let spend = starter("spend");
    assert_eq!(spend.result[0].source, Source::Fetch);
    assert_eq!(
        spend.result_fetch.as_ref().map(|f| f.sql.as_str()),
        Some("SELECT \"balance\" FROM app.\"users\" WHERE \"id\" = $1")
    );
    let give = starter("give");
    assert_eq!(
        give.result
            .iter()
            .map(|r| (r.name.as_str(), r.source.clone()))
            .collect::<Vec<_>>(),
        [("balance", Source::Fetch), ("to.balance", Source::Step(3))]
    );
    let history = starter("history");
    assert_eq!(history.result[0].source, Source::Step(1));
    assert!(history.result_fetch.is_none());
}

#[test]
fn a_derived_read_is_the_whole_plan_with_no_statements() {
    let store = DerivedStore {
        name: "leaderboard".into(),
        windows: vec!["weekly".into()],
        freshest_ms: 1000,
    };
    let text = "[recipe]\nname = \"top\"\nsubject = \"users\"\n[[step]]\nop = \"read_derived\"\nstore = \"leaderboard\"\nwindow = \"weekly\"\nmax_age_ms = 5000\nlimit = 50\n[recipe.result]\nreturn = []\n";
    let limits = Limits::default();
    let recipe = parse_recipe(text, &limits).expect("parses");
    let plan = compile(&schema(), &recipe, &[store], &limits).expect("compiles");
    assert_eq!(plan.lane, Lane::AllShards);
    assert!(
        plan.shards.is_empty()
            && plan.grants.tables.is_empty()
            && plan.reads.is_empty()
            && plan.writes.is_empty()
    );
    assert_eq!(plan.bounds.rows, 50);
    assert!(matches!(plan.steps[0].plan, StepPlan::ReadDerived { .. }));
}

#[test]
fn bounds_and_tables_are_counted() {
    let spend = starter("spend");
    assert_eq!(
        (spend.bounds.steps, spend.bounds.rows, spend.bounds.time_ms),
        (3, 4, 2_000)
    );
    assert_eq!(spend.reads, ["users"]);
    assert_eq!(spend.writes, ["entries", "users"]);
    assert_eq!(
        starter("history").bounds.rows,
        10,
        "a list counts its limit"
    );
}

#[test]
fn a_rejected_recipe_never_becomes_a_plan() {
    let steps = "[[step]]\nop = \"add\"\ntable = \"users\"\ncolumn = \"balance\"\nby = \"-amount\"";
    let err = compile_text(&schema(), &recipe(ARG, steps, "\"balance\"")).unwrap_err();
    assert_eq!(err.code, Code::Bounds);
}

#[test]
fn column_types_are_written_one_way() {
    assert_eq!(type_text(ColumnType::I64), "i64");
    assert_eq!(
        type_text(ColumnType::Decimal {
            precision: 18,
            scale: 2
        }),
        "decimal(18,2)"
    );
    assert_eq!(type_text(ColumnType::Text { max_len: 64 }), "text(64)");
    assert_eq!(type_text(ColumnType::Bool), "bool");
    assert_eq!(type_text(ColumnType::Timestamp), "timestamp");
    assert_eq!(type_text(ColumnType::Bytes { max_len: 16 }), "bytes(16)");
}

// ---- what changes a hash, and what must not ----

fn hash_of(text: &str) -> String {
    compile_text(&schema(), text)
        .expect("compiles")
        .content_hash()
}

#[test]
fn comments_spacing_and_key_order_do_not_change_the_hash() {
    let plain = read("spend.toml");
    let messy = r#"
# a comment up here

[recipe]
args    =   { amount = { max = 1_000_000,   min = 1, type = "i64" } }    # trailing
subject = "users"
name    = "spend"


[[step]]
   op = "ensure"

[[step]]
by     = "-amount"
column = "balance"
above  = "overflow"
below  = "insufficient_funds"
table  = "users"
op     = "add"

[[step]]
set   = { at = "now", delta = "-( amount )" }
table = "entries"
op    = "insert"

[recipe.result]
return = [ "balance" ]
"#;
    assert_eq!(hash_of(&plain), hash_of(messy));
}

#[test]
fn any_change_of_meaning_changes_the_hash() {
    let base = read("spend.toml");
    let h = hash_of(&base);
    let variants = [
        base.replace("1_000_000", "2_000_000"),
        base.replace("insufficient_funds", "too_poor"),
        base.replace("op     = \"add\"", "op     = \"add\"\nlabel  = \"take\""),
        base.replace("by     = \"-amount\"", "by     = \"-amount - 1\""),
        base.replace("min = 1,", "min = 2,"),
        base.replace("name    = \"spend\"", "name    = \"spend2\""),
    ];
    for (i, v) in variants.iter().enumerate() {
        assert_ne!(hash_of(v), h, "variant {i} did not change the hash");
    }
}

#[test]
fn the_same_recipe_under_a_changed_schema_is_a_different_plan() {
    let narrower = read("schema.toml").replace("max = 1_000_000_000", "max = 500_000_000");
    let a = compile_text(&schema(), &read("spend.toml")).expect("a");
    let b = compile_text(
        &parse_schema(&narrower).expect("schema"),
        &read("spend.toml"),
    )
    .expect("b");
    assert_ne!(a.content_hash(), b.content_hash());
}

// ---- reordering never changes the hash (property) ----

use proptest::prelude::*;

fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

fn shuffled<T>(mut items: Vec<T>, seed: &mut u64) -> Vec<T> {
    for i in (1..items.len()).rev() {
        let j = usize::try_from(next(seed) % (i as u64 + 1)).unwrap_or(0);
        items.swap(i, j);
    }
    items
}

fn noise(seed: &mut u64) -> &'static str {
    [
        "",
        "",
        "   ",
        "\n",
        "# a comment\n",
        "  # another one\n\n",
        "\n\n# x\n",
    ][usize::try_from(next(seed) % 7).unwrap_or(0)]
}

/// The same recipe, written with its keys, map entries, spacing and comments in a seeded order.
fn messy_recipe(seed: u64) -> String {
    let mut s = seed | 1;
    let s = &mut s;
    let by_fee = ["-(amount + fee)", "-(amount+fee)", "-( amount + fee )"]
        [usize::try_from(next(s) % 3).unwrap_or(0)];
    let arg = |s: &mut u64, max: &str| {
        let parts = shuffled(
            vec![
                "type = \"i64\"".to_string(),
                "min = 1".to_string(),
                format!("max = {max}"),
            ],
            s,
        );
        format!("{{ {} }}", parts.join(", "))
    };
    let args = shuffled(
        vec![
            format!("amount = {}", arg(s, "1_000_000")),
            format!("fee = {}", arg(s, "1000")),
        ],
        s,
    );

    let mut text = String::new();
    text.push_str(noise(s));
    text.push_str("[recipe]\n");
    let head = shuffled(
        vec![
            "name = \"give\"".to_string(),
            "subject = \"users\"".to_string(),
            "counterparty = \"users\"".to_string(),
            format!("args = {{ {} }}", args.join(", ")),
        ],
        s,
    );
    for line in head {
        text.push_str(noise(s));
        text.push_str(&format!("{line}\n"));
    }
    let set = shuffled(vec!["delta = \"-amount\"", "at = \"now\""], s).join(", ");
    let steps: Vec<Vec<String>> = vec![
        vec!["op = \"ensure\"".into()],
        vec!["op = \"ensure\"".into(), "of = \"counterparty\"".into()],
        vec![
            "op = \"read\"".into(),
            "of = \"counterparty\"".into(),
            "table = \"users\"".into(),
            "columns = [\"balance\"]".into(),
        ],
        vec![
            "op = \"check\"".into(),
            "label = \"enough\"".into(),
            "require = \"amount <= 1000000\"".into(),
            "error = \"too_much\"".into(),
        ],
        vec![
            "op = \"add\"".into(),
            "table = \"users\"".into(),
            "column = \"balance\"".into(),
            format!("by = \"{by_fee}\""),
            "below = \"insufficient_funds\"".into(),
        ],
        vec![
            "op = \"add\"".into(),
            "of = \"counterparty\"".into(),
            "table = \"users\"".into(),
            "column = \"balance\"".into(),
            "by = \"amount\"".into(),
            "above = \"overflow\"".into(),
        ],
        vec![
            "op = \"insert\"".into(),
            "table = \"entries\"".into(),
            format!("set = {{ {set} }}"),
        ],
        vec![
            "op = \"list\"".into(),
            "table = \"entries\"".into(),
            "columns = [\"delta\", \"at\"]".into(),
            "limit = 5".into(),
            "into = \"recent\"".into(),
        ],
    ];
    for step in steps {
        text.push_str(noise(s));
        text.push_str("[[step]]\n");
        for line in shuffled(step, s) {
            text.push_str(noise(s));
            text.push_str(&format!("{line}\n"));
        }
    }
    text.push_str(noise(s));
    text.push_str("[recipe.result]\nreturn = [\"balance\", \"to.balance\", \"recent\"]\n");
    text
}

#[test]
fn the_messy_generator_writes_a_valid_recipe() {
    let plan = compile_text(&schema(), &messy_recipe(1)).expect("the generated recipe compiles");
    assert_eq!(plan.steps.len(), 8);
    assert_eq!(plan.lane, Lane::TwoShards);
}

proptest! {
    #[test]
    fn reordering_keys_and_adding_comments_or_spacing_never_changes_the_hash(seed in any::<u64>()) {
        prop_assert_eq!(hash_of(&messy_recipe(seed)), hash_of(&messy_recipe(1)));
    }
}
