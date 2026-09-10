# Jana2U POS System

Full-stack POS (Point of Sale) system orchestrated via Docker Compose.

## Architecture

| Service | Technology | Internal Port | Host Port | Description |
| :--- | :--- | :--- | :--- | :--- |
| **`document-server`** | Rust / Axum / Typst | `8090` | `8090` | PDF & document generation engine (SQLite-backed) |
| **`backend`** | Rust / Axum / MongoDB | `8080` | `8080` | Core POS REST API & business logic |
| **`frontend`** | React / Vite / Nginx | `8081` | `8081` | Cashier UI / POS Single Page App |

---

---

## Build & Deployment Environments

Jana2U POS supports two distinct operational environments:

1. **Desktop App (Tauri + SQLite)**:
   - Standalone offline POS cashier terminal.
   - Bundles backend & document-server as local sidecar processes on loopback (`127.0.0.1`).
   - Uses embedded local SQLite database files (stored in OS user AppData directory).
   - Zero external network dependencies.

2. **Web & Server Deployment (Docker Compose + MongoDB)**:
   - Multi-terminal network deployment accessible via web browsers.
   - Core API connects to a MongoDB cluster (e.g. MongoDB Atlas) or an optional containerized Mongo service.
   - Frontend SPA is served behind an Nginx reverse proxy routing `/api` to the backend.

---

## 1. Desktop Build Workflow (Tauri + SQLite)

### Setup & Development
```bash
# 1. Switch to desktop environment configuration
npm run env:desktop

# 2. Build local sidecar binaries (backend + document-server)
npm run sidecars

# 3. Launch Tauri in development mode
npm run dev
```

### Building the Desktop Installer
```bash
npm run build:desktop
# Outputs desktop installers (.dmg / .app on macOS, NSIS .exe on Windows) to src-tauri/target/release/bundle/
```

---

## 2. Web & Server Deployment (Docker Compose + MongoDB)

### Setup & Configuration
```bash
# 1. Switch to web environment configuration
npm run env:web

# 2. Open .env and set your MongoDB URI & production secrets:
#    - MONGODB_URI (e.g. your MongoDB Atlas cluster URI)
#    - MONGODB_DB_NAME=jana2u_pos_prod
#    - JWT_SECRET (generate via: openssl rand -hex 32)
#    - DOCUMENT_SERVER_API_KEY (generate via: openssl rand -hex 32)
```

### Start the Web Stack
```bash
# Start all containers in detached mode
npm run web:up
# (Equivalent to: docker compose --env-file .env.web up -d)

# Follow backend logs to confirm database connection & auto-seeding
npm run web:logs
```

> [!TIP]
> **Optional Self-Hosted MongoDB**:
> If you prefer running MongoDB directly in Docker rather than using MongoDB Atlas, add `--profile mongo` and set `MONGODB_URI=mongodb://mongo:27017` in `.env.web`:
> ```bash
> docker compose --profile mongo --env-file .env.web up -d
> ```

### Stopping the Web Stack
```bash
npm run web:down
```

---

## Access Points (Web / Server)

- **Web Frontend**: [http://localhost:8081](http://localhost:8081) (or your configured domain)
- **Backend Health Check**: [http://localhost:8080/api/health](http://localhost:8080/api/health)
- **Backend Swagger Docs**: [http://localhost:8080/docs](http://localhost:8080/docs)
- **Document Server Health**: [http://localhost:8090/api/health](http://localhost:8090/api/health)

Default initial admin bootstrapped by seeder:
- **Email**: `admin@pos.com`
- **Password**: `admin@1234`

---

## Stopping the Stack

```bash
docker compose down
```

To wipe persistent SQLite data and generated invoice PDFs:
```bash
docker compose down -v
```

---

## Updating Submodules

To pull the latest changes from all service repositories:

```bash
git submodule update --remote --merge
```

---

## Releases & Updates

Two independent update channels:

### Web

Pushing to `frontend` / `backend` / `document-server` builds a `:latest` image and
redeploys (each repo's `deploy.yml`). Browsers pick it up automatically: the running
app re-checks the service worker every ~15 min and shows an **"Update available"**
prompt (withheld while a sale is on the till). Clicking **Update now** asks for
confirmation, then reloads. `Settings → Updates` shows the running version
(also at `/version.json`) and a manual **Check for updates** button.

### Desktop

Releases are automatic. Merge a Conventional Commit to `main` and
`.github/workflows/release.yml` does the rest — `feat:` → minor, `fix:`/`perf:` →
patch, `<type>!` / `BREAKING CHANGE` → bump; `docs`/`chore`/`ci`/`refactor` ship
nothing. It computes the next version, pulls the three submodules to their latest
tips, bumps `package.json` (the single source of truth — `src-tauri/tauri.conf.json`
points its `version` there), commits + tags `vX.Y.Z` on `main`, then builds and
publishes.

CI builds Linux (AppImage/deb), Windows (NSIS) and macOS (dmg), signs the updater
artifacts, and publishes a GitHub Release + `latest.json` to the **public**
`jana2u-pos-system/releases` repo. Installed apps check that feed from
`Settings → Updates`, download, install and relaunch. Local data
(`%APPDATA%\com.jana2u.pos\` / `~/Library/Application Support/com.jana2u.pos/`) is
never touched by an update — see `src-tauri/README.md`.

Emergency / offline path: `scripts/release.sh --auto --bump-submodules` then
`git push && git push origin v<ver>`. Full runbook in
[`RELEASE.md`](RELEASE.md).

**One-time setup** — see [`.github/RELEASING.md`](.github/RELEASING.md): create the
public `releases` repo, generate the updater key, and add the repo secrets. (No
branch-protection change needed unless `main` later gets a restrictive ruleset.)
