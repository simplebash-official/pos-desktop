# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

The **desktop shell repo** (`pos-desktop`) for **simplebash-pos**. It pulls three public repos in as git
submodules — `backend` (repo `pos-backend`, Rust/Axum API, branch `master`), `document-server`
(repo `document-server` — shared across future apps, not POS-specific, branch `main`), `frontend`
(repo `pos-frontend`, React/Vite/Mantine PWA, branch `main`) — and adds a
**Tauri 2.x desktop shell** (`src-tauri/`) that bundles all three as sidecar processes on loopback.
Its own code is only `src-tauri/`, the release scripts/workflows and the docs.

> Not to be confused with **`pos-compose`** — that is the *web deployment* repo, checked out as
> `pos/deployment` (Docker Compose, nginx, `scripts/deploy.sh`). Nothing here deploys the web stack.

Two deployment targets from the same backend/frontend code:
- **Web** — Docker Compose + MongoDB (multi-tenant, shop-code login), built and rolled out from
  `pos-compose` (`pos/deployment/scripts/deploy.sh`). While GitHub Actions credits are exhausted this is a
  manual build → GHCR → SSH rollout; see that repo's README.
- **Desktop** — Tauri + SQLite, works offline, one open shop at a time with each shop's data kept separately on the computer (see "Shops on this computer"). Built and released from this repo. See below.

Each submodule has its own directory-scoped `CLAUDE.md` (`backend/CLAUDE.md` etc.) that loads when
working under that tree.

## Before finishing any task: test everything that can be tested

A task is not done until everything in this codebase that *can* be tested, and everything related to the feature you implemented, has been tested and passes. Not just the lines you changed.

1. **The feature itself:** add or extend automated tests for every behaviour you added or changed (happy path, failure paths, edge cases), then run them.
2. **Everything around it:** run this repo's full suite and quality gates (below), not only the new tests. A change can break a caller far away.
3. **Across repos:** when a feature spans repos (backend, frontend, desktop shell, document-server, identity-server, app-frontend, deployment), run the gates in *every* repo you touched, plus the tests that exercise the whole path end to end.
4. **For real:** what automated tests cannot reach (UI behaviour, a running stack, long-lived connections, a deploy) is verified by running it: the local Docker stack (`LOCAL_DOCKER_GUIDE.md` at the workspace root), the desktop app, or the browser.
5. **Match CI's toolchain:** CI uses the latest stable Rust/Node. A lint that passes on an older local toolchain can still fail there, so update (`rustup update`) or run the gate with CI's version before calling it done.
6. **Report honestly:** say what you ran and its result. Anything you could not test is named, with the reason, and never presented as passing. A suite that silently skips (e.g. no database configured) is not a passing suite.

**This repo's gate:** in `src-tauri/`: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`; plus the gate of every app repo you changed, run in its working copy (`pos/backend`, `pos/frontend`, `pos/document-server`). Shell behaviour (sidecars, sync agent, updater) is checked by running the app.

## Repository layout (read before editing)

The workspace has **two checkouts of the same backend/frontend/document-server repos**:

| Path (under `pos/`) | What it is |
|---|---|
| `backend`, `frontend`, `document-server` | Your **working copies** — edit, test and commit app code here. The web deploy builds from these. |
| `desktop` (this repo) | The `pos-desktop` repo: Tauri shell, release pipeline, docs. |
| `desktop/backend`, `desktop/frontend`, `desktop/document-server` | **Submodule pins** of the same three repos, at whatever commit was last bumped. Read-only for day-to-day work. |
| `deployment` | The `pos-compose` repo (web stack). |

Rules that follow from this:

