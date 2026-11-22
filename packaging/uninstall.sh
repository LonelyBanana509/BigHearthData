#!/usr/bin/env bash
# Removes BigHearthData: the service, the sudoers file, the helper, the server and bhdatactl.
#
#   sudo packaging/uninstall.sh                 keeps the data, the config and the service user
#   sudo packaging/uninstall.sh --purge-data    also deletes the data folder, the config and the user
#
# It never removes PostgreSQL packages or the PostgreSQL apt repository: another program on the
# machine may use them, and a PostgreSQL that was there before is left untouched.
set -euo pipefail

USER_NAME=bhdata
LIBEXEC=/usr/libexec/bhdata
CONF_DIR=/etc/bhdata
DATA_DIR=/var/lib/bhdata
UNIT_NAME=bhdata-server.service
CTL_PATH=/usr/local/bin/bhdatactl
SUDOERS_PATH=/etc/sudoers.d/bhdata
UNIT_PATH=/etc/systemd/system/$UNIT_NAME

die() { echo "uninstall.sh: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }

purge=no
while [ $# -gt 0 ]; do
    case "$1" in
        --purge-data) purge=yes; shift ;;
        -h | --help) sed -n '2,8p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done
[ "$(id -u)" -eq 0 ] || die "run as root (sudo packaging/uninstall.sh)"

step "service"
if [ -d /run/systemd/system ]; then
    # Stopping the service also stops the private PostgreSQL (same control group).
    systemctl disable --now "$UNIT_NAME" 2>/dev/null || true
fi
rm -f "$UNIT_PATH"
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl reset-failed "$UNIT_NAME" 2>/dev/null || true
fi

step "sudoers and programs"
rm -f "$SUDOERS_PATH" "$CTL_PATH"
# Only the three files the installer put there, then the folder if that left it empty.
rm -f "$LIBEXEC/bhdata-helper" "$LIBEXEC/bhdata-server"
rmdir "$LIBEXEC" 2>/dev/null || true

if [ "$purge" = yes ]; then
    step "data, config and service user (--purge-data)"
    case "$DATA_DIR" in
        /var/lib/?*) ;;
        *) die "refusing to delete $DATA_DIR" ;;
    esac
    rm -rf --one-file-system "$DATA_DIR"
    rm -f "$CONF_DIR/config.toml"
    rmdir "$CONF_DIR" 2>/dev/null || true
    if id "$USER_NAME" >/dev/null 2>&1; then
        userdel "$USER_NAME" 2>/dev/null || echo "could not remove user $USER_NAME (still in use?)" >&2
    fi
    printf '\nRemoved, including data.\n'
else
    printf '\nRemoved. Kept: %s (data), %s (config), user %s.\nRun again with --purge-data to delete those too.\n' \
        "$DATA_DIR" "$CONF_DIR" "$USER_NAME"
fi
