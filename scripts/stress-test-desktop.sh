#!/usr/bin/env bash
# ==============================================================================
# myrologic-pos: Master Desktop Benchmark & Stress Test Runner
#
# Runs the entire application stack in isolated desktop SQLite mode on loopback,
# executes comprehensive POS checkout & Typst PDF stress testing, profiles PID
# resources (memory, CPU, FDs), and formats an executive benchmark scorecard.
# ==============================================================================

set -euo pipefail

# ---------------------------------------------------------------------------
# Options
#   --logging off|standard|full  activity-log level for the sidecars (default off)
#   --orders N                   run one checkout tier of N orders instead of
#                                the three default tiers
#   --suffix NAME                isolate data/report dirs (used by bench-logging)
# ---------------------------------------------------------------------------
LOGGING_MODE="off"
CHECKOUT_ORDERS=""
RUN_SUFFIX=""
while [ $# -gt 0 ]; do
  case "$1" in
    --logging) LOGGING_MODE="${2:-off}"; shift 2 ;;
    --orders) CHECKOUT_ORDERS="${2:-}"; shift 2 ;;
    --suffix) RUN_SUFFIX="${2:-}"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
case "${LOGGING_MODE}" in
  off|standard|full) ;;
  *) echo "--logging must be off, standard or full" >&2; exit 2 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"

TEST_DIR="${ROOT_DIR}/target/stress-test-data${RUN_SUFFIX:+-${RUN_SUFFIX}}"
REPORT_DIR="${ROOT_DIR}/target/benchmark-reports${RUN_SUFFIX:+/${RUN_SUFFIX}}"
# Where the activity log is written when --logging is on.
LOG_DIR="${TEST_DIR}/logs"
LOG_PIPE="${ROOT_DIR}/src-tauri/target/release/examples/log_pipe"
# Millisecond-accurate CPU sampler (`ps` only resolves whole seconds).
PROC_SAMPLE="${ROOT_DIR}/src-tauri/target/release/examples/proc_sample"
BIN_DIR="${ROOT_DIR}/src-tauri/binaries"

BACKEND_BIN="${BIN_DIR}/myrologic-backend-${TRIPLE}"
DOCS_BIN="${BIN_DIR}/myrologic-document-server-${TRIPLE}"

mkdir -p "${TEST_DIR}" "${REPORT_DIR}"
chmod +x "${ROOT_DIR}/scripts/stress-engine.mjs"
chmod +x "${ROOT_DIR}/scripts/monitor-resources.sh"

echo "================================================================="
echo " MyroLogic POS Desktop Benchmark & Stress Testing Suite"
echo " Host Target Triple: ${TRIPLE}"
echo " Activity log:       ${LOGGING_MODE}"
echo "================================================================="

# 1. Verify Binaries
if [ ! -f "${BACKEND_BIN}" ] || [ ! -f "${DOCS_BIN}" ]; then
  echo "==> Binaries missing, building sidecars via scripts/build-sidecars.sh..."
  bash "${ROOT_DIR}/scripts/build-sidecars.sh"
fi

# 1b. Build the measurement helpers (the same ingest + writer + resource
#     accounting the desktop shell itself uses)
if [ ! -x "${PROC_SAMPLE}" ]; then
  echo "==> Building proc_sample (release)..."
  (cd "${ROOT_DIR}/src-tauri" && cargo build --release --example proc_sample)
fi
if [ "${LOGGING_MODE}" != "off" ]; then
  if [ ! -x "${LOG_PIPE}" ]; then
    echo "==> Building log_pipe (release) for activity-log measurement..."
    (cd "${ROOT_DIR}/src-tauri" && cargo build --release --example log_pipe)
  fi
  mkdir -p "${LOG_DIR}"
fi

# 2. Stage Isolated Assets
echo "==> Staging isolated templates and fonts in ${TEST_DIR}/assets..."
rm -rf "${TEST_DIR}/assets"
mkdir -p "${TEST_DIR}/assets"
cp -R "${ROOT_DIR}/document-server/templates" "${TEST_DIR}/assets/templates"
cp -R "${ROOT_DIR}/document-server/fonts" "${TEST_DIR}/assets/fonts"
find "${TEST_DIR}/assets/templates" -name 'CLAUDE.md' -delete 2>/dev/null || true

