#!/usr/bin/env bash
# ==============================================================================
# myrologic-pos: Activity-log overhead benchmark (real sales)
#
# Runs the same POS checkout workload — real sales against an isolated test
# database, never shop data — with the activity log off, at standard level and
# at full level, then reports what logging costs in CPU, memory, disk and
# checkout latency.
#
#   npm run bench:logging                # 1,000 sales per mode
#   scripts/bench-logging.sh --orders 200
#
# Output: target/benchmark-reports/logging-overhead.{json,md}
#
# Off is run twice (first and last) and averaged, so machine warm-up/thermal
# drift doesn't land on one mode. Each run gets its own database and log dir.
# ==============================================================================

set -euo pipefail

ORDERS=1000
while [ $# -gt 0 ]; do
  case "$1" in
    --orders) ORDERS="${2:-1000}"; shift 2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPORT_ROOT="${ROOT_DIR}/target/benchmark-reports"
OUT_JSON="${REPORT_ROOT}/logging-overhead.json"
OUT_MD="${REPORT_ROOT}/logging-overhead.md"

mkdir -p "${REPORT_ROOT}"

echo "================================================================="
echo " Activity-log overhead benchmark — ${ORDERS} real sales per mode"
echo "================================================================="

RUNS=("off:off-1" "standard:standard" "full:full" "off:off-2")
for RUN in "${RUNS[@]}"; do
  MODE="${RUN%%:*}"
  SUFFIX="${RUN##*:}"
  echo ""
  echo "=================================================================="
  echo " Run: logging=${MODE} (${SUFFIX})"
  echo "=================================================================="
  rm -rf "${ROOT_DIR}/target/stress-test-data-${SUFFIX}"
  bash "${SCRIPT_DIR}/stress-test-desktop.sh" \
    --logging "${MODE}" --orders "${ORDERS}" --suffix "${SUFFIX}"
done

echo ""
echo "==> Comparing runs..."
ORDERS="${ORDERS}" REPORT_ROOT="${REPORT_ROOT}" OUT_JSON="${OUT_JSON}" OUT_MD="${OUT_MD}" \
node - <<'NODE'
import fs from 'node:fs';
import path from 'node:path';

const reportRoot = process.env.REPORT_ROOT;
const orders = Number(process.env.ORDERS);
const runs = ['off-1', 'standard', 'full', 'off-2'];

const read = (suffix, file) => {
  const full = path.join(reportRoot, suffix, file);
  return fs.existsSync(full) ? JSON.parse(fs.readFileSync(full, 'utf8')) : null;
};

const collect = (suffix) => {
  const resources = read(suffix, 'resource_summary.json');
  const stress = read(suffix, 'desktop_stress_report.json');
  if (!resources || !stress) {
    throw new Error(`missing reports for run "${suffix}"`);
  }
  const checkout = stress.checkoutSuite[0];
  const perThousand = 1000 / checkout.totalOrders;
  const cpuSeconds =
    resources.backend.cpuSeconds + resources.documentServer.cpuSeconds + resources.logWriters.cpuSeconds;
  return {
    run: suffix,
    mode: resources.loggingMode,
    orders: checkout.totalOrders,
    successRate: Number(((checkout.successCount / checkout.totalOrders) * 100).toFixed(1)),
    tps: checkout.tps,
    p50Ms: Number(checkout.percentiles.p50.toFixed(2)),
    p95Ms: Number(checkout.percentiles.p95.toFixed(2)),
    cpuSecondsPerThousand: Number((cpuSeconds * perThousand).toFixed(2)),
    cpuByProcess: {
      backend: Number((resources.backend.cpuSeconds * perThousand).toFixed(2)),
      documentServer: Number((resources.documentServer.cpuSeconds * perThousand).toFixed(2)),
      logWriters: Number((resources.logWriters.cpuSeconds * perThousand).toFixed(2)),
    },
    peakRssMb: Number(
      (
        resources.backend.peakRssMb +
        resources.documentServer.peakRssMb +
        resources.logWriters.peakRssMb
      ).toFixed(1)
    ),
    logMbPerThousand: Number(((resources.logBytes / (1024 * 1024)) * perThousand).toFixed(2)),
  };
};

