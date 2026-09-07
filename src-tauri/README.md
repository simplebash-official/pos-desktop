# Jana2U POS — desktop bundle (`src-tauri`)

Packages the `frontend`, `backend`, and `document-server` into one installable
desktop app (Windows + macOS). The two Rust services run as **sidecar child
processes** on loopback; the webview loads the compiled frontend and talks to
`http://127.0.0.1:8080/api` exactly as the web build talks to the server.

## Layout

| Path | What |
|---|---|
| `src/lib.rs` | Tauri builder — plugins, single-instance, spawns `orchestrator::run`, kills sidecars on exit |
| `src/orchestrator.rs` | app-data dirs, generated secrets (`config.json`), first-run Typst asset copy, sidecar spawn + health-gate, reveal main window |
| `tauri.conf.json` | windows (splash + hidden main), CSP, `externalBin`, `resources` |
| `capabilities/default.json` | core perms + the two sidecar `shell:allow-execute` scopes |
| `entitlements.plist` | macOS hardened-runtime entitlements (loopback networking, run unsigned-by-Apple sidecars) |
| `binaries/` | *(gitignored)* sidecar binaries, produced by `scripts/build-sidecars.sh` |
| `resources/` | *(gitignored)* staged `templates/` + `fonts/` for document-server |

## Runtime data

Everything writable lives under the OS app-data dir (`~/Library/Application
Support/com.jana2u.pos/` on macOS, `%APPDATA%\com.jana2u.pos\` on Windows):

```
config.json                 generated jwt_secret + internal_api_key
db/backend/pos.db            backend SQLite (+ -wal/-shm)
db/document-server/…         document-server SQLite
generated_documents/         rendered invoice/receipt PDFs
assets/templates|fonts/      writable copy of the bundled Typst assets
logs/                        sidecar + app logs
```

Back up the whole `com.jana2u.pos/` folder.

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

Cross-OS builds run in CI (`.github/workflows/desktop-build.yml`). Code signing
(Apple Developer ID + notarization, Windows cert) and auto-update are not wired
yet — see the project plan.
