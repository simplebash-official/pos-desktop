# Making a release

Two independent things ship separately:

- **Web** (`frontend` / `backend` / `document-server`) — deploys **automatically** on every
  push to the repo's default branch. No release needed. Browsers pick it up within ~15 min
  (the "Update available" prompt) or via **Settings → Updates**.
- **Desktop** (this `compose` repo) — ships only when **you cut a `v*` tag**. That's what the
  rest of this file is about.

The desktop app bundles *pinned* commits of the three submodules, so a web change is **not**
in the desktop app until you bump the pin and cut a release.

---

## 1. Make the change in the right repo

### A change to the UI / API / PDF templates

```bash
cd frontend            # or backend  (branch: master)  or document-server
git checkout -b my-change
# ... edit, run the repo's test gate ...
git add -A && git commit -m "feat: ..."
git push -u origin my-change
```

Open a PR on GitHub, review, **merge to the default branch**. The web deploy runs itself.

### A change to the desktop shell only (`src-tauri/`, orchestrator, CI)

```bash
cd ~/Documents/projects/personal/jana2u-pos      # the compose repo
git checkout -b my-change
# ... edit src-tauri/... ...
git add -A && git commit -m "..."
git push -u origin my-change
```

PR → merge to `main`.

---

## 2. Point the compose repo at the merged changes

```bash
cd ~/Documents/projects/personal/jana2u-pos
git checkout main && git pull

# pull the latest merged commit of whichever submodules changed:
git submodule update --remote frontend           # and/or backend, document-server
git submodule status                             # confirm the pins moved

git add frontend backend document-server         # only the ones that changed
git commit -m "chore: bump submodules for release"
git push
```

If you **only** changed `src-tauri/`, skip the submodule bump — just `git checkout main && git pull`.

---

## 3. Cut the release

```bash
cd ~/Documents/projects/personal/jana2u-pos
git checkout main && git pull
git status                        # MUST be clean — release.sh refuses a dirty tree

scripts/release.sh 0.2.2          # see "Version numbers" below
```

`release.sh` bumps `tauri.conf.json` + `Cargo.toml` + `package.json` to the same version,
commits `release: v0.2.2`, and creates the annotated tag `v0.2.2`. It does **not** push.

---

## 4. Review, then push

```bash
git show v0.2.2                   # sanity-check: 4 files, version numbers only

git push                         # the release commit
git push origin v0.2.2           # the tag  ← THIS starts the build
```

> Push the tag as `git push origin v0.2.2`, not `git push --tags` (and never with a
> trailing `.` — that's a git syntax error).

---

## 5. Watch (~25 min)

<https://github.com/jana2u-pos-system/compose/actions>

`version-check` → `build` ×3 (macOS Apple Silicon · Linux · Windows) → `publish`.

`publish` creates the GitHub Release + `latest.json` on
<https://github.com/jana2u-pos-system/releases>.

---

## 6. Verify

```bash
curl -sL https://github.com/jana2u-pos-system/releases/releases/latest/download/latest.json | jq .version
# → "0.2.2"
```

Then on a machine running the **previous** version: **Settings → Updates → Check for
updates** → it offers the new version → download → install → restart. Sales and settings
(`~/Library/Application Support/com.jana2u.pos/` · `%APPDATA%\com.jana2u.pos\`) are untouched.

---

## Version numbers

`scripts/release.sh X.Y.Z`:

| bump | when | example |
|---|---|---|
| patch | bug fixes | `0.2.1 → 0.2.2` |
| minor | new features | `0.2.x → 0.3.0` |
| major | breaking / milestone | `0.x → 1.0.0` |

Never hand-edit the version anywhere — always go through `scripts/release.sh`.

---

## The whole thing, once changes are merged

```bash
cd ~/Documents/projects/personal/jana2u-pos
git checkout main && git pull
git submodule update --remote frontend backend document-server
git add frontend backend document-server && git commit -m "chore: bump submodules"
scripts/release.sh 0.2.2
git show v0.2.2
git push && git push origin v0.2.2
```

---

## Notes

- **First launch of a downloaded build is blocked** (unsigned): macOS "damaged" →
  `xattr -dr com.apple.quarantine "/Applications/Jana2U POS.app"`; Windows SmartScreen →
  More info → Run anyway. In-app updates after that are clean. See `.github/RELEASING.md`.
- **`workflow_dispatch`** (Actions → desktop-build → Run workflow) builds all 3 platforms
  **without** publishing — use it to check a build before tagging.
- One-time CI setup (secrets, the releases repo) lives in `.github/RELEASING.md`.
