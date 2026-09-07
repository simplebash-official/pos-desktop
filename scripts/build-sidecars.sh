#!/usr/bin/env bash
# Builds the backend + document-server release binaries for the host target
# triple and stages them (plus the Typst templates/fonts) where Tauri's
# bundler expects them:
#   src-tauri/binaries/jana2u-<svc>-<triple>[.exe]   (bundle.externalBin)
#   src-tauri/resources/{templates,fonts}/           (bundle.resources)
#
# Run automatically by `tauri build` via beforeBuildCommand; run manually
# before `tauri dev` (the sidecars must exist for a dev run too).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
BIN_DIR="$ROOT/src-tauri/binaries"
RES_DIR="$ROOT/src-tauri/resources"
EXE=""
case "$TRIPLE" in *windows*) EXE=".exe" ;; esac

mkdir -p "$BIN_DIR" "$RES_DIR"

echo "==> building jana2u-backend ($TRIPLE)"
cargo build --release --manifest-path "$ROOT/backend/Cargo.toml" --bin jana2u_pos_backend
cp "$ROOT/backend/target/release/jana2u_pos_backend$EXE" \
   "$BIN_DIR/jana2u-backend-$TRIPLE$EXE"

echo "==> building jana2u-document-server ($TRIPLE)"
cargo build --release --manifest-path "$ROOT/document-server/Cargo.toml" --bin document_server
cp "$ROOT/document-server/target/release/document_server$EXE" \
   "$BIN_DIR/jana2u-document-server-$TRIPLE$EXE"

if [ -z "$EXE" ] && command -v strip >/dev/null 2>&1; then
  echo "==> stripping binaries"
  strip "$BIN_DIR/jana2u-backend-$TRIPLE" "$BIN_DIR/jana2u-document-server-$TRIPLE" || true
fi

echo "==> staging Typst templates + fonts"
rm -rf "$RES_DIR/templates" "$RES_DIR/fonts"
cp -R "$ROOT/document-server/templates" "$RES_DIR/templates"
cp -R "$ROOT/document-server/fonts" "$RES_DIR/fonts"
# runtime doesn't need the docs
find "$RES_DIR/templates" -name 'CLAUDE.md' -delete 2>/dev/null || true

echo "==> done"
ls -la "$BIN_DIR"
