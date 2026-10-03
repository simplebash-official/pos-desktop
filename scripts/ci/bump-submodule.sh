#!/usr/bin/env bash
# Move one submodule pin to the tip of its tracked branch and work out the
# Conventional Commit that describes the move.
#
#   scripts/ci/bump-submodule.sh <backend|frontend|document-server>
#
# Leaves the new gitlink in the working tree (not staged, not committed) and
# writes the PR/commit body to $BODY_FILE (default: $RUNNER_TEMP/bump-body.md).
# In CI, appends changed/title/old/new to $GITHUB_OUTPUT; otherwise prints them.
#
# The commit type is the highest one among the submodule commits being pulled in:
#   breaking (type! / BREAKING CHANGE) -> feat!   (release.yml: breaking)
#   feat                               -> feat    (release.yml: minor)
#   fix | perf                         -> fix     (release.yml: patch)
#   anything else                      -> chore   (no release; the pin still moves)
# so a docs-only change in a submodule updates the pin without cutting a release.
set -euo pipefail

SUB="${1:?usage: bump-submodule.sh <backend|frontend|document-server>}"
case "$SUB" in
  backend|frontend|document-server) ;;
  *) echo "bump-submodule: unknown submodule '$SUB'" >&2; exit 1 ;;
esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

BODY_FILE="${BODY_FILE:-${RUNNER_TEMP:-/tmp}/bump-body.md}"

emit() { # key value
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then echo "$1=$2" >> "$GITHUB_OUTPUT"; else echo "$1=$2"; fi
}

OLD="$(git rev-parse "HEAD:$SUB")"
git submodule sync -- "$SUB" >/dev/null
git submodule update --init --remote -- "$SUB"
NEW="$(git -C "$SUB" rev-parse HEAD)"

if [[ "$OLD" == "$NEW" ]]; then
  echo "bump-submodule: $SUB already at ${NEW:0:7}"
  emit changed false
  exit 0
fi

# Commits being pulled in. If the old pin is no longer reachable (history was
# rewritten upstream), fall back to just the new tip.
if git -C "$SUB" cat-file -e "${OLD}^{commit}" 2>/dev/null; then
  RANGE=("${OLD}..${NEW}")
else
  RANGE=(-n 1 "$NEW")
fi
LOG="$(git -C "$SUB" log --no-merges --format='%s%x1f%b%x1e' "${RANGE[@]}" || true)"

rank() { case "$1" in breaking) echo 3;; feat) echo 2;; fix) echo 1;; *) echo 0;; esac; }
level="chore"
consider() { [[ "$(rank "$1")" -gt "$(rank "$level")" ]] && level="$1" || true; }

type_re='^([a-zA-Z]+)(\([^)]*\))?(!)?:'
subjects=()
while IFS= read -r -d $'\x1e' record; do
  record="${record#"${record%%[![:space:]]*}"}"
  [[ -z "$record" ]] && continue
  subject="${record%%$'\x1f'*}"
  body="${record#*$'\x1f'}"
  subjects+=("$subject")

  if [[ "$body" == *"BREAKING CHANGE:"* || "$body" == *"BREAKING-CHANGE:"* ]]; then
    consider breaking
  fi
  if [[ "$subject" =~ $type_re ]]; then
    type="$(echo "${BASH_REMATCH[1]}" | tr '[:upper:]' '[:lower:]')"
    if [[ -n "${BASH_REMATCH[3]}" ]]; then
      consider breaking
    else
      case "$type" in
        feat) consider feat ;;
        fix|perf) consider fix ;;
      esac
    fi
  fi
done <<< "$LOG"

bang=""
case "$level" in
  breaking) type="feat"; bang="!" ;;
  *) type="$level" ;;
esac

TITLE="${type}(${SUB})${bang}: bump ${SUB} to ${NEW:0:7}"

{
  echo "Moves the \`${SUB}\` pin from \`${OLD:0:7}\` to \`${NEW:0:7}\`."
  echo
  echo "Changes pulled in (${#subjects[@]}):"
  echo
  n=0
  for s in "${subjects[@]}"; do
    n=$((n + 1))
    [[ $n -gt 50 ]] && { echo "- … and $(( ${#subjects[@]} - 50 )) more"; break; }
    echo "- ${s}"
  done
  echo
  case "$level" in
    chore) echo "Nothing releasable (no feat/fix/perf) — merging updates the pin without cutting a release." ;;
    *) echo "Merging this to \`main\` cuts a release (\`${type}${bang}\`)." ;;
  esac
} > "$BODY_FILE"

emit changed true
emit old "$OLD"
emit new "$NEW"
emit title "$TITLE"
emit body_file "$BODY_FILE"
