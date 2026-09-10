# Releasing the desktop app

`.github/workflows/desktop-build.yml` builds every OS and publishes a GitHub
Release + the Tauri updater manifest (`latest.json`) on every `v*` tag push.

## One-time setup

### 1. Create the public releases repo

The Tauri updater fetches `latest.json` and the installers over plain HTTPS, so
they must live in a **public** repo (the source repos stay private):

```bash
gh repo create jana2u-pos-system/releases --public \
  --description "Auto-published desktop releases for Jana2U POS. Do not commit here by hand."
```

Add a short `README.md` there explaining it is auto-published.

### 2. Generate the updater signing key

```bash
npx --prefix . tauri signer generate --ci --password "" \
  --write-keys ~/.jana2u-updater/jana2u-updater.key
```

- The **public** key is already in `src-tauri/tauri.conf.json` →
  `plugins.updater.pubkey`. If you regenerate the key, replace it there.
- Keep `~/.jana2u-updater/jana2u-updater.key` **out of git** and backed up
  somewhere safe. Losing it means no client can verify future updates.

> A key was generated during initial setup; its public half is committed. Only
> regenerate if the private key is lost or compromised (then every already-
> installed client must get one manual update to the new-pubkey build).

### 3. Add repo secrets (this repo — `jana2u-pos-system/compose`)

| Secret | Value |
|---|---|
| `CI_SUBMODULE_TOKEN` | fine-grained PAT, **Contents: Read-only** on `backend`, `pdf-server`, `frontend`, `compose` (the workflow's own `GITHUB_TOKEN` can't read the sibling private repos, so `actions/checkout` needs this for the submodule clones) |
| `TAURI_SIGNING_PRIVATE_KEY` | contents of `~/.jana2u-updater/jana2u-updater.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | empty string (key was generated with no password) |
| `RELEASES_REPO_TOKEN` | fine-grained PAT, **Contents: Read and write** on `jana2u-pos-system/releases` |

`CI_SUBMODULE_TOKEN` and `RELEASES_REPO_TOKEN` can be the **same** fine-grained PAT if
you give it Contents: Read+Write on all five repos — least-privilege is two separate
tokens, convenience is one.

```bash
gh secret set CI_SUBMODULE_TOKEN --repo jana2u-pos-system/compose --body "<paste PAT>"
gh secret set TAURI_SIGNING_PRIVATE_KEY --repo jana2u-pos-system/compose \
  < ~/.jana2u-updater/jana2u-updater.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo jana2u-pos-system/compose --body ""
gh secret set RELEASES_REPO_TOKEN --repo jana2u-pos-system/compose --body "<paste PAT>"
```

## Cutting a release

```bash
scripts/release.sh 0.2.0
git show v0.2.0          # sanity-check the version bump
git push && git push --tags
```

Watch the run: `version-check` → 4 `build` jobs → `publish`. When it is green,
`https://github.com/jana2u-pos-system/releases/releases/latest/download/latest.json`
resolves and installed apps will offer the update.

`workflow_dispatch` runs the builds without publishing — use it to check a build
before tagging.

## Verifying

```bash
curl -sL https://github.com/jana2u-pos-system/releases/releases/latest/download/latest.json | jq
```

Every `platforms.*` URL must resolve (200) and there must be one entry each for
`darwin-aarch64` (Apple Silicon), `linux-x86_64`, `windows-x86_64`.

> Intel Macs are not built — GitHub's `macos-13` runners are being retired and
> queue for hours. If you need them, add a `universal-apple-darwin` target
> cross-built on the `macos-14` runner.

Then, on a machine with an **older** version installed: Settings → Updates →
Check for updates → download → install → restart, and confirm prior sales +
login survived.
