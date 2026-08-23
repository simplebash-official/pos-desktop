# Jana2U POS System

Full-stack POS (Point of Sale) system orchestrated via Docker Compose.

## Architecture

| Service | Technology | Internal Port | Host Port | Description |
| :--- | :--- | :--- | :--- | :--- |
| **`document-server`** | Rust / Axum / Typst | `8090` | `8090` | PDF & document generation engine (SQLite-backed) |
| **`backend`** | Rust / Axum / MongoDB | `8080` | `8080` | Core POS REST API & business logic |
| **`frontend`** | React / Vite / Nginx | `8081` | `8081` | Cashier UI / POS Single Page App |

---

## Getting Started

### 1. Clone with Submodules
To clone this orchestrator repository and pull all 3 services at once:

```bash
git clone --recurse-submodules <REPO_URL> jana2u-pos
cd jana2u-pos
```

*(If you already cloned without `--recurse-submodules`, initialize them with:)*
```bash
git submodule update --init --recursive
```

### 2. Configure Environment Variables
Copy the example environment file and fill in your secrets:

```bash
cp .env.example .env
```

Ensure `MONGODB_URI` points to your MongoDB instance (e.g. MongoDB Atlas), and `DOCUMENT_SERVER_API_KEY` matches between services.

### 3. Start the Stack

Build and start all services in detached mode:

```bash
docker compose up -d --build
```

Startup sequence:
1. `document-server` starts first and performs self-checks.
2. `backend` starts once `document-server` is healthy.
3. `frontend` starts once `backend` is healthy.

### 4. Seed First Admin Account (First-time only)

If running against a clean database:

```bash
docker compose exec backend /app/bin/seed_admin
```

### 5. Access the Services

- **Frontend App**: [http://localhost:8081](http://localhost:8081)
- **Backend API & Health**: [http://localhost:8080/api/health](http://localhost:8080/api/health)
- **Backend Swagger Docs**: [http://localhost:8080/docs](http://localhost:8080/docs)
- **Document Server Health**: [http://localhost:8090/api/health](http://localhost:8090/api/health)

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
