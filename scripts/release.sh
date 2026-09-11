#!/usr/bin/env bash
# Emergency / offline desktop release.
#
#   scripts/release.sh 0.2.2                    # explicit version
#   scripts/release.sh --auto                   # compute from conventional commits
#   scripts/release.sh --auto --bump-submodules # also pull frontend/backend/document-server
#
# The normal path is automatic: merge a `feat:` / `fix:` / `perf:` / breaking
# commit to `main` and `.github/workflows/release.yml` does all of this in CI.
# Use this script only when CI can't (offline, Actions outage, a release that
# must pin specific submodule commits).
#
# It bumps package.json (the single source of truth — tauri.conf.json points at
# it), commits `chore(release): v<version>`, and creates the annotated tag
# `v<version>`. It does NOT push. Review, then:
#
#   git show v<version>
#   git push && git push origin v<version>
#
# The tag push starts `.github/workflows/desktop-build.yml` via its `push: tags`
# trigger, which builds every OS and publishes the GitHub Release + `latest.json`
# to `jana2u-pos-system/releases`
set -euo pipefail

VERSION=""
BUMP_SUBMODULES=0
for arg in "$@"; do
  case "$arg" in
    --auto) VERSION="__AUTO__" ;;
    --bump-submodules) BUMP_SUBMODULES=1 ;;
    -*) echo "unknown flag: $arg" >&2; exit 1 ;;
    *) VERSION="$arg" ;;
  esac
done

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "$VERSION" == "__AUTO__" ]]; then
  VERSION="$(scripts/ci/next-version.sh --dry-run)"
  if [[ "$VERSION" == "none" ]]; then
    echo "no releasable commits since the last v* tag — nothing to do" >&2
    exit 1
  fi
  echo "computed version: $VERSION"
fi

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: scripts/release.sh <major.minor.patch> | --auto   [--bump-submodules]" >&2
  exit 1
fi

TAG="v${VERSION}"
if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null; then
  echo "tag ${TAG} already exists — bump to a new version" >&2
  exit 1
fi
if [[ -n "$(git status --porcelain)" ]]; then
  echo "working tree is dirty — commit or stash first" >&2
  exit 1
fi

if [[ "$BUMP_SUBMODULES" -eq 1 ]]; then
  echo "pulling submodules to their tracked branches..."
  git submodule update --init --remote -- frontend backend document-server
  git submodule status
fi

python3 "$ROOT/scripts/set-version.py" "$VERSION"

git add package.json frontend backend document-server
git commit -m "chore(release): ${TAG}"
git tag -a "${TAG}" -m "Jana2U POS desktop ${TAG}"

echo
echo "Committed and tagged ${TAG}. Review with:  git show ${TAG}"
echo "Then publish with:                          git push && git push origin ${TAG}"
