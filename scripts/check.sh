#!/usr/bin/env bash
# The gate every slice passes before it is zipped and handed back (plan section 1, rule 4):
# format, build, test, clippy, dependency check, cargo deny, cargo audit.
#
# One-time setup of the two tools that are not part of Rust itself:
#   cargo install --locked cargo-deny cargo-audit
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

step() { printf '\n==> %s\n' "$*"; }

fail_missing() {
    echo "error: $1 is not installed. $2" >&2
    exit 1
}

command -v cargo >/dev/null 2>&1 || fail_missing cargo "Install Rust from https://rustup.rs"
command -v python3 >/dev/null 2>&1 || fail_missing python3 "On Debian/Ubuntu: sudo apt install python3"
cargo deny --version >/dev/null 2>&1 || fail_missing cargo-deny "Run: cargo install --locked cargo-deny"
cargo audit --version >/dev/null 2>&1 || fail_missing cargo-audit "Run: cargo install --locked cargo-audit"

step "format"
cargo fmt --all -- --check

step "build"
cargo build --workspace --all-targets --locked

step "test"
cargo test --workspace --locked

step "clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "dependency check"
scripts/check-deps.sh

step "cargo deny"
cargo deny --locked check

step "cargo audit"
cargo audit --deny warnings

printf '\nAll checks passed.\n'
