# Desktop activity log

The Jana2U POS desktop app keeps a permanent, append-only record of everything that happens on
the computer, from the first launch (and, on Windows, from the installer) onward: every click,
typed value, screen change, API call, database statement, business event, PDF render, update,
print job, crash and process start/stop, each stamped with the real local time, its UTC offset
and the IANA time zone.

The log is **desktop-only**. The web deployment is unchanged: the services keep plain-text
output, and the frontend logger is a no-op in a browser.

## Where it lives

```
<app data>/com.jana2u.pos/logs/
  2026-09-15/
    shell.jsonl            the Tauri shell: lifecycle, windows, sidecars, commands, panics
    frontend.jsonl         the webview: UI, navigation, API calls, state, errors, console
    backend.jsonl          backend sidecar stdout (HTTP, SQL, domain events, errors)
    document-server.jsonl  document-server sidecar stdout (HTTP, renders, SQL, errors)
    installer.jsonl        Windows installer / updater steps (from the inbox)
    backend.1.jsonl        rollover once a file passes 200 MB in a day
  2026-09-01/*.jsonl.gz    days older than 7 are gzipped; nothing is ever deleted
  inbox/                   JSON lines dropped by external producers, ingested at launch
  logging.json             options set in Settings → Activity Log
```

`<app data>` is `~/Library/Application Support` on macOS, `%APPDATA%` on Windows and
`~/.local/share` on Linux. The folder survives app updates. Admins can view, filter, live-tail,
trace and export the log from **Settings → Activity Log**.

## Architecture

```
 frontend (webview) ── invoke('log_ingest', batch) ─┐
 shell (log:: calls, explicit events, panics) ──────┤
 backend sidecar ───── JSON lines on stdout ────────┤──► LogHub ──► writer thread ──► logs/<day>/<source>.jsonl
 document-server ───── JSON lines on stdout ────────┤    normalize · redact · seq    └─► log://entries (live tail)
 any future sidecar ── JSON lines on stdout ────────┤
 NSIS installer ────── logs/inbox/*.jsonl ──────────┘
```

The shell (`src-tauri/src/logging/`) is the only writer. It's the one process alive from the first
instant, and it already owns every sidecar's stdout. A single writer also avoids cross-process
file-lock problems on Windows.

- **Never blocks.** Producers push onto a bounded queue (100k events). If it overflows, events are
  dropped and a `system/log.dropped` entry records the count.
- **Flush policy.** The writer flushes every 250 ms, immediately on `error`/`fatal`, and
  synchronously on exit, update and panic.
- **Early events.** Events from before the app data folder is known are buffered in memory.

## Record schema (v1)

One JSON object per line:

