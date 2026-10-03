# Releasing the desktop app

Releases are **automatic**: `.github/workflows/release.yml` runs on every push to
`main`, and when it finds a releasable Conventional Commit since the last `v*`
tag (`feat:` → minor, `fix:`/`perf:` → patch, `<type>!` / `BREAKING CHANGE` →
bump; `docs`/`chore`/`ci`/`refactor`/`test`/`build`/`style` → nothing) it:

1. computes the next version (`scripts/ci/next-version.sh`),
2. bumps `package.json` (`scripts/set-version.py` — the single source of truth),
3. commits `chore(release): vX.Y.Z [skip ci]` and tags `vX.Y.Z` on `main`
   (pushed with the default `GITHUB_TOKEN`),
4. calls `.github/workflows/desktop-build.yml` (a reusable workflow) to build
   every OS and publish the GitHub Release + `latest.json` on this repo
   (`simplebash-official/pos-desktop`).

`desktop-build.yml` has three entry points:

| trigger | what it does |
|---|---|
| `workflow_call` (from `release.yml`) | build + publish — the normal path |
| `workflow_dispatch` | build all 3 OSes; publish only if you tick `publish` |
| `push: tags: v*` | emergency / offline path — build + publish |

## Submodule bumps (bot PRs)

The releases ship the `frontend` / `backend` / `document-server` pins that are on
`main`. Those move through reviewed PRs, not automatically:

1. A push to `pos-backend` (`master`), `pos-frontend` or `document-server` (`main`)
   runs that repo's `notify-desktop.yml`, which sends a `repository_dispatch`
   (`submodule-updated`) to this repo.
2. `bump-submodule.yml` moves that pin to the branch tip on `bot/bump-<name>` and
   opens (or updates) one PR. The title is `feat|fix|chore(<name>): bump …`, typed
   from the commits pulled in (`feat!` for breaking). `chore` (docs/ci/test only)
   moves the pin without releasing.
3. `tauri-ci` builds the sidecars from the new pins as the check.
4. Merge the PR -> push to `main` -> `release.yml` as above. Squash or merge
   commit both work; the PR title/commit carries the type.

Run it by hand: Actions -> `bump-submodule` -> Run workflow (one name, or `all`).
It is free on public repos (one short Linux job per push).

Secrets for this:

| Repo | Secret | Value |
|---|---|---|
| `pos-desktop` | `DESKTOP_BOT_TOKEN` | fine-grained PAT (or GitHub App token): **Contents: write**, **Pull requests: write** on `pos-desktop`. Must not be `GITHUB_TOKEN`, or `tauri-ci` never runs on the bump PR. |
| `pos-backend`, `pos-frontend`, `document-server` | `DESKTOP_DISPATCH_TOKEN` | fine-grained PAT (or App token): **Contents: write** on `pos-desktop` (what `repository_dispatch` requires). The same token as above works. |

```bash
gh secret set DESKTOP_BOT_TOKEN --repo simplebash-official/pos-desktop
for r in pos-backend pos-frontend document-server; do
  gh secret set DESKTOP_DISPATCH_TOKEN --repo simplebash-official/$r
done
```

If the bump PRs should merge themselves once `tauri-ci` is green, enable
auto-merge on the repo and run `gh pr merge --auto` in the PR step with the same
token (a merge by `GITHUB_TOKEN` would not start `release.yml`).

## Loop safety

The release commit + tag are pushed with `GITHUB_TOKEN`, which by GitHub's rules
does **not** start another workflow run — so no loop, no double build. Belt and
braces: the commit message carries `[skip ci]`, and even without it
`next-version.sh` finds no releasable commit after the fresh tag.

## Unsigned bundles — first-launch friction

OS code signing is deferred, so a **browser-downloaded** bundle is blocked on
first launch:

- **macOS** → *"SimpleBash POS.app" is damaged and can't be opened.* It is not
  damaged — macOS quarantines un-notarized apps. Fix once per install:
  `xattr -dr com.apple.quarantine "/Applications/SimpleBash POS.app"`.
- **Windows** → SmartScreen "unknown publisher" → *More info → Run anyway*.

The in-app updater (`Settings → Updates`) downloads updates programmatically,
which are **not** quarantined — so every update after the first install is
clean. The release notes on each GitHub Release repeat these steps.

To remove this entirely: Apple Developer ID + notarization and a Windows
Authenticode cert, wired into `desktop-build.yml`.

## One-time setup

### 0. Branch protection — required (the repos are public)

Releases ship the submodule pins on `pos-desktop` `main`, which bump PRs move to
the **tip** of `pos-backend` `master`, `pos-frontend` `main` and `document-server`
`main`, and the result is signed and pushed to every installed POS. The PR review
is the gate, but whatever can land on those branches can reach every till. On all four repos:

