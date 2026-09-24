#!/usr/bin/env bash
# Local desktop build with cloud account + sync switched ON.
#
# The cloud URLs are baked in at compile time (`option_env!` in
# src-tauri/src/cloud/mod.rs), so they must be set for the build itself.
# Official releases get them from repository variables in CI
# (.github/workflows/desktop-build.yml); this script is the local equivalent.
#
#   bash scripts/build-with-cloud.sh                 # build the installer
#   CLOUD_API_URL=https://auth.example.com \
#   CLOUD_SYNC_API_URL=https://pos-api.example.com \
#     bash scripts/build-with-cloud.sh               # self-hosted / fork
#
# Without TAURI_SIGNING_PRIVATE_KEY (only CI has it) the auto-update bundle
# cannot be signed, so it is skipped: you still get the .app / .dmg, just no
# `.app.tar.gz` + `.sig` updater artifacts. Never sign local builds with a
# throwaway key — it would not match the pubkey in tauri.conf.json.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Identity service (sign-in, device linking) and the POS cloud API that serves
# /api/sync/*. They are different hosts: without CLOUD_SYNC_API_URL sync falls
# back to CLOUD_API_URL and fails with "cloud request failed (404)".
export CLOUD_API_URL="${CLOUD_API_URL:-https://auth.simplebash.com}"
export CLOUD_SYNC_API_URL="${CLOUD_SYNC_API_URL:-https://pos-api.simplebash.com}"

echo "==> cloud account URL : $CLOUD_API_URL"
echo "==> cloud sync URL    : $CLOUD_SYNC_API_URL"

TAURI_ARGS=()
if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
  echo "==> no TAURI_SIGNING_PRIVATE_KEY: skipping signed updater artifacts"
  TAURI_ARGS+=(--config '{"bundle":{"createUpdaterArtifacts":false}}')
fi

# `tauri build` runs `npm run sidecars` and the frontend build itself
# (beforeBuildCommand), so there is nothing to run first.
npx tauri build ${TAURI_ARGS[@]+"${TAURI_ARGS[@]}"} "$@"

BUNDLE="$ROOT/src-tauri/target/release/bundle"
echo
echo "==> done. Open the fresh build:"
ls -d "$BUNDLE"/macos/*.app "$BUNDLE"/dmg/*.dmg 2>/dev/null || ls "$BUNDLE"
