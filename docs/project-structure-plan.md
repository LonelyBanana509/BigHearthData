# BigHearthData: Project Structure Plan

Plan v0.3 · 2026-11-22 · Companion to `bighearthdata-design-v1.0.md` (the design; section numbers below refer to it)

This file says how the design becomes a Rust codebase: the workspace, the crates, the files inside them, the order of work, and how each piece is tested. It does not repeat the design.

---

## 1. Ground rules

1. **Stage 0 first.** Everything needed for one machine, one shard, one command ships before any sharding code. Later stages are designed for (traits, version fields, epochs in messages) but not built.
2. **One process at stage 0.** The server binary links every stage-0 crate and passes messages through in-process channels. Splitting into several processes later must not change crate APIs, only the wiring in `bhdata-server`.
3. **Traits at every seam that later becomes a network hop.** `ShardBackend`, `ControlPlane`, `TxnCoordinator`, `DerivedStore`, `BackupTarget`, all defined in `bhdata-api`. Stage 0 has one in-process implementation of each.
4. **Small slices.** Every slice ends with `scripts/check.sh` clean (build, test, clippy, audit, deny, dependency check), and a zip that can be handed back. A slice should be about 1,500 lines or less.
5. **No `unsafe`, no `unwrap` outside tests.** Errors are typed (`thiserror`), and every client-visible error is one of the named errors in section 3 of this plan.
6. **Design says no early; so does the code.** Anything the compiler cannot bound is a registration error, never a runtime surprise.

## 2. Decisions made in this plan (change any of them)

| Decision | Choice | Why |
|---|---|---|
| Language and runtime | Rust, `tokio` | Matches the design's concurrency model |
| Postgres client | `tokio-postgres` with our own small pool | We need exact control of connection roles, pinning and limits (§3, §10c) |
| TLS | `rustls` / `tokio-rustls` | No system OpenSSL; pinned fingerprints are easy |
| Login | SCRAM-SHA-256 built from `hmac`, `sha2`, `pbkdf2` | Same code reused by the SQL adapter (§10c) |
| Wire format (SDK to gateway) | Length-prefixed frames, 1 version byte, JSON body in v1 | Debuggable first; the frame format allows a binary body later |
| Recipe and schema format | TOML files | Declarative, easy to diff, no code execution |
| Config format | TOML | Same reason |
| SQL-mode protocol | `pgwire` crate | Do not write the Postgres wire protocol by hand |
| Python client | Thin, in `clients/python/`, same frames | Needed for tests and for the first users |
| Platform | Linux (Debian/Ubuntu); developed in WSL2 | Matches §1 supported platforms |
| Control plane build | Storage, recipe registry, logins and audit first (M3, M5); admin API, metrics and retire checks at M8 | The writer and gateway need them long before M8 |
| License | `Apache-2.0` for every crate and the Python client | Patent grant, clear contribution terms, accepted by most companies; `cargo deny` allows only compatible licenses |

## 3. Named errors (shared by every crate)

Defined once in `bhdata-types`, mapped to wire codes once in `bhdata-proto`:

`overloaded` (with retry hint), `insufficient_funds`, `overflow`, `already_exists`, `already_imported`, `request_id_conflict`, `request_expired`, `unauthorized`, `rate_limited`, `locked_out`, `unknown_recipe`, `bad_arguments`, `wrong_shard` (internal; the planner retries), `pending` (cross-shard still running), `internal`.

## 4. Workspace layout

