# Spec: Engine manager and installer (M1)

Spec v0.1 · 2026-11-22 · Covers milestone M1 of `docs/project-structure-plan.md`. Section numbers marked "design" refer to `bighearthdata-design-v1.0.md`.

M1 is built in **three slices**, each ending with `scripts/check.sh` clean and a zip that can be handed back:

| Slice | Crates / folders | Status |
|---|---|---|
| A | `bhdata-helper` | Built |
| B | `bhdata-engine` | Not started |
| C | `packaging/` and the clean-VM run | Not started |

## 1. Decisions in this spec (change any of them)

1. **Pinned PostgreSQL major version: 17.** One constant, `PG_MAJOR` in `crates/bhdata-helper/src/actions.rs`. This is a placeholder choice, not a design decision: the design only says "pinned to one major version". Confirm it, or name another, before slice C. (Check that the PostgreSQL apt repository still ships it for your target Debian and Ubuntu releases.)
2. **What each helper action means.**
   - `install-pinned`: adds the PostgreSQL apt repository, sets `create_main_cluster = false`, installs `postgresql-17`. Run by the installer, as root, directly.
   - `update`: installs the newest minor release of the pinned major only (`postgresql-17`, `postgresql-client-17`, `--only-upgrade`). It restarts nothing; the engine manager restarts its own instances in the maintenance window (design section 3).
   - `restart`: `systemctl restart bhdata-server.service`. This is my reading of "restart" in the design's action list; the engine manager restarts its own PostgreSQL instances without root, so the only thing that needs root to restart is the service. Say so if you meant something else.
3. **Only `update` and `restart` are for the running server.** The server calls them through `sudo`, with one `sudoers` line per action (written in slice C). `install-pinned` is never in `sudoers`.
4. **The helper refuses when a Debian-managed cluster of the pinned major already exists** (`/etc/postgresql/17/<name>`), for both `install-pinned` and `update`. A package install can restart such a cluster, and the acceptance suite says an existing PostgreSQL must be left untouched. The data layer's own instances are made with `initdb` and never appear there.
5. **Platform: `ID=debian` or `ID=ubuntu` in `/etc/os-release`, on amd64 or arm64.** Derivatives (Mint and similar) are refused, since the repository setup is only supported on the two.
6. **The repository is added with the script that `postgresql-common` ships** (`apt.postgresql.org.sh -y`). The helper does not download or verify keys itself. The `-y` flag is from memory and could not be tried here (see section 5); if the script rejects it, the fix is one line in `actions.rs`.
7. **No dependencies beyond the standard library** in `bhdata-helper`. It is the one root-run program in the project.
8. **No lock file in the helper.** `apt-get` and `dpkg` already lock against each other; a second lock would add stale-lock failures for little gain.
9. **Audit trail.** `sudo` logs every call to the helper. The helper's own steps go to stderr (the journal). The control plane's audit log (M5, M8) is where admin actions are recorded.

## 2. Slice A: `bhdata-helper` (built)

Files: `main.rs` (argument, preflight, run), `actions.rs` (the three actions and the fixed names), `guard.rs` (root, location, platform), `exec.rs` (command runner), `error.rs` (errors and exit codes). The plan listed only `main.rs` and `actions.rs`; the other three keep the safety checks apart from the action logic, so each can be tested alone.

Rules, all enforced in code and covered by tests:

- Exactly one argument, equal to an action name. No prefixes, trimming, options, or `--help`. A bad argument is reported before anything else is looked at, and is printed escaped and cut to 40 characters.
- Must run as root (all four user ids zero, read from `/proc/self/status`), and the helper file and every directory above it must be root-owned and closed to group and other writes.
- Every command is an absolute program path with a fixed argument list. No shell. The child process gets an empty environment plus six fixed variables, and no stdin.
- `apt-get` is called with `--force-confold`, so a later `postgresql-common` upgrade keeps the `createcluster.conf` edit.
- The `createcluster.conf` edit replaces active `create_main_cluster` lines (or appends one), keeps every other line as it was, keeps the file's mode, and is written to a temporary file and renamed. It happens after `postgresql-common` is installed and before the server package is.
- After install or update the helper checks that `postgres`, `initdb` and `pg_ctl` exist and that no default cluster appeared. It never deletes anything.

Exit codes (stable; the installer and server read them): `0` ok, `2` usage, `3` not allowed to run here (not root, or unsafe location), `4` refused (unsupported platform, existing cluster), `5` a step failed (a command, or the after-check), `6` internal (file error).

## 3. Slice B: `bhdata-engine` (plan)

`instance.rs` (initdb, private port and data directory, no default cluster), `supervise.rs` (start, stop, health check, restart with backoff), `install.rs` (calls the helper's `install-pinned`), `update.rs` (calls `update` in the maintenance window, then restarts instances). Health checks use `pg_isready` or a plain socket connection, not a Postgres client, because `bhdata-pg` stays empty until M2 (dependency rules, plan section 5). Open for that slice: whether instances listen on a Unix socket only or on a private loopback port (design: "reachable only from the data layer's own components").

## 4. Slice C: `packaging/` and the VM run (plan)

`install.sh`, `uninstall.sh`, the systemd unit `bhdata-server.service`, the `sudoers` file (exact lines for `update` and `restart`, nothing else), the service user, and the root-owned install location for the helper. Uninstall removes binaries, helper and service; data only with an explicit flag. Then the acceptance suite from plan section 7.

## 5. What is checked where

| Check | Here (sandbox) | On your machine |
|---|---|---|
| Action parsing, command sequences, file edit, refusals, exit codes | Unit tests (`cargo test`) | `scripts/check.sh` |
| Helper refuses non-root and open locations; runs real commands from a root-owned path | Run for real as root | Not needed again |
| `apt.postgresql.org` repository setup, the `-y` flag, real `postgresql-17` install | Not possible (the sandbox cannot reach that repository) | Slice C, on a fresh VM or WSL2 distro |
| Kill and reboot recovery, existing PostgreSQL untouched, uninstall | Not possible | Slice C |
| `cargo deny` | Not possible (tool needs a newer Rust than the sandbox has) | `scripts/check.sh` |

## 6. Left out of M1 (proposal)

The plan's file list for `bhdata-engine` also names `upgrade.rs` (major upgrade: backup, dry run, `pg_upgrade`, verify) and `tuning.rs` (self-tuning checkpoints). The M1 acceptance suite does not test either. Proposal: they are not part of M1. `upgrade.rs` needs backups, so it moves to after M9; `tuning.rs` moves to M11, where the load runs can show what to tune. Plan section 7 should say so once you agree; until then the plan keeps its current wording.
