#!/usr/bin/env bash
# Fails when a submodule pin moves BACKWARDS between two commits of this repo.
#
# A pin should only ever move forward (to a descendant of the commit it pointed
# at before). Moving it to an ancestor silently drops already-shipped app code
# from the next desktop release. It happens when a commit in this repo is made
# while a local submodule checkout is stale (e.g. `pos/desktop/backend` still at
# an old commit): git records that stale commit as the new pin. 5efa4b7 did
# exactly that and v0.17.1 shipped an old backend.
#
# Usage: scripts/ci/check-submodule-pins.sh <base-rev> [head-rev]   (head defaults to HEAD)
# Only commit graphs are fetched (no file contents), so it takes seconds.
set -euo pipefail

base="${1:?usage: check-submodule-pins.sh <base-rev> [head-rev]}"
head="${2:-HEAD}"
# Submodule URLs in .gitmodules are relative (../pos-backend.git); resolve them
# against the organisation the way GitHub does.
org_url="${SUBMODULE_BASE_URL:-https://github.com/simplebash-official}"

if ! git cat-file -e "${base}^{commit}" 2>/dev/null; then
  echo "base ${base} is not available (new branch?); nothing to compare"
  exit 0
fi

failed=0
for sub in $(git config -f .gitmodules --get-regexp '^submodule\..*\.path$' | awk '{print $2}'); do
  old=$(git ls-tree "$base" "$sub" | awk '{print $3}')
  new=$(git ls-tree "$head" "$sub" | awk '{print $3}')
  if [ -z "$old" ] || [ -z "$new" ] || [ "$old" = "$new" ]; then
    continue
  fi
  rel=$(git config -f .gitmodules "submodule.${sub}.url")
  url="${org_url}/${rel#../}"
  tmp=$(mktemp -d)
  git init -q --bare "$tmp"
  git -C "$tmp" fetch -q --filter=tree:0 "$url" "$new" "$old"
  if git -C "$tmp" merge-base --is-ancestor "$old" "$new"; then
    echo "ok: ${sub} pin moves forward ${old:0:7} -> ${new:0:7}"
  else
    echo "::error::${sub} pin moved BACKWARDS or sideways: ${old:0:7} -> ${new:0:7}. ${new:0:7} does not contain ${old:0:7}, so this would drop shipped code. Usually a stale local submodule checkout: run 'git submodule update --init' and recommit without the pin change."
    failed=1
  fi
  rm -rf "$tmp"
done
exit "$failed"