```
bighearthdata/
├── Cargo.toml                  # workspace
├── rust-toolchain.toml
├── README.md
├── LICENSE                     # Apache-2.0
├── docs/
│   ├── design/                 # bighearthdata-design-v1.0.md lives here
│   ├── specs/                  # one spec per component, written before the component
│   └── operations.md           # how to run, back up, restore, upgrade
├── crates/
│   ├── bhdata-types/           # ids, errors, config structs, shared small types
│   ├── bhdata-api/             # trait definitions only (the seams); no implementations
│   ├── bhdata-proto/           # wire frames and messages (SDK <-> gateway)
│   ├── bhdata-schema/          # schema declaration, shard keys, validation
│   ├── bhdata-recipe/          # recipe format, compiler, lanes, versions
│   ├── bhdata-pg/              # Postgres access: pools, roles, error mapping
│   ├── bhdata-engine/          # engine manager (install, run, upgrade PostgreSQL)
│   ├── bhdata-helper/          # tiny privileged helper binary (fixed actions only)
│   ├── bhdata-writer/          # write queue, admission control, command execution
│   ├── bhdata-changelog/       # change log format, positions, readers, trimming
│   ├── bhdata-derived/         # derived stores and workers
│   ├── bhdata-kinds/           # built-in kinds: ledger, leaderboard, search
│   ├── bhdata-cache/           # memory cache
│   ├── bhdata-planner/         # lane choice, routing, shard map client
│   ├── bhdata-gateway/         # TLS, login, permissions, limits, connections
│   ├── bhdata-control/         # control plane: registry, shard map, audit, admin API
│   ├── bhdata-backup/          # backups and restore tests
│   ├── bhdata-import/          # admin import action
│   ├── bhdata-sqlmode/         # PostgreSQL wire adapter (stage 0 optional, later)
│   ├── bhdata-shard/           # slot maps, epochs, slot moves (stage 2)
│   ├── bhdata-txn/             # Transaction DB, sagas, recovery, fencing (stage 2)
│   ├── bhdata-client/          # Rust SDK
│   ├── bhdata-server/          # binary: wires everything for one process
│   ├── bhdata-cli/             # binary `bhdatactl`: install, start, register, import, backup
│   └── bhdata-stress/          # binary: load and failure-injection harness
├── packaging/                  # install.sh, uninstall.sh, systemd unit, clean-VM install test
├── scripts/                    # check.sh (build, test, clippy, audit, deny), check-deps.sh
├── deps.allow                  # the only allowed crate-to-crate dependencies
├── fuzz/                       # cargo-fuzz targets
├── clients/
│   └── python/                 # Python client and its tests
├── recipes/                    # example schemas and recipes (also used by tests)
└── tests/                      # cross-crate integration tests
```

## 5. Dependency rules

Arrows are written as "depends on". Each crate may depend only on what is listed here; `deps.allow` holds the same list and `scripts/check-deps.sh` compares it with `cargo metadata`, failing the build on any extra edge. Cycles are therefore impossible by construction.

| Crate | Depends on |
|---|---|
| `bhdata-types` | nothing in the workspace |
| `bhdata-proto` | `bhdata-types` (re-exports the named errors and ids for `bhdata-client`) |
| `bhdata-schema` | `bhdata-types` |
| `bhdata-recipe` | `bhdata-types`, `bhdata-schema` (defines the `RecipeStore` trait; knows nothing about the control plane) |
| `bhdata-api` | `bhdata-types`, `bhdata-recipe` (traits: `ShardBackend`, `ControlPlane`, `TxnCoordinator`, `DerivedStore`, `BackupTarget`, `RequestHandler`; re-exports `bhdata-types` and the plan types of `bhdata-recipe`) |
| `bhdata-pg` | `bhdata-types` |
| `bhdata-engine` | `bhdata-types`, `bhdata-pg` |
| `bhdata-helper` | nothing (standalone binary) |
| `bhdata-changelog` | `bhdata-types`, `bhdata-pg` |
| `bhdata-writer` | `bhdata-api`, `bhdata-pg`, `bhdata-changelog` (implements `ShardBackend`) |
| `bhdata-kinds` | `bhdata-types`, `bhdata-schema`, `bhdata-recipe` |
| `bhdata-derived` | `bhdata-api`, `bhdata-changelog`, `bhdata-pg` (implements `DerivedStore`) |
| `bhdata-cache` | `bhdata-types` |
| `bhdata-control` | `bhdata-api`, `bhdata-recipe`, `bhdata-pg` (implements `ControlPlane` and `RecipeStore`) |
| `bhdata-planner` | `bhdata-api`, `bhdata-cache` (talks to shards only through `ShardBackend`) |
| `bhdata-gateway` | `bhdata-proto`, `bhdata-api` (talks to the planner only through `RequestHandler`) |
| `bhdata-backup` | `bhdata-api`, `bhdata-pg`, `bhdata-changelog` |
| `bhdata-import` | `bhdata-api` |
| `bhdata-sqlmode` | `bhdata-api`, `bhdata-pg`, `bhdata-changelog` |
| `bhdata-shard` | `bhdata-api` |
| `bhdata-txn` | `bhdata-api`, `bhdata-pg` (implements `TxnCoordinator`) |
| `bhdata-client` | `bhdata-proto` |
| `bhdata-cli` | `bhdata-client`, `bhdata-engine`, `bhdata-backup` (restore must work with the server down) |
| `bhdata-stress` | `bhdata-client` |
| `bhdata-server` | every stage-0 crate (the only place where implementations are chosen) |

