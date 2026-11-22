# BigHearthData: Target Design

v1.0 · 2026-11-22 · Status of every item: **Approved**

This document describes the target design for BigHearthData, a unified data layer, in words.

---

## 1. Purpose

BigHearthData is one data layer that replaces the separate, individually run databases of a polyglot stack (for example a relational store, a cache, a search engine and a ranking store), so a developer needs only this one and keeps complete control of the app's data through it. A developer should get reasonable speed, safety and room to grow without needing deep database knowledge. The developer declares what the data is and what operations the app needs; the server decides how and where it is stored, cached, indexed and scaled. The name refers to breadth, one place for everything the app stores, not to raw scale.

Primary users: solo developers and small teams (bots, small apps, small shops), including those who would otherwise run several separate stores for one app. The design must work on one machine, started with one command (the data layer installs and runs its own PostgreSQL), and grow without the developer rewriting their app.

Non-goals for the first versions: competing with large distributed databases on raw scale, multi-region deployment, or serving arbitrary SQL by default (an opt-in, single-shard SQL mode is described in section 10c).

**What it replaces, and what it does not.**

- **Replaces, in this design:** application records owned by a subject and reached by key (not by ad hoc queries); balances and ledgers (the ledger kind); rankings and counters (the leaderboard kind); full-text and fuzzy search (the search kind); and hot-read caching, which is built in instead of run as a separate cache. SQL mode (section 10c) covers tools that need plain SQL.
- **Does not replace:** time-series, document or graph stores, blob or file storage, message queues, and analytics that join or aggregate across subjects (those need a derived store, or SQL mode on one shard, section 9). These stay in their own tools.
- New kinds can be added later through the same recipe and derived-store mechanism (section 9b); none beyond the first three are designed here.

**Supported platforms.** Linux with apt (Debian and Ubuntu), and Windows through WSL. macOS and Linux distributions without apt are not supported for now. On Windows, the server and its PostgreSQL data both live inside WSL's own filesystem, not on the mounted Windows drives. WSL2 with systemd enabled is required.

## 2. Principles

1. **Ease first.** Safe defaults. One command to start. The data layer installs and runs its own PostgreSQL; the developer never installs or configures it, and never configures queues, pools, checkpoints, or shards by hand.
2. **Security and correctness before speed**.
3. **One source of truth.** The shards hold the real data. Everything else (rankings, search indexes, counters) is derived and can be rebuilt from the shards' data and change logs; caches are filled on read and can be emptied at any time. SQL-mode tables have no derived stores in the first versions (section 10c).
4. **Everything is idempotent in recipe mode** (SQL mode is the documented exception, section 10c). Every write carries a request ID. A retry never applies twice.
5. **Sharding is built in, but invisible until needed.** A small install runs one shard. Nothing about the app changes when more are added (SQL mode is the documented exception, section 10c).
6. **Say no early.** A recipe the server cannot classify or bound is rejected at registration, not at runtime.

## 3. Components

