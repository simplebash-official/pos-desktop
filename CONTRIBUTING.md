# Contributing

This repository is the compose/desktop shell; the code lives in the
`backend` (Rust/Axum), `frontend` (React/Vite) and `document-server` (Rust)
submodules, each with its own repository and `CLAUDE.md` describing conventions.

- Clone with submodules: `git clone --recurse-submodules <url>`.
- Run the checks of the part you change: backend / document-server `make check`,
  frontend `npm run lint && npm run type-check && npm test`, desktop shell
  `cargo clippy --manifest-path src-tauri/Cargo.toml`.
- Every backend change needs a test (see `backend/CLAUDE.md`).
- Commit messages and PR titles use Conventional Commits (`feat:`, `fix:`,
  `docs:`, `chore:` …). On the official repo, `feat:`/`fix:`/`perf:` merged to
  `main` triggers a desktop release, so use a non-releasing prefix for anything
  that should not ship.
- Do not include customer data or secrets in issues, logs or screenshots.