- **Lower crates never import higher ones.** `bhdata-types` imports nothing from the workspace.
- **Only `bhdata-pg` and `bhdata-engine` talk to PostgreSQL.** Other crates go through their APIs.
- **Only `bhdata-gateway` talks to clients.** It owns TLS, login, permissions and rate limits (§4 step 2).
- **No crate depends on `bhdata-server`.** It is the root.
- **Implementations never depend on each other.** `bhdata-planner` does not import `bhdata-writer`; both see only `bhdata-api`.
- **Types come through the seams.** `bhdata-api` re-exports `bhdata-types` and the plan types of `bhdata-recipe`, and `bhdata-proto` re-exports the named errors and ids. A crate that depends on one of them uses the re-export and does not list `bhdata-types` or `bhdata-recipe` itself, so `deps.allow` matches the real direct edges.

## 6. Crates in detail

### bhdata-types
```
src/lib.rs
src/ids.rs          RequestId, LoginId, SubjectId, TransferId; deterministic id derivation from login ID, request ID, table and the position of the create within the recipe (§6)
src/error.rs        named errors (section 3 above), retry hints
src/config.rs       config structs for every crate; defaults live here (cache TTL 2 s, lag 1 s, retry window 24 h, history 90 days)
src/time.rs         clock trait so tests control time
src/version.rs      RecipeVersion, MapVersion, Epoch, Term (stage 2 types exist from day one)
```

### bhdata-api
```
src/shard.rs        ShardBackend: execute(plan step), read, status
src/control.rs      ControlPlane: shard map, recipe lookup, logins and permissions, audit
src/txn.rs          TxnCoordinator: begin, step, status (stage 2)
src/derived.rs      DerivedStore trait
src/backup.rs       BackupTarget trait
src/handler.rs      RequestHandler: what the gateway calls (the planner implements it)
```
Traits and plain data types only. Nothing in this crate talks to PostgreSQL or the network. It re-exports `bhdata-types` and the plan types of `bhdata-recipe` (section 5).

### bhdata-proto
```
src/frame.rs        length prefix, version byte, size limits
src/message.rs      Request, Reply, Hello, Auth, Ping, Overloaded
src/codec.rs        JSON codec in v1; trait so a binary codec can be added
```
Requests carry: request ID, creation time, recipe name, optional version, arguments, and the map version (so the shard epoch check in §6 needs no change later).

### bhdata-schema
```
src/model.rs        Table, Column, SubjectTable, ShardKey, GlobalTable, Index
src/parse.rs        TOML to model
src/validate.rs     every table has a shard key or is global; children follow owners
src/ddl.rs          model to CREATE TABLE statements (app schema, never sql schema)
src/diff.rs         schema change planning; destructive changes flagged for admin action
```

### bhdata-recipe
```
src/format.rs       recipe TOML structure
src/parse.rs
src/compile.rs      bounds (statement count, time limit), tables used, argument types, keys
src/lanes.rs        one shard / two or more shards / all shards; rejects undecidable recipes
src/plan.rs         compiled plan: per-shard steps, named errors, savepoint boundaries
src/registry.rs     version rules in memory: immutability, retire rules (§9); durable storage is `bhdata-control`'s job
src/derived_decl.rs declared windows and staleness for derived-store reads
```
This is the most rule-heavy crate. It gets its own spec before code (`docs/specs/recipe-compiler.md`).