| field | meaning |
|---|---|
| `v` | schema version (`1`) |
| `seq` | per-launch sequence assigned by the hub, strictly increasing in write order |
| `ts` | origin time, RFC 3339 **with local offset** (`2026-09-15T10:22:01.123456+05:30`) |
| `ts_utc` | the same instant in UTC |
| `tz` | IANA zone of the machine (`Asia/Colombo`) |
| `source` | `shell` · `frontend` · `backend` · `document-server` · `installer` · … (decided by the transport, a producer can't set it) |
| `level` | `trace` · `debug` · `info` · `warn` · `error` · `fatal` |
| `category` | `lifecycle` `window` `sidecar` `command` `system` `ui` `nav` `http` `db` `domain` `render` `print` `updater` `state` `query` `error` `console` `perf` `app` … (open set) |
| `event` | what happened within the category (`click`, `request`, `billing.sale_completed`, …) |
| `msg` | optional human summary |
| `boot_id` | one id per app launch, shared by every source in that launch |
| `installation_id` | from `installation.json` |
| `app_version`, `os` | build identity |
| `session_user` | signed-in user id, when known |
| `route` | frontend screen, when known |
| `request_id` | correlation id for one action across frontend → backend → document-server |
| `data` | everything else (structured, redacted, capped) |

### One action, end to end

The frontend's axios capture gives every API call an `X-Request-Id`. The backend adopts it,
echoes it on the response and forwards it to document-server, and both log inside a span that
carries it. Filtering by `request_id` ("Trace this request" in the viewer) returns the whole chain
in order: the click, the API request, the backend request, its SQL statements, the domain event,
the document-server render, and the API response.

## Redaction

Credentials and secrets are masked everywhere; customer data is kept on purpose. Every producer
applies these rules, and the shell applies them again before writing:

- **Sensitive keys.** The value of any object key matching
  `password|passwd|pwd|secret|token|authorization|cookie|api[_-]?key|jwt|cvv|card_?number|otp|pin`
  becomes `"[REDACTED]"`.
- **Secret-shaped strings.** Bearer tokens, JWTs (`eyJ….….…`) and 64-hex secrets are masked
  inside any string.
- **Frontend fields.** `type=password`, `autocomplete="cc-*"`, one-time codes and anything under
  `[data-log-redact]` are logged by length only.
- **Bodies.** Request and response bodies are capped (32 KB by default). PDFs, uploads and other
  binary content are logged by size only.

## Adding logging

### Frontend

```ts
import { logger } from '@/shared/logging';

logger.info('billing', 'discount.applied', { discountType, valueCents });
logger.error('print', 'native.error', err, { title });
```

Clicks, typing, navigation, API calls, Redux actions, queries, errors and the console are already
captured. Add explicit events for business or workflow steps only.

- **New category:** add it to `LogCategory` in `frontend/src/shared/logging/types.ts`.
- **Critical controls:** add `data-log-id="<feature>.<action>"`.
- **Sensitive fields:** mark them with `data-log-redact`.

### Backend / document-server (Rust)

- **Mutating service functions** wrap their body so each call records outcome, duration and the
  returned `key`:

  ```rust
  crate::core::logging::domain::tracked("customers.created", async move { /* body */ }).await
  ```

- **Any other line** should be a structured tracing event:

  ```rust
  tracing::info!(category = "render", event = "done", template_key = %key, pdf_bytes, "render completed");
  ```

  `category` and `event` become top-level fields. Other fields go into `data`, and `request_id`
  and `user_id` are picked up from the request span.

### Shell (Tauri)

```rust
LogEvent::shell("updater", "download.finished").data(json!({ "version": v })).emit();
```

Existing `log::info!` calls also land in `shell.jsonl`. Wrap new commands with
`CommandLog::start(name, args)` … `.finish(result)` to record their input, output and duration.

### A new server or sidecar (any language)

1. Add a `SidecarSpec` entry to `SIDECARS` in `src-tauri/src/orchestrator.rs` (name, log source,
   port, health path, env), plus `externalBin`, the capability scope, `scripts/build-sidecars.sh`
   and the NSIS `taskkill` list.
2. When the process sees `LOG_FORMAT=json`, print **one JSON object per line on stdout**. Either
   shape is understood:
   - the schema above (`ts`, `level`, `category`, `event`, `msg`, `request_id`, `data`), or
   - `tracing-subscriber` JSON (`timestamp`, `level`, `fields`/flattened, `target`, `span`/`spans`).

   Use a local-offset timestamp. A missing timestamp gets the ingest time. Plain-text lines are
   kept too, as `sidecar/stdout` or `sidecar/stderr`.
3. Honour `X-Request-Id` on incoming HTTP requests and forward it on outgoing ones.

Environment every sidecar receives: `LOG_FORMAT=json`, `LOG_SOURCE`, `LOG_BOOT_ID`,
`LOG_HTTP_BODIES`, `LOG_BODY_CAP_BYTES`, `LOG_SQL` (`all`/`slow`/`off`), `NO_COLOR=1`,
`RUST_LOG=info,tower_http=warn`.

### External tools

Drop a `.jsonl` file into `logs/inbox/`. The part of the file name before the first `-` is the
default source (`installer-20260915.jsonl` → `installer`). Naive local timestamps
(`2026-09-15 10:22:01`) are accepted, and the offset is added on ingest.

## Tests

- **Shell:** `cargo test --manifest-path src-tauri/Cargo.toml --lib`. Covers schema/time,
  redaction, sidecar/tracing ingest, writer, rollover names, gzip retention, inbox, and query/merge.
- **Backend:** `cargo test --test logging_test`. Covers request id echo and minting, redacted
  bodies, and 4xx level. `core::logging` unit tests run with `--lib`.
- **Document-server:** `cargo test --test logging_test`. Covers the request id from the backend,
  the render start/failed chain, and redaction.
- **Frontend:** `npx vitest run src/shared/logging src/features/logs`, which also runs under Jest.