const measured = runs.map(collect);
const mean = (a, b) => Number(((a + b) / 2).toFixed(2));
const [off1, standard, full, off2] = measured;
const off = {
  run: 'off (average of two runs)',
  mode: 'off',
  orders,
  successRate: mean(off1.successRate, off2.successRate),
  tps: mean(off1.tps, off2.tps),
  p50Ms: mean(off1.p50Ms, off2.p50Ms),
  p95Ms: mean(off1.p95Ms, off2.p95Ms),
  cpuSecondsPerThousand: mean(off1.cpuSecondsPerThousand, off2.cpuSecondsPerThousand),
  cpuByProcess: {
    backend: mean(off1.cpuByProcess.backend, off2.cpuByProcess.backend),
    documentServer: mean(off1.cpuByProcess.documentServer, off2.cpuByProcess.documentServer),
    logWriters: 0,
  },
  peakRssMb: mean(off1.peakRssMb, off2.peakRssMb),
  logMbPerThousand: 0,
};

const overheadOf = (mode) => ({
  mode: mode.mode,
  cpuSecondsPerThousand: Number((mode.cpuSecondsPerThousand - off.cpuSecondsPerThousand).toFixed(2)),
  cpuPercent:
    off.cpuSecondsPerThousand > 0
      ? Number(
          (((mode.cpuSecondsPerThousand - off.cpuSecondsPerThousand) / off.cpuSecondsPerThousand) * 100).toFixed(1)
        )
      : 0,
  peakRssMb: Number((mode.peakRssMb - off.peakRssMb).toFixed(1)),
  logMbPerThousand: mode.logMbPerThousand,
  p95DeltaMs: Number((mode.p95Ms - off.p95Ms).toFixed(2)),
  tpsPercent: off.tps > 0 ? Number((((mode.tps - off.tps) / off.tps) * 100).toFixed(1)) : 0,
});

const report = {
  generatedAt: new Date().toISOString(),
  ordersPerMode: orders,
  platform: `${process.platform} ${process.arch}`,
  runs: measured,
  modes: { off, standard, full },
  overhead: { standard: overheadOf(standard), full: overheadOf(full) },
};
fs.writeFileSync(process.env.OUT_JSON, JSON.stringify(report, null, 2));

const row = (m) =>
  `| **${m.mode}** | ${m.cpuSecondsPerThousand} s | ${m.peakRssMb} MB | ${m.logMbPerThousand} MB | ${m.tps} | ${m.p50Ms} ms | ${m.p95Ms} ms |`;

let md = `# Activity-log overhead — ${orders} real sales per mode\n\n`;
md += `> Generated ${report.generatedAt} on ${report.platform}\n`;
md += `> Workload: real POS checkouts (concurrency 5) against an isolated test database.\n`;
md += `> CPU is the sum of backend + document-server + log writers, per 1,000 sales.\n\n`;
md += `| Logging | CPU / 1,000 sales | Peak RSS | Log written / 1,000 sales | Checkout TPS | p50 | p95 |\n`;
md += `| :--- | :---: | :---: | :---: | :---: | :---: | :---: |\n`;
md += `${row(off)}\n${row(standard)}\n${row(full)}\n\n`;
md += `## Overhead versus logging off\n\n`;
for (const key of ['standard', 'full']) {
  const o = report.overhead[key];
  md += `- **${key}**: +${o.cpuSecondsPerThousand} s CPU (${o.cpuPercent}%), ${o.peakRssMb >= 0 ? '+' : ''}${o.peakRssMb} MB peak memory, ${o.logMbPerThousand} MB of log per 1,000 sales, checkout p95 ${o.p95DeltaMs >= 0 ? '+' : ''}${o.p95DeltaMs} ms, throughput ${o.tpsPercent >= 0 ? '+' : ''}${o.tpsPercent}%.\n`;
}
md += `\n## CPU by process (seconds per 1,000 sales)\n\n`;
md += `| Logging | backend | document-server | log writers |\n| :--- | :---: | :---: | :---: |\n`;
for (const m of [off, standard, full]) {
  md += `| **${m.mode}** | ${m.cpuByProcess.backend} | ${m.cpuByProcess.documentServer} | ${m.cpuByProcess.logWriters} |\n`;
}
md += `\n## Raw runs\n\n| Run | CPU / 1,000 | Peak RSS | Log MB | TPS | p95 | Success |\n| :--- | :---: | :---: | :---: | :---: | :---: | :---: |\n`;
for (const m of measured) {
  md += `| ${m.run} | ${m.cpuSecondsPerThousand} s | ${m.peakRssMb} MB | ${m.logMbPerThousand} | ${m.tps} | ${m.p95Ms} ms | ${m.successRate}% |\n`;
}
md += `\nLogs are never deleted by the app; days older than 7 are gzipped, which shrinks them to roughly a tenth.\n`;
fs.writeFileSync(process.env.OUT_MD, md);
console.log(md);
NODE

echo "✅ Report: ${OUT_MD}"