# Generate test secrets
JWT_SECRET="bench_secret_$(openssl rand -hex 16 2>/dev/null || date +%s%N)"
INTERNAL_KEY="bench_internal_key_$(openssl rand -hex 16 2>/dev/null || date +%s%N)"

# Clean up any lingering processes on ports 8080 and 8090
lsof -ti :8080 -ti :8090 | xargs kill -9 2>/dev/null || true
sleep 1

# Activity-log environment for the sidecars (see docs/logging.md).
if [ "${LOGGING_MODE}" = "off" ]; then
  LOG_ENV=(LOG_FORMAT=text RUST_LOG="warn")
else
  LOG_SQL_MODE="slow"
  [ "${LOGGING_MODE}" = "full" ] && LOG_SQL_MODE="all"
  LOG_ENV=(
    LOG_FORMAT=json
    LOG_HTTP_BODIES=true
    LOG_BODY_CAP_BYTES=32768
    LOG_SQL="${LOG_SQL_MODE}"
    NO_COLOR=1
    RUST_LOG="info,tower_http=warn"
  )
fi

# 3. Launch document-server
echo "==> Launching isolated myrologic-document-server on 127.0.0.1:8090..."
if [ "${LOGGING_MODE}" = "off" ]; then
  env "${LOG_ENV[@]}" \
  BIND_ADDR="127.0.0.1" PORT="8090" \
  DATABASE_URL="sqlite://${TEST_DIR}/document_server_stress.db" \
  TEMPLATES_DIR="${TEST_DIR}/assets/templates" \
  FONTS_DIR="${TEST_DIR}/assets/fonts" \
  INTERNAL_API_KEY="${INTERNAL_KEY}" \
  REMOTE_IMAGE_FETCH_ENABLED="false" \
  "${DOCS_BIN}" > "${REPORT_DIR}/document_server.log" 2>&1 &
  DOCS_PID=$!
else
  # Process substitution keeps $! as the sidecar's own pid while its stdout
  # flows through the real log ingest + writer.
  env "${LOG_ENV[@]}" LOG_SOURCE=document-server \
  BIND_ADDR="127.0.0.1" PORT="8090" \
  DATABASE_URL="sqlite://${TEST_DIR}/document_server_stress.db" \
  TEMPLATES_DIR="${TEST_DIR}/assets/templates" \
  FONTS_DIR="${TEST_DIR}/assets/fonts" \
  INTERNAL_API_KEY="${INTERNAL_KEY}" \
  REMOTE_IMAGE_FETCH_ENABLED="false" \
  "${DOCS_BIN}" > >("${LOG_PIPE}" --source document-server --dir "${LOG_DIR}" \
    > "${REPORT_DIR}/document_server.log" 2>&1) 2>&1 &
  DOCS_PID=$!
fi

# 4. Launch backend
echo "==> Launching isolated myrologic-backend on 127.0.0.1:8080..."
if [ "${LOGGING_MODE}" = "off" ]; then
  env "${LOG_ENV[@]}" \
  BIND_ADDR="127.0.0.1" PORT="8080" \
  DATABASE_TYPE="sqlite" \
  DATABASE_URL="sqlite://${TEST_DIR}/pos_stress.db?mode=rwc" \
  JWT_SECRET="${JWT_SECRET}" JWT_EXPIRY_HOURS="12" \
  DOCUMENT_SERVER_URL="http://127.0.0.1:8090" \
  DOCUMENT_SERVER_API_KEY="${INTERNAL_KEY}" \
  AUTO_SEED="true" \
  "${BACKEND_BIN}" > "${REPORT_DIR}/backend.log" 2>&1 &
  BACKEND_PID=$!
else
  env "${LOG_ENV[@]}" LOG_SOURCE=backend \
  BIND_ADDR="127.0.0.1" PORT="8080" \
  DATABASE_TYPE="sqlite" \
  DATABASE_URL="sqlite://${TEST_DIR}/pos_stress.db?mode=rwc" \
  JWT_SECRET="${JWT_SECRET}" JWT_EXPIRY_HOURS="12" \
  DOCUMENT_SERVER_URL="http://127.0.0.1:8090" \
  DOCUMENT_SERVER_API_KEY="${INTERNAL_KEY}" \
  AUTO_SEED="true" \
  "${BACKEND_BIN}" > >("${LOG_PIPE}" --source backend --dir "${LOG_DIR}" \
    > "${REPORT_DIR}/backend.log" 2>&1) 2>&1 &
  BACKEND_PID=$!
