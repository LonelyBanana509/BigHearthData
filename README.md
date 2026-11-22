# BigHearthData

One data layer that replaces the separate, individually run databases of a polyglot stack, so a developer needs only this one and keeps complete control of the app's data through it. The name refers to breadth, not raw scale.

It runs on top of a private PostgreSQL that it installs and manages itself. Developers declare their data and the operations their app needs; the server decides how it is stored, cached, indexed and scaled.

## Status

Design v1.0 is approved. No code yet. The next step is milestone M0 (workspace skeleton) in the plan.

## Documents

- [Design v1.0](docs/design/bighearthdata-design-v1.0.md): what it is and how it behaves
- [Project structure plan](docs/project-structure-plan.md): workspace, crates, build order, tests
- `docs/specs/`: one short spec per component, written before the component

## License

Licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE)).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project, as defined in the Apache-2.0 license, is licensed under the same terms, without any additional terms or conditions.