- **Keep the two checkouts level.** Before any task, compare each working copy with its pin (`git -C pos/<name> fetch ../desktop/<name>` then `git log HEAD..FETCH_HEAD` and `FETCH_HEAD..HEAD`), merge whatever the other side has, and only then build or run the app. The full routine, and the local Docker URLs for dev runs, are in each app repo's CLAUDE.md under "Keep the two checkouts in sync".
- **Never commit a pin change you did not mean to make.** A stale local submodule checkout (e.g. `pos/desktop/backend` still at an old commit) gets recorded as the new pin by `git commit -a` / `git add -A`, silently shipping old app code (5efa4b7 put v0.17.1 on an old backend). Run `git submodule update --init` after pulling, check `git diff --submodule` before committing, and stage only the files you changed. CI (`submodule-pins.yml`, `scripts/ci/check-submodule-pins.sh <base> [head]`) rejects any pin that moves backwards.
- **Never make app changes inside `desktop/backend|frontend|document-server`.** Change the app in its own working copy (`pos/backend` …), commit, **push**, and only then move the pin here
  (`git submodule update --remote <name>` + commit). A pin can only point at a commit that exists on GitHub.
- **Local-only commits never reach an installer.** Releases build the pins committed on `main`. Pushing an app repo makes it dispatch `submodule-updated`; `bump-submodule.yml` then opens a `bot/bump-<name>` PR here moving the pin. Merging that PR ships it. Push the app repos first.
- **The pins go stale.** Before building or testing the desktop app by hand, run `git submodule update --remote` (then `git submodule status` to see what moved). Pure shell work in `src-tauri/`, `scripts/` or the docs does not need it.
- The same code serves both targets; the desktop-only parts are gated with `isTauri()` on the frontend and `DATABASE_TYPE=sqlite` on the backend. Web-only parts (shop-code login, `TENANT_MODE=multi`, `/api/internal/provision`) never run inside the desktop app.

### Cloud sign-up and device link

