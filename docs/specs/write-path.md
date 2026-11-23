# Spec: Write path (M4)

Spec v0.2 · 2026-11-23 · Slices A and B are built; every decision below stays changeable (section 12). Covers milestone M4 of `docs/project-structure-plan.md` (crates `bhdata-writer` and `bhdata-changelog`, plus small additions to `bhdata-recipe`, `bhdata-api`, `bhdata-pg` and `bhdata-types`). Section numbers marked "design" refer to `bighearthdata-design-v1.0.md`. Builds on `pg-access.md` (the request table, the change log and the three narrow functions) and `recipe-compiler.md` (the plan the writer runs).

**Status (2026-11-23): slices A and B are built; slice C is built except test 19 and the clean-VM run.** Your "for now" was taken as a yes to all eight defaults in section 12, so everything stays changeable. Slice A is the evaluator (`bhdata-recipe/src/eval.rs`), the `[writer]` config section, `Command`/`Outcome`/`ShardBackend::execute` and the payload hash (`bhdata-api/src/shard.rs`), the time rule, retry policy, admission with the commit-latency watch and the result encoding (`bhdata-writer`), and the entry format, the `(xid, seq)` position and a pure model of the horizon read (`bhdata-changelog`). Where the code settled something the text left open:

- **`and` / `or` stop at the left side** when it decides the answer, so `b != 0 and ...` works as a guard. The compiler still type-checks both sides. This is a rule of the expression language and belongs in `recipe-format.md` section 2 when you confirm it.
- **Times are `Timestamp` (milliseconds)**, as in `bhdata-types`; an evaluated `now` and a stored timestamp value are whole microseconds.
- **`ShardBackend` has `execute` only** for now. `read` joins it with M6 and `status` with M8, so no method exists without an implementation behind it.
- **The reader statement** compares `(xid, seq)` with the parameters cast to `xid8`, so an index on `(xid, seq)` can be used. Slice B moved it into the function `read_changes` (below), run on a real server.

**Slice B (built).** Migration 3, `exec.rs`, the change-log reader and trim, `history.rs`, and tests 1 to 13, 17 and 18 (`crates/bhdata-writer/tests/write_path.rs`, 17 `--ignored` tests, each on a private PostgreSQL 16; repeated runs at full parallelism were stable). What the code settled:

- **Question 1 is built.** `Error::RecipeDefined(name)`, `ErrorKind::RecipeDefined` (wire name `recipe_defined`) and `Error::from_recipe_name`, which keeps `insufficient_funds` and `overflow` as their own variants. Plan section 3 lists it.
- **Migration 3** is as section 11 says, plus four functions for the admin role only: `read_changes`, `compact_requests`, `purge_history`, `trim_change_log`. So that the admin can call them it now has `USAGE` on schema `bhdata_internal` (still no right on any table). The recipe role has none of the four (test 13). The recipe role's `deadlock_timeout` is 100 ms, set with the other timeouts.
- **`claim_request` now has eight arguments** (the old five-argument one is dropped); `finish_request` also sets `finished_at`. `compact_requests` is one `DELETE ... RETURNING` feeding one `INSERT` in a single statement, so a crash leaves a record in exactly one table; it skips locked rows and never moves a `running` or permanent record.
- **`Executor::run` returns `Finished { outcome, commit_time }`**; slice C's queue feeds `commit_time` to the latency watch. `with_owner_check` supplies the shard-ownership rule behind `wrong_shard` (stage 0 owns everything).
- **Statements are prepared with explicit parameter types** (a decimal is sent as text and cast with `::numeric`; a timestamp as whole microseconds), so no float is involved. Prepared statements are cached per connection.
- **A child row's key needs its `seq`**, which the plan's frozen `insert` statement does not return. The writer reads the owner's highest `seq` right after the insert (the advisory lock is held, so it is the new row's) and takes the table's name from the `INSERT INTO app."<table>"` text, refusing any other shape. Changing the plan to carry it would change every registered plan's hash, so it was not done.
- **A step on a subject or counterparty row that does not exist** (a recipe with no `ensure` on a new id) is `bad_arguments` ("the subject does not exist"), rolled back.
- **A `list` in a result** is a JSON array of objects, in row order.
- **`overloaded` under contention is real.** Thirty commands on one row with sixteen connections can pass the role's 2 s `lock_timeout` on a busy machine; nothing is stored and a retry with the same id succeeds. That is the pressure the slice C queue is for, and the tests that start many commands at once retry like a client.

