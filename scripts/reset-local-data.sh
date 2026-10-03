#!/usr/bin/env bash
# Wipes ALL local data of the SimpleBash POS desktop app (macOS and Linux), so
# the next launch is a first-run install: welcome wizard, empty database,
# device unlinked locally.
#
#   bash scripts/reset-local-data.sh                    # asks for confirmation
#   bash scripts/reset-local-data.sh --backup           # copy the data to a .bak-<time> folder first
#   bash scripts/reset-local-data.sh --yes              # no prompt (scripts / CI)
#   bash scripts/reset-local-data.sh --keep-cloud-login # keep the cloud sign-in in the keychain
#
# Without a checkout:
#   curl -fsSL https://raw.githubusercontent.com/simplebash-official/pos-desktop/main/scripts/reset-local-data.sh | bash
#
# This deletes the SQLite databases (sales, invoices, customers, stock),
# settings, generated documents, logs and the app's generated secrets, plus
# the cloud sign-in stored in the system keychain. It cannot be undone.
# The shop's data in the cloud is NOT touched, but this computer's link is
# forgotten: the device entry stays in the cloud until it is revoked there.
# On Windows, quit the app and delete %APPDATA%\com.simplebash.pos instead.
set -euo pipefail

APP_ID="com.simplebash.pos"
KEYCHAIN_SERVICE="com.simplebash.pos.cloud"
KEYCHAIN_ACCOUNT="vault"

YES=0
BACKUP=0
KEEP_CLOUD_LOGIN=0
for arg in "$@"; do
  case "$arg" in
    --yes | -y) YES=1 ;;
    --backup | -b) BACKUP=1 ;;
    --keep-cloud-login) KEEP_CLOUD_LOGIN=1 ;;
    -h | --help)
      sed -n '2,20p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown option: $arg (try --help)" >&2
      exit 2
      ;;
  esac
done

case "$(uname -s)" in
  Darwin) DATA_DIR="$HOME/Library/Application Support/$APP_ID" ;;
  Linux) DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/$APP_ID" ;;
  *)
    echo "Unsupported system. On Windows, quit the app and delete %APPDATA%\\$APP_ID." >&2
    exit 1
    ;;
esac

# Deleting the database under a running app corrupts it, and the app may
# re-create half of the folder on exit.
if pgrep -f "simplebash-pos-desktop|simplebash-backend|simplebash-document-server" >/dev/null 2>&1; then
  echo "SimpleBash POS is still running. Quit it (Cmd+Q / close the window) and run this again." >&2
  exit 1
fi

if [ ! -d "$DATA_DIR" ]; then
  echo "No local data found at $DATA_DIR."
else
  echo "This will permanently delete:"
  echo "  $DATA_DIR  ($(du -sh "$DATA_DIR" | cut -f1))"
fi
if [ "$KEEP_CLOUD_LOGIN" -ne 1 ]; then
  echo "  and the SimpleBash cloud sign-in saved in this computer's keychain"
fi

if [ "$YES" -ne 1 ]; then
  # Read the answer from the terminal, not stdin, so this also works when the
  # script itself arrives on stdin (curl … | bash).
  if [ ! -r /dev/tty ]; then
    echo "No terminal to confirm on; re-run with --yes to skip the prompt." >&2
    exit 1
  fi
  printf "Type 'delete' to continue: "
  read -r answer </dev/tty
  if [ "$answer" != "delete" ]; then
    echo "Cancelled. Nothing was deleted."
    exit 1
  fi
fi

if [ -d "$DATA_DIR" ]; then
  if [ "$BACKUP" -eq 1 ]; then
    BAK="$DATA_DIR.bak-$(date +%Y%m%d-%H%M%S)"
    cp -R "$DATA_DIR" "$BAK"
    echo "==> backup saved to $BAK"
  fi
  rm -rf "$DATA_DIR"
  echo "==> local data deleted"
fi

if [ "$KEEP_CLOUD_LOGIN" -ne 1 ]; then
  removed=0
  if command -v security >/dev/null 2>&1; then
    security delete-generic-password -s "$KEYCHAIN_SERVICE" -a "$KEYCHAIN_ACCOUNT" >/dev/null 2>&1 && removed=1
  elif command -v secret-tool >/dev/null 2>&1; then
    secret-tool clear service "$KEYCHAIN_SERVICE" username "$KEYCHAIN_ACCOUNT" >/dev/null 2>&1 && removed=1
  fi
  if [ "$removed" -eq 1 ]; then
    echo "==> cloud sign-in removed from the keychain"
  else
    echo "==> no saved cloud sign-in found"
  fi
fi

echo "Done. The next launch starts as a fresh install."