The setup wizard's optional cloud step talks to the identity server (`cloud/identity-server`, auth.simplebash.com). It only appears when `CLOUD_API_URL` was set **at build time** — installers built without it (the current default) hide the step. Official release builds get it from the **repository variable** `CLOUD_API_URL` (Settings → Secrets and variables → Actions → Variables; set to `https://auth.simplebash.com`), read by `desktop-build.yml`. **Also set `CLOUD_SYNC_API_URL` to `https://pos-api.simplebash.com`**: `/api/sync/*` is served by the POS backend, not identity, and without it sync falls back to `CLOUD_API_URL` (auth.) and fails with `CLOUD_ERROR … (404)`. It is a variable rather than a literal on purpose: forks have no such variable, so their builds stay cloud-disabled instead of talking to our servers. Local builds: `CLOUD_API_URL=… npm run build:desktop`.
**Phone verification:** identity requires a verified Sri Lankan mobile for every sign-up (`OTP_REQUIRED=true`). The shell exposes `cloud_otp_send` (texts a 6-digit code) and `cloud_otp_verify` (returns a one-time `phoneProof`), and `cloud_register` takes `phone` + `phoneProof`; the account UI (`pos/frontend` → `features/account/components/PhoneVerification.tsx`, used by both the wizard's `RegisterStep` and Settings → Account) collects them. **Installed desktops that predate this cannot register** (identity answers `PHONE_NOT_VERIFIED`) until they update. The shell never logs the code or the proof, and logs the number as `***4567` only (`logging/redact.rs` also masks any `proof` key).
Desktop sign-ups send only the shop (no `posOwner`), so identity does **not** create a web Admin; the desktop's local Admin arrives later through device sync. Verify how sync handles a tenant that already has an Admin (a shop first created on app.simplebash.com) before enabling device linking for such shops.

### Shops on this computer

Each SimpleBash shop linked to this computer is a **profile** with its own database folder, so switching shop swaps the whole shop and two shops never mix rows. Code: `src-tauri/src/cloud/profiles.rs`; commands `profiles_list`, `profile_activate`, `cloud_link_cancel`; UI in `pos/frontend` (`features/account`: `ShopAccountCard` on the login screen, `SwitchShopModal`).

- **Layout:** `<app_data_dir>/profiles.json` is the registry and always exists after start-up. Each profile's `db/` (pos.db, document_server.db), `assets/` and `generated_documents/` live in `shops/<profileId>/`. The id is generated (`shop_<hex>`), never the server's tenant id, so nothing from the network becomes a path. `config.json`, `installation.json`, `cloud.json`, logs and the device key stay installation-wide.
- **Migration:** the first start after this feature creates the registry from `cloud.json` and moves the old `db/`, `assets/`, `generated_documents/` into the first profile (`recover` in `profiles.rs`, called from `lib.rs` before anything reads the link). Nothing else keeps the old paths. A missing or unreadable registry is an error, never a silent fresh start.
- **One active link:** `cloud.json` and the active keychain keys hold the open profile's link. A switch parks the outgoing link (tokens move to `<key>@<profileId>`) and restores the target's, then restarts the app (`restart_soon`: stops the sidecars itself because Tauri's `restart` skips `RunEvent::Exit`). `switching` in the registry makes an interrupted switch finish on the next boot.
- **A profile belongs to one shop:** once bound, it only ever syncs with that shop. Guards: `sync_session()` refuses a link that is not the open profile's, the sync agent stops with `LOCAL_TENANT_MISMATCH` if the database's `sync_state.tenant_id` differs from the session, and password sign-in (`cloud_login_and_link`) refuses to overwrite another shop's tokens (`SWITCH_NEEDS_BROWSER`); switching goes through the browser link.
- **Cancelling** a waiting link uses `cloud_link_cancel`, never `cloud_unlink` (which would drop the open shop's link).
- Identity's `link/poll` response carries `shopName` (required) so the card can name the shop.

## Releases are automatic — commit messages drive them

`.github/workflows/release.yml` runs on **every push to `main`**. It reads the Conventional Commit
messages since the last `v*` tag and, if any are release-worthy, cuts a full desktop release
(bump `package.json` → commit + tag on `main` → build
macOS/Linux/Windows → publish the GitHub Release + `latest.json` on this repo).

### Commit convention (this repo)

Write every commit — **and every squash-merge PR title** — as a Conventional Commit. The prefix
decides whether merging it ships a desktop release:

| prefix | meaning | release? | version bump |
|---|---|---|---|
| `feat: …` | user-facing feature | **yes** | minor (`0.2.x → 0.3.0`) |
| `fix: …` / `perf: …` | bug fix / performance | **yes** | patch (`0.2.1 → 0.2.2`) |
| `feat!: …` / `fix!: …` / a `BREAKING CHANGE:` footer | breaking | **yes** | minor while `< 1.0`, else major |
| `docs:` `chore:` `ci:` `refactor:` `test:` `build:` `style:` | everything else | **no** | — |

Consequences for how you work here:

- **Non-release work goes on a branch**, or lands on `main` under a non-releasing prefix
  (`chore:`, `ci:`, `docs:`, `refactor:`). A `refactor:` that genuinely changes behaviour should
  be `fix:` instead so it ships.
- **Never hand-edit the version.** `package.json` is the single source of truth
  (`src-tauri/tauri.conf.json` → `"version": "../package.json"`; the Rust side reads
  `app.package_info().version`). `src-tauri/Cargo.toml` `version` is frozen on purpose.
  `scripts/set-version.py` synchronizes both root `package.json` and `frontend/package.json` so the
  desktop app and web deployment share the exact same version number.
- The pipeline commits `chore(release): vX.Y.Z [skip ci]` and tags it. Don't fight that commit —
  `git pull` before starting new work.
- **Submodule pins move through bot PRs.** A push to `pos-backend` / `pos-frontend` / `document-server`
  triggers `bump-submodule.yml`, which opens `bot/bump-<name>` titled `feat|fix|chore(<name>): bump …`
  (type taken from the commits it pulls in; `chore` = no release). Review and merge it; the merge is
  the release trigger. Run it by hand: Actions → bump-submodule. Release builds use exactly the pins on `main`.

### Emergency / offline release

When CI can't run, or a release must pin specific submodule commits:

```bash
git checkout main && git pull && git status      # must be clean
scripts/release.sh --auto --bump-submodules       # or: scripts/release.sh 0.2.2
git show v0.2.2                                    # sanity-check
git push && git push origin v0.2.2               # tag push → desktop-build.yml (build + publish)
```

`scripts/release.sh` bumps `package.json`, optionally pulls the submodules, commits
`chore(release): v…`, tags, and stops (no push).

### `desktop-build.yml` entry points

`desktop-build.yml` is a **reusable workflow**. It runs via: `workflow_call` (from `release.yml`,
the normal path) · `workflow_dispatch` (build all 3 OSes; publishes only if you tick `publish` —
use it to check a build) · `push: tags: v*` (the emergency path above).

### Branch protection

`main` has a ruleset that blocks force-pushes and deletion (org admins bypass). The release
commit + tag are ordinary fast-forward pushes, so the pipeline is unaffected. Setup + fallback:
`.github/RELEASING.md`.
End-user runbook: `RELEASE.md`.

## Commands

```bash
npm run bench:logging            # activity-log overhead: off vs standard vs full (real sales)
npm install                      # @tauri-apps/cli
bash scripts/build-sidecars.sh   # compile backend + document-server, stage Typst assets
npm run tauri dev                # desktop app in dev
npm run tauri build              # desktop installers → src-tauri/target/release/bundle/

scripts/ci/next-version.sh --dry-run   # what version the next push to main would ship ("none" = nothing)

npm run env:desktop              # copy .env.desktop.example to .env
```

Rust gates for `src-tauri/`: `cargo fmt`, `cargo clippy --manifest-path src-tauri/Cargo.toml`,
`cargo check --manifest-path src-tauri/Cargo.toml`.

## Auto-update

Web (service-worker prompt) and desktop (`tauri-plugin-updater` against `latest.json`) are
independent channels, surfaced together in **Settings → Updates**. Desktop updates only replace
the app bundle — data under `com.simplebash.pos/` is never touched. Details: `src-tauri/README.md`.

## Recent Architecture Evolutions (Last 30 Days)

### Landed Features & Systems
1. **Automated CI/CD Release Pipeline (`.github/workflows/release.yml`)**:
   - Pushes to `main` evaluate Conventional Commit prefixes (`scripts/ci/next-version.sh`).
   - `feat:` bumps minor, `fix:`/`perf:` bumps patch, breaking bumps major/minor (`<1.0`). Routine commits (`chore:`, `docs:`, `ci:`, `refactor:`, `test:`) are ignored and do not waste CI runner minutes.
   - `package.json` is the sole version authority (`scripts/set-version.py`). `src-tauri/tauri.conf.json` resolves `"version": "../package.json"` dynamically.
   - Publishes installers for macOS (Apple Silicon `aarch64`), Windows (NSIS `x86_64`), and Linux (`.deb` / `.AppImage`) alongside the updater manifest `latest.json` on this repo's Releases.
2. **Sidecar Process Termination & NSIS Hooks (`src-tauri/installer-hooks.nsh`)**:
   - Running background sidecars (`backend` on 8080, `document-server` on 8090) lock executable files on Windows and macOS.
   - Tauri command `prepare_for_update` (invoked before `update.install()` in `UpdatesSection.tsx`) and uninstaller/installer NSIS hooks forcibly stop orphan processes before replacing binaries.
3. **Desktop Data Backup & Restore**:
   - Full SQLite database export and transactional restore across all 25 tables.
   - Completely isolated to the desktop application (`isTauri()` gating); never exposed on web deployments.
4. **Initial Installation Detection, Welcome Wizard & Deferred Database Seeding**:
   - Desktop orchestrator (`src-tauri/src/orchestrator.rs`) records `installation.json` in `app_data_dir()` on initial boot with unique `installation_id`, timestamp, version, and platform.
   - Sets `AUTO_SEED=false` in the desktop backend environment to defer database population to user choice.
   - Exposes system setup endpoints (`/api/system/setup-status`, `/api/system/setup`, `/api/system/installation`) and registers Tauri commands `get_installation_info` and `complete_installation_setup`.
   - Frontend guides the user through an onboarding wizard (`/welcome`): system verification, capability tour, and explicit choice between "Load Sample / Demo Data" and "Clean Database (Empty Tables)" with automated admin account creation and auto-login into the POS dashboard.

5. **Unified Desktop Activity Log** (`docs/logging.md`):
   - The shell (`src-tauri/src/logging/`) is the single writer of `<app_data_dir>/logs/<YYYY-MM-DD>/<source>.jsonl` — schema v1 JSON lines with local-offset `ts`, `ts_utc`, IANA `tz`, `boot_id`, `request_id`, redacted `data`. Days > 7 old are gzipped; nothing is deleted.
   - Sources: shell (`log::` + `LogEvent::shell(..).emit()`, panics, windows, commands via `CommandLog`), frontend (`invoke('log_ingest')` from `frontend/src/shared/logging`), every sidecar's stdout (`LOG_FORMAT=json` → `logging::ingest`), and `logs/inbox/*.jsonl` (the NSIS installer).
   - Sidecars are declared once in `orchestrator::SIDECARS` (`SidecarSpec`); each gets the `LOG_*` env contract. `X-Request-Id` ties a frontend click → backend request → SQL/domain events → document-server render.
   - Viewer: Settings → Activity Log (desktop + admin only) via `logs_*` commands.

### Future Implementation Rules
- **Submodule Push Invariant**: When adding features across submodules, you **MUST push the submodule commits to their remote tracking branches (`backend:master`, `frontend:main`, `document-server:main`) before merging the PR in this root compose repo**. The bump workflow moves pins to the remote branch tip; if your changes only exist locally, the bump PR (and any release) will not contain them.
- **Version Integrity**: Never edit versions by hand. Use Conventional Commit prefixes to let CI increment versions, or use `scripts/release.sh <ver>` for emergency offline tagging.
- **Logging Invariant**: Anything new must be observable in the activity log — frontend features use `logger.event` (+ `data-log-id` on critical controls, `data-log-redact` on sensitive fields); backend/document-server mutating service functions wrap in `core::logging::domain::tracked`; new Tauri commands use `CommandLog`; a new sidecar is one `SIDECARS` entry that prints JSON lines on stdout. Never log secrets unredacted. Contract: `docs/logging.md`.
- **Sidecar Port & Address Binding**: Desktop sidecars must strictly bind to loopback (`127.0.0.1`), never `0.0.0.0`, to prevent exposing internal endpoints on local networks.
  Another program on `:8080` (for example the Docker POS backend from `docker-compose.local.yml`, which listens on every address) can answer before the sidecar binds, including the startup health check. The log then shows `sidecar/port_busy`. The sync agent talks to the local backend through a non-pooled, no-proxy client (`local_client` in `src-tauri/src/sync/mod.rs`) so it never stays glued to the wrong program; keep it that way. Reach the Docker POS API at `http://localhost:8081`, not `:8080`.

### How Agents Can Help
- **Release Verification**:
  1. Inspect submodule status: `git submodule status` and verify all submodules are up to date with their remotes.
  2. Preview the automated version bump: `scripts/ci/next-version.sh --dry-run`.
  3. Validate conventional commit messages on PR branches to ensure expected release triggers.
- **Logging Overhead Benchmark** (re-run after any change to `src-tauri/src/logging/`, either service's `core/logging/`, or `frontend/src/shared/logging/`):
  1. `npm run bench:logging` — real sales (1,000 per mode, `--orders N` to change) against an isolated test DB with the log off/standard/full, piping each sidecar's stdout through the real ingest + writer (`src-tauri/examples/log_pipe.rs`). Report: `target/benchmark-reports/logging-overhead.md`.
  2. In-app: Settings → System Benchmark → **Measure logging only** (simulated sale flow, writes no shop data).
  3. Compare against the table in `docs/logging.md` and update it when the numbers move.
  - Invariants to keep: capping is one bounded pass (O(cap), never stringify a whole body), the hub queue is bounded by **bytes** (32 MB) as well as count, and a benchmark mode is in-memory only — `logs/logging.json` must never be rewritten by it.
- **Installer & Sidecar Validation**:
  - Test desktop sidecar build script: `bash scripts/build-sidecars.sh`.
  - Check Tauri compilation: `cargo check --manifest-path src-tauri/Cargo.toml`.
  - When modifying sidecars or updater logic, verify that process termination hooks in `src-tauri/src/lib.rs` and `installer-hooks.nsh` are preserved.

