# Spec: Engine manager and installer (M1)

Spec v0.1 · 2026-11-22 · Covers milestone M1 of `docs/project-structure-plan.md`. Section numbers marked "design" refer to `bighearthdata-design-v1.0.md`.

M1 is built in **three slices**, each ending with `scripts/check.sh` clean and a zip that can be handed back:

| Slice | Crates / folders | Status |
|---|---|---|
| A | `bhdata-helper` | Built |
| B | `bhdata-engine` | Built |
| C | `packaging/` and the clean-VM run | Built; clean-VM run passed 2026-11-22 (Ubuntu on WSL2, PostgreSQL 17.12) |

## 1. Decisions in this spec (change any of them)

1. **Pinned PostgreSQL major version: 17.** One constant, `PG_MAJOR` in `crates/bhdata-helper/src/actions.rs`. This is a placeholder choice, not a design decision: the design only says "pinned to one major version". Confirm it, or name another, before slice C. (Check that the PostgreSQL apt repository still ships it for your target Debian and Ubuntu releases.)
2. **What each helper action means.**
   - `install-pinned`: adds the PostgreSQL apt repository, sets `create_main_cluster = false`, installs `postgresql-17`. Run by the installer, as root, directly.
   - `update`: installs the newest minor release of the pinned major only (`postgresql-17`, `postgresql-client-17`, `--only-upgrade`). It restarts nothing; the engine manager restarts its own instances in the maintenance window (design section 3).
   - `restart`: `systemctl restart bhdata-server.service`. This is my reading of "restart" in the design's action list; the engine manager restarts its own PostgreSQL instances without root, so the only thing that needs root to restart is the service. Say so if you meant something else.
3. **Only `update` and `restart` are for the running server.** The server calls them through `sudo`, with one `sudoers` line per action (written in slice C). `install-pinned` is never in `sudoers`.
4. **The helper refuses when a Debian-managed cluster of the pinned major already exists** (`/etc/postgresql/17/<name>`), for both `install-pinned` and `update`. A package install can restart such a cluster, and the acceptance suite says an existing PostgreSQL must be left untouched. The data layer's own instances are made with `initdb` and never appear there.
5. **Platform: `ID=debian` or `ID=ubuntu` in `/etc/os-release`, on amd64 or arm64.** Derivatives (Mint and similar) are refused, since the repository setup is only supported on the two.
6. **The repository is added with the script that `postgresql-common` ships** (`apt.postgresql.org.sh -y`). The helper does not download or verify keys itself. The `-y` flag was confirmed on a real system (section 4).
7. **No dependencies beyond the standard library** in `bhdata-helper`. It is the one root-run program in the project.
8. **No lock file in the helper.** `apt-get` and `dpkg` already lock against each other; a second lock would add stale-lock failures for little gain.
9. **Audit trail.** `sudo` logs every call to the helper. The helper's own steps go to stderr (the journal). The control plane's audit log (M5, M8) is where admin actions are recorded.
10. **Instances listen on a Unix socket only, no network port** (slice B; this was the open question). `listen_addresses` is empty, the socket folder is mode 0700, and the port number only names the socket file. This is the strongest reading of "reachable only from the data layer's own components". If a later milestone needs a loopback port, it is one setting.
11. **Login is by operating-system identity ("peer").** `pg_hba.conf` has one rule: the service user, over the local socket. The instance's superuser is that same user. How the restricted roles of M2 log in is decided at M2, not guessed now.
12. **Start, stop and status go through `pg_ctl`; health is `pg_isready`.** `pg_ctl` finds a server left over from an earlier run through its pid file, so one is reused, never started twice. The engine opens no database connection.
13. **The engine rewrites its settings at every start** (`bhdata.conf`, `pg_hba.conf`, one `include` line), so a changed config takes effect on restart. Relaxed durability only turns off the wait for the sync at commit; `fsync` is on in every mode.
14. **Restart rules.** A missing process is restarted at once. A process that is there but silent is restarted after 3 failed checks in a row. A server that says it is starting up (crash recovery) is left alone. Restarts back off from 1 s to 60 s and the count resets when the server answers.
15. **Minor-release updates: one attempt per day, inside a one-hour window** (`engine.maintenance_hour_utc`, default 03:00 UTC). A failed attempt is not retried until the next day. Instances restart one at a time, only when the installed minor is newer than the one they started with. A different major is never applied.

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

## 3. Slice B: `bhdata-engine` (built)

- `instance.rs`: the instance's folders and settings, and `init` (`initdb` with peer login, UTF-8, data checksums). Refuses to run as root, to touch a folder that is not a cluster, and to reuse a data folder of another major version.
- `supervise.rs`: `start`, `stop`, `restart`, `health`, and the watcher thread (`Supervisor`) with the rules in decision 14. It records the version each instance was started with.
- `install.rs`: calls the helper (`sudo -n` for `update` and `restart`, directly for `install-pinned`). Tests compare the helper's exit codes, action names and `PG_MAJOR` with the helper's source, so the two cannot drift apart.
- `update.rs`: the daily window logic (`Updater::tick`) and `SystemUpdateOps`.
- `bhdata-types` gains `EngineConfig` (`[engine]`: `base_dir`, `port`, `max_connections`, `maintenance_hour_utc`, `locale`).
- `tests/real_postgres.rs` (ignored by default) runs the engine against a real server.

