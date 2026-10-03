# SimpleBash POS — Desktop

[![tauri-ci](https://github.com/simplebash-official/pos-desktop/actions/workflows/tauri-ci.yml/badge.svg)](https://github.com/simplebash-official/pos-desktop/actions/workflows/tauri-ci.yml)
[![Latest release](https://img.shields.io/github/v/release/simplebash-official/pos-desktop)](https://github.com/simplebash-official/pos-desktop/releases/latest)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](LICENSE)

**SimpleBash POS** as an installable app for **Windows, macOS and Linux** — a
point-of-sale system for repair and retail shops that runs entirely on the
shop's own computer and keeps working without an internet connection.

It is a [Tauri 2](https://tauri.app) shell that bundles the three SimpleBash
services into one app:

- the [backend](https://github.com/simplebash-official/pos-backend) API and the
  [document-server](https://github.com/simplebash-official/document-server)
  run as background processes, reachable only from the same computer (`127.0.0.1:8080` / `:8090`);
- the [frontend](https://github.com/simplebash-official/pos-frontend) runs in the app window;
- data is stored in local SQLite databases.

## The SimpleBash POS family

| Repository | What it is |
|---|---|
| [pos-backend](https://github.com/simplebash-official/pos-backend) | REST API — Rust / Axum, SQLite or MongoDB |
| [pos-frontend](https://github.com/simplebash-official/pos-frontend) | The cashier & back-office UI — React / Vite / Mantine |
| [document-server](https://github.com/simplebash-official/document-server) | Renders invoices, receipts, reports and labels to PDF — Rust / Typst |
| **pos-desktop** (this repo) | Windows / macOS / Linux app (Tauri) that bundles all three |

This repo holds only the desktop shell (`src-tauri/`), the build and release
pipeline, and docs. The three services are included as git submodules.

## Install

Download the installer for your system from the
[latest release](https://github.com/simplebash-official/pos-desktop/releases/latest):

| System | File |
|---|---|
| Windows (64-bit) | `…-setup.exe` |
| macOS (Apple Silicon) | `.dmg` |
| Linux (x86-64) | `.AppImage` |

The installers are not yet signed by Apple or Microsoft, so the first launch
needs one extra step.

**macOS** — macOS says *"SimpleBash POS.app" is damaged and can't be opened*.
It isn't damaged; the app just isn't notarized by Apple yet, so macOS blocks a
browser-downloaded copy. To allow it:

1. Open the `.dmg` and drag **SimpleBash POS** into **Applications**.
2. Open **Terminal** and run:

   ```bash
   curl -fsSL https://raw.githubusercontent.com/simplebash-official/pos-desktop/main/scripts/macos-allow-app.sh | bash
   ```

   This runs [`scripts/macos-allow-app.sh`](scripts/macos-allow-app.sh), which
   removes the download "quarantine" flag from the app. Prefer to type it
   yourself? The same thing is:

   ```bash
   xattr -dr com.apple.quarantine "/Applications/SimpleBash POS.app"
   ```

3. Open SimpleBash POS from Applications.

You only need this once per installation — in-app updates aren't affected.

**Windows** — SmartScreen shows "unknown publisher": choose **More info → Run anyway**.

On first launch a setup wizard creates the shop's admin account and can load
sample data to explore with.

### Updates

**Settings → Updates → Check for updates** downloads, verifies and installs a
new version, then restarts the app. Updates are signed, and the app refuses
any update not signed with the project's key. In-app updates don't need the
first-launch steps above.

### Where your data lives

Everything the app writes is kept in one folder, which updates never touch:

- macOS: `~/Library/Application Support/com.simplebash.pos/`
- Windows: `%APPDATA%\com.simplebash.pos\`
- Linux: `~/.local/share/com.simplebash.pos/`

It holds the databases, generated PDFs, logs and the app's locally generated
secrets. Back up that whole folder, or use **Settings → Backup** inside the app.

### Start fresh (delete all local data)

To wipe this computer's copy and start again from the setup wizard — for
example after testing with sample data — quit SimpleBash POS, then on macOS
or Linux run:

```bash
curl -fsSL https://raw.githubusercontent.com/simplebash-official/pos-desktop/main/scripts/reset-local-data.sh | bash
```

[`scripts/reset-local-data.sh`](scripts/reset-local-data.sh) asks you to type
`delete` before it removes anything, refuses to run while the app is open, and
also signs this computer out of the SimpleBash cloud. Options (add after
`bash -s --` when using the one-liner, e.g. `… | bash -s -- --backup`):

| Option | Effect |
|---|---|
| `--backup` | Copy the data to a `com.simplebash.pos.bak-<date>` folder first |
| `--keep-cloud-login` | Keep the cloud sign-in saved in the keychain |
| `--yes` | Skip the confirmation (for scripts) |

**This permanently deletes every sale, invoice, customer and product on this
computer.** Data already synced to the SimpleBash cloud is not touched. On
Windows, quit the app and delete the `%APPDATA%\com.simplebash.pos` folder.

### Optional: cloud account and sync

Builds made with a cloud address (`CLOUD_API_URL`, set at build time) show an
optional step to create or link a SimpleBash account and sync the shop's data
across devices. Builds without it run fully offline and never contact a server.

## Develop

### Prerequisites

- [Rust](https://rustup.rs) stable and [Node.js](https://nodejs.org) 22
- Tauri's system dependencies for your OS —
  see [Tauri prerequisites](https://tauri.app/start/prerequisites/)
  (on Linux: `libwebkit2gtk-4.1-dev`, `librsvg2-dev`, `patchelf` and friends)

### Run the app from source

```bash
git clone --recurse-submodules https://github.com/simplebash-official/pos-desktop.git
cd pos-desktop
npm ci                           # Tauri CLI
npm --prefix frontend ci         # frontend dependencies
npm run sidecars                 # build backend + document-server, stage templates/fonts
npm run dev                      # start the app (frontend hot-reloads)
```

`npm run sidecars` compiles the two Rust services in release mode, so the first
run takes a while. Re-run it after changing the backend or document-server.

### Build an installer

```bash
npm run build:desktop            # → src-tauri/target/release/bundle/
bash scripts/build-with-cloud.sh # the same, with cloud sign-in and sync enabled
```

Local builds can't produce the signed update bundle (only CI has the signing
key); you still get a working app and installer.

### Checks

```bash
cargo fmt   --manifest-path src-tauri/Cargo.toml --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test  --manifest-path src-tauri/Cargo.toml
```

Changes to the backend, frontend or document-server belong in their own
repositories, not in the submodule folders here.

### Project layout

```
src-tauri/
  src/orchestrator.rs   starts the two services, waits until healthy, shows the window
  src/cloud/            optional cloud account sign-in and device linking
  src/sync/             background sync agent
  src/logging/          unified activity log (Settings → Activity Log)
  src/printer.rs        native PDF printing (macOS print dialog via PDFKit)
  tauri.conf.json       windows, security policy, bundled binaries, updater
backend/ frontend/ document-server/   git submodules
scripts/                build, release and diagnostic scripts
docs/logging.md         activity log format
```

More detail on the shell: [`src-tauri/README.md`](src-tauri/README.md).

## Releases

Releases are automatic. Merging a [Conventional Commit](https://www.conventionalcommits.org)
to `main` with `feat:` (minor), `fix:` / `perf:` (patch) or a breaking change
runs [`.github/workflows/release.yml`](.github/workflows/release.yml), which:

1. works out the next version,
2. moves the three submodules to their latest commits,
3. bumps `package.json` (the single source of the app version) and tags `vX.Y.Z`,
4. builds macOS, Windows and Linux, signs the update bundles, and publishes a
   GitHub Release with the `latest.json` update manifest that installed apps check.

`docs:`, `chore:`, `ci:`, `refactor:`, `test:` and `style:` commits release nothing.
Runbook: [`RELEASE.md`](RELEASE.md); one-time setup: [`.github/RELEASING.md`](.github/RELEASING.md).

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Please use Conventional Commit
messages — on this repo they decide whether a merge ships a release.

## Security

See [`SECURITY.md`](SECURITY.md). Report vulnerabilities privately through
[GitHub's "Report a vulnerability"](https://github.com/simplebash-official/pos-desktop/security/advisories/new).

## License

[GNU Affero General Public License v3.0](LICENSE).