> **Settings → Rules → New branch ruleset** targeting the default branch:
> require a pull request with at least one approving review, block force
> pushes and deletions. On `pos-desktop`, add the **`GitHub Actions`** actor
> (`github-actions[bot]`) to the ruleset's **Bypass list** ("Always allow") so
> `release.yml` can still push the `chore(release)` commit and the tag; humans
> keep going through PRs. Add the same bypass to a `v*` **tag** ruleset if you
> create one.
>
> Can't grant a bot bypass? Change the "Commit + tag on main" step in
> `release.yml` to push only the tag (`git push origin "$TAG"`), not `HEAD:main`.
> The tag still carries the bumped `package.json` + submodule pins; `main` just
> lags by the `chore(release)` commit, and `next-version.sh` works off tags.

### 1. Where releases are published

Releases (installers + the signed `latest.json` updater manifest) are published
on **this** repo, and the app checks
`https://github.com/simplebash-official/pos-desktop/releases/latest/download/latest.json`.

> **Keep `simplebash-official/releases`.** Installs older than v0.8 only know that
> old feed. Its v0.8.0 release (the last one published there) is what moves them
> onto this repo's feed, so don't delete it or the repo. It is archived
> (read-only) and receives no new releases.

### 2. Generate the updater signing key

```bash
npx --prefix . tauri signer generate --ci --password "" \
  --write-keys ~/.simplebash-updater/simplebash-updater.key
```

- The **public** key is already in `src-tauri/tauri.conf.json` →
  `plugins.updater.pubkey`. If you regenerate the key, replace it there.
- Keep `~/.simplebash-updater/simplebash-updater.key` **out of git** and backed up
  somewhere safe. Losing it means no client can verify future updates.

> A key was generated during initial setup; its public half is committed. Only
> regenerate if the private key is lost or compromised (then every already-
> installed client must get one manual update to the new-pubkey build).
>
> **That key has no password.** Anyone who obtains it can sign an update that
> every install accepts, so with the source public it is worth protecting: the
> build workflow only exposes it to the final `tauri build` step, but a
> password means a leaked secret value alone is not enough. Generate keys with
> `--password "<strong password>"` from now on; adding one to the current key
> means a new key pair, i.e. the one-time manual update described above.

### 3. Add repo secrets (this repo — `simplebash-official/pos-desktop`)

| Secret | Value |
|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | contents of `~/.simplebash-updater/simplebash-updater.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | the key's password (see the note in step 2) |
| `CI_SUBMODULE_TOKEN` | *only while a submodule repo is private* — fine-grained PAT, **Contents: Read-only** on `pos-backend`, `document-server`, `pos-frontend`. Public submodules are cloned with the built-in token. |

Publishing on this repo uses the workflow's own `GITHUB_TOKEN`; no PAT is needed for it.

```bash
gh secret set TAURI_SIGNING_PRIVATE_KEY --repo simplebash-official/pos-desktop \
  < ~/.simplebash-updater/simplebash-updater.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo simplebash-official/pos-desktop --body "<key password>"
```

## Cutting a release

Normally you don't — merge a `feat:` / `fix:` / `perf:` / breaking commit to
`main` and `release.yml` does everything. See `RELEASE.md`.

Watch the run: `release / prepare` → `desktop-build / version-check` → 3 `build`
jobs → `publish`. When it is green,
`https://github.com/simplebash-official/pos-desktop/releases/latest/download/latest.json`
resolves and installed apps will offer the update.

### Emergency / offline

```bash
git checkout main && git pull && git status   # must be clean
scripts/release.sh --auto --bump-submodules    # or: scripts/release.sh 0.2.2
git show v0.2.2                                # sanity-check
git push && git push origin v0.2.2            # tag push → desktop-build.yml (build + publish)
```

This path needs no `main` ruleset bypass (a human pushes) and works offline up to
the `git push`. `workflow_dispatch` builds all 3 OSes without publishing (unless
you tick `publish`) — use it to check a build.

## Verifying

```bash
curl -sL https://github.com/simplebash-official/pos-desktop/releases/latest/download/latest.json | jq
```

Every `platforms.*` URL must resolve (200) and there must be one entry each for
`darwin-aarch64` (Apple Silicon), `linux-x86_64`, `windows-x86_64`.

> Intel Macs are not built — GitHub's `macos-13` runners are being retired and
> queue for hours. If you need them, add a `universal-apple-darwin` target
> cross-built on the `macos-14` runner.

Then, on a machine with an **older** version installed: Settings → Updates →
Check for updates → download → install → restart, and confirm prior sales +
login survived.