Not wired up yet: nothing calls `Supervisor` or `Updater` until the server exists (M2 and later). Slice B gives them as library pieces.

## 4. Slice C: `packaging/` and the VM run (built; the VM run is still to do)

Files in `packaging/`: `install.sh`, `uninstall.sh`, `bhdata-server.service`, `bhdata.sudoers`, `config.toml`.

- **What the service runs at M1.** `bhdata-server` is the engine host (`bhdata_engine::host`): read `/etc/bhdata/config.toml`, create the instance once, start it, watch it, apply minor updates in the window. The real server (M5) grows around it. `bhdatactl` has one command, `status`. Both binaries are thin; the code is in the engine crate so no new crate edges are needed.
- **Layout.** Helper, server in `/usr/libexec/bhdata/` (root-owned, 755); `bhdatactl` in `/usr/local/bin/`; config `/etc/bhdata/config.toml` (root-owned, kept on reinstall); data `/var/lib/bhdata` (service user, 750); unit `/etc/systemd/system/bhdata-server.service`; `/etc/sudoers.d/bhdata` (0440, checked with `visudo -c` before it is installed).
- **Service user** `bhdata`: system user, no login shell, home is the data folder. Reused if it already exists.
- **sudoers** has one rule: `bhdata ALL=(root) NOPASSWD:` the helper with `update`, and the helper with `restart`. `install-pinned` is not in it.
- **Unit.** Runs as `bhdata`, `Restart=always`, no start limit. `NoNewPrivileges` and `ProtectSystem` are left out on purpose, because they stop `sudo` from working; a test fails if they appear.
- **install.sh** needs root, systemd and apt. It takes the programs from `--bin-dir`, or next to itself, or `../target/release`. Order: user, programs, config and data folder, PostgreSQL through the helper, sudoers, unit, start, then waits up to 90 s for `bhdatactl status` to say the instance accepts connections. Safe to re-run. If the helper refuses (an existing PostgreSQL 17 cluster, or an unsupported system), it stops before the unit starts.
- **uninstall.sh** stops and removes the service, sudoers file, programs. It keeps data, config and the user unless `--purge-data` is given. It never removes PostgreSQL packages or the apt repository.
- **Checked in the sandbox:** syntax, `visudo -c`, the unit against `systemd-analyze` (only the missing-binary warning), argument handling, uninstall with and without `--purge-data` on fake state, and tests that the files agree with the code's constants. **Not possible here:** the real install (no systemd, no access to the PostgreSQL repository). That is the clean-VM run.

**Clean-VM result (2026-11-22).** Ubuntu on WSL2 with systemd: `scripts/check.sh` clean (including `cargo deny` and `cargo audit`); `install.sh` works from nothing and again on top of itself; the real `apt.postgresql.org.sh -y` works, so decision 6 is confirmed; PostgreSQL 17.12 started and `bhdatactl status` healthy; kill recovery, reboot recovery and uninstall (with and without `--purge-data`) behaved as specified. Kali was refused by design (decision 5).

## 5. What is checked where

| Check | Here (sandbox) | On your machine |
|---|---|---|
| Action parsing, command sequences, file edit, refusals, exit codes | Unit tests (`cargo test`) | `scripts/check.sh` |
| Helper refuses non-root and open locations; runs real commands from a root-owned path | Run for real as root | Not needed again |
| `apt.postgresql.org` repository setup, the `-y` flag, real `postgresql-17` install | Not possible (the sandbox cannot reach that repository) | Slice C, on a fresh VM or WSL2 distro |
| Engine against a real PostgreSQL: init, private settings, start, kill -9 recovery, restart, stop | `tests/real_postgres.rs` ran for real on PostgreSQL **16** (the sandbox's archive has no 17) | Run it with 17: `BHDATA_PG_BIN=/usr/lib/postgresql/17/bin cargo test -p bhdata-engine --test real_postgres -- --ignored` as a non-root user |
| Kill and reboot recovery, existing PostgreSQL untouched, uninstall | Not possible | Slice C |
| `cargo deny` | Not possible (tool needs a newer Rust than the sandbox has) | `scripts/check.sh` |

## 6. Left out of M1 (proposal)

The plan's file list for `bhdata-engine` also names `upgrade.rs` (major upgrade: backup, dry run, `pg_upgrade`, verify) and `tuning.rs` (self-tuning checkpoints). The M1 acceptance suite does not test either. Proposal: they are not part of M1. `upgrade.rs` needs backups, so it moves to after M9; `tuning.rs` moves to M11, where the load runs can show what to tune. Plan section 7 should say so once you agree; until then the plan keeps its current wording.