fi

cleanup() {
  echo ""
  echo "==> Tearing down background test processes..."
  if [ -n "${MONITOR_PID:-}" ]; then kill "${MONITOR_PID}" 2>/dev/null || true; fi
  if [ -n "${BACKEND_PID:-}" ]; then kill "${BACKEND_PID}" 2>/dev/null || true; wait "${BACKEND_PID}" 2>/dev/null || true; fi
  if [ -n "${DOCS_PID:-}" ]; then kill "${DOCS_PID}" 2>/dev/null || true; wait "${DOCS_PID}" 2>/dev/null || true; fi
  echo "==> Sidecars stopped."
}
trap cleanup EXIT

# 5. Wait for Health Checks
echo "==> Awaiting sidecar health probes..."
for i in {1..30}; do
  if curl -sf http://127.0.0.1:8090/api/health >/dev/null 2>&1 && curl -sf http://127.0.0.1:8080/api/health >/dev/null 2>&1; then
    echo "✅ Both sidecars healthy and ready!"
    break
  fi
  if [ "$i" -eq 30 ]; then
    echo "❌ Timed out waiting for sidecars to become ready."
    exit 1
  fi
  sleep 1
done

# 6. Start Resource Monitor
# Two files on purpose: one holds the start/end samples CPU time is diffed
# across, the other the periodic samples peak memory is taken from. One file
# with two writers would have them overwrite each other.
RESOURCES_CSV="${REPORT_DIR}/resources.csv"
PEAKS_CSV="${REPORT_DIR}/resource_peaks.csv"
MONITOR_TARGETS=("backend=${BACKEND_PID}" "document-server=${DOCS_PID}")
# Left unset (not an empty array) when logging is off: on bash 3.2 expanding an
# empty array under `set -u` is an error, hence the ${arr[@]+...} guards below.
if [ "${LOGGING_MODE}" != "off" ]; then
  # The log writers are separate processes; their cost belongs in the report.
  # Matched by command line, so they are tracked however they were started.
  MONITOR_MATCH=(--match "log_pipe=examples/log_pipe")
fi
rm -f "${RESOURCES_CSV}" "${PEAKS_CSV}"
# Opening sample, before any workload runs.
"${PROC_SAMPLE}" --csv "${RESOURCES_CSV}" --once \
  "${MONITOR_TARGETS[@]}" ${MONITOR_MATCH[@]+"${MONITOR_MATCH[@]}"} 2> "${REPORT_DIR}/monitor.err"
"${PROC_SAMPLE}" --csv "${PEAKS_CSV}" --interval-ms 250 \
  "${MONITOR_TARGETS[@]}" ${MONITOR_MATCH[@]+"${MONITOR_MATCH[@]}"} 2>> "${REPORT_DIR}/monitor.err" &
MONITOR_PID=$!
echo "✅ Resource monitoring active (PID ${MONITOR_PID})"

# 7. Run Stress & Benchmark Engine
echo ""
echo "================================================================="
echo " Executing Stress & Benchmark Suites via Node.js"
echo "================================================================="
BACKEND_URL="http://127.0.0.1:8080" \
DOCS_URL="http://127.0.0.1:8090" \
INTERNAL_KEY="${INTERNAL_KEY}" \
REPORT_DIR="${REPORT_DIR}" \
CHECKOUT_ORDERS="${CHECKOUT_ORDERS}" \
LOGGING_BENCH="$([ -n "${CHECKOUT_ORDERS}" ] && echo 1 || echo 0)" \
node "${ROOT_DIR}/scripts/stress-engine.mjs"

# 8. Post-Process Resource Metrics
# A closing sample first: a short run can otherwise finish inside one sampling
# interval, leaving nothing to diff CPU time against.
# Closing sample: CPU time used by the run is this minus the opening sample.
"${PROC_SAMPLE}" --csv "${RESOURCES_CSV}" --once \
  "${MONITOR_TARGETS[@]}" ${MONITOR_MATCH[@]+"${MONITOR_MATCH[@]}"} 2>> "${REPORT_DIR}/monitor.err" || true
