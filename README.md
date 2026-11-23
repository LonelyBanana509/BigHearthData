# BigHearthData

One data layer that replaces the separate, individually run databases of a polyglot stack, so a developer needs only this one and keeps complete control of the app's data through it. The name refers to breadth, not raw scale.

It runs on top of a private PostgreSQL that it installs and manages itself. Developers declare their data and the operations their app needs; the server decides how it is stored, cached, indexed and scaled.

## Status

Design v1.0 is approved. Milestone M0 (workspace skeleton) is done: the workspace builds, `bhdata-types` has the ids, named errors, config and clock/version types, and `scripts/check.sh` passes.

M1 (engine manager and installer) is built in three slices, each ending with `scripts/check.sh` clean and a zip:

- **A. `bhdata-helper`** (the small root-run helper): done, except that the real PostgreSQL install through it is checked in slice C.
- **B. `bhdata-engine`** (install, start, supervise, update PostgreSQL instances): done, except the run against PostgreSQL 17, which is checked on your machine (the sandbox only had 16).
- **C. `packaging/`** (installer, uninstaller, systemd unit, `sudoers`) and the clean-VM run.

The spec is [docs/specs/engine-manager.md](docs/specs/engine-manager.md). Only the helper and the engine have code so far; the other crates are still empty.

M3 (recipe compiler and registry) is built in slices A (`bhdata-schema`), B (`bhdata-recipe`: parser, checks, and the plan with its hash and grants) and C (`bhdata-control`: the registry, migration 2 and the grants, with `bhdatactl schema apply`, `recipe register` and `recipe list`). All three are built; what is left is the run on PostgreSQL 17 (the sandbox only had 16). The spec is [docs/specs/recipe-compiler.md](docs/specs/recipe-compiler.md).

M4 (the write path) is in progress: slice A (the pure parts) and slice B (migration 3, the one-command transaction in `bhdata-writer/src/exec.rs`, the change-log reader and trim, and the real-server write-path suite) are built; slice C (the queue, the compaction job, the host wiring in `bhdata-server` and the dev command `bhdatactl call`) is built apart from test 19 and the clean-VM run. The spec is [docs/specs/write-path.md](docs/specs/write-path.md). The real-server tests need PostgreSQL and a non-root user: `BHDATA_PG_BIN=/usr/lib/postgresql/17/bin cargo test -p bhdata-writer --test write_path -- --ignored`.

## Building and checking

Needs Rust 1.88 or newer, `python3`, and two cargo tools (one-time: `cargo install --locked cargo-deny cargo-audit`).
On a fresh Debian or Ubuntu machine, `./bootstrap.sh` installs all of that and builds (`--check` also runs the full gate, `--install` also installs the service).

- `scripts/check.sh` runs format, build, test, clippy, the dependency check, `cargo deny` and `cargo audit`. Every slice ends with it clean.
- `scripts/check-deps.sh` compares the crate-to-crate dependencies with `deps.allow` and fails on any extra edge.

## Documents

- [Design v1.0](docs/design/bighearthdata-design-v1.0.md): what it is and how it behaves
- [Project structure plan](docs/project-structure-plan.md): workspace, crates, build order, tests
- [docs/specs/engine-manager.md](docs/specs/engine-manager.md): M1 spec (helper, engine manager, packaging)
- `docs/specs/`: one short spec per component, written before the component

## License

Licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE)).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project, as defined in the Apache-2.0 license, is licensed under the same terms, without any additional terms or conditions.
