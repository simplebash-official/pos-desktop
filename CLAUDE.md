# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

The **orchestrator / "compose" repo** for **jana2u-pos**. It pulls three private repos in as git
submodules — `backend` (Rust/Axum API, branch `master`), `document-server` (Rust/Typst PDF service,
repo `pdf-server`, branch `main`), `frontend` (React/Vite/Mantine PWA, branch `main`) — and adds a
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
macOS/Linux/Windows → publish the GitHub Release + `latest.json` to `jana2u-pos-system/releases`).

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
the app bundle — data under `com.jana2u.pos/` is never touched. Details: `src-tauri/README.md`.

## Recent Architecture Evolutions (Last 30 Days)

### Landed Features & Systems
1. **Automated CI/CD Release Pipeline (`.github/workflows/release.yml`)**:
   - Pushes to `main` evaluate Conventional Commit prefixes (`scripts/ci/next-version.sh`).
   - `feat:` bumps minor, `fix:`/`perf:` bumps patch, breaking bumps major/minor (`<1.0`). Routine commits (`chore:`, `docs:`, `ci:`, `refactor:`, `test:`) are ignored and do not waste CI runner minutes.
   - `package.json` is the sole version authority (`scripts/set-version.py`). `src-tauri/tauri.conf.json` resolves `"version": "../package.json"` dynamically.
   - Publishes installers for macOS (Apple Silicon `aarch64`), Windows (NSIS `x86_64`), and Linux (`.deb` / `.AppImage`) alongside the updater manifest `latest.json` in `jana2u-pos-system/releases`.
2. **Sidecar Process Termination & NSIS Hooks (`src-tauri/installer-hooks.nsh`)**:
   - Running background sidecars (`backend` on 8080, `document-server` on 8090) lock executable files on Windows and macOS.
   - Added Tauri command `terminate_sidecar_processes` (invoked before `downloadAndInstall()` in `UpdatesSection.tsx`) and uninstaller/installer NSIS hooks to forcibly stop orphan processes before replacing binaries.
3. **Desktop Data Backup & Restore**:
   - Full SQLite database export and transactional restore across all 25 tables.
   - Completely isolated to the desktop application (`isTauri()` gating); never exposed on web deployments.

### Future Implementation Rules
- **Submodule Push Invariant**: When adding features across submodules, you **MUST push the submodule commits to their remote tracking branches (`backend:master`, `frontend:main`, `document-server:main`) before merging the PR in this root compose repo**. The cloud CI pipeline pulls submodules using `git submodule update --remote`; if your changes only exist on a local detached HEAD, CI will compile a release with the stale remote code.
- **Version Integrity**: Never edit versions by hand. Use Conventional Commit prefixes to let CI increment versions, or use `scripts/release.sh <ver>` for emergency offline tagging.
- **Sidecar Port & Address Binding**: Desktop sidecars must strictly bind to loopback (`127.0.0.1`), never `0.0.0.0`, to prevent exposing internal endpoints on local networks.

### How Agents Can Help
- **Release Verification**:
  1. Inspect submodule status: `git submodule status` and verify all submodules are up to date with their remotes.
  2. Preview the automated version bump: `scripts/ci/next-version.sh --dry-run`.
  3. Validate conventional commit messages on PR branches to ensure expected release triggers.
- **Installer & Sidecar Validation**:
  - Test desktop sidecar build script: `bash scripts/build-sidecars.sh`.
  - Check Tauri compilation: `cargo check --manifest-path src-tauri/Cargo.toml`.
  - When modifying sidecars or updater logic, verify that process termination hooks in `src-tauri/src/lib.rs` and `installer-hooks.nsh` are preserved.