kill "${MONITOR_PID}" 2>/dev/null || true

echo ""
echo "================================================================="
echo " Analyzing Process Resource Utilization (RSS Memory, CPU, FDs)"
echo "================================================================="

# Peak RSS, CPU seconds used during the run (last sample - first sample), FDs.
peak_rss() { awk -F',' -v n="$1" '$3 == n {if ($5 > max) max=$5} END {print (max?max:"0")}' "${RESOURCES_CSV}" "${PEAKS_CSV}"; }
cpu_used() { awk -F',' -v n="$1" '$3 == n {if (first == "") first=$4; last=$4} END {printf "%.3f", (last=="" ? 0 : last - first)}' "${RESOURCES_CSV}"; }
peak_fds() { awk -F',' -v n="$1" '$3 == n {if ($6 > max) max=$6} END {print (max?max:"0")}' "${RESOURCES_CSV}"; }

BACKEND_PEAK_RSS="$(peak_rss backend)"; BACKEND_CPU_S="$(cpu_used backend)"; BACKEND_PEAK_FDS="$(peak_fds backend)"
DOCS_PEAK_RSS="$(peak_rss document-server)"; DOCS_CPU_S="$(cpu_used document-server)"; DOCS_PEAK_FDS="$(peak_fds document-server)"
PIPE_CPU_S="$(cpu_used log_pipe)"
PIPE_PEAK_RSS="$(peak_rss log_pipe)"
LOG_BYTES="$([ -d "${LOG_DIR}" ] && find "${LOG_DIR}" -name '*.jsonl' -exec cat {} + 2>/dev/null | wc -c | tr -d ' ' || echo 0)"

echo "  myrologic-backend:         Peak RSS: ${BACKEND_PEAK_RSS} MB | CPU used: ${BACKEND_CPU_S}s"
echo "  myrologic-document-server: Peak RSS: ${DOCS_PEAK_RSS} MB | CPU used: ${DOCS_CPU_S}s"
if [ "${LOGGING_MODE}" != "off" ]; then
  echo "  log writers:            Peak RSS: ${PIPE_PEAK_RSS} MB | CPU used: ${PIPE_CPU_S}s | Log written: ${LOG_BYTES} bytes"
fi

# Machine-readable summary for scripts/bench-logging.sh.
cat > "${REPORT_DIR}/resource_summary.json" <<JSON
{
  "loggingMode": "${LOGGING_MODE}",
  "checkoutOrders": "${CHECKOUT_ORDERS}",
  "backend": { "peakRssMb": ${BACKEND_PEAK_RSS:-0}, "cpuSeconds": ${BACKEND_CPU_S:-0}, "peakFds": ${BACKEND_PEAK_FDS:-0} },
  "documentServer": { "peakRssMb": ${DOCS_PEAK_RSS:-0}, "cpuSeconds": ${DOCS_CPU_S:-0}, "peakFds": ${DOCS_PEAK_FDS:-0} },
  "logWriters": { "peakRssMb": ${PIPE_PEAK_RSS:-0}, "cpuSeconds": ${PIPE_CPU_S:-0} },
  "logBytes": ${LOG_BYTES:-0}
}
JSON

# 9. Format Markdown Report
JSON_REPORT="${REPORT_DIR}/desktop_stress_report.json"
MD_REPORT="${REPORT_DIR}/desktop_stress_report.md"

node -e "
import fs from 'node:fs';
const data = JSON.parse(fs.readFileSync('${JSON_REPORT}', 'utf8'));

let md = '# Desktop Mode Benchmark & Stress Test Report\\n\\n';
md += '> **Application**: MyroLogic POS Desktop (Tauri v2 + SQLite Engine + Typst Sidecars)\\n';
md += '> **Timestamp**: ' + data.metadata.generatedAt + '\\n';
md += '> **Environment**: macOS Apple Silicon (' + data.metadata.arch + ') | Node ' + data.metadata.nodeVersion + '\\n\\n';

