#!/usr/bin/env bash
# Compares the crate-to-crate dependencies in the workspace with deps.allow (plan section 5).
#
#   - an edge that cargo has but deps.allow does not  -> failure
#   - a workspace crate with no line in deps.allow    -> failure
#   - a line in deps.allow for a crate that is gone   -> failure
#   - an allowed edge that no crate uses              -> warning only
#
# Needs: cargo, python3.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

meta="$(mktemp)"
trap 'rm -f "$meta"' EXIT
cargo metadata --format-version 1 --no-deps --locked >"$meta"

python3 - "$meta" deps.allow <<'PY'
import json
import sys

meta_path, allow_path = sys.argv[1], sys.argv[2]

with open(meta_path, encoding="utf-8") as f:
    packages = json.load(f)["packages"]  # --no-deps: workspace members only
members = {p["name"] for p in packages}

# What the manifests say (normal, build and dev dependencies all count).
actual = {
    p["name"]: {d["name"] for d in p["dependencies"] if d["name"] in members}
    for p in packages
}

# What deps.allow says.
allowed = {}
problems = []
with open(allow_path, encoding="utf-8") as f:
    for number, raw in enumerate(f, start=1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if ":" not in line:
            problems.append(f"deps.allow:{number}: expected 'crate: dep dep ...', got {raw.strip()!r}")
            continue
        name, deps = line.split(":", 1)
        name = name.strip()
        if name in allowed:
            problems.append(f"deps.allow:{number}: {name} is listed twice")
        allowed[name] = set(deps.split())

for name in sorted(set(allowed) - members):
    problems.append(f"deps.allow lists {name}, which is not a workspace crate")
for name in sorted(members - set(allowed)):
    problems.append(f"{name} has no line in deps.allow")
for name, deps in sorted(allowed.items()):
    for dep in sorted(deps - members):
        problems.append(f"deps.allow: {name} -> {dep}, but {dep} is not a workspace crate")

warnings = []
for name in sorted(members & set(allowed)):
    for dep in sorted(actual[name] - allowed[name]):
        problems.append(f"{name} depends on {dep}, which deps.allow does not allow")
    for dep in sorted((allowed[name] & members) - actual[name]):
        warnings.append(f"{name} -> {dep} is allowed but not used")

for w in warnings:
    print(f"warning: {w}")
if problems:
    for p in problems:
        print(f"error: {p}", file=sys.stderr)
    sys.exit(1)

edges = sum(len(d) for d in actual.values())
print(f"dependency check ok: {len(members)} crates, {edges} workspace edges, all allowed")
PY
