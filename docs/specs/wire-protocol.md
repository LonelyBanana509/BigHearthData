# Wire protocol (spec)

**Status (2026-11-23): approved, not built.** All nine defaults in section 12 were accepted as they stand; M5 slice A is next. This is the spec the plan requires before M5. It fixes what travels between a client SDK and the gateway: the transport, the frames, the login, the messages, the limits, and the rules for safe retries. Section 12 keeps the questions and the answers (all yes, for now).

## 1. What this fixes

One TCP connection carries one login and many requests. The gateway (`bhdata-gateway`) is the only part that talks to clients (plan section 5); the Rust client (`bhdata-client`) and the Python client (`clients/python`) speak exactly this. The gateway hands each request to the rest of the system only through the `RequestHandler` seam of `bhdata-api`. At M5 the handler is thin wiring in `bhdata-server` that does what `bhdatactl call` does today (load the plan, build a `Command`, `ShardBackend::execute`), and the queue finally gets its sender. The planner replaces it in M6; the wire does not change.

Out of scope here: the PostgreSQL wire adapter for SQL mode (M12, `pgwire`), reads (M6; they will be a second message type on this protocol), the `register` and `admin` operations (CLI only until M8), cross-shard polling (the `poll` message is reserved for M15).

## 2. Decisions in this spec (change any of them)

1. **TCP, and TLS 1.3 only** (rustls). TLS is required on any non-loopback address. A plain connection is allowed only on a loopback address and only with `gateway.insecure_local = true`; the server refuses to start with it on any other address. **Authentication is never optional**: there is no auth-off mode, so a development setup uses a login like any other (design section 10 allows an auth-off mode on loopback; this spec does not build one).
2. **The client pins the server's key**, not the whole certificate: the SHA-256 of the leaf certificate's `SubjectPublicKeyInfo`, as a list. Renewing a certificate with the same key breaks nobody. Rotating the key works with an overlap: the client trusts a list of two fingerprints, the server swaps its certificate on `bhdatactl gateway reload` (and `SIGHUP`) without dropping open connections, then the old fingerprint is removed from the clients. A client with no pins refuses to connect unless it is told `insecure_no_verify` for a loopback address.
3. **A frame is `u32` length (big endian), `u8` version, then the body.** The length counts the version byte and the body. The body is UTF-8 JSON in version 1. The version byte is in every frame so that a binary body can be added as version 2 without a new handshake (plan section 2).
4. **Frame and JSON limits** (all in `[gateway]`, defaults):

   | Limit | Before login | After login |
   |---|---|---|
   | Frame size | 16 KiB | 1 MiB |
   | JSON nesting depth | 8 | 8 |
   | Object keys in one object | 64 | 256 |
   | One string | 4 KiB | 64 KiB |

   The parser refuses, and closes the connection with an `error` frame, on: a length above the limit, a version it does not know, invalid UTF-8, **duplicate keys, unknown fields, floating-point numbers, and integers outside the 64-bit range**. The JSON is parsed once, exactly: an integer is never passed through a float. Fuzz targets for the frame reader and the JSON parser are part of M5.
