# Recipe and schema format (v0.3)

Status: decisions confirmed by the owner (section 5). Written before M3 (`bhdata-schema`, `bhdata-recipe`). Design: sections 6 and 9.
Two TOML files: the **schema** (tables) and **recipes** (commands). Both are plain data; nothing in them is code.

## 1. Schema file

```toml
[[table]]
name = "users"
kind = "subject"                 # owns rows, sharded by its id
columns = [
  { name = "balance", type = "i64", min = 0, max = 1_000_000_000 },
  { name = "name",    type = "text", max_len = 64, nullable = true },
]

[[table]]
name = "entries"
kind = "child"                   # follows its owner, so a user's rows stay together
owner = "users"
columns = [{ name = "delta", type = "i64" }, { name = "at", type = "timestamp" }]
indexes = [{ columns = ["at"] }]

[[table]]
name = "shop"
kind = "global"                  # small, read-mostly, copied to every shard
key  = "item"                    # its primary key: a required i64 or text column
columns = [{ name = "item", type = "text" }, { name = "price", type = "i64" }]
```

- **Kinds.** `subject`: has an implicit `id` (text, id rules of `bhdata-types`, primary key); the shard key. `child`: an implicit `owner_id` (references the owner's `id`), its own implicit `seq` (i64, assigned in order); unique on (`owner_id`, `seq`). `global`: `key` names its primary key, a required `i64` or `text` column; never written by recipes. A `child` must name a `subject` as its owner (no children of children in v0.2).
- **Types.** `i64`; `decimal` (`precision` up to 38, `scale`; exact, stored as `numeric(precision, scale)`, sent and received as text, never as a float); `text` (`max_len` required, at most 4096); `bool`; `timestamp` (UTC, microseconds); `bytes` (`max_len` required, at most 4096). No floating point at all. Money is `decimal`, or whole minor units in `i64`.
- **Bounds.** `min` and `max` on an `i64` or `decimal` column are enforced by the database (a `CHECK`) and by the compiler: a write that would leave the range is `overflow` (above `max`) or the recipe's named error (below `min`).
- **Names.** Lower-case letters, digits and `_`, starting with a letter, at most 40 characters. Names starting `bhdata_` and the implicit column names are reserved. Tables live in the `app` schema, never `sql`.
- **Changes.** Adding a table, a nullable column or an index is applied directly. Anything else (drop, rename, type or bound change) is a destructive change: planned, flagged, and applied only by an explicit admin action.

## 2. Recipe file

```toml
[recipe]
name    = "spend"
subject = "users"                # the table whose row this command owns; the request's `subject` is its id
# counterparty = "users"         # optional: a second subject, passed as argument `to`
args    = { amount = { type = "i64", min = 1, max = 1_000_000 } }

[[step]]
op = "ensure"                    # create the subject row if it is missing (id derived, design section 6)

[[step]]
op     = "add"                   # balance = balance + by, within the column's bounds
table  = "users"
column = "balance"
by     = "-amount"
below  = "insufficient_funds"    # named error when the result is under `min`
above  = "overflow"              # named error when the result is over `max`

[[step]]
op    = "insert"
table = "entries"
set   = { delta = "-amount", at = "now" }

[recipe.result]
return = ["balance"]             # columns of the subject row to return
```

- **Operations** (the whole list): `ensure`, `read` (one row by key into a name), `check` (a condition or a named error), `add`, `set`, `insert` (a child row), `list` (child rows of one owner, with a `limit`, newest first), `read_derived` (see section 3). Nothing else exists: no joins, no scans, no raw SQL.
- **Step fields** (v0.3, written with the parser; unknown or misplaced fields are `parse` rejections). Every step has `op` and an optional `label` (a name). Then:

  | `op` | needs | may also have |
  |---|---|---|
  | `ensure` | | `of` |
  | `read` | `table`, `columns` | `of` |
  | `check` | `require` (a condition), `error` | |
  | `add` | `table`, `column`, `by` | `of`, `below`, `above` |
  | `set`, `insert` | `table`, `set` (a map of column to expression) | `of` |
  | `list` | `table`, `columns`, `limit`, `into` | `of` |
  | `read_derived` | `store`, `window`, `max_age_ms`, `limit` | |

  `of` is `"subject"` (the default) or `"counterparty"` (only when `[recipe]` declares one); it says whose row or whose child rows the step reaches. A `read` of the counterparty's row names its values `to.<column>`, as expressions and `return` do. The `set` map is read sorted by column, so key order never matters. `list` returns its rows under the name `into`. `return` entries are a column of the subject row, a name from a `read` (`balance`, `to.balance`) or a `list` name, each at most once. With a counterparty, the argument name `to` is reserved: it carries the counterparty's id.
- **Expressions** are only: an argument name, a column read by an earlier `read` step, a literal, `now`, the operators `+`, `-`, `*`, `/` and the comparisons `== != < <= > >=` with `and`/`or`/`not`, and four pure functions: `round(x, scale, mode)` (mode `half_even`, `half_up` or `down`), `min`, `max`, `abs`. Nothing else: no time or random functions, no text built from input. Every value reaches the database as a bound parameter.
- **Rounding is always explicit.** `/` is allowed only as a direct argument of `round`. A `decimal` result with more digits than the column's scale must be wrapped in `round`, or the recipe is rejected (`rounding`): the database never rounds on its own.
- **Named errors.** A built-in name from the error table, or a recipe-defined name of lower-case letters and `_`, at most 40 characters. A failed check rolls back to the savepoint, stores the error, and returns it (design section 9). Every error is reported with the recipe name and version, the step number and the step's optional `label`, and the request record keeps all four, so any failure can be traced to the exact step.
- **Result.** Only columns of the subject row, values from `read`, or `list` output. At most 32 values and 64 KiB.
- **Limits.** Defaults: 16 steps, `list` limit 100, 32 result values, 64 KiB result. They are settings in a `[recipes]` section of the config, and are checked at registration; a registered version keeps working if a limit is lowered later. Recipe names are 1 to 40 characters, as other names.

Interest, with a `users.balance` of `decimal(18, 2)`:

```toml
[recipe]
name = "add_interest"
subject = "users"
args = { rate = { type = "decimal", precision = 9, scale = 6, min = "0", max = "1" } }

[[step]]
op = "read"
table = "users"
columns = ["balance"]

[[step]]
op = "add"
label = "interest"
table = "users"
column = "balance"
by = "round(balance * rate, 2, half_even)"
above = "overflow"
```

## 3. Reading a derived store

```toml
[[step]]
op     = "read_derived"
store  = "leaderboard"
window = "weekly"                # must be declared by the store
max_age_ms = 5000                # how stale a result may be
limit  = 50                      # at most 100, with paging
```
A recipe with `read_derived` has no other step (it is the all-shards lane). Undeclared store or window, or a `max_age_ms` the store cannot promise, is rejected at registration.

## 4. Versions

A recipe is registered as `name@N`, N from 1. The content is hashed (SHA-256 of the canonical form: comments, spacing and key order removed). Registering identical content again returns the same version; changed content makes the next N. A registered version never changes. Callers name `name@N` or just `name` (the latest). Retiring is an admin action, refused while anything unfinished uses the version (enforced from M8).

## 5. Decided

1. `decimal` is a type (exact, explicit rounding, no floats).
2. Recipes may define their own named errors; every error carries recipe, version, step and label.
3. `id`, `owner_id` and `seq` are implicit (as SQLite's `rowid`, MongoDB's `_id`, Django's and Rails' `id`).
4. The limits are config settings with the defaults in section 2.
5. Expressions are as in section 2: operators plus `round`, `min`, `max`, `abs`.