### bhdata-pg
```
src/pool.rs         pools: recipe writers, readers, SQL sessions (separate caps, §10c)
src/roles.rs        creates and checks the restricted roles (recipe, sql, reader, admin)
src/internal.rs     internal tables: requests, change log, meta (DDL and access functions)
src/errors.rs       maps Postgres errors: deadlock, serialization, unique violation, timeout
src/conn_budget.rs  total connection budget checked at startup (§10c)
```
Internal tables live in schema `bhdata_internal`; recipe-managed tables in `app`; SQL-mode tables in `sql`. The recipe role reaches `bhdata_internal` only through narrow SQL functions (§9 Locked down).

### bhdata-engine
```
src/install.rs      calls bhdata-helper to install the pinned version (PGDG repo)
src/instance.rs     initdb, private port and data directory, no default cluster
src/supervise.rs    start, stop, health checks, restart with backoff
src/update.rs       minor updates in the maintenance window
src/upgrade.rs      major upgrade: backup, dry run, pg_upgrade, verify, keep old on failure (§6)
src/tuning.rs       self-tuning checkpoints, wal_level handling
```

### bhdata-helper (separate binary, root-owned)
```
src/main.rs         parses a fixed action name only: install-pinned, update, restart
src/actions.rs      one function per action; no free-form arguments, no shell strings
src/guard.rs        root check, root-owned location check, platform check
src/exec.rs         command runner: absolute paths, fixed arguments, fixed environment
src/error.rs        errors and the stable exit codes
```
No dependencies beyond the standard library. Details in `docs/specs/engine-manager.md`.
The installer sets this up with sudo; the running server never has root (§3 Engine manager).

### bhdata-writer
```
src/queue.rs        bounded write queue per shard
src/admission.rs    rejects when the oldest queued write waited too long; also watches commit latency
src/exec.rs         runs one compiled plan: request record, savepoint, data change, change-log entry, one commit
src/retry.rs        deadlock and serialization retries, limited count
src/replies.rs      stored results, failed-check records, request_id_conflict, expiry by creation time
src/history.rs      compact history rows, retention, copy markers
```
`exec.rs` is the most important file in the project: request ID, data change and change-log entry in one commit (§4 step 5).

### bhdata-changelog
```
src/format.rs       entry layout and per-shard position
src/read.rs         reader API with positions, snapshot-plus-position for rebuilds
src/trim.rs         trim after all readers pass an entry and retention passes
src/logical.rs      logical decoding for sql-schema tables, WAL cap, gap marking (with bhdata-sqlmode)
```

### bhdata-derived
```
src/store.rs        DerivedStore trait: apply(entry), rebuild(snapshot, position), read(query)
src/worker.rs       reads the change log, applies entries once (position-carrying updates)
src/rebuild.rs      snapshot, load, follow the log
src/memory.rs       in-process store used at stage 0
src/staleness.rs    declared staleness checks
```
Stage 0 keeps derived stores in PostgreSQL tables or memory behind the trait; a separate store can replace them later.

### bhdata-kinds
```
src/ledger.rs       schema and recipes: grant, spend, transfer, read balance
src/leaderboard.rs  windowed rankings (daily, weekly, all-time), top N, rank of one
src/search.rs       English full-text search plus trigram fuzzy matching
```
Kinds only generate schema, recipes and derived-store declarations. They add no special code paths (§9b).

### bhdata-cache
```
src/cache.rs        short-TTL cache, not cleared on writes, safe to empty
```

### bhdata-planner
```
src/route.rs        shard key to slot (mask of a 64-bit hash, 4,096 slots) to shard
src/lane.rs         uses the compiled lane; picks one-shard commit vs saga
src/read.rs         cache, then replica, then primary; derived reads
src/map_client.rs   cached shard map with versions; refresh on wrong_shard
```
At stage 0 the shard map has one entry type: all slots to shard 0.

