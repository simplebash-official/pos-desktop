#!/usr/bin/env bash
# ==============================================================================
# jana2u-pos: Master Desktop Benchmark & Stress Test Runner
#
# Runs the entire application stack in isolated desktop SQLite mode on loopback,
# executes comprehensive POS checkout & Typst PDF stress testing, profiles PID
# resources (memory, CPU, FDs), and formats an executive benchmark scorecard.
# ==============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"

TEST_DIR="${ROOT_DIR}/target/stress-test-data"
REPORT_DIR="${ROOT_DIR}/target/benchmark-reports"
BIN_DIR="${ROOT_DIR}/src-tauri/binaries"

BACKEND_BIN="${BIN_DIR}/jana2u-backend-${TRIPLE}"
DOCS_BIN="${BIN_DIR}/jana2u-document-server-${TRIPLE}"

mkdir -p "${TEST_DIR}" "${REPORT_DIR}"
chmod +x "${ROOT_DIR}/scripts/stress-engine.mjs"
chmod +x "${ROOT_DIR}/scripts/monitor-resources.sh"

echo "================================================================="
echo " Jana2U POS Desktop Benchmark & Stress Testing Suite"
echo " Host Target Triple: ${TRIPLE}"
echo "================================================================="

# 1. Verify Binaries
if [ ! -f "${BACKEND_BIN}" ] || [ ! -f "${DOCS_BIN}" ]; then
  echo "==> Binaries missing, building sidecars via scripts/build-sidecars.sh..."
  bash "${ROOT_DIR}/scripts/build-sidecars.sh"
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

# 3. Launch document-server
echo "==> Launching isolated jana2u-document-server on 127.0.0.1:8090..."
BIND_ADDR="127.0.0.1" \
PORT="8090" \
DATABASE_URL="sqlite://${TEST_DIR}/document_server_stress.db" \
TEMPLATES_DIR="${TEST_DIR}/assets/templates" \
FONTS_DIR="${TEST_DIR}/assets/fonts" \
INTERNAL_API_KEY="${INTERNAL_KEY}" \
REMOTE_IMAGE_FETCH_ENABLED="false" \
RUST_LOG="document_server=warn,warn" \
"${DOCS_BIN}" > "${REPORT_DIR}/document_server.log" 2>&1 &
DOCS_PID=$!

# 4. Launch backend
echo "==> Launching isolated jana2u-backend on 127.0.0.1:8080..."
BIND_ADDR="127.0.0.1" \
PORT="8080" \
DATABASE_TYPE="sqlite" \
DATABASE_URL="sqlite://${TEST_DIR}/pos_stress.db?mode=rwc" \
JWT_SECRET="${JWT_SECRET}" \
JWT_EXPIRY_HOURS="12" \
DOCUMENT_SERVER_URL="http://127.0.0.1:8090" \
DOCUMENT_SERVER_API_KEY="${INTERNAL_KEY}" \
AUTO_SEED="true" \
RUST_LOG="jana2u_pos_backend=warn,warn" \
"${BACKEND_BIN}" > "${REPORT_DIR}/backend.log" 2>&1 &
BACKEND_PID=$!

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
RESOURCES_CSV="${REPORT_DIR}/resources.csv"
"${ROOT_DIR}/scripts/monitor-resources.sh" "${BACKEND_PID}" "${DOCS_PID}" "${RESOURCES_CSV}" &
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
node "${ROOT_DIR}/scripts/stress-engine.mjs"

# 8. Post-Process Resource Metrics
kill "${MONITOR_PID}" 2>/dev/null || true

echo ""
echo "================================================================="
echo " Analyzing Process Resource Utilization (RSS Memory, CPU, FDs)"
echo "================================================================="

BACKEND_PEAK_RSS="$(awk -F',' -v pid="${BACKEND_PID}" '$2 == pid {if ($5 > max) max=$5} END {print (max?max:"0")}' "${RESOURCES_CSV}")"
BACKEND_AVG_CPU="$(awk -F',' -v pid="${BACKEND_PID}" '$2 == pid {sum+=$4; count++} END {if (count>0) printf "%.1f", sum/count; else print "0"}' "${RESOURCES_CSV}")"
BACKEND_PEAK_FDS="$(awk -F',' -v pid="${BACKEND_PID}" '$2 == pid {if ($6 > max) max=$6} END {print (max?max:"0")}' "${RESOURCES_CSV}")"

DOCS_PEAK_RSS="$(awk -F',' -v pid="${DOCS_PID}" '$2 == pid {if ($5 > max) max=$5} END {print (max?max:"0")}' "${RESOURCES_CSV}")"
DOCS_AVG_CPU="$(awk -F',' -v pid="${DOCS_PID}" '$2 == pid {sum+=$4; count++} END {if (count>0) printf "%.1f", sum/count; else print "0"}' "${RESOURCES_CSV}")"
DOCS_PEAK_FDS="$(awk -F',' -v pid="${DOCS_PID}" '$2 == pid {if ($6 > max) max=$6} END {print (max?max:"0")}' "${RESOURCES_CSV}")"

echo "  jana2u-backend:        Peak RSS: ${BACKEND_PEAK_RSS} MB | Avg CPU: ${BACKEND_AVG_CPU}% | Peak Open FDs: ${BACKEND_PEAK_FDS}"
echo "  jana2u-document-server: Peak RSS: ${DOCS_PEAK_RSS} MB | Avg CPU: ${DOCS_AVG_CPU}% | Peak Open FDs: ${DOCS_PEAK_FDS}"

# 9. Format Markdown Report
JSON_REPORT="${REPORT_DIR}/desktop_stress_report.json"
MD_REPORT="${REPORT_DIR}/desktop_stress_report.md"

node -e "
import fs from 'node:fs';
const data = JSON.parse(fs.readFileSync('${JSON_REPORT}', 'utf8'));

let md = '# Desktop Mode Benchmark & Stress Test Report\\n\\n';
md += '> **Application**: Jana2U POS Desktop (Tauri v2 + SQLite Engine + Typst Sidecars)\\n';
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
md += '| Process Component | Peak Memory (RSS) | Average CPU % | Peak Open File Descriptors |\\n';
md += '| :--- | :---: | :---: | :---: |\\n';
md += '| **jana2u-backend** (Axum + SQLite WAL) | ' + '${BACKEND_PEAK_RSS}' + ' MB | ' + '${BACKEND_AVG_CPU}' + '% | ' + '${BACKEND_PEAK_FDS}' + ' |\\n';
md += '| **jana2u-document-server** (Typst Engine) | ' + '${DOCS_PEAK_RSS}' + ' MB | ' + '${DOCS_AVG_CPU}' + '% | ' + '${DOCS_PEAK_FDS}' + ' |\\n';

fs.writeFileSync('${MD_REPORT}', md);
"

echo "✅ Benchmark Scorecard generated at: ${MD_REPORT}"
echo "================================================================="
