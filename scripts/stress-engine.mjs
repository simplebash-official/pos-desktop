#!/usr/bin/env node
// ==============================================================================
// MyroLogic POS Desktop: Comprehensive High-Concurreny Stress & Benchmark Engine
//
// Evaluates:
//   1. End-to-End POS Checkout Latency & TPS under multi-worker concurrency
//   2. Typst Document Server compilation latency across all 5 document types
//   3. SQLite concurrent analytical queries (timeseries, heatmaps, aging)
//   4. Desktop full database backup & transactional restore stress
// ==============================================================================

import fs from 'node:fs';
import path from 'node:path';
import { performance } from 'node:perf_hooks';

const BACKEND_URL = process.env.BACKEND_URL || 'http://127.0.0.1:8080';
const DOCS_URL = process.env.DOCS_URL || 'http://127.0.0.1:8090';
const INTERNAL_KEY = process.env.INTERNAL_KEY || '';
const REPORT_DIR = process.env.REPORT_DIR || './target/benchmark-reports';
// Activity-log benchmark mode: one checkout tier of N real sales, nothing else,
// so the only difference between runs is how much is being logged.
const CHECKOUT_ORDERS = Number(process.env.CHECKOUT_ORDERS || 0);
const LOGGING_BENCH = process.env.LOGGING_BENCH === '1' && CHECKOUT_ORDERS > 0;

fs.mkdirSync(REPORT_DIR, { recursive: true });

function formatMs(ms) {
  return ms < 1 ? `${(ms * 1000).toFixed(0)} µs` : `${ms.toFixed(2)} ms`;
}

function calculatePercentiles(latencies) {
  if (!latencies || latencies.length === 0) {
    return { min: 0, p50: 0, p90: 0, p95: 0, p99: 0, max: 0, mean: 0, stddev: 0 };
  }
  const sorted = [...latencies].sort((a, b) => a - b);
  const sum = sorted.reduce((a, b) => a + b, 0);
  const mean = sum / sorted.length;
  const variance = sorted.reduce((acc, val) => acc + Math.pow(val - mean, 2), 0) / sorted.length;
  const stddev = Math.sqrt(variance);

  const getP = (p) => {
    const idx = Math.min(Math.floor((p / 100) * sorted.length), sorted.length - 1);
    return sorted[idx];
  };

  return {
    min: sorted[0],
    p50: getP(50),
    p90: getP(90),
    p95: getP(95),
    p99: getP(99),
    max: sorted[sorted.length - 1],
    mean,
    stddev,
  };
}

async function runConcurrentPool(items, concurrency, workerFn) {
  const results = [];
  let index = 0;

  const workers = Array.from({ length: concurrency }, async () => {
    while (index < items.length) {
      const currentIndex = index++;
      const item = items[currentIndex];
      const res = await workerFn(item, currentIndex);
      results.push(res);
    }
  });

  await Promise.all(workers);
  return results;
}

// -----------------------------------------------------------------------------
// Authentication & Setup
// -----------------------------------------------------------------------------
async function authenticateAdmin() {
  const resp = await fetch(`${BACKEND_URL}/api/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      email: 'admin@pos.com',
      password: 'admin@1234',
    }),
  });

  if (!resp.ok) {
    throw new Error(`Admin login failed: ${resp.status} ${await resp.text()}`);
  }

  const data = await resp.json();
  return data.data.token;
}

async function seedTestProducts(token, count = 20) {
  // Check if products already exist
  const listResp = await fetch(`${BACKEND_URL}/api/inventory/products?limit=100`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (listResp.ok) {
    const list = await listResp.json();
    if (list.data && list.data.items && list.data.items.length >= 5) {
      return list.data.items;
    }
  }

  // Get or create category
  let categoryKey = '';
  let subcategoryKey = '';
  const catResp = await fetch(`${BACKEND_URL}/api/inventory/categories`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (catResp.ok) {
    const catData = await catResp.json();
    if (catData.data && catData.data.length > 0) {
      categoryKey = catData.data[0].key;
      if (catData.data[0].subcategories && catData.data[0].subcategories.length > 0) {
        subcategoryKey = catData.data[0].subcategories[0].key;
      }
    }
  }

  if (!categoryKey) {
    const newCatResp = await fetch(`${BACKEND_URL}/api/inventory/categories`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: 'Stress Test Category', description: 'Benchmarking Category' }),
    });
    if (newCatResp.ok) {
      const c = await newCatResp.json();
      categoryKey = c.data.key;
    }
  }

  const seeded = [];
  for (let i = 1; i <= count; i++) {
    const prodPayload = {
      name: `Benchmark Test Item #${i}`,
      categoryKey: categoryKey || 'cat_default',
      subcategoryKey: subcategoryKey || undefined,
      costPriceCents: 100000 + i * 5000,
      sellingPriceCents: 150000 + i * 8000,
      stockQuantity: 100000,
      minStockThreshold: 10,
      isSerialized: false,
      autoGenerateBarcode: true,
    };

    const pResp = await fetch(`${BACKEND_URL}/api/inventory/products`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
      body: JSON.stringify(prodPayload),
    });

    if (pResp.ok) {
      const p = await pResp.json();
      seeded.push(p.data);
    }
  }

  return seeded;
}

