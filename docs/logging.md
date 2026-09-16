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

- **Never blocks.** Producers push onto a queue bounded by both count (50k events) and memory
  (32 MB). Past either limit, events are dropped and a `system/log.dropped` entry records the
  count — logging is skipped rather than slowing a sale.
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

## Logging levels

The shell can switch every source's logging at runtime, in memory only
(`logs/logging.json` is never rewritten, so a crash can't leave a shop with
logging off). This is what the benchmark below uses.

| Mode | Bodies | SQL | Everything else |
|---|---|---|---|
| `off` | no | no | nothing recorded except `category: "benchmark"` markers |
| `standard` | yes | slow statements only | yes |
| `full` | yes | every statement | yes |

The default for a normal install is `full` (`logs/logging.json`), adjustable in
**Settings → Activity Log**.

### Sidecar control contract (stdin)

A sidecar started with `LOG_FORMAT=json` must read lines from **stdin** and
honour:

```json
{"cmd":"log_mode","enabled":true,"http_bodies":true,"sql":"slow"}
```

Unknown commands and malformed lines are ignored, so the shell can add fields
later. Under Docker/web nothing writes to stdin, so nothing changes there.
Reference implementation: `backend/src/core/logging/control.rs`, applied by a
runtime filter in `core::logging::init` plus atomics in `LogSettings`.

## Performance

Logging is designed so it can never stall a sale:

- **Capping is bounded work.** A body is redacted and cut in one pass that stops
  after the cap (32 KB by default), so logging a 6 MB response costs the same as
  a small one — `frontend/src/shared/logging/redact.ts`'s `redactAndCap` and
  `body_for_log` in each service's `core/logging/redact.rs`.
- **The queue is bounded in bytes, not just events** (32 MB, `src-tauri/src/logging/hub.rs`).
  Past that, events are dropped and counted (`system/log.dropped`) rather than
  growing memory.
- **Writes are batched** every 250 ms on a dedicated thread, flushed at once on
  errors and on exit.

### Measured cost

Two ways to measure, both comparing `off` / `standard` / `full`:

| | Command-line (real sales) | In-app (simulated) |
|---|---|---|
| How | `npm run bench:logging` (`--orders N`, default 1,000) | Settings → System Benchmark → **Measure logging only** |
| Workload | real POS checkouts against an isolated test database | replays a sale flow that writes no shop data |
| Reports | `target/benchmark-reports/logging-overhead.{json,md}` | the "What the activity log costs" card, saved in the benchmark report |
| Measures | CPU seconds, peak RSS and log MB per 1,000 sales, TPS, p50/p95 | the same per 1,000 flows, per process, plus webview self-time |

Re-run the command-line benchmark after any change to capture, ingest or the
writer, and compare against the table below.

#### Baseline: 1,000 real sales, Apple Silicon (M-series), 2026-09-16

| Logging | CPU / 1,000 sales | Log written / 1,000 sales | Checkout TPS | p95 | 
|---|---:|---:|---:|---:|
| off | 0.62 s | — | 1691 | 7.6 ms |
| standard | 0.80 s (+29%) | 4.3 MB | 1622 (−4%) | 7.7 ms (+0.1) |
| full | 1.06 s (+71%) | 13.1 MB | 1491 (−12%) | 10.9 ms (+3.3) |

Read it as: with **standard** logging a 1,000-sale day costs about a fifth of a
CPU-second more and writes ~4 MB (~0.4 MB once gzipped); **full** logging —
every SQL statement — roughly triples that and is what costs the extra 3 ms at
p95. Percentages are of the sidecars' own CPU, which is itself under a second
per 1,000 sales, so in wall-clock terms all three are negligible on a shop PC;
the reason to prefer `standard` is the p95 and the disk, not the CPU.

Two caveats on memory: the command-line harness runs the log writer as its own
process (~35 MB, mostly Rust process baseline), while in the desktop app the
writer lives **inside** the shell process, so the real figure is much smaller —
the in-app benchmark is the one to trust for memory. Raw report:
`target/benchmark-reports/logging-overhead.json`.

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
