# Spec: Postgres access and internal tables (M2)

Spec v0.2 · 2026-11-22 · Covers milestone M2 of `docs/project-structure-plan.md` (crate `bhdata-pg`). Section numbers marked "design" refer to `bighearthdata-design-v1.0.md`. Builds on `docs/specs/engine-manager.md`.

Plan acceptance for M2: **roles, internal tables and narrow functions exist; connection budget checked; error mapping tested.**

M2 is built in **three slices**, each ending with `scripts/check.sh` clean and a zip:

| Slice | What | Needs a real PostgreSQL |
|---|---|---|
| A | Error mapping, connection budget, role and SQL text (pure functions), config | No |
| B | Connections and pools, bootstrap (database, roles, schemas, internal tables, functions), engine changes for login | Yes (tests are `--ignored`, as in M1) |
| C | Wire bootstrap and budget check into the host, then the clean-VM run | Yes |

## 1. Decisions in this spec (change any of them)

1. **`tokio-postgres` for the driver and `deadpool-postgres` for the pools** (decided: use existing, maintained crates, not a pool of our own). This makes `tokio` the project's async runtime from M2 on; M5's gateway uses the same one. Each role gets its own `deadpool` pool with a fixed size and a short wait timeout. Nothing outside `bhdata-pg` sees the driver types. The licence check (`cargo deny`) has to pass for the new crates; if it flags one, it is added to `deny.toml` only after checking it is compatible with Apache-2.0. Until the server itself is async (M5), the host runs the bootstrap on a small private runtime it builds for that one call. Slice A adds no driver dependency: it only deals in SQLSTATE codes and SQL text.
2. **Connections use the Unix socket the engine already makes; no passwords, no network** (decided: the best-fit option). Each pool logs in as its own database role. This needs one change in M1 code: `pg_hba.conf` gets `local all all peer map=bhdata` and a new `pg_ident.conf` maps the service user's operating-system name to exactly the roles below (decision 3). Without this, every connection would be the superuser and the restricted roles would only be a label. Be clear about what this does and does not give: a bug or injection that reaches the recipe pool is held to the recipe role's rights, but anyone who controls the whole server process can log in as any of the roles, since they all map from the same operating-system user. Process isolation is not claimed.
3. **Four login roles, one NOLOGIN owner.**
   - `bhdata_recipe`: recipe writers (design section 9, "Locked down"). Rights on declared recipe tables in schema `app` only, plus `EXECUTE` on the narrow functions in decision 6. No rights on `bhdata_internal` tables.
   - `bhdata_sql`: SQL mode (design section 10c). Read on `app`; write only in schema `sql`; nothing on `bhdata_internal`. Created now, unused until M12.
   - `bhdata_reader`: reads (primary now, replicas at M13). Read on `app`, and `EXECUTE` on read-only functions. Nothing on `bhdata_internal` tables.
   - `bhdata_admin`: the data layer's own work: migrations of internal tables, creating and altering tables in `app` and `sql` at registration, backups. Not a superuser.
   - `bhdata_owner` (NOLOGIN): owns the three schemas, every object in `bhdata_internal` and every narrow function. Nobody logs in as it; `bhdata_admin` is a member that may `SET ROLE` to it (not inherited), which is how the internal migrations run without the superuser. The functions run with its rights (`SECURITY DEFINER`), which is how the recipe role can use the request table without having any right on it. Tables in `app` and `sql` are created by `bhdata_admin` at registration (M3) and owned by it; default privileges give the reader and SQL roles read on `app`, and the SQL role its rights on `sql`. (v0.2 correction: v0.1 said the owner owned these tables too.)
   - The service user itself stays the superuser. The server uses it only to bootstrap (decision 5), never in a pool.