md += '## 1. POS Checkout Concurrency & Transaction Latency (Suite 1)\\n\\n';
md += '| Load Tier | Concurrency | Orders | Success Rate | Throughput (TPS) | p50 Latency | p95 Latency | p99 Latency | Max Latency |\\n';
md += '| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: | :---: |\\n';
for (const row of data.checkoutSuite) {
  const rate = ((row.successCount / row.totalOrders) * 100).toFixed(1) + '%';
  md += '| **' + row.tier + '** | ' + row.concurrency + ' | ' + row.totalOrders + ' | ' + rate + ' | **' + row.tps + '** | ' + row.percentiles.p50.toFixed(2) + ' ms | ' + row.percentiles.p95.toFixed(2) + ' ms | ' + row.percentiles.p99.toFixed(2) + ' ms | ' + row.percentiles.max.toFixed(2) + ' ms |\\n';
}

md += '\\n## 2. Typst Document Server Compilation Benchmarks (Suite 2)\\n\\n';
md += '| Document Template | Iterations | Success Rate | Avg PDF Size | Renders / Sec | p50 Latency | p95 Latency | Max Latency |\\n';
md += '| :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |\\n';
for (const row of data.typstSuite) {
  const rate = ((row.success / row.iterations) * 100).toFixed(1) + '%';
  md += '| **' + row.name + '** | ' + row.iterations + ' | ' + rate + ' | ' + row.avgSizeKb + ' KB | **' + row.rendersPerSecond + '** | ' + row.percentiles.p50.toFixed(2) + ' ms | ' + row.percentiles.p95.toFixed(2) + ' ms | ' + row.percentiles.max.toFixed(2) + ' ms |\\n';
}

md += '\\n## 3. SQLite Analytics Queries Under Concurrent Load (Suite 3)\\n\\n';
md += '| Analytical Query Endpoint | Queries | Success Rate | Throughput (QPS) | p50 Latency | p95 Latency | p99 Latency |\\n';
md += '| :--- | :---: | :---: | :---: | :---: | :---: | :---: |\\n';
for (const row of data.analyticsSuite) {
  const rate = ((row.success / row.totalQueries) * 100).toFixed(1) + '%';
  md += '| **' + row.label + '** | ' + row.totalQueries + ' | ' + rate + ' | **' + row.qps + '** | ' + row.percentiles.p50.toFixed(2) + ' ms | ' + row.percentiles.p95.toFixed(2) + ' ms | ' + row.percentiles.p99.toFixed(2) + ' ms |\\n';
}

md += '\\n## 4. Desktop Database Backup & Restore Stress (Suite 4)\\n\\n';
md += '| Operation | Total Database Rows | Payload Size | Total Duration | Throughput |\\n';
md += '| :--- | :---: | :---: | :---: | :---: |\\n';
md += '| **Full SQLite Export** | ' + data.backupSuite.totalRows + ' | ' + data.backupSuite.sizeKb + ' KB | ' + data.backupSuite.exportDurationMs.toFixed(2) + ' ms | ' + data.backupSuite.exportThroughputRowsSec + ' rows/s |\\n';
md += '| **Full Transactional Restore** | ' + data.backupSuite.totalRows + ' | ' + data.backupSuite.sizeKb + ' KB | ' + data.backupSuite.restoreDurationMs.toFixed(2) + ' ms | ' + data.backupSuite.restoreThroughputRowsSec + ' rows/s |\\n';

md += '\\n## 5. Host Process Resource Utilization Profile\\n\\n';
md += '| Process Component | Peak Memory (RSS) | CPU Used |\\n';
md += '| :--- | :---: | :---: |\\n';
md += '| **myrologic-backend** (Axum + SQLite WAL) | ' + '${BACKEND_PEAK_RSS}' + ' MB | ' + '${BACKEND_CPU_S}' + ' s |\\n';
md += '| **myrologic-document-server** (Typst Engine) | ' + '${DOCS_PEAK_RSS}' + ' MB | ' + '${DOCS_CPU_S}' + ' s |\\n';
md += '| **activity log writers** (${LOGGING_MODE}) | ' + '${PIPE_PEAK_RSS}' + ' MB | ' + '${PIPE_CPU_S}' + ' s |\\n';

fs.writeFileSync('${MD_REPORT}', md);
"

echo "✅ Benchmark Scorecard generated at: ${MD_REPORT}"
echo "================================================================="
