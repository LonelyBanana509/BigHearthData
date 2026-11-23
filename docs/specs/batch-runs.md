# Batch runs (spec)

**Status (2026-11-23): written, not built.** The write path already carries everything a batch run needs: `Command.retention = Permanent`, the derived request id, and one command per subject through the normal queue (`write-path.md`, section 9). The action itself (the record, the runner, `bhdatactl batch`) is built with import in M10, which shares its machinery. Every default below is changeable; section 8 lists the questions.

## 1. What this is

A recipe's limits are per run on one subject (`recipe-compiler.md`, section 1). Work over many subjects, such as interest for every user, is therefore not a recipe that scans. It is a **batch run**: an admin action that runs one named recipe, with one set of arguments, once per subject. Each run is a separate command on the subject's own shard, with its own request id, through the same queue as every other command. There is no batch transaction; what makes a batch safe to stop, repeat and resume is that each command is idempotent by its request id.

## 2. Decisions

1. **A batch is named by an id the admin chooses** (`interest-2027-01-31`): 1 to 24 characters of `a-z 0-9 -`. No colon, dot or underscore, so the derived ids below cannot be read two ways. A scheduler that runs interest daily passes the date in the id; a missed or doubled schedule is then harmless, because a second start with the same id is a resume or a refusal, never a second application.
2. **The derived request id is `batch:<batch id>:<sha256 of the subject id, 64 hex>`**, at most 95 characters. This replaces the form `batch:<batch id>:<subject id>` that `write-path.md` section 9 first gave: a subject id may be 128 characters and may contain `:`, so that form can exceed `MAX_ID_LEN` and is ambiguous to read. The hash is fixed-length and cannot collide in practice. Readability is not lost: the request record stores `subject` (migration 3), and the batch record below is what the admin UI shows.
3. **The login id is `batch:<batch id>`** (at most 30 of the 64 allowed). The request key is `(login_id, request_id)`, so batches never share keys with clients, with import, or with each other.
4. **The recipe version is pinned at start.** "Latest" is resolved once, stored in the batch record, and every command names it (`version_named = true`). A recipe registered half-way through the run does not split the batch across two versions, and a resume runs the same one. Retiring that version is refused while the batch is unfinished, as for imports (design section 7).
5. **One recipe, one argument set, no counterparty.** The arguments are typed against the plan once, stored in the record, and sent unchanged for every subject. A recipe that needs a counterparty is refused at start (`bad_arguments`): a run is one subject on one shard.
6. **The subjects** are every subject row of the recipe's subject table, read in id order in pages through the admin pool, or the ids in a file given with `--subjects`. The list is read again on a resume, not stored: subjects that finished return their stored result (`replayed`), subjects created since are run, and no subject runs twice. A run is "every subject that existed when it was read", not a snapshot; a subject created during the run may be missed, and the next batch id picks it up.
7. **Retention is `Permanent`.** The records are never compacted, so a batch can be resumed after any delay, and a finished subject's request id never expires. The cost is one small record per subject per batch, the same trade as import (`write-path.md`, section 9).
8. **`requested_at` is the time of sending**, not the start of the batch. The 24-hour rule of `write-path.md` section 4 then never expires a long run or a late resume, and a replay with a fresh `requested_at` still finds its stored record.
9. **Through the normal queue, with a lower share.** The runner keeps at most `batch.inflight` commands in the queue's hands (default 32, half of the default recipe pool), so interactive traffic is not starved. On `overloaded` it waits the hinted time and sends the same id again (nothing was stored). After ten refusals in a row for one subject the run pauses as `stopped (overloaded)`, and a resume continues.
10. **What ends a subject, and what stops the batch.**

    | Command ends | The batch |
    |---|---|
    | `done` | counts it, goes on |
    | `failed` with a named error (`insufficient_funds`, a recipe's own name) | counts it under that name, goes on: for most batches this is normal, and the final outcome is stored like any other |
    | `overloaded` | retries as above |
    | `internal`, `bad_arguments`, `wrong_shard` | **stops** and reports the subject and the error: the plan or the arguments are wrong for the whole batch, not for one row |

    This differs from import on purpose: a bad import row means a bad file, a failed subject in a batch means a normal outcome.
11. **The batch record** (a table in `bhdata_internal`, written by the admin role only; the recipe role cannot touch it): batch id, recipe, pinned version, arguments and their hash, status (`running`, `stopped`, `cancelled`, `done`), the reason it stopped, counts of `done` and of each named failure, `replayed`, started and ended, who started it. Counts are written in batches, not per command. The record is permanent and is the only thing that says a batch exists; the request records say what each subject got.
12. **Start, stop, resume, repeat.**
    - Starting an id that does not exist creates the record and runs.
    - Starting an id that is `stopped` or `cancelled` with the same recipe, version and arguments **resumes**. With other arguments it is refused up front (`bad_arguments`), before any command is sent; the write path would also answer each subject `request_id_conflict`, but nothing should get that far.
    - Starting an id that is `done` is refused with the meaning of `already_imported` (the same rule: a finished run is not run again). The admin who really wants it again uses a new batch id, which is audited like any start.
    - **Cancel** stops sending. Finished subjects stay finished; it undoes nothing.
    - **Dry run** reads the subject list, checks the recipe, version and arguments, reports the count and sends nothing.
13. **Audit.** Start, resume, stop and cancel go to the audit log of M5 and M8 (who, batch id, recipe, version, argument hash).
14. **Sharding.** At stage 0 there is one shard. With more (M15), the runner reads the subject list shard by shard and sends each command to its subject's shard; the ids and records are unchanged. A batch never holds locks on two shards, since each command is one subject.

## 3. Interface

```text
bhdatactl batch run <recipe> --batch-id <id> [--version <n>] [--arg name=value]...
                     [--subjects <file>] [--inflight <n>] [--dry-run] [--config <absolute path>]
bhdatactl batch status <id>
bhdatactl batch cancel <id>
```

Local admin only, as the other admin commands; the gateway has no batch endpoint. `run` prints one line of JSON per progress report and a final summary: `{"batch":"…","status":"done","done":9812,"failed":{"insufficient_funds":3},"replayed":0}`.

## 4. How it is checked

Already covered by the write-path suite (M4): a `Permanent` record is never compacted and a running one is never moved (test 17); one key with other payloads is a conflict (test 2); one key from several tasks applies once (tests 1 and 19). Built and tested with the action in M10:

| # | Test | Expected |
|---|---|---|
| B1 | A batch over many subjects, stopped by killing the runner and PostgreSQL at random moments, then resumed until `done` | Each subject applied exactly once; the counts equal the number of subjects |
| B2 | The same batch id started twice at once | One runner proceeds, the other is refused; nothing applied twice |
| B3 | A subject whose command fails with a named error | Counted under its name, stored, the batch finishes |
| B4 | A recipe that returns `internal` for one subject | The batch stops and names the subject; nothing else is lost |
| B5 | A recipe version registered during the run | The run keeps the pinned version |
| B6 | A resume with other arguments | Refused before any command is sent |
| B7 | Subject ids of 128 characters with colons | Derived ids stay inside the limit and are distinct |
| B8 | A resume after more than 24 hours | Finished subjects replay, the rest run, no `request_expired` |
| B9 | A flood of interactive commands during a batch | Their latency stays inside the admission limits |

## 5. Left out

A scheduler (use cron or a systemd timer with a dated batch id); priorities beyond the in-flight limit (after the M11 load runs); per-subject arguments (a recipe whose arguments differ by subject is a different feature, and import covers file-driven values); a batch over subjects of several recipes; progress shown in the admin UI (M8, which reads the batch record); undoing a batch (nothing is undone, as for import).

## 6. Questions for you (my default in brackets)

1. **Change the derived id** to the hashed form in decision 2, and amend `write-path.md` section 9? [Yes; the old form can exceed 128 characters and is ambiguous.]
2. **A named failure does not stop a batch**, and an `internal`, `bad_arguments` or `wrong_shard` does (decision 10)? [Yes.]
3. **The version is pinned at start** (decision 4)? [Yes.]
4. **Subjects are re-read on resume** rather than stored as a snapshot (decision 6)? [Yes; simpler, and idempotence covers the overlap.]
5. **In-flight limit 32** as the default (decision 9)? [Yes, a placeholder until M11.]
6. **A finished batch id is refused**, not rerun (decision 12)? [Yes.]
