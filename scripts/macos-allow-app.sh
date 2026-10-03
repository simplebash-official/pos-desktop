#!/usr/bin/env bash
# ==============================================================================
# Let macOS open SimpleBash POS after downloading it from GitHub.
#
# The app is not yet notarized by Apple, so a browser-downloaded copy carries
# a "quarantine" flag and macOS refuses to open it with a misleading
# '"SimpleBash POS.app" is damaged and can't be opened' message. The app is
# not damaged: this script removes that flag. Needed once per installation;
# in-app updates are not quarantined.
#
# Usage (after dragging the app into Applications):
#
#   curl -fsSL https://raw.githubusercontent.com/simplebash-official/pos-desktop/main/scripts/macos-allow-app.sh | bash
#
# or, with the repository checked out:
#
#   bash scripts/macos-allow-app.sh
#   bash scripts/macos-allow-app.sh "/path/to/SimpleBash POS.app"   # app elsewhere
# ==============================================================================
set -euo pipefail

APP_NAME="SimpleBash POS.app"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "This script is only for macOS." >&2
  exit 1
fi

if [ $# -ge 1 ]; then
  app="$1"
elif [ -d "/Applications/$APP_NAME" ]; then
  app="/Applications/$APP_NAME"
elif [ -d "$HOME/Applications/$APP_NAME" ]; then
  app="$HOME/Applications/$APP_NAME"
else
  echo "Couldn't find \"$APP_NAME\" in /Applications or ~/Applications." >&2
  echo "Drag the app from the downloaded disk image into Applications first," >&2
  echo "or pass its location: bash macos-allow-app.sh \"/path/to/$APP_NAME\"" >&2
  exit 1
fi

if [ ! -d "$app" ] || [ "${app%.app}" = "$app" ]; then
  echo "\"$app\" is not an .app bundle." >&2
  exit 1
fi

echo "Removing the download quarantine flag from: $app"
if ! xattr -dr com.apple.quarantine "$app" 2>/dev/null; then
  # An app copied in by another user can be owned by them; ask for the
  # administrator password only in that case.
  echo "Administrator permission is needed for this copy of the app."
  sudo xattr -dr com.apple.quarantine "$app"
fi

echo "Done. You can now open SimpleBash POS from Applications."
