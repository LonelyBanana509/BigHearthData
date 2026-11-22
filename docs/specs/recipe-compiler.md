# Recipe compiler and registry (v0.4)

Status: decisions confirmed by the owner (section 8). Written before M3. Input: the format in `recipe-format.md`. Crates: `bhdata-schema` (tables), `bhdata-recipe` (compiler and plan), `bhdata-control` (durable registry). Design sections 5, 6 and 9.

## 1. What it does

`compile(schema, recipe) -> Result<Plan, Rejection>`, a pure function: no database, no clock, no randomness. The same input always gives the same plan.

A **Plan** holds: name and version; subject table (and counterparty table if any); argument names, types and bounds; the **lane**; the steps; named errors; the tables and columns it reads and writes; the savepoint boundary; its bounds (step count, rows touched, time limit). It serialises to canonical JSON, which is what the registry stores.

**Limits are per run.** A recipe's bounds (steps, rows, result size) describe one run on one subject. Work over many subjects (interest for every user) is not a recipe that scans: it is a **batch run**, an admin action that runs a named recipe once per subject, each run a separate command with its own request id derived from the batch id and the subject id (as import does for file rows), so it is idempotent, resumable and one-shard per run. The batch action is not part of M3; it gets its own spec before the write path (M4) is final, because it needs the request records.

## 2. Checks, in order (first failure is the rejection)

| Code | Rejected when |
|---|---|
| `parse` | Not valid TOML, an unknown key, a wrong value type |
| `name` | A name breaks the rules, or the recipe name is reserved |
| `unknown_table`, `unknown_column` | It names something the schema lacks, or a global table is written, or the `sql` schema is named |
| `not_by_key` | A step reaches rows other than by the subject's id (or the counterparty's id): a scan, a lookup by another column |
| `type` | An expression's type is wrong, or an argument is used before it exists |
| `rounding` | A `decimal` result has more digits than its column's scale and is not wrapped in `round`, or `/` is used outside `round` |
| `bounds` | A written value can leave the column's `min`/`max` with no named error for that side |
| `too_big` | Over a configured limit (default 16 steps, a `list` limit of 100, 32 result values) |
| `error_name` | A named error is malformed, or reuses a built-in name with another meaning |
| `lane` | The lane cannot be decided (section 3) |
| `derived` | A `read_derived` names an undeclared store, window or staleness |

The rejection carries the code, the step number and a short plain sentence. Nothing is "fixed" by the compiler.

## 3. Lanes

- **One shard**: every step is keyed by the subject's id.
- **Two or more shards**: the recipe declares a `counterparty`, and every step is keyed by either the subject or the counterparty (never a value read from a row). Steps are split per key; each key's steps form one per-shard step list, with the order kept. At run time the planner picks one commit or a saga from where the two ids route.
- **All shards**: only a `read_derived`.

Anything else is `lane`.

## 4. The plan's SQL

Each step compiles to one fixed template with `$n` parameters; identifiers come only from the validated schema, never from input. The compiler outputs the statement text per step, and the exact **grants** the recipe role needs (`SELECT`, `INSERT`, `UPDATE`, each on named columns of named tables, never on a whole table). The union of grants over all registered versions is what `bhdata-pg` applies to `bhdata_recipe`; it never gets more. Registering a recipe is not complete until its grants are applied.

## 5. Registry (control plane)

- **Storage.** Schema `bhdata_control` in the shard database, owned by `bhdata_owner` and made by internal migration 2 (the same bootstrap path as M2). Tables: `schema_versions` (number, hash, source, applied_at), `recipes` (name, version, content_hash, source, plan JSON, status `active`/`retired`, registered_at, registered_by). A registered row is never updated except `status`.
- **Write rule.** One transaction per registration: take an advisory lock on the name, compute the next version, insert, apply grants. A repeat of identical content finds its hash and returns the existing version ("already registered as spend@3, unchanged"), so a retried or repeated deploy script is safe and makes no duplicate version.
- **Reads.** The planner loads all active plans at start and refreshes by registry version (M5). M3 gives `list`, `get(name, version)`, and `latest(name)`.
- **Callers.** `bhdatactl schema apply <file>`, `bhdatactl recipe register <file>`, `bhdatactl recipe list`. Permission checks arrive with logins (M5); at M3 they run as the local admin only.
- **Errors to clients.** A rejection is `bad_arguments` with the code and sentence; an unknown name or version is `unknown_recipe`.

## 6. Tests

1. Table-driven accept and reject cases: one file per rejection code, one per lane.
2. Frozen known-answer tests: plan JSON and content hash for the three starter recipes (`spend`, `give`, `daily`), so a plan never changes by accident.
3. Property tests: compile is deterministic; reordering keys, spacing or comments never changes the hash.
4. Real PostgreSQL (`--ignored`): a schema becomes tables; registered versions survive a server restart; identical registration is idempotent; the recipe role can do exactly what the plan's grants say and nothing more.

## 7. Build order

- **Slice A** `bhdata-schema`: model, parse, validate, DDL, diff. Pure.
- **Slice B** `bhdata-recipe`: format, parse, compile, lanes, plan, version rules. Pure.
- **Slice C** `bhdata-control` and the bootstrap: migration 2, registry, grants, `bhdatactl` commands, real-server tests.

## 8. Decided

