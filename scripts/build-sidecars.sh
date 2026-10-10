#!/usr/bin/env bash
# Builds the backend + document-server release binaries for the host target
# triple and stages them (plus the Typst templates/fonts) where Tauri's
# bundler expects them:
#   src-tauri/binaries/simplebash-<svc>-<triple>[.exe]   (bundle.externalBin)
#   src-tauri/resources/{templates.pack,fonts/}         (bundle.resources)
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

# Prevent macOS ld64 dyld "mis-aligned LINKEDIT string pool" bug on proc-macro dylibs
export RUSTFLAGS="${RUSTFLAGS:-} -C strip=none"

echo "==> building simplebash-backend ($TRIPLE)"
cargo build --release --manifest-path "$ROOT/backend/Cargo.toml" --bin simplebash_pos_backend
cp "$ROOT/backend/target/release/simplebash_pos_backend$EXE" \
   "$BIN_DIR/simplebash-backend-$TRIPLE$EXE"

echo "==> building simplebash-document-server ($TRIPLE)"
cargo build --release --manifest-path "$ROOT/document-server/Cargo.toml" --bin document_server
cp "$ROOT/document-server/target/release/document_server$EXE" \
   "$BIN_DIR/simplebash-document-server-$TRIPLE$EXE"

if [ -z "$EXE" ] && command -v strip >/dev/null 2>&1; then
  echo "==> stripping binaries"
  strip "$BIN_DIR/simplebash-backend-$TRIPLE" "$BIN_DIR/simplebash-document-server-$TRIPLE" || true
fi

echo "==> staging Typst templates + fonts"
rm -rf "$RES_DIR/templates" "$RES_DIR/templates.pack" "$RES_DIR/fonts"
# The real designs are private (simplebash-official/document-templates): CI
# checks it out into ./document-templates, the dev workspace has it at
# ../document-templates. Without it (forks, fresh clones) the public examples ship.
TPL_DIR="$(mktemp -d)"
trap 'rm -rf "$TPL_DIR"' EXIT
PRIVATE_TPL=""
for d in "$ROOT/document-templates" "$ROOT/../document-templates"; do
  [ -d "$d/documents" ] && { PRIVATE_TPL="$d"; break; }
done
if [ -n "$PRIVATE_TPL" ]; then
  "$ROOT/document-server/scripts/assemble-templates.sh" "$PRIVATE_TPL" "$TPL_DIR"
else
  echo "    (no document-templates/ checkout: bundling the public example templates)"
  cp -R "$ROOT/document-server/templates/." "$TPL_DIR/"
fi
# runtime doesn't need the docs
find "$TPL_DIR" -name 'CLAUDE.md' -delete 2>/dev/null || true
# One AES zip instead of plain files (key: $TEMPLATES_PACK_KEY, else the dev key).
cargo run --release --quiet --manifest-path "$ROOT/scripts/pack-templates/Cargo.toml" -- \
  "$TPL_DIR" "$RES_DIR/templates.pack"
cp -R "$ROOT/document-server/fonts" "$RES_DIR/fonts"

echo "==> done"
ls -la "$BIN_DIR"
