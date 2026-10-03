# SimpleBash POS — desktop bundle (`src-tauri`)

Packages the `frontend`, `backend`, and `document-server` into one installable
desktop app (Windows + macOS). The two Rust services run as **sidecar child
processes** on loopback; the webview loads the compiled frontend and talks to
`http://127.0.0.1:8080/api` exactly as the web build talks to the server.

## Layout

| Path | What |
|---|---|
| `src/lib.rs` | Tauri builder — plugins, single-instance, spawns `orchestrator::run`, kills sidecars on exit |
| `src/orchestrator.rs` | app-data dirs, generated secrets (`config.json`), first-run Typst asset copy, `SIDECARS` table + spawn + health-gate, reveal main window |
| `src/logging/` | unified activity log hub: event schema, writer thread, redaction, sidecar/frontend ingest, retention, viewer commands |
| `tauri.conf.json` | windows (splash + hidden main), CSP, `externalBin`, `resources` |
| `capabilities/default.json` | window/event/updater permissions for the webview; no shell access (the sidecars are spawned from Rust) |
| `entitlements.plist` | macOS hardened-runtime entitlements (loopback networking, run unsigned-by-Apple sidecars) |
| `binaries/` | *(gitignored)* sidecar binaries, produced by `scripts/build-sidecars.sh` |
| `resources/` | *(gitignored)* staged `templates/` + `fonts/` for document-server |

## Runtime data

Everything writable lives under the OS app-data dir (`~/Library/Application
Support/com.simplebash.pos/` on macOS, `%APPDATA%\com.simplebash.pos\` on Windows):

```
config.json                 generated jwt_secret + internal_api_key
installation.json            installation id + first-run time
db/backend/pos.db            backend SQLite (+ -wal/-shm)
db/document-server/…         document-server SQLite
generated_documents/         rendered invoice/receipt PDFs
assets/templates|fonts/      writable copy of the bundled Typst assets
logs/<YYYY-MM-DD>/<source>.jsonl   unified activity log (shell, frontend, backend,
                             document-server, installer); days > 7 old are gzipped
logs/inbox/                  JSON lines from external producers, ingested at launch
logs/logging.json            logging settings (bodies, SQL, UI trace)
```

The activity log is never deleted by the app. Schema, sources and how to add
logging: [`docs/logging.md`](../docs/logging.md). View it in **Settings → Logs**.

Back up the whole `com.simplebash.pos/` folder.

### Updates and your data

The in-app updater (Settings → Updates) and every installer only replace the
**application bundle**. Nothing under `com.simplebash.pos/` is touched, so sales,
settings, generated PDFs and the generated secrets survive an update. On the
first launch after a version change, `orchestrator::sync_render_assets` re-lays
the bundled Typst templates/fonts into `assets/` via `copy_dir_merge`, which
overwrites the shipped files but never deletes a template a shop added itself.
(The version it compares against `assets/.version` is `app.package_info().version`
— i.e. `package.json`, resolved through `tauri.conf.json`.)

Updates are delivered through the Tauri updater:

| Piece | Where |
|---|---|
| Update feed | `https://github.com/simplebash-official/pos-desktop/releases/latest/download/latest.json` |
| Signature check | `plugins.updater.pubkey` in `tauri.conf.json` (minisign; the private key is a CI secret) |
| Trigger | user clicks **Check for updates** in Settings — no silent/auto install |

## Develop

```bash
# from the repo root
npm install                        # installs @tauri-apps/cli
bash scripts/build-sidecars.sh     # compile backend + document-server (release), stage Typst assets
npm run tauri dev                  # or: npm run dev
```

`scripts/build-sidecars.sh` must be re-run whenever `backend/` or
`document-server/` source changes — `tauri dev` does not rebuild the sidecars.
`tauri build` runs it automatically (`beforeBuildCommand`).

## Build

```bash
npm run tauri build                # -> .app + .dmg (macOS) / .exe (Windows, NSIS)
```

Cross-OS builds (Linux AppImage, Windows NSIS, macOS dmg) run in CI on every
releasable push to `main` (and on a manual `v*` tag) —
`.github/workflows/desktop-build.yml` builds every platform, signs the updater
artifacts, and publishes a GitHub Release plus `latest.json` on this repo
(`simplebash-official/pos-desktop`). OS-level code signing (Apple Developer ID +
notarization, Windows Authenticode) is still deferred — the bundles are unsigned
to the OS, but the updater artifacts are cryptographically signed so auto-update
stays safe.

### Building your own copy (forks)

Forks do not have the official signing keys or release repo, and must not
receive the official update feed. Generate your own updater key and override the
two settings at build time — no source edit needed:

```bash
npx tauri signer generate -w ~/.tauri/myfork.key      # prints your public key
# fork.conf.json
# { "plugins": { "updater": {
#     "pubkey": "<your public key>",
#     "endpoints": ["https://example.com/latest.json"] } } }
TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/myfork.key)" \
  npm run tauri build -- --config fork.conf.json
```

Without a signing key, also set `"bundle": { "createUpdaterArtifacts": false }`
in the same override file and the build produces plain unsigned installers
(macOS/Windows will warn about an unidentified developer). The release workflows
only run on `simplebash-official/pos-compose`; a fork's CI never tries to
publish.

### Cutting a release

Automatic — merge a `feat:` / `fix:` / `perf:` / breaking commit to `main` and
`.github/workflows/release.yml` bumps `package.json`, pulls the submodules, tags,
builds and publishes. See [`../RELEASE.md`](../RELEASE.md). Emergency / offline:
`scripts/release.sh --auto --bump-submodules` then `git push && git push origin
v<ver>`.