### bhdata-gateway
```
src/listener.rs     TCP and TLS accept, connection and idle limits
src/tls.rs          certificate loading, rotation overlap with fingerprint lists
src/auth.rs         SCRAM-SHA-256, lockout after repeated failures
src/perm.rs         execute, register, admin, sql; per-command limits
src/ratelimit.rs    per login, per copy at stage 1
src/shed.rs         early overloaded replies
src/session.rs      one task per connection, request to planner, reply
```

### bhdata-control
```
src/registry.rs     stores compiled recipes durably, with versions
src/shardmap.rs     slot to shard map, version and per-slot epoch
src/audit.rs        audit log: logins, registrations, admin actions, SQL grants
src/logins.rs       logins, SCRAM verifiers, permissions, per-command limits, lockout state
src/admin_api.rs    commands used by bhdatactl (and the admin UI later)
src/metrics.rs      metrics, health, lag, alarms
src/store.rs        control-plane storage (its own small schema in the shard at stage 0)
```
Built in two steps: storage, registry, logins and audit first (M3 and M5); admin API, metrics and retire checks at M8.

### bhdata-backup
```
src/target.rs       BackupTarget trait: local disk, off-machine target
src/run.rs          base backup plus continuous log shipping
src/restore.rs      restore procedure
src/verify.rs       restore tests; measured restore time reported to admin
```

### bhdata-import
```
src/record.rs       permanent import record: file hash, finished rows, status, label
src/run.rs          dry run, real run through recipes, resume, cancel, throttling
```

### bhdata-sqlmode (after stage-0 core)
```
src/adapter.rs      pgwire server at the gateway, extended protocol only
src/session.rs      pinned connection per session, SQL role, limits
src/call.rs         CALL recipe_name(args) interception; refused inside a transaction
src/limits.rs       statement timeout, row cap, memory and temp file caps, pool budget
src/txn_lane.rs     duration, idle, lock-wait timeouts; open-transaction cap
```

### bhdata-shard and bhdata-txn (stage 2)
```
bhdata-shard/src/epoch.rs      ownership epochs and refusal of stale writes
bhdata-shard/src/move_slot.rs  copy, catch up, hold, flip, abort
bhdata-txn/src/db.rs          Transaction DB storage
bhdata-txn/src/saga.rs        intent, debit, credit, done/refunded/failed
bhdata-txn/src/recovery.rs    resume unfinished, find orphaned steps after a takeover
bhdata-txn/src/reconcile.rs   pending must end; totals unchanged
bhdata-txn/src/term.rs        terms, leases, fencing; shards store the highest term
bhdata-txn/src/ha.rs          primary, standby, witness, rollover
```

### bhdata-client, bhdata-server, bhdata-cli, bhdata-stress
```
bhdata-client/src/client.rs    connect, pinned fingerprints, request ID generation, retry with jitter
bhdata-client/src/retry.rs     safe retries, backoff, `pending` polling
bhdata-server/src/main.rs      load config, start engine, start writer, workers, gateway
bhdata-server/src/wiring.rs    the only place that decides in-process vs remote
bhdata-cli/src/main.rs         install, start, status, register, import, backup, restore, stress
bhdata-stress/src/main.rs      load generator, failure injection, latency reports
```

## 7. Build order

Each milestone is several small slices. Each ends green and zip-able.

