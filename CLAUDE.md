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
