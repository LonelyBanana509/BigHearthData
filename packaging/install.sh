#!/usr/bin/env bash
# Installs BigHearthData (milestone M1): the service user, the root-owned helper and server, the
# config, the sudoers file, the systemd unit, the pinned PostgreSQL, then starts the service.
#
# Run as root, from a release folder or a source tree that has been built:
#   sudo packaging/install.sh [--bin-dir <folder with bhdata-helper, bhdata-server, bhdatactl>]
#
# Safe to run again: it replaces the program files, keeps an existing config and data, and an
# existing PostgreSQL on the machine is not touched (the helper refuses if a PostgreSQL 17 cluster
# managed by Debian already exists, and nothing is installed in that case).
set -euo pipefail

USER_NAME=bhdata
LIBEXEC=/usr/libexec/bhdata
CONF_DIR=/etc/bhdata
DATA_DIR=/var/lib/bhdata
UNIT_NAME=bhdata-server.service
CTL_PATH=/usr/local/bin/bhdatactl
SUDOERS_PATH=/etc/sudoers.d/bhdata
UNIT_PATH=/etc/systemd/system/$UNIT_NAME
WAIT_SECONDS=90

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
src=$here
bin_dir=""

die() { echo "install.sh: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --bin-dir)
            [ $# -ge 2 ] || die "--bin-dir needs a folder"
            bin_dir=$2
            shift 2
            ;;
        -h | --help)
            sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

# Where the three programs are: the given folder, else next to this script, else the cargo output.
if [ -z "$bin_dir" ]; then
    for candidate in "$here" "$here/../target/release"; do
        if [ -f "$candidate/bhdata-helper" ]; then
            bin_dir=$candidate
            break
        fi
    done
fi
[ -n "$bin_dir" ] || die "cannot find the programs; build them (cargo build --release) or pass --bin-dir"
[ -d "$bin_dir" ] || die "the folder $bin_dir does not exist"
bin_dir=$(cd "$bin_dir" && pwd)

step "checking this machine"
[ "$(id -u)" -eq 0 ] || die "run as root (sudo packaging/install.sh)"
[ -d /run/systemd/system ] || die "systemd is not running here. On WSL2, enable it first (/etc/wsl.conf: [boot] systemd=true), then restart the distro."
command -v apt-get >/dev/null 2>&1 || die "apt-get not found; only Debian and Ubuntu are supported"
os_id=$(. /etc/os-release 2>/dev/null && printf '%s' "${ID:-}")
case "$os_id" in
    debian | ubuntu) ;;
    *) die "unsupported system (ID=${os_id:-unknown}): only Debian and Ubuntu are supported, not derivatives such as Kali or Mint. Nothing was changed." ;;
esac
for program in bhdata-helper bhdata-server bhdatactl; do
    [ -f "$bin_dir/$program" ] || die "$bin_dir/$program is missing"
done
for file in config.toml bhdata.sudoers $UNIT_NAME; do
    [ -f "$src/$file" ] || die "$src/$file is missing"
done
command -v visudo >/dev/null 2>&1 || die "visudo not found (package: sudo)"
command -v sudo >/dev/null 2>&1 || die "sudo not found; the server needs it to call the helper"

step "service user $USER_NAME"
if id "$USER_NAME" >/dev/null 2>&1; then
    echo "user $USER_NAME exists; keeping it"
else
    useradd --system --user-group --home-dir "$DATA_DIR" --no-create-home \
        --shell /usr/sbin/nologin "$USER_NAME"
fi

step "programs (root-owned, in $LIBEXEC)"
install -d -o root -g root -m 755 "$LIBEXEC"
install -o root -g root -m 755 "$bin_dir/bhdata-helper" "$LIBEXEC/bhdata-helper"
install -o root -g root -m 755 "$bin_dir/bhdata-server" "$LIBEXEC/bhdata-server"
install -o root -g root -m 755 "$bin_dir/bhdatactl" "$CTL_PATH"

step "config ($CONF_DIR) and data folder ($DATA_DIR)"
install -d -o root -g root -m 755 "$CONF_DIR"
if [ -e "$CONF_DIR/config.toml" ]; then
    echo "$CONF_DIR/config.toml exists; keeping it"
else
    install -o root -g root -m 644 "$src/config.toml" "$CONF_DIR/config.toml"
fi
install -d -o "$USER_NAME" -g "$USER_NAME" -m 750 "$DATA_DIR"

step "PostgreSQL (pinned major, through the helper)"
# Exit 4 means the helper refused: an existing PostgreSQL cluster of the pinned major, or an
# unsupported system. Nothing was installed then, and the message above says why.
"$LIBEXEC/bhdata-helper" install-pinned

step "sudoers ($SUDOERS_PATH)"
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
install -m 440 "$src/bhdata.sudoers" "$tmp"
visudo -c -f "$tmp" >/dev/null || die "the sudoers file did not pass visudo; nothing was installed for it"
install -o root -g root -m 440 "$tmp" "$SUDOERS_PATH"

step "service ($UNIT_NAME)"
install -o root -g root -m 644 "$src/$UNIT_NAME" "$UNIT_PATH"
systemctl daemon-reload
systemctl enable "$UNIT_NAME"
systemctl restart "$UNIT_NAME"

step "waiting for the instance (up to $WAIT_SECONDS s)"
waited=0
until runuser -u "$USER_NAME" -- "$CTL_PATH" status >/dev/null 2>&1; do
    if [ "$waited" -ge "$WAIT_SECONDS" ]; then
        echo "The instance did not become ready. Last status and log:" >&2
        runuser -u "$USER_NAME" -- "$CTL_PATH" status >&2 || true
        journalctl -u "$UNIT_NAME" --no-pager -n 30 >&2 || true
        die "install finished, but the server is not healthy"
    fi
    sleep 2
    waited=$((waited + 2))
done

printf '\nInstalled. Status:\n'
runuser -u "$USER_NAME" -- "$CTL_PATH" status
printf '\nCheck again any time with: sudo -u %s bhdatactl status\nLogs: journalctl -u %s\n' "$USER_NAME" "$UNIT_NAME"