4. **One database, `bhdata`, with three schemas.** `bhdata_internal` (requests, change log, meta), `app` (recipe-managed tables), `sql` (SQL-mode tables). The database is created by the superuser at bootstrap, owned by `bhdata_owner`. `public` schema rights are revoked from `PUBLIC`; `CREATE` on the database is revoked from `PUBLIC`; extensions cannot be created by any non-superuser role. Every function pins its `search_path`.
5. **Bootstrap is one idempotent step at server start**, run as the superuser over the socket: create roles, database, schemas, grants; then run the internal migrations as `bhdata_admin`. Running it again changes nothing. A role or schema that exists with the wrong attributes is an error that names it, not something silently fixed. Rights are re-asserted every start (granted and revoked from a fixed list), so a manual change does not survive a restart.
6. **Internal tables and narrow functions at M2 are the minimum the write path (M4) needs.** The exact columns are in section 3. The recipe role gets three functions: claim a request, finish a request, append a change-log entry. M4 may add a migration if the real write path needs a column; migrations are numbered and recorded in `bhdata_internal.meta`, never edited after release.
7. **Internal migrations are append-only SQL**, one numbered statement list per version, each applied in its own transaction together with its row in `meta`. An unknown newer version in `meta` (the data is from a newer release) stops start-up with a clear error; it is never downgraded.
8. **Error mapping works on the SQLSTATE code**, never on message text (section 4). Each mapped error says whether the command may be retried, and which `bhdata_types::ErrorKind` it becomes if it reaches a client. Anything not listed is `Internal`, with the code kept for the log.
9. **Connection budget** (section 5): the sum of the pools' caps plus the reserved connections must fit inside `max_connections`; checked at start, and the server refuses to start if it does not. `bhdata-types` gains a `[pools]` config section with defaults, so nobody sets these by hand.
10. **Timeouts are set per role, in the database**, so they hold even if a pool is misused: `lock_timeout`, `statement_timeout`, `idle_in_transaction_session_timeout` (values in section 6). The recipe role gets short ones; SQL mode gets its own at M12.
11. **No connection is opened by `bhdata-engine`'s own supervision**; it still uses `pg_isready`. Only the bootstrap and the pools use the driver. The bootstrap is called from `host::run`, so the dependency the plan already allows (`bhdata-engine` to `bhdata-pg`) is the only edge used.
12. **Where the real-PostgreSQL tests live.** `bhdata-pg` may depend only on `bhdata-types`, and a test there cannot start an instance without the engine. So the ignored tests that need a server live in `crates/bhdata-engine/tests/pg_access.rs`, which already can use both crates. `bhdata-pg` itself holds the pure-function tests.

## 2. Config (added to `bhdata-types`)

New section `[pools]`, all defaults, unknown keys refused like the rest:

| Key | Default | Meaning |
|---|---|---|
| `recipe_writers` | 16 | Connections of `bhdata_recipe` |
| `readers` | 8 | Connections of `bhdata_reader` |
| `sql_sessions` | 8 | Cap for `bhdata_sql` (design section 10c); unused until M12 |
| `admin` | 2 | Connections of `bhdata_admin` |
| `reserved_backup` | 4 | Held back for backups and replication (design section 10c) |

