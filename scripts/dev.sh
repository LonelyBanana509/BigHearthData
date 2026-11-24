#!/usr/bin/env bash
# The usual development round in WSL, in one command. Run it from the project folder on the
# Windows drive (or from anywhere inside the project):
#
#   bash scripts/dev.sh                 copy to WSL, clean, bootstrap with the gate, install, smoke test
#   bash scripts/dev.sh --no-clean      keep target/ (much faster; the clean is rarely needed)
#   bash scripts/dev.sh --no-install    stop after the gate (no sudo packaging/install.sh)
#   bash scripts/dev.sh --skip-copy     work on the copy that is already in WSL
#   bash scripts/dev.sh --dest <dir>    copy somewhere else (default ~/projects/BigHearthData)
#   bash scripts/dev.sh --fuzz          also run the four fuzz targets (test W16), 60 seconds each
#   bash scripts/dev.sh --fuzz-secs 300 ... with another time per target (implies --fuzz)
#
# What it runs, in order:
#   1. scripts/copy-to-wsl.sh            (skipped when you run it from the WSL copy itself)
#   2. cargo clean                       (unless --no-clean)
#   3. ./bootstrap.sh --check [--install]
#        the gate: format, build, tests, real-server tests, clippy, dependency check, deny, audit
#   4. the real-server tests again, only if step 3 had to skip them
#        PostgreSQL 17 is installed by the installer, which runs after the gate, so on a fresh
#        machine the gate skips them. This step runs them once the installer has put it there.
#   5. with --fuzz: the fuzz targets in fuzz/ (frame, json, message, scram). They need nightly Rust
#      and cargo-fuzz, which this step installs; they are not part of the gate
#   6. a smoke test of the installed service (`bhdatactl status`), reported but never fatal
#
# It exits non-zero when the real-server tests did not run, so a green run always means they did.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
dest="$HOME/projects/$(basename "$here")"
do_clean=yes
do_install=yes
do_copy=yes
do_fuzz=no
fuzz_secs=60

step() { printf '\n==> %s\n' "$*"; }
die() { echo "dev.sh: $*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --no-clean) do_clean=no ;;
        --no-install) do_install=no ;;
        --skip-copy) do_copy=no ;;
        --fuzz) do_fuzz=yes ;;
        --fuzz-secs)
            [ $# -ge 2 ] || die "--fuzz-secs needs a number of seconds"
            case "$2" in '' | *[!0-9]* | 0) die "--fuzz-secs needs a whole number above 0" ;; esac
            do_fuzz=yes
            fuzz_secs="$2"
            shift
            ;;
        --dest)
            [ $# -ge 2 ] || die "--dest needs a folder"
            dest="$2"
            shift
            ;;
        -h | --help) sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
    shift
done

[ "$(id -u)" -ne 0 ] || die "run as your normal user, not root"

mkdir -p "$dest"
dest="$(cd "$dest" && pwd -P)"

# Running from the WSL copy itself: there is nothing to copy.
if [ "$here" = "$dest" ]; then
    do_copy=no
fi

if [ "$do_copy" = yes ]; then
    step "copy to $dest"
    "$here/scripts/copy-to-wsl.sh" "$dest"
fi

[ -x "$dest/bootstrap.sh" ] || chmod +x "$dest/bootstrap.sh" "$dest"/scripts/*.sh 2>/dev/null || true
cd "$dest"

# A Windows checkout can leave CRLF line endings that break shell scripts.
if grep -Il $'\r' bootstrap.sh scripts/*.sh >/dev/null 2>&1; then
    step "fixing CRLF line endings in the shell scripts"
    sed -i 's/\r$//' bootstrap.sh scripts/*.sh
fi

# cargo may be installed already, but not on this shell's path.
if ! command -v cargo >/dev/null 2>&1 && [ -r "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

if [ "$do_clean" = yes ]; then
    step "cargo clean"
    if command -v cargo >/dev/null 2>&1; then
        cargo clean
    else
        echo "cargo is not installed yet; bootstrap.sh will install it"
    fi
fi

pg_bin="${BHDATA_PG_BIN:-/usr/lib/postgresql/17/bin}"
export BHDATA_PG_BIN="$pg_bin"
pg_before=no
[ -x "$pg_bin/initdb" ] && pg_before=yes

step "bootstrap.sh --check$([ "$do_install" = yes ] && echo ' --install')"
if [ "$do_install" = yes ]; then
    ./bootstrap.sh --check --install
else
    ./bootstrap.sh --check
fi

# Cargo is on the path now, even if it was not when this script started.
if ! command -v cargo >/dev/null 2>&1 && [ -r "$HOME/.cargo/env" ]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

real_server=ran
if [ "$pg_before" = yes ]; then
    echo
    echo "real-server tests: ran inside the gate (PostgreSQL was already at $pg_bin)"
elif [ -x "$pg_bin/initdb" ]; then
    step "real-server tests (--ignored), now that PostgreSQL is installed"
    cargo test --workspace --locked -- --ignored --test-threads=1
else
    real_server=skipped
fi

if [ "$do_fuzz" = yes ]; then
    step "fuzz targets (nightly Rust and cargo-fuzz), ${fuzz_secs}s each"
    rustup toolchain install nightly --profile minimal
    command -v cargo-fuzz >/dev/null 2>&1 || cargo install --locked cargo-fuzz
    for target in frame json message scram; do
        echo "--- fuzz $target"
        cargo +nightly fuzz run "$target" -- -max_total_time="$fuzz_secs"
    done
fi

if [ "$do_install" = yes ]; then
    step "smoke test: bhdatactl status (as the service user)"
    if command -v bhdatactl >/dev/null 2>&1; then
        if sudo -u bhdata "$(command -v bhdatactl)" status; then
            echo "smoke test: ok"
        else
            echo "smoke test: bhdatactl status failed (see above); the gate itself passed" >&2
        fi
    else
        echo "smoke test: bhdatactl is not on the path; the install may not have completed" >&2
    fi
fi

echo
if [ "$real_server" = skipped ]; then
    echo "The gate passed, but the real-server tests did NOT run: no initdb in $pg_bin." >&2
    if [ "$do_install" = yes ]; then
        echo "The install ran, so PostgreSQL 17 is somewhere else: set BHDATA_PG_BIN to its bin folder." >&2
    else
        echo "Run again without --no-install so PostgreSQL 17 gets installed, or set BHDATA_PG_BIN." >&2
    fi
    exit 3
fi
echo "dev.sh: all done."