// -----------------------------------------------------------------------------
// Suite 1: POS Checkout Transaction Pipeline
// -----------------------------------------------------------------------------
async function runPosCheckoutStressSuite(token, products, tier) {
  const { name, concurrency, totalOrders } = tier;
  console.log(`\n▶ Starting Suite 1: POS Checkout Stress [${name}]`);
  console.log(`  Concurrency: ${concurrency} workers | Total Orders: ${totalOrders}`);

  const orderTasks = Array.from({ length: totalOrders }, (_, i) => i);
  const latencies = [];
  let successCount = 0;
  let failureCount = 0;
  let sampleErrorLogged = false;

  const startWall = performance.now();

  await runConcurrentPool(orderTasks, concurrency, async (_, orderIdx) => {
    // Pick 1-3 random products from the catalog
    const numItems = 1 + (orderIdx % 3);
    const items = [];
    let subtotalCents = 0;

    for (let j = 0; j < numItems; j++) {
      const prod = products[(orderIdx + j) % products.length];
      const qty = 1 + ((orderIdx + j) % 2);
      const totalCents = prod.sellingPriceCents * qty;
      subtotalCents += totalCents;

      items.push({
        productKey: prod.key,
        quantity: qty,
        discountCents: 0,
        sourceType: 'retail',
      });
    }

    const salePayload = {
      items,
      pricingAdjustments: orderIdx % 5 === 0 ? { discountType: 'percentage', discountValue: 5.0 } : undefined,
      payment: {
        paymentMethod: orderIdx % 3 === 0 ? 'card' : 'cash',
        isCredit: false,
        amountReceivedCents: subtotalCents,
        cardLast4: orderIdx % 3 === 0 ? '4242' : undefined,
      },
      customer: {
        customerName: `Walk-in Customer #${orderIdx}`,
      },
      staff: {
        cashierName: 'Benchmark Cashier',
      },
      notes: `Automated stress order #${orderIdx}`,
      shopProfileSnapshot: {
        shopName: 'MyroLogic POS Benchmark Store',
        shopAddressLines: ['123 Main St', 'Colombo 03'],
        shopPrimaryPhone: '011-2345678',
        shopTradingName: 'MyroLogic POS',
        shopLegalName: 'MyroLogic POS (Pvt) Ltd',
      },
    };

    const t0 = performance.now();
    try {
      const resp = await fetch(`${BACKEND_URL}/api/billing/sales`, {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${token}`,
          'Content-Type': 'application/json',
          'X-Device-Id': `term-worker-${orderIdx % concurrency}`,
        },
        body: JSON.stringify(salePayload),
      });

      const dur = performance.now() - t0;
      latencies.push(dur);

      if (resp.ok) {
        successCount++;
      } else {
        failureCount++;
        if (!sampleErrorLogged) {
          sampleErrorLogged = true;
          console.error(`  [!] Sample Sale Error: HTTP ${resp.status} ${await resp.text()}`);
        }
      }
    } catch (err) {
      const dur = performance.now() - t0;
      latencies.push(dur);
      failureCount++;
      if (!sampleErrorLogged) {
        sampleErrorLogged = true;
        console.error(`  [!] Sample Network Error:`, err);
      }
    }
  });

  const totalWallTimeMs = performance.now() - startWall;
  const tps = (successCount / (totalWallTimeMs / 1000)).toFixed(1);
  const p = calculatePercentiles(latencies);

  console.log(`  ✅ Finished: ${successCount} successful, ${failureCount} failed in ${formatMs(totalWallTimeMs)}`);
  console.log(`  Throughput: ${tps} TPS | p50: ${formatMs(p.p50)} | p95: ${formatMs(p.p95)} | p99: ${formatMs(p.p99)} | Max: ${formatMs(p.max)}`);

  return {
    tier: name,
    concurrency,
    totalOrders,
    successCount,
    failureCount,
    totalDurationMs: totalWallTimeMs,
    tps: parseFloat(tps),
    percentiles: p,
  };
}

// -----------------------------------------------------------------------------
// Suite 2: Typst Document Server Compilation Benchmark
// -----------------------------------------------------------------------------
async function runTypstDocumentServerBenchmark(templatesDir, concurrency = 10, iterationsPerType = 20) {
  console.log(`\n▶ Starting Suite 2: Typst Document Server Compilation Benchmark`);
  console.log(`  Concurrency: ${concurrency} workers | Iterations per template: ${iterationsPerType}`);

  // Fetch registered templates from document-server to map names to runtime keys
  let nameToKey = new Map();
  try {
    const tplListResp = await fetch(`${DOCS_URL}/api/templates`);
    if (tplListResp.ok) {
      const tplListData = await tplListResp.json();
      const list = tplListData.data?.templates || tplListData.data || [];
      for (const t of list) {
        if (t.name && t.key) {
          nameToKey.set(t.name, t.key);
        }
      }
    }
  } catch (err) {
    console.warn(`  Failed to query /api/templates: ${err.message}`);
  }

  const templateConfigs = [
    { name: 'doc_temp_4pz79z5iba7TcEIp', label: '80mm Thermal Receipt', file: 'documents/doc_temp_4pz79z5iba7TcEIp.json' },
    { name: 'doc_temp_vEf0Y7jQHQj2rIuO', label: 'A4 Commercial Invoice', file: 'documents/doc_temp_vEf0Y7jQHQj2rIuO.json' },
    { name: 'doc_temp_5DHl8hUQTX3oLBSR', label: 'Customer Credit Note', file: 'documents/doc_temp_5DHl8hUQTX3oLBSR.json' },
    { name: 'doc_temp_An1yT1csRep0rtV1', label: 'Analytics Multi-page Report', file: 'documents/doc_temp_An1yT1csRep0rtV1.json' },
    { name: 'lbl_temp_qklcWIolwoFFN3xk', label: 'Product Sticker Label', file: 'labels/lbl_temp_qklcWIolwoFFN3xk.json' },
  ];

  const templateResults = [];

  for (const tpl of templateConfigs) {
    const samplePath = path.join(templatesDir, tpl.file);
    if (!fs.existsSync(samplePath)) {
      console.warn(`  Skipping ${tpl.label}: sample ${samplePath} not found`);
      continue;
    }

    const targetKey = nameToKey.get(tpl.name) || tpl.name;
    const sampleData = JSON.parse(fs.readFileSync(samplePath, 'utf8'));
    const tasks = Array.from({ length: iterationsPerType }, (_, i) => i);
    const latencies = [];
    let bytesSum = 0;
    let success = 0;
    let failed = 0;
    let sampleErrorLogged = false;

    const tStart = performance.now();

    await runConcurrentPool(tasks, concurrency, async () => {
      const t0 = performance.now();
      try {
        const resp = await fetch(`${DOCS_URL}/api/render/${targetKey}`, {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json',
            'X-Internal-Api-Key': INTERNAL_KEY,
          },
          body: JSON.stringify(sampleData),
        });

        const dur = performance.now() - t0;
        latencies.push(dur);

        if (resp.ok) {
          const buf = await resp.arrayBuffer();
          bytesSum += buf.byteLength;
          success++;
        } else {
          failed++;
          if (!sampleErrorLogged) {
            sampleErrorLogged = true;
            console.error(`  [!] Render error for ${tpl.label} (${targetKey}): HTTP ${resp.status} ${await resp.text()}`);
          }
        }
      } catch (err) {
        latencies.push(performance.now() - t0);
        failed++;
        if (!sampleErrorLogged) {
          sampleErrorLogged = true;
          console.error(`  [!] Network render error:`, err);
        }
      }
    });

    const wallTime = performance.now() - tStart;
    const p = calculatePercentiles(latencies);
    const avgBytes = success > 0 ? (bytesSum / success / 1024).toFixed(1) : 0;
    const rps = (success / (wallTime / 1000)).toFixed(1);

    console.log(`  - [${tpl.label}]: ${success}/${iterationsPerType} compiled | avg size: ${avgBytes} KB | p50: ${formatMs(p.p50)} | p95: ${formatMs(p.p95)} | ${rps} renders/s`);

    templateResults.push({
      key: targetKey,
      name: tpl.label,
      iterations: iterationsPerType,
      success,
      failed,
      avgSizeKb: parseFloat(avgBytes),
      rendersPerSecond: parseFloat(rps),
      percentiles: p,
    });
  }

  return templateResults;
}

// -----------------------------------------------------------------------------
// Suite 3: SQLite Analytics Query Under Load
// -----------------------------------------------------------------------------
async function runAnalyticsQueryBenchmark(token, concurrency = 10, totalQueries = 40) {
  console.log(`\n▶ Starting Suite 3: SQLite Analytics & Aggregation Query Benchmark`);
  console.log(`  Concurrency: ${concurrency} workers | Total queries: ${totalQueries}`);

  const endpoints = [
    { label: 'Yearly Analytics Timeseries', path: '/api/reports/analytics/timeseries?preset=this_year' },
    { label: 'Monthly Analytics Timeseries', path: '/api/reports/analytics/timeseries?preset=this_month' },
    { label: 'Weekly Analytics Timeseries', path: '/api/reports/analytics/timeseries?preset=this_week' },
    { label: 'Executive Dashboard KPIs', path: '/api/reports/dashboard' },
    { label: 'Inventory Hierarchical Overview', path: '/api/inventory/overview' },
    { label: 'Billing Invoices Stats KPIs', path: '/api/billing/invoices/stats' },
  ];

  const results = [];

  for (const ep of endpoints) {
    const tasks = Array.from({ length: totalQueries }, (_, i) => i);
    const latencies = [];
    let success = 0;
    let failed = 0;
    let sampleErrorLogged = false;

    const tStart = performance.now();

    await runConcurrentPool(tasks, concurrency, async () => {
      const t0 = performance.now();
      try {
        const resp = await fetch(`${BACKEND_URL}${ep.path}`, {
          headers: { Authorization: `Bearer ${token}` },
        });
        const dur = performance.now() - t0;
        latencies.push(dur);
        if (resp.ok) {
          success++;
        } else {
          failed++;
          if (!sampleErrorLogged) {
            sampleErrorLogged = true;
            console.error(`  [!] Suite 3 error on ${ep.path}: HTTP ${resp.status} ${await resp.text()}`);
          }
        }
      } catch (err) {
        latencies.push(performance.now() - t0);
        failed++;
        if (!sampleErrorLogged) {
          sampleErrorLogged = true;
          console.error(`  [!] Suite 3 network error on ${ep.path}:`, err);
        }
      }
    });

    const wallTime = performance.now() - tStart;
    const wallSec = Math.max(wallTime / 1000, 0.0001);
    const p = calculatePercentiles(latencies);
    const qps = (success / wallSec).toFixed(1);

    console.log(`  - [${ep.label}]: p50: ${formatMs(p.p50)} | p95: ${formatMs(p.p95)} | p99: ${formatMs(p.p99)} | ${qps} QPS`);

    results.push({
      label: ep.label,
      path: ep.path,
      totalQueries,
      success,
      failed,
      qps: parseFloat(qps),
      percentiles: p,
    });
  }

  return results;
}

// -----------------------------------------------------------------------------
// Suite 4: Desktop Backup & Restore Stress Test
// -----------------------------------------------------------------------------
async function runBackupRestoreStressSuite(token) {
  console.log(`\n▶ Starting Suite 4: Desktop Database Backup & Restore Stress`);

  // 1. Export
  const tExportStart = performance.now();
  const exportResp = await fetch(`${BACKEND_URL}/api/backup/export`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${token}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({ includeSettings: true }),
  });

  if (!exportResp.ok) {
    throw new Error(`Backup export failed: ${exportResp.status} ${await exportResp.text()}`);
  }

  const exportPayload = await exportResp.json();
  const exportDur = performance.now() - tExportStart;
  const backupData = exportPayload.data;
  const payloadStr = JSON.stringify(backupData);
  const sizeKb = (payloadStr.length / 1024).toFixed(1);

  let totalRows = 0;
  if (backupData?.tables && typeof backupData.tables === 'object') {
    for (const rows of Object.values(backupData.tables)) {
      if (Array.isArray(rows)) totalRows += rows.length;
    }
  }

  console.log(`  ✅ Backup Export: ${totalRows} rows across tables exported in ${formatMs(exportDur)} (${sizeKb} KB)`);

  // 2. Restore
  const tRestoreStart = performance.now();
  const restoreResp = await fetch(`${BACKEND_URL}/api/backup/import`, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${token}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({ backup: backupData }),
  });

  if (!restoreResp.ok) {
    throw new Error(`Backup restore failed: ${restoreResp.status} ${await restoreResp.text()}`);
  }

  const restoreDur = performance.now() - tRestoreStart;
  console.log(`  ✅ Backup Restore: ${totalRows} rows restored transactionally in ${formatMs(restoreDur)}`);

  return {
    totalRows,
    sizeKb: parseFloat(sizeKb),
    exportDurationMs: exportDur,
    restoreDurationMs: restoreDur,
    exportThroughputRowsSec: parseFloat((totalRows / Math.max(exportDur / 1000, 0.0001)).toFixed(0)),
    restoreThroughputRowsSec: parseFloat((totalRows / Math.max(restoreDur / 1000, 0.0001)).toFixed(0)),
  };
}

// -----------------------------------------------------------------------------
// Main Runner
// -----------------------------------------------------------------------------
async function main() {
  console.log('================================================================');
  console.log(' MyroLogic POS Desktop Benchmark & Stress Test Execution');
  console.log('================================================================');
  console.log(`Backend URL:        ${BACKEND_URL}`);
  console.log(`Document Server:    ${DOCS_URL}`);
  console.log(`Timestamp:          ${new Date().toISOString()}`);

  const token = await authenticateAdmin();
  console.log('✅ Authenticated successfully with Backend');

  const products = await seedTestProducts(token, 20);
  console.log(`✅ Loaded ${products.length} catalog products for checkout benchmarking`);

  const ROOT_DIR = path.resolve(process.cwd());
  const templatesDir = path.join(ROOT_DIR, 'document-server', 'templates');

  // Suite 1: POS Checkout Tiers
  const checkoutTiers = LOGGING_BENCH
    ? [{ name: `Activity-log benchmark (${CHECKOUT_ORDERS} sales)`, concurrency: 5, totalOrders: CHECKOUT_ORDERS }]
    : [
        { name: 'Tier 1: Baseline (Normal)', concurrency: 5, totalOrders: 50 },
        { name: 'Tier 2: Peak Store Traffic', concurrency: 15, totalOrders: 150 },
        { name: 'Tier 3: High Stress Burst', concurrency: 30, totalOrders: 300 },
      ];

  const checkoutResults = [];
  for (const tier of checkoutTiers) {
    const res = await runPosCheckoutStressSuite(token, products, tier);
    checkoutResults.push(res);
  }

  // Suites 2-4 are skipped in activity-log benchmark mode: the checkout suite
  // alone is what gets compared across logging levels.
  const typstResults = LOGGING_BENCH
    ? []
    : await runTypstDocumentServerBenchmark(templatesDir, 10, 20);
  const analyticsResults = LOGGING_BENCH ? [] : await runAnalyticsQueryBenchmark(token, 10, 40);
  const backupResults = LOGGING_BENCH
    ? { totalRows: 0, sizeKb: 0, exportDurationMs: 0, restoreDurationMs: 0, exportThroughputRowsSec: 0, restoreThroughputRowsSec: 0 }
    : await runBackupRestoreStressSuite(token);

  // Compile final JSON report
  const finalReport = {
    metadata: {
      generatedAt: new Date().toISOString(),
      backendUrl: BACKEND_URL,
      docsUrl: DOCS_URL,
      nodeVersion: process.version,
      platform: process.platform,
      arch: process.arch,
    },
    checkoutSuite: checkoutResults,
    typstSuite: typstResults,
    analyticsSuite: analyticsResults,
    backupSuite: backupResults,
  };

  const jsonOutPath = path.join(REPORT_DIR, 'desktop_stress_report.json');
  fs.writeFileSync(jsonOutPath, JSON.stringify(finalReport, null, 2));
  console.log(`\n📊 Full structured benchmark JSON saved to: ${jsonOutPath}`);
}

main().catch((err) => {
  console.error('Stress engine failed:', err);
  process.exit(1);
});
