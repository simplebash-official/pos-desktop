# Releasing the desktop app

Releases are **automatic**: `.github/workflows/release.yml` runs on every push to
`main`, and when it finds a releasable Conventional Commit since the last `v*`
tag (`feat:` → minor, `fix:`/`perf:` → patch, `<type>!` / `BREAKING CHANGE` →
bump; `docs`/`chore`/`ci`/`refactor`/`test`/`build`/`style` → nothing) it:

1. computes the next version (`scripts/ci/next-version.sh`),
2. pulls `frontend` + `backend` + `document-server` to their tracked-branch tips,
3. bumps `package.json` (`scripts/set-version.py` — the single source of truth),
4. commits `chore(release): vX.Y.Z [skip ci]` and tags `vX.Y.Z` on `main`
   (pushed with the default `GITHUB_TOKEN`),
5. calls `.github/workflows/desktop-build.yml` (a reusable workflow) to build
   every OS and publish the GitHub Release + `latest.json` to
   `simplebash-official/releases`.

`desktop-build.yml` has three entry points:

| trigger | what it does |
|---|---|
| `workflow_call` (from `release.yml`) | build + publish — the normal path |
| `workflow_dispatch` | build all 3 OSes; publish only if you tick `publish` |
| `push: tags: v*` | emergency / offline path — build + publish |

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

### 0. Branch protection — only if you have any

`release.yml` pushes the release commit + tag straight to `main`. This works
out of the box on a **Free** org plan with a **private** repo — rulesets aren't
enforced there and classic branch protection doesn't apply, so there is nothing
to do.

You only need to act if **both**: (a) the org is on **GitHub Team/Enterprise**
or the repo is **public**, *and* (b) `main` has a ruleset or classic protection
that restricts pushes / requires a PR / requires linear history. Then the CI
push fails with `GH006: Protected branch update failed`, and you fix it with:

> **Settings → Rules →** the `main` ruleset **→ Bypass list → Add bypass →** add
> the **`GitHub Actions`** actor (`github-actions[bot]`), mode "Always allow".
> Keep "Require a pull request before merging" for humans — the bypass only
> exempts the Actions bot. Add the same bypass to a `v*` **tag** ruleset if one
> exists.
>
> Can't grant a bot bypass? Change the "Commit + tag on main" step in
> `release.yml` to push only the tag (`git push origin "$TAG"`), not `HEAD:main`.
> The tag still carries the bumped `package.json` + submodule pins; `main` just
> lags by the `chore(release)` commit, and `next-version.sh` works off tags.

### 1. Create the public releases repo

The Tauri updater fetches `latest.json` and the installers over plain HTTPS, so
they must live in a **public** repo (the source repos stay private):

```bash
gh repo create simplebash-official/releases --public \
  --description "Auto-published desktop releases for SimpleBash POS. Do not commit here by hand."
```

Add a short `README.md` there explaining it is auto-published.

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

### 3. Add repo secrets (this repo — `simplebash-official/pos-compose`)

| Secret | Value |
|---|---|
| `CI_SUBMODULE_TOKEN` | fine-grained PAT, **Contents: Read-only** on `pos-backend`, `document-server`, `pos-frontend`, `pos-compose` (the workflow's own `GITHUB_TOKEN` can't read the sibling private repos, so `actions/checkout` needs this for the submodule clones) |
| `TAURI_SIGNING_PRIVATE_KEY` | contents of `~/.simplebash-updater/simplebash-updater.key` |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | empty string (key was generated with no password) |
| `RELEASES_REPO_TOKEN` | fine-grained PAT, **Contents: Read and write** on `simplebash-official/releases` |

`CI_SUBMODULE_TOKEN` and `RELEASES_REPO_TOKEN` can be the **same** fine-grained PAT if
you give it Contents: Read+Write on all five repos — least-privilege is two separate
tokens, convenience is one.

```bash
gh secret set CI_SUBMODULE_TOKEN --repo simplebash-official/pos-compose --body "<paste PAT>"
gh secret set TAURI_SIGNING_PRIVATE_KEY --repo simplebash-official/pos-compose \
  < ~/.simplebash-updater/simplebash-updater.key
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo simplebash-official/pos-compose --body ""
gh secret set RELEASES_REPO_TOKEN --repo simplebash-official/pos-compose --body "<paste PAT>"
```

## Cutting a release

Normally you don't — merge a `feat:` / `fix:` / `perf:` / breaking commit to
`main` and `release.yml` does everything. See `RELEASE.md`.

Watch the run: `release / prepare` → `desktop-build / version-check` → 3 `build`
jobs → `publish`. When it is green,
`https://github.com/simplebash-official/releases/releases/latest/download/latest.json`
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
curl -sL https://github.com/simplebash-official/releases/releases/latest/download/latest.json | jq
```

Every `platforms.*` URL must resolve (200) and there must be one entry each for
`darwin-aarch64` (Apple Silicon), `linux-x86_64`, `windows-x86_64`.

> Intel Macs are not built — GitHub's `macos-13` runners are being retired and
> queue for hours. If you need them, add a `universal-apple-darwin` target
> cross-built on the `macos-14` runner.

Then, on a machine with an **older** version installed: Settings → Updates →
Check for updates → download → install → restart, and confirm prior sales +
login survived.
