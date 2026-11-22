#!/usr/bin/env bash
# One-time setup of a fresh Debian or Ubuntu machine for building BigHearthData, then the build.
#
#   ./bootstrap.sh                 install tools, build release binaries (locked)
#   ./bootstrap.sh --check         ... and run scripts/check.sh (the full gate)
#   ./bootstrap.sh --install       ... and run sudo packaging/install.sh
#   ./bootstrap.sh --check --install
#
# Run it as your normal user, not root: Rust is installed for you with rustup, and only the system
# packages use sudo. Safe to run again; finished steps are skipped.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

die() { echo "bootstrap.sh: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

do_check=no
do_install=no
for arg in "$@"; do
    case "$arg" in
        --check) do_check=yes ;;
        --install) do_install=yes ;;
        -h | --help) sed -n '2,10p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $arg (try --help)" ;;
    esac
done

[ "$(id -u)" -ne 0 ] || die "run as your normal user, not root (it uses sudo where it must)"
command -v sudo >/dev/null 2>&1 || die "sudo is needed for the system packages"
[ -r /etc/os-release ] || die "cannot tell which system this is"
os_id=$(. /etc/os-release && printf '%s' "${ID:-}")
case "$os_id" in
    debian | ubuntu) ;;
    *) die "unsupported system (ID=${os_id:-unknown}): only Debian and Ubuntu are supported" ;;
esac

step "system packages"
sudo apt-get update -qq
sudo apt-get install -y -qq build-essential pkg-config curl ca-certificates git python3 zip unzip

step "Rust (rustup, stable; rustfmt and clippy come from rust-toolchain.toml)"
if ! command -v rustup >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/rustup" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
fi
if command -v rustup >/dev/null 2>&1; then
    echo "rustup found: $(rustup --version 2>/dev/null | head -1)"
else
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    export PATH="$HOME/.cargo/bin:$PATH"
fi
# Reads rust-toolchain.toml and installs that toolchain with its components.
rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install stable --profile minimal
cargo --version
rustc --version

step "cargo tools used by scripts/check.sh (cargo-deny, cargo-audit; --locked)"
for tool in cargo-deny cargo-audit; do
    if command -v "$tool" >/dev/null 2>&1; then
        echo "$tool already installed"
    else
        cargo install --locked "$tool"
    fi
done

step "build (release, locked to Cargo.lock)"
cargo build --release --locked --workspace

if [ "$do_check" = yes ]; then
    step "scripts/check.sh"
    scripts/check.sh
fi

if [ "$do_install" = yes ]; then
    step "install (sudo packaging/install.sh)"
    sudo packaging/install.sh --bin-dir target/release
fi

cat <<'MSG'

Done. If this is a new shell without cargo on its path, run:  . "$HOME/.cargo/env"
Next:  sudo packaging/install.sh      (or re-run ./bootstrap.sh --install)
MSG