Defaults total 38. With `superuser_reserved_connections` (PostgreSQL's own, 3, set by the engine) and `max_connections = 100`, that leaves 59 spare. The numbers are placeholders until the M11 load runs show what a real machine needs; the budget check is what must not change.

## 3. Internal tables (schema `bhdata_internal`)

- `meta(key text primary key, value text not null)`: `schema_version`, `shard_id`, `created_at`.
- `requests(login_id text, request_id text, payload_hash bytea not null, recipe text, recipe_version int, status text not null, error_kind text, result bytea, created_at timestamptz not null default now(), primary key (login_id, request_id))`. Status is `running`, `done` or `failed`. The id lengths match `bhdata-types` (`MAX_LOGIN_LEN` 64, `MAX_ID_LEN` 128), enforced by a check constraint.
- `change_log(seq bigint generated always as identity primary key, request_login text, request_id text, subject text not null, table_name text not null, op text not null, row_key text not null, changed_at timestamptz not null default now())`. Written in the same commit as the data change (design section 4). Payload columns arrive with M4/M7 when their shape is known.

Narrow functions (owned by `bhdata_owner`, `SECURITY DEFINER`, fixed `search_path`, `EXECUTE` for `bhdata_recipe` only):

- `claim_request(login, request_id, payload_hash, recipe, recipe_version)` returns one of: `new` (row inserted as `running`), `same` with the stored status and result (an identical retry), or `conflict` (same id, different hash: becomes `request_id_conflict`). A concurrent identical request waits on the row lock until the first commits, so both get one result.
- `finish_request(login, request_id, status, error_kind, result)` sets the outcome. Refuses a request not in `running`, and refuses to change a finished one.
- `append_change(login, request_id, subject, table_name, op, row_key)` inserts a change-log row, only for a request that is `running`.

Nothing deletes from these tables at M2. Expiry of old request records (default 24 hours, design section 4) is a later job, so the function for it is out of scope here.

## 4. Error mapping

`bhdata-pg::errors` turns a driver error into `PgError { kind, retry, code }` by SQLSTATE:

| SQLSTATE | Kind | Retry the command | Becomes |
|---|---|---|---|
| `40P01` deadlock detected | `Deadlock` | yes, limited times (design section 9) | internal, unless retries run out: `overloaded` |
| `40001` serialization failure | `Serialization` | yes, limited times | as above |
| `23505` unique violation | `UniqueViolation` | no | the caller decides (`already_exists`, request conflict) |
| `23503`, `23502`, `23514` foreign key, not null, check | `Constraint` | no | `bad_arguments` or internal, by where it came from |
| `55P03` lock not available | `LockTimeout` | no | `overloaded` with a retry hint |
| `57014` query canceled (statement timeout) | `StatementTimeout` | no | `overloaded` |
| `53300`, `53400` too many connections, configuration limit | `TooManyConnections` | no | `overloaded` |
| `53100`, `53200` disk full, out of memory | `Resources` | no | `overloaded`, and an alarm |
| `57P01`, `57P02`, `57P03`, class `08` | `Unavailable` | after reconnect | `overloaded` while the engine manager restarts it |
| `25006` read-only transaction, `42501` insufficient privilege | `Forbidden` | no | `internal`: it means a bug or a lockdown test hit |
| `BH001` (our own code, raised by the narrow functions when a request is not `running`) | `Refused` | no | `internal`: the caller broke the request protocol |
| anything else | `Other` | no | `internal` |

A closed or reset connection with no SQLSTATE is `Unavailable`. The table is one `match` with a test per row, and a property test that no input panics.

## 5. Connection budget

`budget = recipe_writers + readers + sql_sessions + admin + reserved_backup + superuser_reserved_connections`, and it must be at most `max_connections`. `bhdata_pg::conn_budget::check(pools, max_connections, superuser_reserved)` returns the sum, or an error that names every term and the limit ("pools use 38 + 3 reserved = 41 of 100: ok" or "… 130 of 100: lower recipe_writers or raise engine.max_connections"). The engine reads `superuser_reserved_connections` from what it wrote into the settings, not a second copy of the number: one constant, used by both.

## 6. Per-role timeouts (defaults; set with `ALTER ROLE … SET`)

| Role | `lock_timeout` | `statement_timeout` | `idle_in_transaction_session_timeout` |
|---|---|---|---|
| `bhdata_recipe` | 2 s | 5 s | 10 s |
| `bhdata_reader` | 1 s | 5 s | 5 s |
| `bhdata_sql` | 2 s | 30 s | 30 s |
| `bhdata_admin` | 10 s | 10 min | 1 min |

Recipe bounds (statement count, time limit) are checked at registration in M3; these are the backstop in the database.

## 7. Pools

`pool.rs`: one `deadpool-postgres` pool per role, with `max_size` equal to its cap and a wait timeout of 250 ms, so that a caller finds out quickly that nothing is free and turns it into `overloaded` (no unbounded waiting, no queue here: the bounded write queue is the writer's job in M4). A connection that failed with `Unavailable`, or was returned in the middle of a transaction, is discarded and replaced, never reused. The pools are separate, so the SQL pool can never take a recipe writer's connection.

## 8. What changes outside `bhdata-pg`

- `bhdata-types`: `[pools]` config and its validation.
- `bhdata-engine`: `render_hba` and a new `render_ident` (roles from the one list in `bhdata-pg`), applied at every start with the other managed files; `host::run` calls bootstrap and the budget check after the instance is up; `bhdatactl status` adds the schema version and the pool budget line.
- `deps.allow`: no new workspace edges. `Cargo.toml` workspace (slice B): `tokio`, `tokio-postgres` and `deadpool-postgres`, each pinned to its major (0.x: minor) line.

## 9. What is checked where

| Check | Sandbox | Your machine |
|---|---|---|
| Error table, budget arithmetic, SQL text for roles/grants/functions, migration order | Unit tests | `scripts/check.sh` |
| Bootstrap on a real server: roles exist with the right attributes; second run changes nothing; wrong-attribute role is reported | `--ignored` tests on the PostgreSQL 16 in the sandbox | Same tests on 17 |
| Lockdown: the recipe role cannot read or write internal tables directly, cannot create objects, cannot load an extension; can only use the three functions | `--ignored` tests | Same on 17 |
| `claim_request` semantics: new, same, conflict, concurrent identical requests | `--ignored` tests | Same on 17 |
| Real error mapping: provoke a deadlock, unique violation, lock timeout, statement timeout and check the mapped kind | `--ignored` tests | Same on 17 |
| Install and start from nothing, then bootstrap inside the service, restart, reboot | Not possible | Clean-VM run (slice C) |
| `cargo deny`, `cargo audit` for the new crates | Not possible | `scripts/check.sh` |

## 10. Left out of M2

Query execution helpers, prepared-statement caching and row decoding (they belong with the compiler and writer, M3 and M4); TLS (the socket is local); read replicas and their pools (M13); expiry of request records (M4 or later); the Transaction DB (M15).

## 11. Resolved questions (2026-11-22)

1. Driver: `tokio-postgres` with `deadpool-postgres`, not a hand-made pool ("don't reinvent wheels").
2. Login: role per pool through the `pg_ident` map ("go with what's the best").
3. Pool defaults: 16, 8, 8, 2, 4 stay as placeholders until M11.
4. Names: `bhdata`, `bhdata_internal`, `app`, `sql`, unchanged.