5. **One correlation `tag` per request, replies out of order.** A client may have up to `gateway.max_inflight_per_connection` (default 32) requests open on one connection; each carries a `tag` (a number the client picks, unique among its open requests), and the reply carries the same tag.
6. **Login is SCRAM-SHA-256** (RFC 5802, RFC 7677) built from `hmac`, `sha2` and `pbkdf2` (plan section 2), run inside `auth` messages. No channel binding in version 1 (the pinned key already covers a man in the middle; `SCRAM-SHA-256-PLUS` with `tls-exporter` is a later addition). Passwords are normalised with SASLprep when they are valid UTF-8 and used as given otherwise, as PostgreSQL does. The login id alphabet (`A-Z a-z 0-9 . _ : -`) has no `=` or `,`, so SCRAM's user-name escaping never applies. The server stores a verifier (salt, iteration count, stored key, server key), **never the password**. Default iteration count 100,000 (a placeholder until M11).
7. **A login that does not exist looks like one that does.** The server answers the first SCRAM step with a salt and iteration count derived by HMAC from a server secret and the name, and fails at the last step with the same `unauthorized` as a wrong password. A client cannot learn which logins exist.
8. **Lockout.** Five consecutive failed logins for one name (existing or not) lock that name for 15 minutes; the failure counter resets on a success. While locked, even the right password is answered `locked_out`. Failures are also counted per source address (twenty in a minute close further attempts from it for a minute). Both counters are per gateway copy at stage 1, as the design says.
9. **The check order for every request**, and what each failure answers: frame valid, else `error` and close; login authenticated, else the connection never gets here; **permission** (`unauthorized`); **rate limit** (`rate_limited` with a hint); **shed** (`overloaded` with a hint, when the gateway's own in-flight count is over `gateway.max_inflight_total`, default 1,024); then the handler. A request refused at any of these did no work and stored nothing, so its request id is unused and a retry is safe.
10. **Permissions.** A login record holds the permissions `execute`, `register`, `admin`, `sql` (design section 10), plus, for `execute`, a recipe allow-list (default: none; an admin grants names or `*`). At M5 only `execute` has any effect on this wire. For a login whose allow-list is not `*`, a recipe that is unknown **and** one that is not allowed both answer `unauthorized`, so a restricted login cannot probe which recipes exist. A login with `*` gets `unknown_recipe` for an unknown name.
11. **Rate limits are on by default** (design section 10): a token bucket per login, default 200 requests a second with a burst of 400, counted per gateway copy, and counting every request that passes the permission check, including those that later fail. Also per login: at most 32 connections and `max_inflight_per_connection` as above. All placeholders until M11.
12. **The server's clock reaches the client.** The server `hello` carries `now_ms`; the SDK keeps the difference to its own clock and stamps `created_at_ms` with the corrected time. A client whose clock is wrong still passes the write path's time rule (`write-path.md`, section 4: 5 minutes ahead is refused, 24 hours back is expired). **A retry keeps the original `created_at_ms`**, because the expiry rule depends on it.
13. **The request id is the client's.** The SDK makes one (32 lowercase hex characters of 128 random bits) when the application gives none. An application that must not apply a command twice across a restart supplies its own id, derived from the business event. Ids are unique per login (the write path's key is `(login_id, request_id)`).

## 3. Connection flow

```
client                                   gateway
  | -- TLS handshake (pinned key) ------> |
  | -- hello {max_version, client} -----> |
  | <----- hello {version, now_ms, limits, mechanisms} |
  | -- auth {client_first} -------------> |      the three SCRAM steps; any wrong step,
  | <----- auth {server_first} ---------- |      or a failed proof, ends in `error`
  | -- auth {client_final} -------------> |      `unauthorized` or `locked_out`, then close
  | <----- auth {server_final} + ready -- |
  | -- request {tag, ...} --------------> |      up to max_inflight_per_connection open
  | <----- reply {tag, ...} ------------- |      in any order
  | -- ping --> / <-- pong                |      keeps the idle timer from firing
```

Timers (defaults): the whole handshake and login must finish within 10 s; after login a connection with no frame in either direction for 60 s is closed (an SDK pings every 20 s); a reply that cannot be written within 10 s closes the connection (a client that does not read cannot hold server memory). At most 3 login attempts per connection, then it is closed.

## 4. Messages

Every body is a JSON object with a `type`. A message with a field not listed here is refused.

**Client to server**

| `type` | Fields | Notes |
|---|---|---|
| `hello` | `max_version` (1), `client` (text, for logs, at most 64 bytes) | First frame, nothing else is accepted before it |
| `auth` | `step` (`client_first`, `client_final`), `mechanism` (`SCRAM-SHA-256`, first step only), `message` (the SCRAM text) | |
| `request` | `tag`, `request_id`, `created_at_ms`, `recipe`, `version` (number or null), `subject`, `counterparty` (or null), `args` (object), `map_version` (number or null) | `map_version` is accepted and ignored at stage 0; it is there so the stage 2 epoch check needs no change |
| `ping` | none | |
| `bye` | none | A clean close; open requests are still answered first |

**Server to client**

| `type` | Fields | Notes |
|---|---|---|
| `hello` | `version`, `server`, `now_ms`, `max_frame`, `max_inflight`, `idle_timeout_secs`, `mechanisms` | |
| `auth` | `step` (`server_first`, `server_final`), `message` | |
| `ready` | none | Sent after `server_final`; requests may follow |
| `reply` | `tag`, then **either** `ok: true`, `replayed`, `result` **or** `ok: false`, `error` | |
| `pong` | none | |
| `shutdown` | `drain_ms` | The server is stopping (section 8) |
| `error` | `error` | A connection-level failure (a bad frame, a failed login); the connection is closed after it |

**Arguments** use the forms the stored result already uses (`write-path.md`, section 7), so one set of rules covers both directions: `i64` is a JSON integer; `decimal` is a string with exactly the plan's digits after the point (`"1.50"`; an integer is also accepted); `bool` is `true` or `false`; `text` is a string; `bytes` is a base64 string; `timestamp` is a JSON integer, whole microseconds since 1970-01-01 UTC. The handler converts each argument by the plan's type (one function in `bhdata-recipe`, next to `Value`) and a mismatch is `bad_arguments` naming the argument.

