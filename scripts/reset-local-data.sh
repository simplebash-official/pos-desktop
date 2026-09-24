#!/usr/bin/env bash
# Wipes ALL local data of the macOS desktop app, so the next launch is a
# first-run install (welcome wizard, empty database, device unlinked locally).
#
#   bash scripts/reset-local-data.sh           # asks for confirmation
#   bash scripts/reset-local-data.sh --yes     # no prompt (scripts / CI)
#   bash scripts/reset-local-data.sh --backup  # copy it to a .bak-<time> folder first
#
# This deletes the SQLite database (sales, invoices, customers, stock),
# cloud.json, settings, generated documents and logs. It cannot be undone.
# The shop's data in the cloud is NOT touched, but this computer's link is
# forgotten: the device entry stays in the cloud until it is revoked there.
set -euo pipefail

DATA_DIR="$HOME/Library/Application Support/com.simplebash.pos"
YES=0
BACKUP=0
for arg in "$@"; do
  case "$arg" in
    --yes | -y) YES=1 ;;
    --backup | -b) BACKUP=1 ;;
    -h | --help)
      sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown option: $arg (try --help)" >&2
      exit 2
      ;;
  esac
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "This script is for macOS only (data lives in ~/Library/Application Support)." >&2
  exit 1
fi

if [ ! -d "$DATA_DIR" ]; then
  echo "Nothing to reset: $DATA_DIR does not exist."
  exit 0
fi

# Deleting the database under a running app corrupts it and the app may
# re-create half of the folder on exit.
if pgrep -f "simplebash-pos-desktop|simplebash-backend|simplebash-document-server" >/dev/null 2>&1; then
  echo "SimpleBash POS is still running. Quit it (Cmd+Q) and run this again." >&2
  exit 1
fi

echo "This will permanently delete:"
echo "  $DATA_DIR  ($(du -sh "$DATA_DIR" | cut -f1))"

if [ "$YES" -ne 1 ]; then
  printf "Type 'delete' to continue: "
  read -r answer
  if [ "$answer" != "delete" ]; then
    echo "Cancelled. Nothing was deleted."
    exit 1
  fi
fi

if [ "$BACKUP" -eq 1 ]; then
  BAK="$DATA_DIR.bak-$(date +%Y%m%d-%H%M%S)"
  cp -R "$DATA_DIR" "$BAK"
  echo "==> backup saved to $BAK"
fi

rm -rf "$DATA_DIR"
echo "==> deleted. The next launch starts as a fresh install."