| Component | Role | Count |
|---|---|---|
| Client SDK | Sends requests with a request ID, retries safely, verifies TLS (pinned certificate) | per app |
| Gateway | TLS, login, permissions, rate limits, connection and idle limits, load shedding | no durable state, many copies; rate-limit and lockout counters are enforced per copy at stage 1, with limits divided by the number of copies (so approximate when traffic is uneven), and live in a shared store from stage 2 |
| Planner | Looks up the compiled recipe, validates arguments, finds the shard key, picks the lane | no durable state, many copies |
| Transaction DB | Database that tracks cross-shard transactions (intent, progress, outcome) and holds the global command history | one (stage 2 onward; itself replicated) |
| Memory cache | Short-lived cache for hot reads, filled on read (not by workers) and safe to empty at any time | **shardable** |
| Read replicas | Read-only copies of a shard's primary | **shardable** |
| Write queue | Bounded queue in front of a shard's writer | **shardable** |
| Primary writer | The write path of one shard: a pool of concurrent writer connections to the shard's PostgreSQL, which does its own group commit | **shardable** |
| Engine manager | Installs, starts, monitors, restarts and upgrades each shard's PostgreSQL as a private instance (own data directory, user and port) that never touches another PostgreSQL on the machine. Keeps it reachable only from the data layer's own components. The installer runs with administrator (sudo) rights; the running server does not. Package installs and minor-release updates go through a small privileged helper that the installer sets up (or an apt hook), never through the running server. The default `main` cluster that the Debian/Ubuntu package creates is disabled (`create_main_cluster = false`), so the data layer only uses its own `initdb` instances. Supported on amd64 and arm64 (the architectures the official packages cover). The privileged helper accepts only a fixed list of actions (install the pinned version, restart, update), runs from a root-owned location, and takes no free-form input | **shardable** |
| Backups | Continuous, restore-tested copies of each shard, kept on a different disk and, where possible, a different machine; the admin UI warns until an off-machine target is set. Aim: at most a few seconds of data loss with continuous log shipping; restore time is measured by each restore test and shown in the admin UI; backups are encrypted; default retention is 7 daily copies plus the log needed to restore any point in them | **shardable** |
| Change log | Record of every committed change, saved in the same commit as the change (for SQL-mode tables: appended shortly after the commit, read from PostgreSQL's write-ahead log by logical decoding) | **shardable** |
| Workers | Read the change logs and update derived stores | scalable |
| Derived stores | Rankings, search indexes, counters. Rebuildable | scalable |
| Control plane | Recipe registry, shard map, admin UI, migrations, metrics, health | one (from stage 2 replicated with the same primary, standby and witness pattern as the Transaction DB, sharing the witness; a single copy at stages 0 and 1) |

"Shardable" means: one copy of this part exists per shard. Parts without the tag exist once regardless of the number of shards.

## 4. Request flow

1. The **client SDK** sends the request with a request ID over TLS.
2. The **gateway** authenticates the login, checks its permission for this command, applies rate limits, and rejects early with `overloaded` (plus a retry hint) if the system is falling behind.
3. The **planner** finds the compiled recipe, validates the arguments, and reads the recipe's shard key from the request (usually the `subject`). It decides the lane (section 5).
4. **Reads** go to the memory cache, then to a read replica on a miss. Reads of derived data (for example a leaderboard) go to the derived stores. Cache and replica reads may be slightly stale: the cache expires entries after a short time and is not cleared on writes, so staleness is bounded by that time plus a maximum replica lag (defaults: entries expire after 2 seconds; a replica more than 1 second behind is taken out of rotation, and if none is acceptable the read goes to the primary). A recipe declares whether it accepts that or must read the primary; the Ledger's balance read defaults to the primary.
5. **Writes** are placed in the shard's bounded write queue and run by one of the shard's concurrent writer connections. Each command is its own transaction, so one failure does not affect the others, and PostgreSQL groups the disk syncs. The request ID, the data change and the change-log entry are saved in the same commit. A command that hits a deadlock or serialization conflict is retried automatically (section 9). A write that spans shards is first recorded in the Transaction DB (section 7), then run as per-shard steps through the same queues.
6. The reply is sent **after** the commit is durable. For a cross-shard write the reply is sent when the transaction reaches a final state (`done`, `refunded` or `failed`); if that takes longer than a set time, the reply is `pending` with the transaction ID, which the client can poll.
7. **Workers** read the change log asynchronously and update derived stores. The reply never waits for them.
8. The **control plane** runs backups, migrations, the admin UI and health checks, and holds the shard map and the recipe registry.

Text sketch:

```
Client SDK -> Gateway -> Planner --reads--> Memory cache -> Read replicas
                            |                                 ^
                            |--derived reads--> Derived stores | (copies of primary)
                            |                          ^
                            +--writes--> Write queue -> Primary writer -> Backups
                                                                              |
                                                                              +--> Change log -> Workers -> Derived stores
Cross-shard writes: Planner -> Transaction DB (intent) -> per-shard Write queues (section 7)
Control plane: recipe registry, shard map, admin UI, migrations, metrics, health
```

## 5. Lanes

The compiler works out, when a recipe is registered, which keys it touches. The planner uses this at request time.

| Lane | When | Behavior |
|---|---|---|
| One shard | The recipe touches only rows owned by one key (for example `daily`, `balance`) | One commit on one shard. The fast path |
| Two or more shards | The recipe touches rows owned by different keys (for example `give` between users on different shards) | Handled as a saga with an intent record (section 7) |
| All shards | A global read (for example `leaderboard`) | Read from a derived store. Recipes do not scatter and merge by themselves |

A recipe whose lane cannot be determined is rejected at registration.

## 6. Sharding

- **Shard key in the schema.** A table declares what it is sharded by (for example `users` by `id`). Child tables (for example `transactions`) follow their owning row, so a user's data stays together.
- **Virtual slots.** Keys hash into a fixed number of slots: 4,096 (a power of two, so the hash-to-slot step is a simple mask). The control plane keeps a map from slots to physical shards. Adding a machine moves slots; it does not re-hash everything.
  - The number is arbitrary but must be fixed for the life of the system, because changing it re-hashes every key. It must be at least as large as the most physical shards ever wanted, and a larger number balances better.
  - A slot is only an entry in the shard map, not a separate database, so thousands of slots cost almost nothing. For comparison, Redis Cluster uses 16,384 slots, Couchbase uses 1,024 vBuckets, and Citus defaults to 32 shards per table.
- **Identity and uniqueness.** The only unique value the server guarantees is the id assigned when a subject is created (or the id the app supplies, for example one from an outside service). Names and other fields are the developer's choice and may repeat. Enforcing uniqueness on other fields (for example login emails) is not offered in the first versions; a developer who needs it must handle it in the app, and a check followed by a create can race on a sharded setup. A generated id is created before routing and needs no coordination between shards. It is derived deterministically from the login ID, the request ID, the table and the position of the create within the recipe (for example a keyed hash of all four, or a globally unique ID generated by the SDK and sent with the request), so two logins using the same request ID never collide, two creates in one request get different ids, and a retried create produces the same id, routes to the same shard, and finds the stored result. A create with an id that already exists, under a different request ID, fails with the named error `already_exists`.
- **Global tables.** Small, read-mostly tables (for example `shop`) are copied to every shard. A write to a global table is a versioned change applied through the control plane to every shard (idempotent, retried until all shards have it); a new shard receives a full copy before it takes any slots. The change is visible on a shard once that shard has applied it, and the update is reported done when every shard has; until then shards may briefly disagree, and a recipe that reads a global table accepts that. A recipe uses the latest version its shard has, and the request record stores which version of each global table it used.
- **Hot shared rows** (for example the jackpot) live on one owner shard. Counters that many users read are kept in the derived stores.
- **Moving a slot:** copy it to the new shard, catch up from the change log, briefly pause writes to that slot, flip the map, resume. The slot's request records move with its rows, so a retry that arrives after the move still returns the stored result. Derived stores are keyed by subject and do not change; workers ignore change-log entries from the old shard once the map has flipped. Each version of the shard map carries an ownership epoch per slot, every request carries the map version it was routed with, and a shard refuses a write for a slot it does not own at that epoch (the planner then refreshes its map and retries). During the pause, writes to that slot wait in a short, bounded hold and are refused with `overloaded` and a retry hint if the pause runs long; writes already in the old shard's queue finish before the flip. Cache entries for the slot are dropped at the flip. If copying or catch-up fails, the move is aborted: the map is unchanged, the old shard stays the owner, and the partial copy is deleted.
- **Separate disks.** Shards should sit on separate disks. Several shards sharing one disk share one sync queue and gain little.
- **Storage engine per shard.** Every shard is a PostgreSQL instance that the data layer installs and runs itself (the engine manager, section 3). The recipe compiler sits behind a backend interface, so the engine could change later without changing recipes. Concurrent writers help when statements are slow or touch different rows; they do not help when the disk sync is the bottleneck or when many commands update the same row. For sync-bound load, the fixes are tuned checkpointing, a faster disk, and more shards on separate disks.
- **PostgreSQL upgrades.** Minor releases (security and bug fixes within the same major version) install automatically, since they do not change the data. The restart that a minor release needs happens in a maintenance window the admin sets (default: a low-load hour), with notice; requests get `overloaded` with a retry hint while it runs. An admin can force a security fix sooner. Each release of the data layer is pinned to one PostgreSQL major version. Because distro repositories ship only one version, the installer adds the official PostgreSQL (PGDG) apt repository so the pinned version is available. Moving to a new major version is never automatic: it is an explicit admin action, with an automatic backup first and a dry run. The data is converted and checked before the old copy is retired. If the check fails, the old version and its data stay in place and the developer is told.
- **Stage 0 is one shard.** All slots map to one shard, a single PostgreSQL instance.

## 7. Transaction records

Every action has a transaction record with a state: `pending`, `done`, `failed` or `refunded`. `failed` means the command ended without effect (for example a failed check such as `insufficient_funds`, or a debit that could not happen); nothing needs refunding.

- **Single-shard commands.** The record lives inside the shard, in the same commit as the data change. There is no extra commit, no extra sync, and no gap to track. After the retry window the stored result is dropped, but a compact history row (command ID, status, time, recipe and version, subject) stays for the history retention period (default 90 days), and is never dropped before the history copier has copied it.
- **Cross-shard commands.** The central **Transaction DB** tracks the steps, because no single commit covers both sides. Pattern (an orchestrated saga):
  1. Write the intent first (for example "A pays B 50, started").
  2. Debit on the sender's shard, with the transfer ID as an idempotency key.
  3. Deliver the credit to the receiver's shard, again idempotent. A repeated message is ignored.
  4. Mark the transaction `done`. If the credit is permanently refused (the receiver does not exist or is closed, the credit would overflow, or the receiver's checks fail; temporary failures such as a down shard are retried, never refunded), refund the sender with a new ledger entry and mark it `refunded`. Nothing is deleted.
  5. A recovery job scans for unfinished transactions after a crash and resumes them.
  6. A reconciliation job checks that every `pending` transaction eventually ends, and that totals across shards are unchanged.
- **Developer view.** Every command has an ID and a status, and the admin UI lists both kinds in one place. The storage differs; the experience does not.
- **Global history.** The Transaction DB is the one place that holds a record of every command. Single-shard commands are copied into it asynchronously from the shard's request table (which also holds failed commands, unlike the change log), never inside the commit, so there is no extra sync. Copies are compact (command ID, status, time, recipe, subject, not the data) and idempotent by request ID. The admin UI shows how far the copying is behind. Because every shard feeds this one database, its write rate is the sum of all commands, so copies are batched and old finished records are expired after a retention period (never a `pending` one). At stage 0 the shard's own request table is the history; the central copy starts when the Transaction DB is introduced (section 12, stage 2). When it is introduced, the existing request tables are backfilled into it first.
- **Availability of the Transaction DB (from stage 2).** Stages 0 and 1 have no Transaction DB. From stage 2 it runs as one primary and a standby, plus a small witness that only votes, so that two machines can never both act as primary after a network split.
  - **Synchronous standby by default:** a write is acknowledged only after the standby has it, so a takeover loses no record. If the standby is unavailable longer than a set time, the primary keeps running and drops to asynchronous with an alarm; during that window the no-loss guarantee does not hold, and the admin UI says so. A strict setting that refuses writes instead is available. An **asynchronous standby** is also available as a clearly labeled relaxed setting (like section 10b), and one of each may run together.
  - **Rollover:** when the primary fails, the standby takes over automatically. The failed machine restarts with growing delays, discards any records the new primary never received, copies what it missed, and rejoins as the standby. Roles rotate on each later failure. After a set number of failed restarts it stops and alerts the developer.
  - Until the restarted machine has caught up there is no standby, and the admin UI shows that plainly. The recovery job resumes interrupted transactions after a takeover.
  - **Fencing.** Leadership is a numbered term with a lease. The primary may accept writes only while it holds a lease renewed by a majority (primary, standby and witness). A new term starts only after the old lease has expired. Every write and every step sent to a shard carries the term, so a former primary that lost its lease refuses writes, and shards refuse steps from an older term. Each shard durably stores the highest term it has seen and never accepts a step from a lower one, so safety does not depend on synchronized clocks. A new primary first sends its term to every shard and waits for each shard's confirmation before it issues steps to that shard.
  - **Records lost in an asynchronous takeover.** The intent is written before any debit, so a lost intent can leave a debit on a shard with no intent. Each shard keeps its saga steps, with the full transfer details (sender, receiver, amount), in its own request table, so after a takeover the recovery job asks every shard for steps whose transfer ID the Transaction DB does not know, and finishes or refunds them with the same idempotent steps. The admin UI reports how many it found.
- **Cost of cross-shard.** A brief in-between state (the sender has paid, the receiver has not yet received) and slightly delayed reads. This is normal in payment systems and must be documented clearly. There is no free solution; the design keeps cross-shard commands rare and explicit.

## 8. Derived data

Rankings, search and counters are derived from the change logs. Caches are filled on read and can be emptied at any time. Rules for derived data:

- They are never the source of truth.
- They can be dropped and rebuilt: a rebuild takes a consistent snapshot of each shard together with that shard's change-log position, loads it, then follows the log from that position. Derived updates carry the log position, so an entry is applied once and a repeated one is ignored: nothing is missed or counted twice, and the full history is not needed.
- Change logs are trimmed once every reader (the workers) has passed an entry and a set retention period has gone by.
- A recipe can declare how stale a derived result may be.
- If a derived store is down, reads fall back to the shards where possible, or return a clear error.

## 9. Recipes

- **Authored** by a login with `register` permission, in the declarative recipe format.
- **Compiled once** at registration, then stored durably in the control plane with a version. A registered version never changes; a change registers a new version (see Versions below). The compiler checks bounds (statement count, time limit), the tables used, the argument types, and the shard keys. Rows are reached only by key (the subject's shard key), never by scanning, so a write touches a bounded number of rows; a read of a derived store has a maximum row count and payload size, with paging, and a recipe that asks for more is rejected at registration.
- **Used by the planner**, which keeps the compiled plans in memory and looks them up per request.
- **Executed by the shards.** A cross-shard plan is split into per-shard steps, tracked by the Transaction DB.
- **Versions.** Old versions stay callable until an admin retires them. A request runs on the version it started with. A caller names a version or defaults to the latest. Retiring a version is an explicit admin action, and is refused while unfinished transactions or in-flight imports still use it. Each request record stores the version it ran.
- **Import.** The admin import action loads rows into recipe-managed tables by running each row through a named recipe, so the recipe rules still apply (`overflow`, `insufficient_funds`, subject-row creation). Each row's request ID is derived from the file's hash and the row number, so running the same file again is idempotent. The import keeps its own permanent record (file hash, finished rows, status), and request IDs for finished rows do not expire. Cancelling an import stops future rows; it does not undo or forget finished ones. Running a file whose hash is already recorded resumes it, or is refused as `already_imported` if it finished. An admin can start the same file again as a new import under an explicit label; it gets its own identity and record, is audited, and its rows run again as new commands. A dry run checks every row first. If the real run hits a bad row it stops and reports that row; a rerun continues, because finished rows return their stored results. Rows go through the normal write queue, so a big import is throttled like any other load. Imports are written to the audit log.
- **Retried on conflict.** If PostgreSQL reports a deadlock or serialization conflict, the command is rolled back and retried automatically a limited number of times. The request ID makes this safe.
- **Locked down.** Recipes can touch only declared tables, cannot change the schema, load extensions or run raw SQL from clients. The engine's own permissions enforce this, not only the compiler: recipes run through a restricted database role that has rights only on the declared tables (and, through narrow functions, the request table and change log), does not own the database, and cannot create objects or extensions.

**Recipe scope.** Recipes stay small so they never become a second query language:

- A recipe either **changes one subject** (read, check, write, for example "spend 50 coins if the balance allows it") or **reads a named derived store** (for example a ranking). A recipe may name one **counterparty subject** (for example the receiver in `give`); it then runs in the two-or-more-shards lane when the counterparty is on another shard (section 5). It never joins, groups or searches across subjects.
- Anything that joins, groups or searches across subjects is defined once as a **derived store**, which the recipe then reads by name, or it is done in SQL mode (section 10c).
- A recipe names its **subject** (the row it owns), so the planner can find the shard key.
- A failed check returns a **named error** (for example `insufficient_funds`), rolls back the data change (to a savepoint), commits the request record with status `failed` and the named error, and a retry returns that stored result. This matches the `overflow` rule.
- A recipe that reads a derived store declares how stale the result may be (section 8). A ranking over a time window uses **declared windows** (for example daily, weekly, all-time) that the derived store keeps ready. A window that is not declared is rejected at registration ("say no early"). Arbitrary start times are not offered to recipes.
- SQL logins may also **call recipes** (with `CALL`, section 10c), so power users keep the recipe rules (overflow rejection, subject-row creation) where they want them.

## 9b. Built-in kinds of data

A **kind** is a ready-made type of data. The developer declares "this is a currency", "this is a leaderboard" or "this is a search index" and gets the schema, recipes and derived stores that go with it, instead of writing them by hand. Kinds are built from the same parts as everything else (recipes, derived stores, the change log) and follow the recipe scope rules in section 9. They have no special paths. The first three:

- **Ledger.** A balance per subject, plus an append-only list of entries; nothing is deleted, and a refund is a new entry (section 7). Prebuilt recipes: grant, spend, transfer and read balance. A spend that the balance cannot cover returns the named error `insufficient_funds`, and an amount that would overflow is rejected with `overflow`. A transfer between subjects on different shards uses the saga lane (section 5). Every recipe is idempotent by request ID.
- **Leaderboard.** A ranking over a score or a ledger total, kept in a derived store that workers update from the change logs (section 8). It uses declared time windows (for example daily, weekly, all-time) and a declared staleness. Prebuilt reads: the top N, and the rank of one subject. It can be dropped and rebuilt at any time.
- **Search.** An index over declared text fields of a table, kept in a derived store and updated from the change logs. It declares its staleness. Both exact matching and fuzzy matching (which tolerates typos) are supported. The first version supports English only: exact matching uses PostgreSQL full-text search with English stemming, so "run" matches "running", and fuzzy matching uses trigram matching, which tolerates typos in any language. Other languages come later as a per-index setting; an index declares its language when it is created. A search returns the ids of matching subjects with a relevance order, not full rows. It can be dropped and rebuilt at any time.

## 10. Security

- TLS required on any network address. Pinned certificate fingerprints in the client. For rotation, the server can present the old and new certificate during an overlap window, and the client accepts a list of fingerprints.
- Login by SCRAM-SHA-256, with lockout after repeated failures.
- Permissions: `execute`, `register`, `admin`, and `sql` (section 10c), plus per-command limits per login.
- Rate limits per login by default (not optional).
- **Authentication cannot be turned off on a non-loopback address.** Insecure mode is a local-development feature only.
- **Idle timeout** after login, plus connection and size limits.
- **The shard's PostgreSQL is never exposed to clients.** It listens only on a local or private address and is reachable only through the data layer's own components.
- An audit log of logins, registrations and administrative actions.
- Request IDs are unique per login. The stored hash of the request prevents a reused ID from returning the result of a different request (it fails with `request_id_conflict`).

## 10b. Durability

Full durability by default: a result is returned only after its commit is synced. A faster, relaxed mode may exist as an optional, clearly labeled unsafe setting. It is never the default.

## 10c. SQL compatibility mode (opt-in)

Status: **Approved**

Purpose: let tools that speak SQL connect to the server without changing the recipe model that beginners use. Targets are JDBC applications, benchmark harnesses (BenchBase, YCSB, sysbench) and power users who already know SQL.

**Two modes, chosen per login.**

- **Recipe mode** is the default. Everything in sections 1 to 10b applies unchanged.
- **SQL mode** needs the `sql` permission, which an admin grants per login. It is never granted by default.

**Rules for the `sql` permission.**

- It cannot be granted on a non-loopback address unless authentication and TLS are on (same rule as section 10).
- SQL logins have their own limits: statement timeout, row cap per result, connection cap, rate limit, memory per statement, temporary-file size per session, and one budget for the whole SQL pool; the cumulative cost of a long transaction is bounded by its duration limit. Admission control also watches the shard's commit latency and the oldest queued recipe write: when either is too high, new SQL statements are refused with `overloaded` and a retry hint, so SQL load cannot slow recipe writers.
- Granting and using it are written to the audit log.
- Schema creation (for example a benchmark loading its tables) needs the `admin` permission. The usual pattern is an admin login for setup and a `sql`-only login for the measured run.

**Protocol.** A PostgreSQL wire-protocol adapter sits at the gateway. Stock PostgreSQL drivers (including pgjdbc) and tools built on them connect without a custom JDBC driver. SCRAM-SHA-256 login and TLS reuse the gateway code. Because shards are PostgreSQL, statements pass through to the shard after the gateway's checks (login, limits, the restricted role) with no dialect translation. The adapter is off unless enabled, and recipe-mode users never see it. A SQL login calls a recipe with a reserved statement, `CALL recipe_name(args)`, which the adapter intercepts and sends through the planner instead of straight to the shard. The adapter generates one request ID per call, so a retry after a timeout is not deduplicated (the same rule as other SQL statements). `CALL` is refused inside an open transaction, because the recipe runs in its own commit and would not be part of it. It needs prepared statements (the extended protocol). Prepared statements belong to one connection, so each SQL session is pinned to its own connection to the shard, not drawn from the shared writer pool.

**Lockdown stays on.** SQL logins run through their own restricted database role, separate from the recipe role (section 9). It blocks loading extensions and schema changes outside the explicit admin action, and has no rights on the data layer's internal tables (request table, change log), so it cannot forge request IDs or edit history. It can only read recipe-managed tables; it can write only to tables in the `sql` schema, which an admin creates.

**What SQL mode changes.**

| Topic | Recipe mode | SQL mode |
|---|---|---|
| Idempotency | Request ID in the same commit; a retry returns the first result | Clients do not send request IDs. A retry after a timeout is not deduplicated. Must be documented |
| Bounds | Checked at registration ("say no early") | Checked at runtime: statement timeout, row cap, transaction length |
| Shards | One shard, several shards (saga) or all shards | One shard only. If the shard map has more than one shard, SQL logins are refused until the statement-to-recipe bridge exists |
| Transactions | One atomic command | Autocommit statements and multi-statement transactions both run on the session's pinned connection, under the SQL limits; the transaction lane rules (below) bound the long ones |
| Reads | Cache, then replica, then primary | Always the primary, so a session sees its own writes |
| Durability | Full by default | Same. Benchmark results must state the setting |
| Recipe rules (overflow rejection, subject-row creation) | Enforced | Cannot be bypassed: recipe-managed tables are read-only to SQL logins. A SQL login changes them only by calling a recipe (section 9) |

**Tables in SQL mode.**

- Recipe-managed tables (including every table a kind creates) are read-only to SQL logins. A SQL login changes them only by calling a recipe.
- SQL logins write only to tables in the `sql` schema, created by an admin login. Recipe rules do not apply there, and the docs say so.
- Writes to `sql`-schema tables reach the change log shortly after commit, read from PostgreSQL's write-ahead log by logical decoding, not by triggers (triggers add work inside every commit, and a role with enough rights can disable them). The "same commit" guarantee of section 4 therefore does not cover them. The reader's retained WAL is capped, with an alarm before the cap. If a lagging reader hits the cap, it is reset and the change log for `sql`-schema tables is marked as having a gap, instead of letting the disk fill. Logical decoding needs the instance's `wal_level` set to `logical`, which is done only when SQL mode is first enabled (one restart, announced in advance), so recipe-only installs do not pay for it.
- `sql`-schema tables have no derived stores in the first versions. They are outside the rebuild guarantee of section 2, principle 3.
- `sql`-schema tables are unsharded and live on the first shard. Recipes cannot see them. Adding a second shard turns SQL mode off (logins are refused, as in the table above) but leaves the tables and their data in place.
- Bulk loading into recipe-managed tables goes through a recipe or an admin import action, not raw SQL.

**Transaction lane.** A multi-statement transaction runs on its session's pinned connection to the one shard, from its first statement until commit or rollback. It holds that connection and its row locks, so it is bounded:

- SQL sessions draw their connections from their own pool, capped separately and never taken from the recipe writers' pool. A stuck SQL transaction therefore cannot use up the recipe writers' connections. It can still hold row locks that recipe writes wait on, so lock waits have a short timeout.
- Each transaction has a maximum duration, an idle-in-transaction timeout (the session is closed and the transaction rolled back) and a lock-wait timeout.
- A cap on how many transactions may be open at once. When a cap is reached, a new transaction is refused with `overloaded` and a retry hint, as in admission control, and is not queued.
- The shard's PostgreSQL connection total (recipe writers, the SQL pool, and the data layer's own backup and replication connections) is fixed and checked at startup, so the sum can never exceed what the instance allows.
- The cost is stated openly in the docs.

**Benchmark rules.**

- Run with full durability and say so in the results.
- Run on one shard. Cross-shard sagas have an in-between state, so no result may be presented as TPC-C compliant on a sharded setup.
- Do not compare directly against replicated cloud systems unless replication is configured.
- Run published benchmarks on native Linux, not WSL, because WSL adds a virtual disk layer that changes sync times.
- Expect the scale of hosted benchmark runs (for example tens of GB and 30-minute runs) to be beyond what a stage-0 single shard is designed for.

**Order of compatibility work.**

1. NoOp and YCSB: simple key reads, inserts, updates and range scans.
2. SmallBank, TATP and Voter.
3. TPC-C, last, because it is the heaviest test of the transaction lane.

**Later: graduation path.** SQL mode could register each distinct statement shape (literals replaced by parameters) as a recipe, so a team that starts in SQL can move to recipes, lanes and sharding without rewriting. Not needed for the first version.

---

## 11. Failure handling

| Failure | Handling |
|---|---|
| Client retries after a timeout | Request ID stored in the same commit; the retry returns the first result. Request IDs are kept at least as long as the SDK's maximum retry window (default 24 hours; imports excepted, section 9), and each request carries its creation time so an expired retry is rejected, not re-applied |
| Overload, or a slow disk | Admission control rejects when the oldest queued write has waited too long (not only when the queue is full); the SDK backs off with jitter |
| Commit time suddenly rises (for example checkpoint stalls) | Self-tuning checkpoints; a commit-latency alarm; recovery of normal latency after load drops is a test requirement |
| Derived data stale or lost | Rebuilt from the shards' data and change logs; stale reads are declared per recipe |
| Primary machine dies | Single node: restore from backups, with downtime. With replicas: an admin promotes one, after confirming the old primary is stopped (the engine manager stops it, or the admin confirms the machine is off) so two primaries cannot both accept writes (automatic promotion comes later, section 12) |
| A shard's PostgreSQL crashes | The engine manager restarts it and it recovers from its own log; until then requests get `overloaded` with a retry hint |
| Transaction DB machine dies | The standby takes over; the failed machine restarts and rejoins as the standby (section 7) |
| Crash during a cross-shard transaction | Recovery job resumes from the Transaction DB; steps are idempotent |
| Stolen credential | Per-login permissions, rate limits, TLS pinning, audit log |
| Idle or abusive connections | Idle timeout and connection caps at the gateway |
| PostgreSQL major-version upgrade fails | The old version and its data stay in place; the developer is told (section 6) |
| Bad schema change | Automatic backup, dry run, and destructive changes only through an explicit admin action |
| Hot row | One owner shard, with counters in the derived stores |
| Wrong shard map | Map changes are versioned and applied only through the control plane; a request that reaches the wrong shard is refused, not applied |
| Control plane down | Gateways and planners keep serving from their cached shard map and compiled recipes; registrations, migrations, slot moves and the admin UI wait |
| Gateway or planner copy dies | No durable state; the SDK retries against another copy (a single-copy install restarts it) |
| Worker dies or falls behind | Resumes from its last change-log position; derived reads are stale as declared; the admin UI shows the lag |
| Memory cache lost | Reads fall through to replicas and the primary; admission control protects the shard while the cache refills |
| Read replica dies or lags | Reads fall back to the primary or another replica; a lagging replica is taken out of rotation |
| Witness unreachable | Nothing changes while primary and standby can see each other; a failover cannot happen until a majority exists again |
| Backup target unreachable | Alarm; the shard keeps running; restore readiness is reported as degraded |
| Disk nearly full | Alarm at a set threshold; new writes are refused with `overloaded` before the disk is full, so PostgreSQL never has to stop on a full disk; change logs and old backups are trimmed by their retention rules |

## 12. Scaling stages

| Stage | What runs | When |
|---|---|---|
| 0 | One machine, one command to start everything, including a PostgreSQL the data layer installs and runs itself; one shard | Default. What a beginner runs |
| 1 | Read replicas and a second gateway copy | Reads grow faster than writes |
| 2 | Multiple shards (each on its own disk), separate worker processes, the Transaction DB in use for cross-shard commands (with its standby and witness, section 7), and a replicated control plane | Write capacity of one shard is reached |
| Later | Automatic failover for shard primaries (a read replica is promoted without manual steps) | Availability demands it |

A load balancer or proxy is added when a second node of something exists. It is not needed in stage 0.
