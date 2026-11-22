#!/usr/bin/env bash
# Copies this whole project folder into WSL's own filesystem (fast builds, and the place the
# design wants the server and its data to live). Leaves out target/ (build output).
#
#   scripts/copy-to-wsl.sh                  # copies to ~/projects/BigHearthData
#   scripts/copy-to-wsl.sh ~/work/bhdata    # copies to a folder of your choice
#
# Safe to run again: it adds and overwrites files, and never deletes anything in the destination.
set -euo pipefail

src="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
dest="${1:-$HOME/projects/$(basename "$src")}"

mkdir -p "$dest"
dest="$(cd "$dest" && pwd -P)"

# Refuse a destination that is the project, inside it, or a folder that contains it.
case "$dest/" in "$src/"*)
    echo "error: the destination is the project folder or inside it: $dest" >&2
    exit 1 ;;
esac
case "$src/" in "$dest/"*)
    echo "error: the destination contains the project folder: $dest" >&2
    exit 1 ;;
esac

echo "From: $src"
echo "To:   $dest"

# tar (always installed) keeps permissions, so scripts stay executable.
(cd "$src" && tar --exclude=./target -cf - .) | (cd "$dest" && tar -xf -)

# Windows checkouts can turn line endings into CRLF, which breaks shell scripts.
if grep -rIl $'\r' "$dest/scripts" >/dev/null 2>&1; then
    echo "warning: CRLF line endings found in scripts/. Fix with: sed -i 's/\r\$//' \"$dest\"/scripts/*.sh" >&2
fi

echo "Done. Next:  cd \"$dest\" && scripts/check.sh"
