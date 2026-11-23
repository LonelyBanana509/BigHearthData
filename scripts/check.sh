#!/usr/bin/env bash
# The gate every slice passes before it is zipped and handed back (plan section 1, rule 4):
# format, build, test, real-server tests, clippy, dependency check, cargo deny, cargo audit.
#
# The real-server tests (the ones marked "ignored, needs a real PostgreSQL and a non-root user")
# run when BHDATA_PG_BIN points at a PostgreSQL bin folder (default /usr/lib/postgresql/17/bin)
# and this is not root. Otherwise that step is skipped with a note. Set BHDATA_SKIP_PG_TESTS=1
# to skip it on purpose.
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

step "real-server tests (--ignored)"
export BHDATA_PG_BIN="${BHDATA_PG_BIN:-/usr/lib/postgresql/17/bin}"
if [[ "${BHDATA_SKIP_PG_TESTS:-0}" == "1" ]]; then
    echo "skipped: BHDATA_SKIP_PG_TESTS=1"
elif [[ "$(id -u)" == "0" ]]; then
    echo "skipped: these tests refuse to run as root"
elif [[ ! -x "$BHDATA_PG_BIN/initdb" ]]; then
    echo "skipped: no initdb in $BHDATA_PG_BIN (set BHDATA_PG_BIN to the PostgreSQL bin folder)"
else
    cargo test --workspace --locked -- --ignored --test-threads=1
fi

step "clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "dependency check"
scripts/check-deps.sh

step "cargo deny"
cargo deny --locked check

step "cargo audit"
cargo audit --deny warnings

printf '\nAll checks passed.\n'
