#!/usr/bin/env bash
# Cut a new desktop release.
#
#   scripts/release.sh 0.2.0
#
# Sets the version in the three files that must agree, commits, and creates the
# annotated tag `v<version>`. Pushing that tag is what starts
# `.github/workflows/desktop-build.yml`, which builds every OS and publishes the
# GitHub Release + `latest.json` to `jana2u-pos-system/releases`.
#
# It does NOT push — review `git show` first, then:
#   git push && git push --tags
set -euo pipefail

VERSION="${1:-}"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: scripts/release.sh <major.minor.patch>   (e.g. 0.2.0)" >&2
  exit 1
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TAG="v${VERSION}"
if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
  echo "tag ${TAG} already exists — bump to a new version" >&2
  exit 1
fi
if [[ -n "$(git status --porcelain)" ]]; then
  echo "working tree is dirty — commit or stash first" >&2
  exit 1
fi

# Surgical version bump — replace only the version string in each file, byte-for-
# byte everything else (a JSON round-trip would reflow arrays and escape non-ASCII).
python3 - "$VERSION" <<'PY'
import re, sys

version = sys.argv[1]
targets = [
    ("src-tauri/tauri.conf.json", r'("version":\s*)"[^"]*"'),   # top-level, first match
    ("package.json", r'("version":\s*)"[^"]*"'),                 # top-level, first match
    ("src-tauri/Cargo.toml", r'(?m)^(version\s*=\s*)"[^"]*"'),   # [package], first match
]
for path, pattern in targets:
    src = open(path).read()
    out, n = re.subn(pattern, lambda m: f'{m.group(1)}"{version}"', src, count=1)
    if n != 1:
        sys.exit(f"{path}: expected exactly 1 version match, found {n}")
    open(path, "w").write(out)
    print("updated", path)
PY

# keep Cargo.lock in step so CI doesn't have a dirty tree
( cd src-tauri && cargo update -p jana2u-pos-desktop --precise "$VERSION" >/dev/null 2>&1 || true )

git add src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock package.json
git commit -m "release: v${VERSION}"
git tag -a "${TAG}" -m "Jana2U POS desktop ${TAG}"

echo
echo "Committed and tagged ${TAG}. Review with:  git show ${TAG}"
echo "Then publish with:                          git push && git push --tags"