**The error object** is `{"kind": <one of the 16 names of plan section 3>, "name": <text>, "retry_after_ms": <number, only for overloaded and rate_limited>, "transfer": <id, only for pending>, "message": <text>, "incident": <text, only for internal>}`. `name` equals `kind`, except for `recipe_defined`, where it is the name the recipe gave its own error (`already_claimed`); a client matches on `name` for business errors and on `kind` for the rest. `message` is for people and never for logic. **`internal` carries no detail** (design: clients get none), only an `incident` id that the server logs with the cause. `wrong_shard` never reaches the wire (the planner retries it; at M5 there is one shard).

A named failure the recipe stored (`failed`, for example `insufficient_funds`) is an `ok: false` reply with its error object, and it is final: a retry with the same id returns it again with `replayed: true`.

## 5. Retry rules (what the SDKs must do)

| What the client sees | Outcome unknown? | The SDK |
|---|---|---|
| `done` or `failed` reply | no | Returns it. `replayed: true` just means an earlier attempt got there first |
| `overloaded`, `rate_limited` | no, nothing was stored | Waits the hint (times a random 1 to 1.5, so clients do not retry in step) and sends the **same** request id and `created_at_ms` |
| Connection lost, timeout, `shutdown`, a connection error before the reply | **yes** | Opens a new connection and sends the same id and `created_at_ms`; the write path settles it (applied once, or applied now) |
| `request_expired` | yes, but the window is over | Returns it. The application must read the state to find out what happened |
| `request_id_conflict`, `bad_arguments`, `unknown_recipe`, `unauthorized`, `locked_out`, `already_exists`, `already_imported` | no | Returns it, never retries |
| `internal` | no (the write path stores nothing) | Retries once after a second, then returns it with its `incident` |
| `pending` | not final | Polls with the `transfer` (M15; until then it does not occur) |

The SDK stops retrying after `max_retry_secs` (default 60, and never more than the retry window, 24 h) and returns the last error, or "outcome unknown" if the last attempt had no answer. A call that is cancelled by the application does not cancel the request: the write may still happen, and the same id can be sent later.

## 6. Where the code goes

- **`bhdata-proto`**: frame reader and writer with the limits, the messages, the JSON codec behind a `Codec` trait, the mapping between `bhdata_types::Error` and the error object (the only place wire names exist). No I/O of its own.
- **`bhdata-gateway`**: `listener` (accept, connection and idle limits), `tls` (certificates, reload), `auth` (SCRAM, lockout), `perm`, `ratelimit`, `shed`, `session` (one task per connection), as in plan section 6. It depends on `bhdata-proto` and `bhdata-api` only and calls the handler through `RequestHandler`: `handle(&self, caller: Caller, call: Call) -> Result<Reply, Error>`, where `Caller` is the authenticated login and `Call` is the request with its arguments still as JSON.
- **`bhdata-client`**: `client` (connect, pins, request id, clock offset), `retry` (section 5). **`clients/python`**: the same with only the standard library (`ssl`, `socket`, `hashlib`, `hmac`, `json`, `struct`); a blocking client first, an async one later.
- **`bhdata-control`**: the `logins` table (login id, verifier, permissions, allow-list, limits, created, disabled) and the audit log, both in a new migration; `bhdatactl login create|grant|revoke|disable|set-password`. A password is typed at a prompt or read from a file, never taken from the command line.
- **`bhdata-types`**: a `[gateway]` config section (listen address, certificate and key paths, `insecure_local`, every limit above), unknown keys refused, like `[writer]`.
- **`bhdata-server`**: starts the gateway and the thin handler; `write_path.rs` hands the queue to it.
- **`deps.allow`** gets the edges the plan already lists (`gateway` to `proto` and `api`; `client` to `proto`). New external crates for review under `cargo deny`: `rustls` with one crypto provider, `tokio-rustls`, `rustls-pemfile`.

## 7. The audit log

Written by the control plane, one row each: a login success, a login failure (with the source address and whether the name exists, which only the log sees), a lockout and its release, a permission or limit change, a login created or disabled. Not written: individual requests (the request table already records every command), rate-limit hits (a metric from M8). Audit rows are written best effort and never delay a reply, except that a lockout is stored before the `locked_out` answer is sent.

## 8. Shutdown and restart

