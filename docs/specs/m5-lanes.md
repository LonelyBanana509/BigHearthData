# M5 lanes: building the gateway and client in parallel

**Status (2026-11-23): the shared contracts are in; the lanes can start.** This page says who owns
which files, what each lane builds first, and the few rules that stop two sessions from colliding.
The design itself is `wire-protocol.md`; this page changes none of it except where it says so.

## 1. What is already fixed (nobody edits these in a lane)

| Piece | Where | Notes |
|---|---|---|
| `[gateway]` config, every limit, `validate()` (loopback-only `insecure_local`) | `bhdata-types/src/config.rs` | Defaults are the ones in `wire-protocol.md` |
| `RequestHandler`, `Caller`, `Call`, `Reply` | `bhdata-api/src/handler.rs` | `Reply` is the write path's `Outcome` |
| `LoginStore`, `LoginRecord`, `ScramVerifier`, `Permissions`, `RecipeAllow`, `AuditEvent` | `bhdata-api/src/login.rs` | The gateway reads logins through this; `bhdata-control` implements it |
| One empty file per piece of work, already declared in each crate's `lib.rs` | `bhdata-proto`, `bhdata-gateway`, `bhdata-client`, `bhdata-control` | A lane fills its own files and never edits `lib.rs` |
| **One change to the plan, for you to confirm:** the SCRAM maths lives in `bhdata-proto::scram`, not in the gateway, and `deps.allow` gains the edge `bhdata-control` to `bhdata-proto` | `deps.allow` | The gateway, both clients and `bhdatactl login create` all need the same SCRAM code; one copy avoids four |

A change to any row above is a change to the contract: stop, tell the others, change it once.

## 2. The lanes

| Lane | Builds | Owns (only these) | Needs first | Tests it ends with |
|---|---|---|---|---|
| **P1** SCRAM | `scram`: client and server messages, proofs, verifier derivation, the answer for an unknown name | `bhdata-proto/src/scram.rs` | nothing | W2 (RFC 7677 vectors), part of W16 |
| **P2** frames and messages | `frame`, `json`, `message`, `error_map`; argument conversion (one function in `bhdata-recipe` next to `Value`); the fuzz targets for the frame reader and the JSON parser | `bhdata-proto/src/{frame,json,message,error_map}.rs`, `bhdata-recipe/src/` (the one function), `fuzz/` | nothing | W8 (pure part), W16 |
| **S** gateway pure parts | `lockout`, `ratelimit`, `shed`, `perm`, all on a fake clock | `bhdata-gateway/src/{lockout,ratelimit,shed,perm}.rs` | nothing | unit tests of decisions 8, 9, 10 and 11; W5 and W10 logic |
| **C** control plane | migration 4 (`logins`, `audit`), `LoginStore` over PostgreSQL, `bhdatactl login ...` | `bhdata-control/src/{logins,audit}.rs`, the migration in `bhdata-pg/src/internal.rs`, `bhdata-cli/src/` (the `login` command) | the contract in section 1; P1 only for password-to-verifier | login and audit tests on PostgreSQL |
| **Py** Python client | the standard-library client, blocking first | `clients/python/` | the spec only (frames and messages; SCRAM written against RFC 7677 in Python) | W15 once B exists |
| **B** gateway on sockets | `tls`, `auth`, `listener`, `session`, shutdown | `bhdata-gateway/src/{tls,auth,listener,session}.rs` | P1, P2, S | W2 on the wire, W3, W5, W7 to W10, W12, W13 |
| **K** Rust client and server wiring | `client`, `retry`, the thin handler and the queue sender in `bhdata-server`, `bhdatactl gateway reload` | `bhdata-client/src/`, `bhdata-server/src/`, `bhdata-cli/src/` (the gateway command) | B, C | W1, W4, W6, W11, W14, W15; then the clean-VM run |

P1, P2, S, C and Py can all run at the same time. B starts when P1, P2 and S are merged; K when B and C are.
Slice A of the spec is P1 + P2 + S; slice B is B; slice C is C + K + Py.

## 3. The rules that keep lanes apart

1. **Own files only.** A lane edits the files in its row. A change anywhere else (another lane's file,
   a shared crate) is asked for, not made.
2. **`Cargo.toml` of your own crate** is yours. The root `Cargo.toml` (`[workspace.dependencies]`) and
   `deps.allow` are shared: add lines only, in alphabetical order, one line per commit, and say so in the
   message. A new external crate needs `cargo deny` and `cargo audit` clean before it is merged.
3. **`Cargo.lock` is never merged by hand.** On a conflict, take either side and run `cargo build`; the
   lock is rebuilt.
4. **Migration numbers.** The schema is at 3. Lane C owns migration 4 (logins and audit) and nobody else
   touches `bhdata-pg/src/internal.rs`. If another lane needs a table, it asks lane C for migration 5.
5. **Each lane ends the same way.** `cargo fmt`, `cargo test -p <its crates>`, and
   `cargo clippy -p <its crates> --all-targets -- -D warnings` clean, then a short branch ready to merge.
   `scripts/check.sh` (the whole gate) is run once, by whoever merges.
6. **Merge order:** P1, P2, S, C, Py in any order; then B; then K. After each merge, the next lane rebases.
7. **Fake handler for B.** Lane B tests against a handler and a `LoginStore` made in its own tests (a
   map of logins in memory); it never waits for C or K.

## 4. How to run a lane in its own session

Give the session this page, `wire-protocol.md`, and the rows of its lane. Tell it: "Build lane X as
described in `docs/specs/m5-lanes.md`; touch only the files it owns; finish with the commands in rule 5."
A lane that finds the contract wrong says so instead of working around it.