| # | Milestone | Crates touched | Done when |
|---|---|---|---|
| M0 | Skeleton | all (empty), `bhdata-types` | Workspace builds; errors, ids, config exist; `scripts/check.sh` runs build, test, clippy, audit, deny and the dependency check; the license file and the workspace license field are in place |
| M1 | Engine manager and installer | `bhdata-helper`, `bhdata-engine`, `packaging/` | On a clean VM, one command installs everything and starts a private PostgreSQL; see acceptance suites below |
| M2 | Postgres access and internal tables | `bhdata-pg` | Roles, internal tables and narrow functions exist; connection budget checked; error mapping tested |
| M3 | Schema and recipes | `bhdata-schema`, `bhdata-recipe`, `bhdata-control` (registry, store) | A TOML schema becomes tables; a recipe compiles to a plan or is rejected with a reason; registered recipe versions survive a restart |
| M4 | Write path | `bhdata-writer`, `bhdata-changelog` | The write-path suite below passes |
| M5 | Gateway and client | `bhdata-proto`, `bhdata-gateway`, `bhdata-client`, `bhdata-server`, `bhdata-control` (logins, audit), `clients/python` | TLS, SCRAM login, permissions, rate limits, idle timeout; logins and audit entries stored by the control plane; Rust and Python clients run a recipe end to end; fuzz targets for frames and JSON exist and the size and nesting limits are enforced |
| M6 | Reads, cache and kinds | `bhdata-cache`, `bhdata-planner`, `bhdata-kinds` | The ledger suite below passes |
| M7 | Derived stores | `bhdata-derived` | The derived-store suite below passes |
| M8 | Control plane admin | `bhdata-control`, `bhdata-cli` | Admin API for logins and recipes, retire rules (refused while a version is in use), metrics and health, status command |
| M9 | Backups | `bhdata-backup` | Continuous backup to a second target; a restore test passes and reports its time |
| M10 | Import | `bhdata-import` | Dry run, resume, cancel, already_imported, labeled rerun |
| M11 | Stress and failure tests | `bhdata-stress` | Load run with full durability; kill PostgreSQL during load; disk-full and slow-disk behavior match section 11 of the design |
| **Stage 0 complete** | | | One command starts everything; all of M0 to M11 pass |
| M12 | SQL mode | `bhdata-sqlmode`, `bhdata-changelog` | pgjdbc connects; YCSB and NoOp run; recipe tables read-only; `CALL` works; limits enforced |
| M13 | Read replicas, second gateway | `bhdata-pg`, `bhdata-planner`, `bhdata-gateway` | Stage 1 behaviors: lag-based rotation, per-copy rate limits |
| M14 | Shard map, epochs | `bhdata-shard`, `bhdata-control`, `bhdata-planner` | Several shards on one machine; wrong-shard refusal; slot move with abort |
| M15 | Transaction DB and sagas | `bhdata-txn` | Cross-shard transfer with intent, recovery, reconciliation, `pending` replies |
| M16 | Failover and fencing | `bhdata-txn` | Primary, standby, witness; terms stored by shards; orphan-step recovery after async takeover |

**M1 slices.** M1 is built as three slices, each ending with `scripts/check.sh` clean and a zip: **A** `bhdata-helper`, **B** `bhdata-engine`, **C** `packaging/` and the clean-VM run. The spec is `docs/specs/engine-manager.md`; it also lists which checks can only be run on a real VM. `upgrade.rs` and `tuning.rs` in `bhdata-engine` are proposed to move out of M1 (spec section 6); this table is unchanged until that is agreed.

**M2 slices.** M2 is three slices, like M1: **A** pure parts of `bhdata-pg` (error mapping, connection budget, SQL text), **B** pools, bootstrap and the engine's login settings, **C** wiring into the host and the clean-VM run. The spec is `docs/specs/pg-access.md`. Slice A is done: `errors`, `conn_budget`, `roles` and `internal` in `bhdata-pg`, and `[pools]` in `bhdata-types`; the SQL was run once against PostgreSQL 16. Slice B adds the driver crates, so `cargo deny` runs on them then.

**Thin slice first.** M0 to M4 need no network: the write-path suite runs in-process against a real private PostgreSQL, with recipes loaded from files. Finish and harden M4 before starting M5, because it is the riskiest part and everything else sits on it.

### Acceptance suites

**M1 installer.**
- On a fresh VM (or a fresh WSL2 distro), with no manual steps: installs `bhdatactl`, the server and `bhdata-helper`; creates the service user; sets file permissions; writes the systemd unit; installs the pinned PostgreSQL and starts a private instance.
- Killing PostgreSQL or the server brings it back; a reboot brings it back.
- A pre-existing PostgreSQL on the machine is untouched by install, upgrade and uninstall.
- Uninstall removes the binaries, helper and service; data is removed only on an explicit flag.

