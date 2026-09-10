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

# tauri.conf.json — the version the updater compares and the release is named for
tmp="$(mktemp)"
python3 - "$VERSION" > "$tmp" <<'PY'
import json, sys
p = "src-tauri/tauri.conf.json"
d = json.load(open(p))
d["version"] = sys.argv[1]
json.dump(d, open(p, "w"), indent=2)
open(p, "a").write("\n")
print("updated", p)
PY
cat "$tmp"; rm -f "$tmp"

# src-tauri/Cargo.toml — first `version = "..."` under [package]
perl -0pi -e 's/^(version\s*=\s*)"[^"]+"/$1"'"$VERSION"'"/m if !$done++' src-tauri/Cargo.toml
echo "updated src-tauri/Cargo.toml"

# root package.json
python3 - "$VERSION" <<'PY'
import json, sys
p = "package.json"
d = json.load(open(p))
d["version"] = sys.argv[1]
json.dump(d, open(p, "w"), indent=2)
open(p, "a").write("\n")
print("updated", p)
PY

# keep Cargo.lock in step so CI doesn't have a dirty tree
( cd src-tauri && cargo update -p jana2u-pos-desktop --precise "$VERSION" >/dev/null 2>&1 || true )

git add src-tauri/tauri.conf.json src-tauri/Cargo.toml src-tauri/Cargo.lock package.json
git commit -m "release: v${VERSION}"
git tag -a "${TAG}" -m "Jana2U POS desktop ${TAG}"

echo
echo "Committed and tagged ${TAG}. Review with:  git show ${TAG}"
echo "Then publish with:                          git push && git push --tags"
