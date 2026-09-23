#!/usr/bin/env bash
# Sets up relative symlinks to sibling repositories (frontend, backend, document-server)
# for local development in pos-desktop.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "==> Creating sibling symlinks in desktop workspace..."

# Ensure target sibling directories exist
for repo in frontend backend document-server; do
  if [ ! -d "$ROOT/../$repo" ]; then
    echo "::warning:: Sibling directory $ROOT/../$repo not found. Run git clone for $repo first."
  else
    ln -sfn "../$repo" "$ROOT/$repo"
    echo "  ✓ $repo -> ../$repo"
  fi
done

echo "==> Sibling links established:"
ls -l "$ROOT/frontend" "$ROOT/backend" "$ROOT/document-server"