**M4 write path** (the most heavily tested part of the project).
- Two concurrent requests with the same request ID: one executes, both get the same result.
- Same request ID, different payload: `request_id_conflict`.
- Client disconnects just before commit, and just after commit: a retry returns the single stored result.
- PostgreSQL crash during a transaction, and just after commit: after recovery, a retry applies once.
- A failed check stores a `failed` record; a retry returns the same named error.
- Retry after the result expired: `request_expired`, not re-applied.
- Retry after the stored result was compacted into a history row: `request_expired`; the history row is never used to re-apply.
- Every committed change has exactly one change-log entry in the same commit.

**M6 ledger.**
- Create account, grant, spend, reject overdraft with `insufficient_funds`, reject `overflow`, transfer on one shard, retry each of these idempotently.
- Balance read matches the sum of entries; no entry is ever deleted.
- Each change has its change-log entry.

**M7 derived stores.**
- A rebuilt leaderboard and search index equal the originals (compared to ground truth computed from the shard data).
- A rebuild interrupted at several points and resumed gives the same result.
- A repeated change-log entry is applied once.
- Staleness declarations are enforced.

M12 may be built before M13 to M16. The order from M13 on follows the scaling stages in section 12 of the design.

## 8. Testing

| Layer | What | Tools |
|---|---|---|
| Unit | Compiler rules, id derivation, retry classification, admission control | `cargo test`, `proptest` for ids and the compiler |
| Integration | Real private PostgreSQL per test run | A test helper that starts an instance from `bhdata-engine` in a temp directory |
| Idempotency | Same request sent N times, concurrently, with kills in between; stored result must match | In `bhdata-stress` and `tests/` |
| Failure injection | Kill PostgreSQL, fill the disk, slow the disk, drop connections | `bhdata-stress` with fault hooks in `bhdata-pg` |
| Rebuild | Drop derived stores mid-load; rebuild; compare with ground truth | `tests/derived_rebuild.rs` |
| Security | Restricted roles cannot write internal tables; SQL role cannot write recipe tables; recipes cannot touch undeclared tables; the helper accepts only its fixed actions and the server cannot gain root; lockout works and login comparison is constant-time; SQL-mode resource exhaustion is contained | `tests/security/` |
| Fuzz | Frame decoder, JSON size and nesting limits, recipe and schema parsers, the compiler | `cargo-fuzz` targets in `fuzz/` |
| Supply chain | Known vulnerabilities and licenses | `cargo audit` and `cargo deny` in `scripts/check.sh` |
| Stage 2 | Deterministic simulation of terms, leases and saga recovery with a fake clock and fake network | `bhdata-txn` test harness |
| Benchmarks | Full-durability throughput and latency, stating the setting | `bhdata-stress`, native Linux for published numbers |

## 9. Specs to write before their crates

Short documents in `docs/specs/`, each a page or two, written before the milestone starts:

1. `recipe-format.md` and `recipe-compiler.md` (before M3)
2. `write-path.md`: exact transaction layout of one command (before M4)
3. `wire-protocol.md` (before M5)
4. `change-log-and-rebuild.md` (before M7)
5. `backup-and-restore.md` (before M9)
6. `sql-mode.md` (before M12)
7. `shard-map-and-epochs.md` (before M14)
8. `transaction-db-and-fencing.md` (before M15): must state the exact transition sequence (old lease expires, new term chosen, term installed and confirmed on every shard, only then the first step), how the old primary is fenced, and what a shard does with a lower term

## 10. Open items for this plan

1. Where derived stores live at stage 0 (PostgreSQL tables or in memory behind the trait). The plan assumes either works; M7 picks one.
2. Admin UI: the plan only has the CLI (`bhdatactl`). A web UI is a later, separate crate.
3. Whether to add an application namespace to generated ids if multi-tenant use is ever wanted. For now the login is the namespace.
