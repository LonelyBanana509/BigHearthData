#!/usr/bin/env python3
"""Zip the BigHearthData project so a slice can be handed back.

Usage (from anywhere):
    python scripts/make_zip.py                  # writes ../BigHearthData.zip
    python scripts/make_zip.py --stamp          # writes ../BigHearthData-20261122-1530.zip
    python scripts/make_zip.py --out C:\\temp   # choose the output folder
    python scripts/make_zip.py --list           # show what would be zipped, write nothing

What goes in: every file in the project that git would track, i.e. everything
except what `.gitignore` excludes (target/, *.log, .idea/, .vscode/, ...) plus a
few hard excludes that must never be handed back (.git/, build output, caches,
virtualenvs, secrets, and earlier zips).

Details:
  * Everything sits under one top folder named after the project folder, so the
    zip unpacks to BigHearthData/ exactly like the one you start from.
  * Empty folders are kept (docs/specs/ must survive even before it has files).
  * Paths use forward slashes and file times are kept, so the zip is the same
    when built on Windows, WSL2 or Linux.
  * Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import fnmatch
import os
import re
import sys
import zipfile
from datetime import datetime
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
TOP_FOLDER = PROJECT_ROOT.name

# Never included, whatever .gitignore says. Directory names match at any depth.
ALWAYS_EXCLUDE_DIRS = {
    ".git", ".hg", ".svn",
    "target",                    # Rust build output (workspace and fuzz/)
    "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache",
    ".venv", "venv", "node_modules",
    "corpus", "artifacts",       # cargo-fuzz output under fuzz/
}
# File-name patterns never included.
ALWAYS_EXCLUDE_FILES = [
    "*.zip", "*.pyc", "*.pyo",
    "*.log", "*.rs.bk", ".DS_Store", "Thumbs.db",
    "*.swp", "*~",
    ".env", ".env.*", "*.pem", "*.key",   # secrets, e.g. test TLS keys
]


def parse_gitignore(path: Path) -> list[tuple[bool, bool, bool, str]]:
    """Return rules as (negated, dir_only, anchored, pattern)."""
    rules = []
    if not path.is_file():
        return rules
    for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        negated = line.startswith("!")
        if negated:
            line = line[1:]
        dir_only = line.endswith("/")
        line = line.rstrip("/")
        anchored = line.startswith("/") or "/" in line
        line = line.lstrip("/")
        if line:
            rules.append((negated, dir_only, anchored, line))
    return rules


def _glob_to_regex(pattern: str) -> re.Pattern[str]:
    """Translate a gitignore-style glob (with **) into a regex for a full relative path."""
    i, out = 0, ""
    while i < len(pattern):
        c = pattern[i]
        if pattern.startswith("**/", i):
            out += "(?:.*/)?"
            i += 3
        elif pattern.startswith("**", i):
            out += ".*"
            i += 2
        elif c == "*":
            out += "[^/]*"
            i += 1
        elif c == "?":
            out += "[^/]"
            i += 1
        else:
            out += re.escape(c)
            i += 1
    return re.compile(out + r"\Z")


def ignored_by_gitignore(rel: str, is_dir: bool, rules) -> bool:
    """Last matching rule wins, as in git. `rel` uses forward slashes."""
    result = False
    for negated, dir_only, anchored, pattern in rules:
        if dir_only and not is_dir:
            continue
        if anchored:
            hit = bool(_glob_to_regex(pattern).match(rel))
        else:
            hit = fnmatch.fnmatchcase(rel.rsplit("/", 1)[-1], pattern)
        if hit:
            result = not negated
    return result


def hard_excluded(name: str, is_dir: bool) -> bool:
    if is_dir:
        return name in ALWAYS_EXCLUDE_DIRS
    return any(fnmatch.fnmatchcase(name, pat) for pat in ALWAYS_EXCLUDE_FILES)


def collect(root: Path):
    """Yield (relative_posix_path, is_dir) for everything that belongs in the zip."""
    rules = parse_gitignore(root / ".gitignore")

    def walk(directory: Path, rel_prefix: str):
        try:
            entries = sorted(directory.iterdir(), key=lambda p: p.name.lower())
        except PermissionError:
            print(f"skipping unreadable folder: {directory}", file=sys.stderr)
            return
        kept_any = False
        for entry in entries:
            is_dir = entry.is_dir() and not entry.is_symlink()
            rel = f"{rel_prefix}{entry.name}"
            if hard_excluded(entry.name, is_dir) or ignored_by_gitignore(rel, is_dir, rules):
                continue
            if entry.is_symlink():
                print(f"skipping symlink: {rel}", file=sys.stderr)
                continue
            kept_any = True
            if is_dir:
                yield rel + "/", True
                yield from walk(entry, rel + "/")
            else:
                yield rel, False
        # (empty folders are still yielded by the parent, so docs/specs/ survives)
        _ = kept_any

    yield from walk(root, "")


def build_zip(root: Path, out_path: Path, items) -> tuple[int, int]:
    files = dirs = 0
    with zipfile.ZipFile(out_path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
        zf.write(root, f"{TOP_FOLDER}/")  # top folder entry
        for rel, is_dir in items:
            arcname = f"{TOP_FOLDER}/{rel}"
            src = root / rel.rstrip("/")
            zf.write(src, arcname)
            if is_dir:
                dirs += 1
            else:
                files += 1
    return files, dirs


def main() -> int:
    ap = argparse.ArgumentParser(description="Zip the project for handing back.")
    ap.add_argument("--out", type=Path, default=PROJECT_ROOT.parent,
                    help="folder for the zip (default: the folder that contains the project)")
    ap.add_argument("--name", default=TOP_FOLDER, help="zip base name (default: project folder name)")
    ap.add_argument("--stamp", action="store_true", help="append YYYYMMDD-HHMM to the zip name")
    ap.add_argument("--list", action="store_true", help="list what would be zipped and exit")
    args = ap.parse_args()

    items = list(collect(PROJECT_ROOT))

    if args.list:
        for rel, _ in items:
            print(rel)
        n_files = sum(1 for _, d in items if not d)
        print(f"\n{n_files} files would be zipped.")
        return 0

    name = args.name + (datetime.now().strftime("-%Y%m%d-%H%M") if args.stamp else "")
    args.out.mkdir(parents=True, exist_ok=True)
    out_path = (args.out / f"{name}.zip").resolve()

    # Never zip the zip into itself if --out points inside the project.
    items = [(r, d) for r, d in items if (PROJECT_ROOT / r.rstrip("/")).resolve() != out_path]

    tmp = out_path.with_suffix(".zip.tmp")
    files, dirs = build_zip(PROJECT_ROOT, tmp, items)
    os.replace(tmp, out_path)  # replaces any older zip of the same name

    size_kb = out_path.stat().st_size / 1024
    print(f"Wrote {out_path}")
    print(f"{files} files, {dirs} folders, {size_kb:,.1f} KB")
    return 0


if __name__ == "__main__":
    sys.exit(main())