1. Registry tables in a separate `bhdata_control` schema.
2. Grants limited to named columns.
3. Identical re-registration succeeds and returns the same version.
4. The three starter recipes (`spend`, `give`, `daily`) are the frozen examples, with `add_interest` as a fourth for the `decimal` rules.
5. Limits are config settings (`[recipes]`), checked at registration.

## 9. Rules fixed while building the checks (slice B2a, v0.3)

The table in section 2 did not say these; the code and its tests now do.

1. **Order.** Tables, then columns, then keys, each over the whole recipe; then step by step the types, rounding and bounds; then the result, the error names, and last the lane. The first failure is the rejection.
2. **Keys.** `read`, `add`, `set` reach only the row of the subject (or counterparty) table itself. `insert` and `list` reach only a child table whose owner is that subject table. Anything else is `not_by_key`, including a `read` of a global table: recipes cannot read global tables yet (see open question below).
3. **`add` and bounds.** The column is already inside `min`/`max`, so only the side the amount can push across needs a named error: `below` if `by` can be negative and the column has a `min`, `above` if `by` can be positive and it has a `max`. The range of `by` comes from the arguments' bounds and the columns' bounds, as exact arithmetic.
4. **`set` and `insert` have no named error,** so the value must provably stay inside the column's `min`/`max`, else `bounds`.
5. **`insert` names every required column.** A column that is not nullable and has no default must be in `set`; else `type`.
6. **Rounding.** A decimal stored in a column with fewer digits, or in an `i64` column, is `rounding`. `/` is allowed only as a direct argument of `round`, and never by a literal zero.
7. **Names.** Arguments, values from `read` (`to.<column>` for the counterparty) and `list` names share one namespace; a clash is `type`. A value is usable only after the step that reads it.
8. **Built-in error names.** `overflow` only as `above`; `insufficient_funds` only as `below` or in a `check`; every other built-in name is refused (`error_name`). Recipe-defined names are free.
9. **Result size** is estimated as: `i64` and `timestamp` 8 bytes, `decimal` 16, `bool` 1, `text` 4 bytes per character of `max_len`, `bytes` its `max_len`; a `list` is its row size times its `limit`. Over the limit is `too_big`.
10. **Derived stores** are a stub (`DerivedStore`: name, windows, freshest promise in ms) until the derived stores exist. A `read_derived` is the whole recipe: no other step, no counterparty.

**Open question.** Decision 2 means a recipe cannot look up a price in a global table such as `shop`. Allowing a `read` of a global table by an argument key (a value the caller passes, never one read from a row) fits the "reached by key" rule and the global tables' purpose; it is left out of B2a until you decide.

## 10. The plan (slice B2b, v0.4)

`compile(schema, recipe, derived, limits)` runs the checks of section 2 and returns a `Plan`. Code: `plan.rs` (the plan and its builder) and `canon.rs` (canonical JSON and the hash). Frozen examples: `crates/bhdata-recipe/tests/data/` (`spend`, `give`, `daily`, `add_interest` and `history`, each with its `.plan.json` and `.hash`; `BHDATA_BLESS=1` rewrites them, and the diff is read before it is kept).

1. **Canonical JSON.** Keys sorted, no whitespace, integers only, strings escaped one way. The plan carries `format: 1`. The hash is SHA-256 of that text as 64 lower-case hex digits. The version is not in the plan: the registry numbers a plan, and the hash says whether it is new. Reordering keys, spacing and comments never changes the hash (property test).
2. **The writer evaluates expressions.** It does so in Rust with exact arithmetic, so the SQL has no arithmetic of its own. Every value is a bound parameter: `$1` is the id of the row's owner, `$2`... are the step's `params`, each with the type to send (decimals as text, timestamps as whole microseconds). Identifiers in the text come only from the validated schema.
3. **`add`** is one `UPDATE` whose `WHERE` holds only the bounds that have a named error. After a miss, `miss_sql` locks and reads the row; the writer picks `below` or `above` by exact comparison, and runs the `UPDATE` again only if neither applies.
4. **`read`** takes `FOR UPDATE` when a later step writes the same row (same key, same table).
5. **`insert`** takes a transaction advisory lock on (table, owner) in a statement of its own, once per owner and table in a plan, and then writes `seq` as the owner's highest plus one. The lock needs no table privilege, so the recipe role gets no `UPDATE` right on the owner row. Without the lock, two concurrent inserts for one owner collide on the key (checked on PostgreSQL 16).
6. **`ensure`** writes a start value into every non-nullable column: the allowed value nearest to zero (`min` if it is above zero, `max` if it is below), and `''`, `false`, the epoch or empty bytes for the other types. Nullable columns start `NULL`. `ON CONFLICT DO NOTHING` leaves an existing row alone.
7. **Results** come from the `read` or `list` step that produced them, or from one final read of the subject row (`result_fetch`) for plain columns.
8. **Step lists per key.** `shards` holds the step numbers of each key in order, with the savepoint before the first. A `check` is in none: where it runs is decided with the planner (M5).
9. **Grants** are exact named columns per table and right (`Grants::statements`); `Grants::merge` gives the union over versions. A table is never granted whole, and no plan needs `DELETE`.
10. **Bounds.** Steps, rows touched or returned (a `list` counts its limit), and a time limit of 2,000 ms enforced by the writer.

Checked against a real PostgreSQL 16 (outside the test suite, to be made a `--ignored` test in slice C): every starter statement, the miss paths on both sides, decimals, the exact-maximum edge, the denied rights of the recipe role, and the insert race. Not yet done: PostgreSQL 17, and the registry, migration 2 and `bhdatactl` (slice C).

