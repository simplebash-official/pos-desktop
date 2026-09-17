# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

The **orchestrator / "pos-compose" repo** for **simplebash-pos**. It pulls three private repos in as git
submodules — `backend` (repo `pos-backend`, Rust/Axum API, branch `master`), `document-server`
(repo `document-server` — shared across future apps, not POS-specific, branch `main`), `frontend`
(repo `pos-frontend`, React/Vite/Mantine PWA, branch `main`) — and adds a
**Tauri 2.x desktop shell** (`src-tauri/`) that bundles all three as sidecar processes on loopback.

Two deployment targets:
- **Web** — Docker Compose + MongoDB, deployed by each submodule's own `deploy.yml` on push to its
  default branch. Nothing in this repo drives the web deploy.
- **Desktop** — Tauri + SQLite. Built and released from this repo. See below.

Each submodule has its own directory-scoped `CLAUDE.md` (`backend/CLAUDE.md` etc.) that loads when
working under that tree.

## Releases are automatic — commit messages drive them

`.github/workflows/release.yml` runs on **every push to `main`**. It reads the Conventional Commit
messages since the last `v*` tag and, if any are release-worthy, cuts a full desktop release
(bump `package.json` → pull the 3 submodules to their tips → commit + tag on `main` → build
macOS/Linux/Windows → publish the GitHub Release + `latest.json` to `simplebash-official/releases`).

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
- **Bumping a submodule pin is not needed for a release** — the pipeline runs
  `git submodule update --remote` itself. You only bump a pin by hand for a non-release reason.

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

None needed on the current Free-org / private-repo setup — the pipeline pushes the release commit
+ tag to `main` directly. Only matters if `main` later gets a restrictive ruleset (Team plan or
public repo): then add a `github-actions[bot]` bypass. Setup + fallback: `.github/RELEASING.md`.
End-user runbook: `RELEASE.md`.

## Commands

```bash
npm run bench:logging            # activity-log overhead: off vs standard vs full (real sales)
npm install                      # @tauri-apps/cli
bash scripts/build-sidecars.sh   # compile backend + document-server, stage Typst assets
npm run tauri dev                # desktop app in dev
npm run tauri build              # desktop installers → src-tauri/target/release/bundle/

scripts/ci/next-version.sh --dry-run   # what version the next push to main would ship ("none" = nothing)

npm run env:desktop / env:web    # select the .env for the target
npm run web:up / web:down        # docker compose for the self-hosted web stack
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
   - Publishes installers for macOS (Apple Silicon `aarch64`), Windows (NSIS `x86_64`), and Linux (`.deb` / `.AppImage`) alongside the updater manifest `latest.json` in `simplebash-official/releases`.
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
- **Submodule Push Invariant**: When adding features across submodules, you **MUST push the submodule commits to their remote tracking branches (`backend:master`, `frontend:main`, `document-server:main`) before merging the PR in this root compose repo**. The cloud CI pipeline pulls submodules using `git submodule update --remote`; if your changes only exist on a local detached HEAD, CI will compile a release with the stale remote code.
- **Version Integrity**: Never edit versions by hand. Use Conventional Commit prefixes to let CI increment versions, or use `scripts/release.sh <ver>` for emergency offline tagging.
- **Logging Invariant**: Anything new must be observable in the activity log — frontend features use `logger.event` (+ `data-log-id` on critical controls, `data-log-redact` on sensitive fields); backend/document-server mutating service functions wrap in `core::logging::domain::tracked`; new Tauri commands use `CommandLog`; a new sidecar is one `SIDECARS` entry that prints JSON lines on stdout. Never log secrets unredacted. Contract: `docs/logging.md`.
- **Sidecar Port & Address Binding**: Desktop sidecars must strictly bind to loopback (`127.0.0.1`), never `0.0.0.0`, to prevent exposing internal endpoints on local networks.

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