On stop the gateway sends `shutdown {drain_ms}` to every connection, answers new requests `overloaded`, waits up to `drain_ms` (default 5,000, the writer's own drain) for open ones, then closes. A client treats the close as a lost connection (section 5) and reconnects with backoff; a gateway copy that comes back is simply another endpoint. A gateway holds no durable state.

## 9. Tests

Pure tests run on every `cargo test`; the rest use real sockets and a real PostgreSQL (`--ignored`, as the write-path suite).

| # | Test | Expected |
|---|---|---|
| W1 | Run a recipe from the Rust client over TLS with the right pin; then with a wrong pin | Done and replayed correctly; the wrong pin is refused by the client before any login |
| W2 | SCRAM against the RFC 7677 test vectors; a wrong password; an unknown login | Vectors match; the two failures are identical on the wire |
| W3 | Five wrong passwords, then the right one; then wait out the lock | `locked_out` even for the right one; accepted after the window |
| W4 | A login without `execute`; a recipe outside its allow-list; an unknown recipe with `*` | `unauthorized`; `unauthorized`; `unknown_recipe` |
| W5 | A burst beyond the rate limit, then a retry after the hint | `rate_limited` with a hint; nothing stored for the refused ones; the retry applies |
| W6 | Kill the connection at random moments around a request, the client retries | Applied exactly once (the wire version of write-path test 3) |
| W7 | 32 requests in flight on one connection | Every reply matches its tag, in any order |
| W8 | Bad frames: too long, wrong version, invalid UTF-8, nesting too deep, duplicate keys, a float, an unknown field, an unknown type | An `error` frame and a closed connection; the server keeps serving others |
| W9 | Login never finished, idle connection, a client that does not read | Closed at the timers; the server's memory stays flat |
| W10 | More connections than the per-login cap; more in flight than the global cap | Refused with `overloaded` or closed; nothing queued without bound |
| W11 | The shard queue answers `overloaded` | The hint reaches the client; the SDK waits and retries with the same id |
| W12 | A certificate swap with `reload`, clients holding both pins | No open connection dropped; new ones see the new key |
| W13 | The server configured on a non-loopback address without TLS | It refuses to start |
| W14 | The clock of the client is an hour fast and an hour slow | Requests still pass the time rule; a retry keeps its `created_at_ms` |
| W15 | The Python client runs W1, W4, W6 | Same results as the Rust client |
| W16 | Fuzz: the frame reader, the JSON parser, the SCRAM message parser | No panic, no allocation above the limits |

## 10. Build order (three slices, as M1 to M4)

The slices can also be built as parallel lanes: see [m5-lanes.md](m5-lanes.md), which fixes the shared contracts and who owns which files. It moves the SCRAM maths from the gateway to `bhdata-proto::scram` (section 6 above says gateway `auth`; the gateway's `auth` module still runs the server side of the login).

- **A.** `bhdata-proto` (frames, limits, messages, codec, error mapping, argument conversion) and the fuzz targets; SCRAM and the lockout counter as pure code on a fake clock; the rate limiter and the shed rule on a fake clock. No sockets.
- **B.** `bhdata-gateway` on real sockets with a fake handler: TLS, login, permissions, limits, timers, shutdown; tests W2, W3, W5, W7 to W10, W12, W13.
- **C.** `bhdata-control` logins and audit, `bhdatactl login`, the server wiring and the thin handler, `bhdata-client`, `clients/python`; tests W1, W4, W6, W11, W14, W15; then the clean-VM run with a client on another machine.

## 11. Left out of M5

Reads and `describe` (a request that returns a recipe's argument and result types, so a client can decode decimals and bytes by type; M6); SQL mode and its `pgwire` adapter (M12); `register` and `admin` over the wire (M8); channel binding; a binary codec; compression; HTTP; a connection pool in the SDK (one connection with many requests in flight is enough at stage 0); client certificates.

## 12. Questions for you (my default in brackets)

1. **No auth-off mode**, even on loopback (decision 1)? [Yes; one less way to ship a server with no login. A development login costs one command.]
2. **Pin the public key (SPKI hash)** instead of the whole certificate (decision 2)? [Yes; the design says certificate fingerprints, and this keeps its rotation overlap while surviving a renewal.]
3. **No SCRAM channel binding** in version 1 (decision 6)? [Yes, pinning covers it.]
4. **Lockout** 5 failures, 15 minutes, counted per name including unknown names (decision 8)? [Yes.]
5. **A restricted login cannot tell "unknown" from "not allowed"** (decision 10)? [Yes.]
6. **The server's clock goes to the client** and the SDK corrects `created_at_ms` (decision 12)? [Yes.]
7. **The Python client uses only the standard library**, blocking first (section 6)? [Yes.]
8. **Argument forms** as in section 4, base64 for bytes (the CLI keeps hex for typing by hand)? [Yes.]
9. **The three slices** of section 10? [Yes, same shape as M1 to M4.]
