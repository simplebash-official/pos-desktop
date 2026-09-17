# Making a release

Two things ship separately:

- **Web** (`frontend` / `backend` / `document-server`) — deploys **automatically** on every
  push to each repo's default branch. No release needed. Browsers pick it up within ~15 min
  (the "Update available" prompt) or via **Settings → Updates**.
- **Desktop** (this `pos-compose` repo) — ships **automatically** when a release-worthy commit
  lands on `main`. The rest of this file is about that.

The desktop app bundles *pinned* commits of the three submodules; the release pipeline pulls
them to their latest tracked branch tip as part of every release, so a merged web change is in
the next desktop release automatically.

---

## The whole thing

1. Make your change in the right repo, on a branch, and open a PR.
   - UI / API / PDF templates → `frontend` (branch `main`), `backend` (branch `master`), or
     `document-server` (branch `main`).
   - Desktop shell / orchestrator / CI → this `pos-compose` repo (branch `main`).
2. **Write the commit (or PR title, if you squash-merge) as a Conventional Commit:**

   | prefix | effect | version bump |
   |---|---|---|
   | `feat: …` | new feature | minor (`0.2.x → 0.3.0`) |
   | `fix: …` / `perf: …` | bug fix / perf | patch (`0.2.1 → 0.2.2`) |
   | `feat!: …` or a `BREAKING CHANGE:` footer | breaking | minor while `< 1.0`, else major |
   | `docs:` `chore:` `ci:` `refactor:` `test:` `build:` `style:` | — | **no release** |

3. Merge to the default branch.
   - A web repo → the web deploy runs itself.
   - This `pos-compose` repo → `.github/workflows/release.yml` runs. If your commit was
     `feat:` / `fix:` / `perf:` / breaking, it:
     1. computes the next version,
     2. pulls `frontend` + `backend` + `document-server` to their latest tips,
     3. bumps `package.json`,
     4. commits `chore(release): vX.Y.Z [skip ci]` + tags `vX.Y.Z` on `main`,
     5. builds macOS + Linux + Windows and publishes the GitHub Release + `latest.json` to
        `simplebash-official/releases`.

That's it. No script, no manual tag.

> **Only changed a web repo?** You still need one releasable commit on `pos-compose` to ship a
> desktop build (the release pipeline picks up the newer submodule tips regardless). Merge a
> `fix:`/`feat:` there, or push an empty one: `git commit --allow-empty -m "fix: pull latest
> submodules" && git push`.

---

## Watch (~25 min)

<https://github.com/simplebash-official/pos-compose/actions>

`release / prepare` → `desktop-build / version-check` → `build` ×3 (macOS Apple Silicon ·
Linux · Windows) → `publish`.

`publish` creates the GitHub Release + `latest.json` on
<https://github.com/simplebash-official/releases>.

---

## Verify

```bash
curl -sL https://github.com/simplebash-official/releases/releases/latest/download/latest.json | jq .version
# → "0.2.2"
```

Then on a machine running the **previous** version: **Settings → Updates → Check for
updates** → it offers the new version → download → install → restart. Sales and settings
(`~/Library/Application Support/com.simplebash.pos/` · `%APPDATA%\com.simplebash.pos\`) are untouched.

---

## Version numbers

`package.json` is the single source of truth. `src-tauri/tauri.conf.json` points its
`"version"` at `../package.json`; the Rust side reads it at runtime. **Never hand-edit the
version** — the pipeline (or `scripts/release.sh`) owns it.

`src-tauri/Cargo.toml` `version` is intentionally frozen and no longer tracks the app version.

---

## Emergency / offline release

When CI can't do it (offline, Actions outage, or you must pin specific submodule commits):

```bash
cd ~/Documents/projects/personal/jana2u-pos
git checkout main && git pull
git status                                  # must be clean

scripts/release.sh --auto --bump-submodules # or: scripts/release.sh 0.2.2
git show v0.2.2                             # sanity-check
git push && git push origin v0.2.2         # the tag push starts the build + publish
```

`scripts/release.sh` bumps `package.json`, optionally pulls the submodules, commits
`chore(release): v0.2.2`, and tags `v0.2.2`. It does **not** push. The tag push triggers
`.github/workflows/desktop-build.yml` via its `push: tags: v*` trigger.

---

## Notes

- **First launch of a browser-downloaded build is blocked** (unsigned): macOS "damaged" →
  `xattr -dr com.apple.quarantine "/Applications/SimpleBash POS.app"`; Windows SmartScreen →
  More info → Run anyway. In-app updates after that are clean. See `.github/RELEASING.md`.
- **`workflow_dispatch`** (Actions → desktop-build → Run workflow) builds all 3 platforms;
  it publishes only if you tick `publish`. Use it to check a build.
- One-time CI setup (secrets, the releases repo) lives in `.github/RELEASING.md`.