**Slice C (built, except test 19 and the clean-VM run).** What the code settled:

- **`bhdata-writer/src/queue.rs`**: the bounded FIFO queue with one writer task per recipe connection, generic over a `CommandRunner` (the `Executor` is the real one) and implementing `ShardBackend`. It refuses before queueing by the admission rule, drops a command that waited four times the limit, feeds each commit time to the latency watch, skips a command whose caller has gone away (nothing was stored; a retry settles it by id), and `shutdown(drain)` refuses the waiting and the new, then waits up to `drain` for the running. `QueueStatus` is its snapshot.
- **`bhdata-writer/src/compaction.rs`**: the per-shard timer job (`run_round`, `CompactionJob`): compacts in batches, then purges history, through the admin functions.
- **`bhdata-writer/src/startup.rs`**: `verify_synchronous_commit` (decision 14): `on` for full durability, `off` for relaxed, read as the recipe role sees it.
- **Host wiring is in `bhdata-server`, not `bhdata-engine`.** `bhdata-writer` already depends on the engine for its tests, so an engine to writer edge would be a cycle. Instead `host::run_with` / `server_main_with` take a hook that runs after the bootstrap and the login check and returns whatever must stay alive; `bhdata-server/src/write_path.rs` is that hook (pools, the `synchronous_commit` check, the queue, the compaction job, on a two-thread runtime of its own). A refused check stops the start.
- **`bhdatactl status` does not show the queue yet.** `status` runs in another process and cannot read the server's in-memory queue; that needs a status channel, which arrives with M8. `QueueStatus` is ready for it.
- **`bhdatactl call` reads files, not the registry** (decided with you; the planner of M5 will own plan loading): `bhdatactl call <recipe file> --schema <schema file> --subject <id> [--counterparty <id>] [--arg name=value]... [--request-id <id>] [--config <absolute path>]`. The schema must already be applied and the recipe registered, because that gives the recipe role its rights. It runs one command through the `Executor` in its own process (so, not through the server's queue), as the service user, and prints one line of JSON.
- **Tests.** The queue on a fake runner and a manual clock covers test 14 and the alarm part of test 15; test 16 is `the_writer_refuses_to_start_when_synchronous_commit_does_not_match_the_durability` in `write_path.rs` (`--ignored`, never run in the sandbox: no PostgreSQL). The real-database part of the commit-latency test (15) and test 19 (the property test) are not written yet.

Plan acceptance for M4: **the write-path suite passes** (section 10). The plan calls this the most heavily tested part of the project, and says to finish and harden it before M5.

## 1. What this spec fixes

The exact order of everything that happens between "a command is admitted" and "the reply is sent", for a command whose rows all live on **one shard**. Cross-shard commands (design section 7, M15) are out of scope, but the writer is built so that a saga step is just a command with a derived request id (section 9). This spec also fixes three things the earlier specs left open:

1. a change-log read can **skip an entry** if it reads by sequence number alone (section 6);
2. **where "request expired" is decided**, so the answer never depends on whether a cleanup job has run (section 4);
3. **how deadlocks between two rows on one shard are kept short** (section 5).

## 2. Decisions in this spec (change any of them)

1. **A command is a plan with at least one write.** A plan whose `writes` list is empty is a read. It never enters the write path: no request record, no queue. It is run by `ShardBackend::read` (M6). Sent to `execute`, it is refused as `internal`.
2. **One command, one transaction, one commit.** `READ COMMITTED`. The plan's own SQL (row locks by `FOR UPDATE`, bounds in the `WHERE` of an `UPDATE`) is what makes concurrent commands correct; `SERIALIZABLE` is not used.
3. **Only final outcomes are stored.** A command that ends `done` or `failed` (a named error from the recipe) stores its record. Everything else (`overloaded`, `internal`, `bad_arguments`, `request_expired`, `request_id_conflict`) rolls back **everything, including the claim**, and stores nothing, so a retry runs fresh. The one exception is a retry that finds a stored outcome: it returns that outcome.
4. **The reply is sent only after `COMMIT` returns.** If the connection breaks during `COMMIT`, the outcome is unknown; the client gets `overloaded`, and its retry settles it through the request id (section 3).
5. **Writers evaluate expressions in Rust, with exact arithmetic**, as the plan says (`recipe-compiler.md`, section 10). The evaluator is new code in `bhdata-recipe` (`eval.rs`, pure, no database), re-exported through `bhdata-api`. No new dependency edge.
6. **Change-log entries point at rows; they carry no values** at M4 (section 6). The values question belongs to `change-log-and-rebuild.md`, before M7.
7. **Change-log positions are `(xid, seq)`, and readers stop at a safe horizon** (section 6). Without this, a reader can lose entries. This needs migration 3.
8. **Request expiry is decided by the clock alone**, from the request's own creation time (section 4).
9. **Bounded automatic retry** of deadlocks and serialization failures: 3 attempts in all, with a short jittered pause (section 5).
10. **Admission control rejects on the age of the oldest queued command, not only on a full queue** (section 7). A rejected command has done no database work, so it has no record.
11. **Compaction moves a request record into a history row in one statement**, so there is never a moment when neither exists (section 8). The recipe role cannot delete anything; compaction runs as `bhdata_admin` through a narrow function.
12. **Import and batch runs use the same entry point** with derived request ids and permanent records (section 9). The batch-run feature itself still needs its own spec before M4 is final (plan open item 4); this spec only provides what it will need.
13. **`deadlock_timeout` is set to 100 ms for `bhdata_recipe`** (PostgreSQL's default is 1 s, longer than the role's 2 s `lock_timeout` allows for a retry).
14. **The writer refuses to start if the server's `synchronous_commit` does not match the configured durability** (`on` for full, as `bhdata-engine` writes it). A reply after a commit that was not synced would break "full durability by default".
15. **A local dev command, `bhdatactl call`,** runs one recipe as the local admin (section 11). It makes the clean-VM run and the by-hand checks possible before the gateway exists (M5). It is not a client interface.

## 3. One command, step by step

Input, built by the caller (the planner at M5, tests and `bhdatactl call` at M4):

```
Command {
    login_id, request_id,          // the request key (limits as in bhdata-types)
    requested_at,                  // when the client created the request (section 4)
    plan, version, version_named,  // the compiled recipe and which version runs
    subject, counterparty?,        // shard keys, as text ids
    args,                          // typed values, one per plan argument
    retention,                     // Window (default) or Permanent (import, batch; section 9)
}
```

Steps. Each is one statement, or one small group sent together, on the **same connection** taken from the `bhdata_recipe` pool for the whole command.

0. **Checks that need no database.** Every argument is present, has the plan's type and is inside the plan's bounds, else `bad_arguments` (stored: no). `requested_at` is inside the allowed window (section 4), else `request_expired` (or `bad_arguments` if it is in the future). The plan has a write (decision 1).
1. **Queue.** The command waits in the shard's queue (section 7). A writer connection takes it.
2. **`BEGIN`.** Isolation `READ COMMITTED`. The role's `lock_timeout`, `statement_timeout` and `deadlock_timeout` apply.
3. **`claim_request(login, request_id, payload_hash, recipe, version, subject, requested_at, permanent)`** (the function changes in migration 3). It returns:
   - `new`: the record exists as `running`, locked by this transaction. Go on.
   - `same`, with the stored status and result: an identical retry of a finished command. **Return the stored result and `COMMIT` (nothing was written).** No step runs. The reply says it was replayed.
   - `conflict`: same key, different payload hash. `ROLLBACK`; reply `request_id_conflict`.
   - A concurrent identical command is not a fourth case: its `INSERT` waits on the row lock until this transaction ends, then sees `same` (or, if this one rolled back, `new`). Both get one result. The wait is bounded by `lock_timeout` (then `overloaded`, a retry settles it).
   - `same` with status `running` cannot happen while the lock is held; if it is ever seen it is `internal` (a bug).
4. **`SAVEPOINT s`.**
5. **Run the plan's steps in order**, as `recipe-compiler.md` section 10 defines each kind. Before each step the writer checks the time budget (`bounds.time_ms`, default 2,000 ms, measured from step 2). Out of time means `StatementTimeout`: roll back everything, reply `overloaded`, store nothing. The role's 5 s `statement_timeout` is only the backstop for one runaway statement.
   - For every write step the writer notes the **rows affected**. `ensure` that affected 0 rows (the row existed) is not a change.
   - A `check` that fails, an `add` that hits `below` or `above`, and a unique violation on `insert` that the plan names, are **named errors**, not failures of the system. Go to step 6b.
   - Any other database error is mapped by SQLSTATE (`pg-access.md`, section 4). Deadlock and serialization go to section 5; the rest roll back, store nothing, and reply as that table says.
6. **Outcome.**
   - **6a. Success.** `RELEASE SAVEPOINT s`. Collect the changes (section 6), call `append_change` once per changed row, build the result (section 7), call `finish_request(…, 'done', null, result)`.
   - **6b. Named error.** `ROLLBACK TO SAVEPOINT s` (this also undoes `ensure`, so a failed first command leaves no subject row), then `finish_request(…, 'failed', error_name, result)`. No `append_change`.
7. **`COMMIT`.** One synced commit (full durability). The record, the data changes and the change-log entries become durable together or not at all.
8. **Reply.** `Outcome { status, result, replayed }`, or the named error for a `failed` record. Only now.

What each ending leaves behind:

| Ending | Record | Data | Change-log | Reply |
|---|---|---|---|---|
| Success | `done` | applied | one entry per changed row | the result |
| Named error | `failed` + name | none | none | the named error |
| Identical retry of a finished command | unchanged | none | none | the stored outcome, `replayed` |
| Same key, other payload | unchanged | none | none | `request_id_conflict` |
| Out of time, lock timeout, deadlock after the last attempt, connection lost, disk full | none | none | none | `overloaded` (with a retry hint) |
| Unexpected database error, broken plan | none | none | none | `internal` |
| Crash of PostgreSQL before `COMMIT` returns | none, or all of it | all or nothing | all or nothing | `overloaded`; the retry finds out (step 3) |

Round trips are one per statement at M4. Sending the `append_change` calls, `finish_request` and `COMMIT` as one pipelined batch is a later optimisation, to be decided from the M11 numbers.

## 4. Request key, payload hash and time

**The key** is `(login_id, request_id)`, as in the M2 table.

**The payload hash** is SHA-256 of canonical JSON (the plan's canonical form: sorted keys, no spaces) of `{recipe, version, subject, counterparty, args}`:

- `version` is in the hash **only if the caller named one**, else `null`. A caller that asked for "latest" and retries after a new version was registered must not get a false `request_id_conflict`. On `same`, the stored version is the one that ran.
- Arguments are hashed **after** typing: a decimal is normalised (`1.50` and `1.5` are one value), a timestamp is whole microseconds.
- `requested_at` and `retention` are not in the hash.
- The function is pure, in `bhdata-api`, so the planner, the writer and the tests use one copy.

**`requested_at`** is the creation time the client's SDK puts on the request (design section 11). The rule, decided by the writer from its own clock before the queue and again before the claim:

| `requested_at` | Result |
|---|---|
| older than `now − retry_window` (default 24 h) | `request_expired`; nothing is looked up, nothing is stored |
| more than `request_time_tolerance` (default 5 min) ahead of `now` | `bad_arguments` ("request time is in the future") |
| otherwise | go on |

Because the answer depends only on the clock, a retry just after the window gets `request_expired` whether or not the cleanup job has already moved the record. That removes the question "was it compacted yet?".

**Retention.** The record is kept until `requested_at < now − retry_window − retention_grace` (default grace 1 h, so a client whose clock runs a little ahead still finds its record for as long as the rule above lets it ask). A `Permanent` record (section 9) is never compacted and skips the age rule.

**Stored time.** `requested_at` is stored with the record (migration 3) and is what compaction uses. `created_at` stays the server's insert time, for diagnosis.

## 5. Locking, retries and deadlocks

- **Row locks** come from the plan: `read` takes `FOR UPDATE` when a later step writes the same row; `add` is one `UPDATE` with its bounds in the `WHERE`. Under `READ COMMITTED` PostgreSQL re-checks the `WHERE` on the latest row version after a wait, which is what makes the bound exact. `insert` takes an advisory transaction lock on (table, owner), as the compiler specified.
- **Duplicates serialize on the request row** (step 3), before any data statement runs.
- **Two rows on one shard** (a counterparty on the same shard) can deadlock: `give` A to B and B to A at once. PostgreSQL detects it after `deadlock_timeout`. Decision 13 sets 100 ms for the recipe role (`ALTER ROLE … SET`, done by the bootstrap with the other timeouts). Deadlocks across shards cannot happen in a single command; sagas never hold two shards' locks (M15).
- **Retry.** `Deadlock` and `Serialization` (`pg-access.md`, section 4) roll the whole transaction back, drop the attempt's results, wait 5 to 25 ms (random) and run again from step 2. `writer.max_attempts` is 3, counting the first. After the last: `overloaded` with a retry hint, nothing stored. Safe because nothing was committed.
- **A broken connection** is discarded and replaced (the pool already does this for `Unavailable`). The command is **not** retried by the writer in this case: the commit outcome may be unknown for step 7, and one rule is simpler. The client's retry settles it.

## 6. The change log

**Entries.** One entry per **changed row** per command: `(table, row_key)` is the identity; two steps that touch the same row give one entry. `op` is `insert` if the command created the row (an `ensure` that affected 1 row, or an `insert`), else `update`. `subject` is the **shard key of the row's owner** (the counterparty's id for a counterparty row), so a worker can route by it. `row_key` is the row's key as text: the owner's id for a subject row, `"<owner>/<seq>"` for a child row. There is no `delete` at M4 (no plan needs it).

**No values.** An entry says "this row changed", nothing more. A derived store that needs the new value reads the row (it is allowed to be slightly stale, as declared), or, for a counter, is given a delta by a later format. The reason is ordering, below. Payload columns are decided in `change-log-and-rebuild.md`; adding a nullable column later is a cheap migration.

**The problem with `seq` alone.** `change_log.seq` is an identity column, and identity values are handed out when a statement runs, not when the transaction commits. Transaction A can take seq 10, transaction B seq 11, and B can commit first. A worker that has read up to 11 and remembers "position 11" never sees 10 when A commits. Derived stores would silently lose changes, which breaks design principle 3. The same happens with a full-durability commit that is slow while later ones finish.

**The fix.** Migration 3 adds `xid xid8 not null default pg_current_xact_id()` to `change_log`. A **position** is `(xid, seq)`. A reader asks for entries after its position **and below the horizon**:

```
WHERE (xid, seq) > ($last_xid, $last_seq)
  AND xid < pg_snapshot_xmin(pg_current_snapshot())
ORDER BY xid, seq
```

Every transaction with an id below the snapshot's `xmin` has finished (committed or aborted), so nothing can still appear below the horizon: a reader sees a gap-free prefix, in order. The position is stored by the reader (workers, M7; backup, M9). Entries of one transaction share an `xid` and keep `seq` order.

**What this does to ordering.** Order is by transaction id, which for two commands on **one row** can differ from their commit order (A gets its id, then waits for a lock held by B, which gets its id later but commits first). So consumers must **not** assume that entries for one row arrive in commit order. That is why entries carry no values (decision 6): a consumer that reads the current row on apply always converges. This rule goes into `change-log-and-rebuild.md` as a fixed constraint.

**Cost of the horizon.** One long transaction anywhere on the shard holds the horizon back, and derived stores lag. The role timeouts bound this (`bhdata_admin` may run 10 minutes; the admin's work must stay short). The reader reports the lag.

**Trimming** (`trim.rs`): delete entries that every reader's position is past **and** that are older than the change-log retention. Done in batches by a narrow function for `bhdata_admin`. M4 builds the format, the reader API and the trim function; the workers that use them are M7.

## 7. Queue, admission and results

**Queue.** Per shard, bounded (`writer.queue_capacity`, default 256), first in first out. `recipe_writers` connections (default 16, `pools`) each run one command at a time. No priorities at M4; per-login fairness is the gateway's rate limit (M5). Commands for one subject are not pinned to one writer: the row lock serializes them, and a hot row is handled by derived counters (design section 11).

**Admission** (`admission.rs`). A new command is **rejected before it is queued** when any of these holds:

- the queue is full;
- the oldest queued command has already waited more than `writer.max_queue_wait_ms` (default 500). A queue that is not full but not moving (a slow disk) is overload too;
- the commit-latency watch is above its alarm (below), and the queue is more than half full.

The rejection is `overloaded` with `retry_after`, a number that grows with how far over the limit the queue is (the SDK adds jitter). A rejected command did no work and left no record. A command that was admitted and then waits longer than `writer.max_queue_wait_ms × 4` is dropped at the front of the queue with the same reply, so a stuck shard does not hold requests for minutes.

**Commit-latency watch.** The writer keeps the 99th-percentile `COMMIT` time of the last 10 seconds. Above `writer.commit_latency_alarm_ms` (default 250) it raises an alarm (a metric and a log line, shown by `bhdatactl status` from M8). Recovery to normal after load drops is a test (section 10), as design section 11 requires.

**Shutdown.** Stop admitting (reply `overloaded`), let running commands finish for up to `writer.shutdown_drain_ms` (default 5,000), then close the connections. A command that did not finish was never committed, so its client retries safely.

**Results.** The stored `result` is canonical JSON, keys sorted: for `done`, `{"values": {name: value, …}}` in the plan's result order, decimals as text, timestamps as whole microseconds, bytes as base64; for `failed`, `{"error": "<name>"}`. The result comes from the plan: the values of the `read` or `list` steps named in `result`, or one final `result_fetch` read of the subject row inside the same transaction. A replay returns the stored bytes exactly; it never recomputes.

**Named errors in the record.** `error_kind` holds the name (`insufficient_funds`, `overflow`, or a name the recipe defined). Today `bhdata-types::Error` has no place for a recipe-defined name; see question 1 (section 12).

## 8. History and compaction

Two things end the life of a request record: the retry window passes, or the command was `Permanent` (never). The design says a compact history row stays for the history retention (default 90 days) and is **never used to re-apply** a command.

- **`request_history`** (migration 3): `login_id, request_id, status, error_kind, recipe, recipe_version, subject, requested_at, finished_at, copied_at null`; key `(login_id, request_id)`. No `payload_hash`, no `result`.
- **`compact_requests(cutoff, batch)`**, owned by `bhdata_owner`, `SECURITY DEFINER`, `EXECUTE` for `bhdata_admin` only: in **one statement**, `WITH moved AS (DELETE FROM requests WHERE requested_at < cutoff AND NOT permanent AND status <> 'running' … LIMIT batch RETURNING …) INSERT INTO request_history …`. Delete and insert are one statement, so a crash leaves both or neither. Batches of up to `writer.compaction_batch` (default 1,000), every `writer.compaction_interval_secs` (default 60), at most one running per shard. A `running` record is never touched.
- **`purge_history(cutoff, batch)`**, same ownership: deletes history rows older than `history_retention` **and**, when the central history copy exists (stage 2), only those with `copied_at` set. At stage 0 the shard's own tables are the history (design section 7), so the copy rule is switched off by a config flag that the Transaction DB turns on.
- **Later retry of a compacted command** is `request_expired` by the clock rule (section 4), before anything is looked up. The history row is for people and for the admin UI, not for the write path. A test checks that the writer never reads `request_history`.
- **The recipe role has no `DELETE` anywhere** (as in M2). Only the two functions above delete, and only for the admin role.

## 9. Hooks for import, batch runs and saga steps

The writer has one entry point, `ShardBackend::execute(Command)`. Three other features are commands with different ids and retention:

| Caller | `request_id` | `retention` | Notes |
|---|---|---|---|
| Client (gateway, M5) | the client's id | Window | the normal case |
| Import (M10) | `import:<file hash, 64 hex>:<row number>` | Permanent | the length fits `MAX_ID_LEN` (128). A rerun of a finished row returns its stored result, which makes a stopped import resumable |
| Batch run (a later spec) | `batch:<batch id>:<subject id>` | Permanent | one command per subject, one shard each |
| Saga step (M15) | `<transaction id>:<step number>` | Window, or longer by the transaction's own record | the per-shard step list runs through the same transaction layout |

`retention` can be set only by code that builds a `Command` directly (import, batch, saga). The gateway builds commands from client input and never sets it. Rows written for a `Permanent` command are the cost of "request ids for finished rows do not expire"; one small record per imported row, noted for M10.

## 10. Tests (the write-path suite)

Real-PostgreSQL tests are `--ignored` and live in `crates/bhdata-writer/tests/` (the engine as a dev dependency, as `bhdata-control` does). Pure tests run on every `cargo test`. The first eight are the plan's acceptance list; the rest are what this spec adds.

| # | Test | What must hold |
|---|---|---|
| 1 | Two concurrent commands, same key | One executes; both get the same result; one data change |
| 2 | Same key, different payload | `request_id_conflict`; nothing changed |
| 3 | Client gone just before `COMMIT`, and just after | A retry returns the single stored result, applied once |
| 4 | PostgreSQL killed during the transaction, and just after `COMMIT` | After recovery a retry applies exactly once (`pg_ctl -m immediate`, then the engine restarts it) |
| 5 | A failed check | A `failed` record; a retry returns the same named error; the data is unchanged; no change-log entry |
| 6 | Retry after the window | `request_expired`, not re-applied, decided with the record still present and with it gone |
| 7 | Retry after compaction | `request_expired`; `request_history` is never read by the writer |
| 8 | Every committed change | Exactly one change-log entry per changed row, in the same commit; none for a failed or replayed command |
| 9 | `ensure` then a failed check | No subject row is left behind |
| 10 | `give` both ways at once on one shard, many times | Both finish; deadlocks are retried; no `overloaded` when the load is small |
| 11 | Reader with interleaved commits (A takes a lower id, B commits first) | The reader never loses A's entry, and sees nothing above the horizon |
| 12 | Two commands on one row with inverted id order | Documented order is by id; a consumer that re-reads the row converges |
| 13 | The recipe role tries `DELETE`, `UPDATE requests`, a forged `append_change` for a finished request | All refused (`Forbidden` or `Refused`) |
| 14 | Admission with a fake clock | Rejects at "oldest waited too long" with the queue half empty; the retry hint grows with the overload |
| 15 | Commit latency raised (a slow-disk stand-in), then removed | Alarm raised, then cleared; throughput returns to the earlier level |
| 16 | `synchronous_commit` set differently from the config | The writer refuses to start |
| 17 | Compaction under load, and killed in the middle | No record is lost or duplicated; a running record is never moved |
| 18 | Arguments out of bounds, a read-only plan, a counterparty on another shard | `bad_arguments`, `internal`, `wrong_shard`; nothing stored |
| 19 | Property test: random interleavings of the same key from several tasks | Never two applications, never two different results |

## 11. What changes outside the two crates

- **`bhdata-recipe`**: `eval.rs`, an exact evaluator for the plan's `Param` expressions (decimals use the shared helpers in `bhdata-schema`). Pure; the same tests as the parser's, plus property tests against a plain big-integer reference.
- **`bhdata-api`**: `shard.rs` gets `ShardBackend { execute, read, status }`, `Command`, `Outcome`, and the payload-hash function. Nothing here talks to PostgreSQL.
- **`bhdata-pg`**: **migration 3** (append-only, as M2 decision 7): `requests` gets `subject`, `requested_at` (not null), `finished_at`, `permanent`; `change_log` gets `xid xid8`; `claim_request`, `finish_request` and `append_change` are replaced as needed; `request_history`, `compact_requests`, `purge_history` and the change-log trim function are added; the recipe role's `deadlock_timeout` is set. `errors.rs` is unchanged.
- **`bhdata-types`**: a `[writer]` config section (all defaults, unknown keys refused): `queue_capacity` 256, `max_queue_wait_ms` 500, `max_attempts` 3, `commit_latency_alarm_ms` 250, `request_time_tolerance_secs` 300, `retention_grace_secs` 3600, `compaction_interval_secs` 60, `compaction_batch` 1000, `shutdown_drain_ms` 5000. These are placeholders until the M11 load runs, like the pool sizes. And, if you agree to question 1, a place for recipe-defined error names.
- **`bhdata-engine` / `host`**: starts the writer and the compaction job after the bootstrap; `bhdatactl status` adds the queue length, the oldest wait and the commit-latency line.
- **`bhdata-cli`**: `bhdatactl call <recipe> --subject <id> [--counterparty <id>] [--arg name=value]… [--request-id <id>] [--config <absolute path>]`. Local admin only, one command, prints the outcome as JSON. Without `--request-id` it makes a fresh one and prints it, so a retry can reuse it.
- **`deps.allow`**: the writer uses `bhdata-api`, `bhdata-pg` and `bhdata-changelog`; the recipe types come through `bhdata-api`. Two edges are for its tests only: `bhdata-engine` (to start a private instance) and `bhdata-schema` (to create the tables).

## 12. Questions for you (my default in brackets)

1. **Recipe-defined error names.** `bhdata-types::Error` has the fixed names of plan section 3 and nothing for a name a recipe defines (`recipe-compiler.md`, section 9, rule 8: recipe-defined names are free). Add one variant that carries the name, with a matching `ErrorKind`, and update plan section 3? [Yes. Without it a recipe's own named error cannot reach a client.]
2. **No values in change-log entries at M4**, decided properly before M7? [Yes, section 6.]
3. **Migration 3** with the columns, functions and table in section 11? [Yes. Nothing is written by M2 or M3 into these tables yet, so there is no data to convert.]
4. **`bhdatactl call`** as a dev command for the VM run? [Yes.]
5. **Batch runs.** Keep the batch-run action as its own short spec (`batch-runs.md`) written after this one, before M4 is closed? [Yes; this spec only fixes the request ids and the retention.]
6. **Time defaults**: tolerance 5 min, grace 1 h, retry window 24 h (already in config). [Keep.]
7. **Reads of a plan with no write** go through `ShardBackend::read` in M6, not the write path. [Yes.]
8. **Slices.** A: the pure parts (evaluator, `Command`/`Outcome`, payload hash, time rule, result encoding, retry policy, queue and admission on a fake clock, change-log format and position). B: migration 3, `exec.rs`, the change-log reader and trim, and tests 1 to 13, 17 and 18 on a real server. C: queue and admission wired to real connections, compaction, the host, `bhdatactl call`, tests 14 to 16 and 19, then the clean-VM run on PostgreSQL 17. [Same shape as M1 to M3.]

## 13. What is checked where

| Check | Sandbox (PostgreSQL 16) | Your machine |
|---|---|---|
| Evaluator, payload hash, time rule, retry policy, admission on a fake clock, change-log format | Yes (`cargo test`) | `scripts/check.sh` |
| Migration 3, `exec.rs`, tests 1 to 3, 5 to 13, 17, 18, 19 | Yes (`--ignored`, a private instance) | Yes |
| Kill PostgreSQL during and after commit (test 4) | Yes (`pg_ctl -m immediate`) | Yes |
| Commit-latency test (15) | Rough (a throttled stand-in) | Native Linux for real numbers (M11) |
| PostgreSQL 17 | No | The clean-VM run |
| Reboot recovery of the service | No (no systemd) | The clean-VM run |

## 14. Left out of M4

Cross-shard commands and the Transaction DB (M15); the gateway, logins and rate limits (M5); reads, the cache and the planner (M6); the workers that read the change log (M7); backups (M9); the import action and the batch-run action themselves (M10 and `batch-runs.md`); change-log payloads (before M7); pipelined commit and any tuning (after M11); `DELETE` through recipes (not planned).
