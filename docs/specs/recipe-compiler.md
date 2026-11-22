# Recipe compiler and registry (v0.2)

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
