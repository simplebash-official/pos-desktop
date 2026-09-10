#!/usr/bin/env python3
# Set the app version. `package.json` is the single source of truth — the Tauri
# bundle version comes from `src-tauri/tauri.conf.json` -> `"version": "../package.json"`,
# and the Rust side reads it at runtime via `app.package_info().version`.
#
#   python3 scripts/set-version.py 0.2.2
#
# Surgical: replaces only the version string, byte-for-byte everything else (a
# JSON round-trip would reflow arrays and escape non-ASCII). Used by both
# `scripts/release.sh` and `.github/workflows/release.yml`.
import re
import sys
from pathlib import Path

if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+", sys.argv[1]):
    sys.exit("usage: set-version.py <major.minor.patch>   (e.g. 0.2.2)")

version = sys.argv[1]
path = Path(__file__).resolve().parent.parent / "package.json"
src = path.read_text()
out, n = re.subn(r'("version":\s*)"[^"]*"', lambda m: f'{m.group(1)}"{version}"', src, count=1)
if n != 1:
    sys.exit(f"{path}: expected exactly 1 version match, found {n}")
path.write_text(out)
print(f"package.json -> {version}")
