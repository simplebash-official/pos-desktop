// One sync cycle between the local backend and the POS cloud, plus the pure
// helpers around it (backoff, clock median, number-block rule). The cycle only
// talks HTTP through `LocalApi` / `CloudSyncApi`; it never reads SQLite and
// never logs a record body.
//
// Ordering guarantees:
//   * the local outbox is acknowledged ONLY after the cloud answered 2xx to the
//     push (a crash in between just re-sends the same, deterministic batch id
//     and the cloud answers `duplicate`);
//   * a pull page advances the local cursor in the same `apply` call that
//     stores its changes, so a failed apply leaves the cursor where it was.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::logging::{Level, LogEvent};

use super::http::{
    BlockInfo, CloudSyncApi, ErrorKind, LocalApi, LocalState, OutboxItem, ResourceCount, SyncError,
};
use super::state::{CycleSummary, HistoryKind, SyncControl, SyncPhase, SyncStep};

/// Changes per upload request. Small on purpose: the cloud applies a batch one
/// change at a time, so a big batch can outlast the request timeout, and the
/// progress bar only moves when a batch is confirmed.
pub const PUSH_PAGE: usize = 25;
pub const PULL_PAGE: usize = 200;
/// Pause between cycles when everything is quiet.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(15);
/// Full number-block top-up check.
pub const BLOCK_CHECK_INTERVAL: Duration = Duration::from_secs(20 * 60);
/// Only push a new clock offset to the backend when it moved this much.
pub const CLOCK_RESYNC_THRESHOLD_MS: i64 = 500;

/// Sequences the cloud can reserve blocks for, with the block size to request.
/// SKU is deliberately absent — it has no single fixed name (one block family
/// per category+subcategory prefix, e.g. "sku:PHO-SCR"), so it's discovered
/// and topped up separately; see `skus_due`/`SKU_BLOCK_SIZE`.
pub const BLOCK_NAMES: &[(&str, u64)] = &[
    ("invoice", 100),
    ("creditNote", 100),
    ("repair", 100),
    ("printJob", 100),
    ("purchase", 100),
    ("barcode", 200),
];

/// Block size for a `sku:<prefix>` family — smaller than invoice/etc. since
/// demand per category is usually much lower.
pub const SKU_BLOCK_SIZE: u64 = 50;

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Retry schedule after consecutive failures: 2 s, 5 s, 15 s, 60 s, then 5 min.
pub const BACKOFF_SECS: [u64; 5] = [2, 5, 15, 60, 300];

#[derive(Debug, Default)]
pub struct Backoff {
    attempt: usize,
}

impl Backoff {
    /// `jitter` is a fraction in about -0.2..0.2 so devices do not retry in lockstep.
    pub fn next_delay(&mut self, jitter: f64) -> Duration {
        let base = BACKOFF_SECS[self.attempt.min(BACKOFF_SECS.len() - 1)] as f64;
        self.attempt += 1;
        Duration::from_secs_f64((base * (1.0 + jitter)).max(0.0))
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Median of the last five `server - local` clock samples.
#[derive(Debug, Default)]
pub struct ClockSampler {
    samples: Mutex<VecDeque<i64>>,
}

impl ClockSampler {
    pub fn record(&self, offset_ms: i64) {
        let mut s = self.samples.lock().unwrap();
        if s.len() == 5 {
            s.pop_front();
        }
        s.push_back(offset_ms);
    }

    pub fn median(&self) -> Option<i64> {
        let s = self.samples.lock().unwrap();
        if s.is_empty() {
            return None;
        }
        let mut sorted: Vec<i64> = s.iter().copied().collect();
        sorted.sort_unstable();
        let mid = sorted.len() / 2;
        Some(if sorted.len() % 2 == 1 {
            sorted[mid]
        } else {
            (sorted[mid - 1] + sorted[mid]) / 2
        })
    }
}

/// Which sequences need a fresh block: missing ones, ones that are 60 % used
/// up (any cycle), and - on the periodic check - ones at or below half.
pub fn blocks_due(
    have: &[BlockInfo],
    wanted: &[(&'static str, u64)],
    periodic_check: bool,
) -> Vec<(&'static str, u64)> {
    wanted
        .iter()
        .filter(
            |(name, _)| match have.iter().find(|b| b.name.eq_ignore_ascii_case(name)) {
                None => true,
                Some(b) if b.block_size == 0 => true,
                Some(b) => {
                    let consumed_60 = b.remaining * 100 <= b.block_size * 40;
                    let half_or_less = b.remaining * 2 <= b.block_size;
                    consumed_60 || (periodic_check && half_or_less)
                }
            },
        )
        .copied()
        .collect()
}

/// Same "when is a block due" rule as `blocks_due`, for the dynamically
/// discovered `sku:<prefix>` families — these can't live in the `'static`
/// `BLOCK_NAMES` list since they come from the local catalog at runtime.
pub fn skus_due(have: &[BlockInfo], prefixes: &[String], periodic_check: bool) -> Vec<String> {
    prefixes
        .iter()
        .filter(|prefix| {
            let name = format!("sku:{prefix}");
            match have.iter().find(|b| b.name.eq_ignore_ascii_case(&name)) {
                None => true,
                Some(b) if b.block_size == 0 => true,
                Some(b) => {
                    let consumed_60 = b.remaining * 100 <= b.block_size * 40;
                    let half_or_less = b.remaining * 2 <= b.block_size;
                    consumed_60 || (periodic_check && half_or_less)
                }
            }
        })
        .cloned()
        .collect()
}

/// Snapshot records have no cloud sequence; the apply endpoint wants one.
fn as_pulled_change(mut record: Value) -> Value {
    if let Value::Object(map) = &mut record {
        map.entry("seq").or_insert(json!(0));
    }
    record
}

// ---------------------------------------------------------------------------
// The agent
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CycleReport {
    pub pushed: usize,
    pub pulled: usize,
    pub blocks_reserved: usize,
    pub refused: usize,
    pub bootstrap_required: bool,
}

/// The backend's per-resource counts as plain pairs for the status helpers.
fn pairs(list: &[ResourceCount]) -> Vec<(String, u64)> {
    list.iter().map(|r| (r.resource.clone(), r.count)).collect()
}

/// How many of these changes belong to each record type (by their `resource`).
fn tally<'a>(changes: impl Iterator<Item = &'a Value>) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = Vec::new();
    for change in changes {
        let Some(resource) = change.get("resource").and_then(Value::as_str) else {
            continue;
        };
        match out.iter_mut().find(|(r, _)| r == resource) {
            Some((_, n)) => *n += 1,
            None => out.push((resource.to_string(), 1)),
        }
    }
    out
}

/// Upload batch id: deterministic per outbox range, so a retry after a failure
/// re-sends the very same id (which the cloud dedups). The epoch keeps ids
/// unique when the local database is recreated and its numbering restarts.
fn batch_id(device_id: &str, epoch: &str, first_seq: i64, last_seq: i64) -> String {
    if epoch.is_empty() {
        format!("{device_id}-{first_seq}-{last_seq}")
    } else {
        format!("{device_id}-{epoch}-{first_seq}-{last_seq}")
    }
}

pub struct Agent {
    pub local: LocalApi,
    pub cloud: CloudSyncApi,
    pub ctl: Arc<SyncControl>,
    pub clock: Arc<ClockSampler>,
    pub tenant_id: String,
    pub device_id: String,
    pub device_name: String,
    pub app_version: String,
    registered: AtomicBool,
    /// A full cycle has completed since this agent was built (device
    /// registered, capture on, first sync done), so the fast paths may run.
    ready: AtomicBool,
    /// The cloud already knows this shop is set up (nothing left to tell it).
    cloud_setup_marked: AtomicBool,
    last_block_check: Mutex<Option<Instant>>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        local: LocalApi,
        cloud: CloudSyncApi,
        ctl: Arc<SyncControl>,
        clock: Arc<ClockSampler>,
        tenant_id: String,
        device_id: String,
        device_name: String,
        app_version: String,
    ) -> Self {
        Self {
            local,
            cloud,
            ctl,
            clock,
            tenant_id,
            device_id,
            device_name,
            app_version,
            registered: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            cloud_setup_marked: AtomicBool::new(false),
            last_block_check: Mutex::new(None),
        }
    }

    /// One full pass: prepare, push, pull, top up number blocks.
    pub async fn cycle(&self) -> Result<CycleReport, SyncError> {
        let mut report = CycleReport::default();
        self.ctl.update(|s| {
            s.linked = true;
            if s.state != SyncPhase::Paused {
                s.state = SyncPhase::Syncing;
            }
            s.next_retry_at = None;
        });
        self.ctl.set_step(SyncStep::Preparing, 0);

        let mut state = self.local.state().await?;
        // A shop's database only ever syncs with that shop. If this one was last
        // linked to another, pushing or pulling would mix the two shops' rows.
        if state
            .tenant_id
            .as_deref()
            .is_some_and(|t| !t.is_empty() && t != self.tenant_id)
        {
            return Err(SyncError::new(
                ErrorKind::Invalid,
                "LOCAL_TENANT_MISMATCH",
                "this computer's data belongs to another shop; switch shop to open it",
                0,
            ));
        }
        if !state.linked || !state.capture_enabled {
            self.local.enable(&self.tenant_id, &self.device_id).await?;
            state = self.local.state().await?;
        }
        if !self.registered.load(Ordering::SeqCst) {
            self.cloud
                .register_device(&self.device_id, &self.device_name, &self.app_version)
                .await?;
            self.registered.store(true, Ordering::SeqCst);
        }
        self.sync_clock(&state).await?;

        if state.cloud_cursor == 0
            && state.last_pushed_outbox_seq == 0
            && !self.first_sync(&state).await?
        {
            report.bootstrap_required = true;
            return Ok(report);
        }
        let state = self.local.state().await?;

        (report.pushed, report.refused) = self.push_all(&state).await?;
        if !self.pull_step(&state, &mut report).await? {
            return Ok(report);
        }
        self.ctl.set_step(SyncStep::Finishing, 0);
        report.blocks_reserved = self.top_up_blocks(&state).await?;
        self.sync_setup_flag().await;

        let report = self.finish(report, state).await;
        self.ready.store(true, Ordering::SeqCst);
        Ok(report)
    }

    /// The realtime path: only what a wake asked for. `push` after a local
    /// write, `pull` after the cloud announced a change. Skips everything the
    /// full cycle does on its timer (registration, clock, number blocks, setup
    /// flag) and falls back to the full cycle until one has completed.
    pub async fn quick(&self, push: bool, pull: bool) -> Result<CycleReport, SyncError> {
        if !self.ready.load(Ordering::SeqCst) {
            return self.cycle().await;
        }
        let mut report = CycleReport::default();
        self.ctl.update(|s| {
            if s.state != SyncPhase::Paused {
                s.state = SyncPhase::Syncing;
            }
            s.next_retry_at = None;
        });
        let state = self.local.state().await?;
        if !state.linked || !state.capture_enabled {
            self.ready.store(false, Ordering::SeqCst);
            return self.cycle().await;
        }
        if push {
            (report.pushed, report.refused) = self.push_all(&state).await?;
        } else {
            self.ctl.begin_modules(
                &pairs(&state.pending_by_resource),
                &pairs(&state.conflicts_by_resource),
            );
        }
        if pull && !self.pull_step(&state, &mut report).await? {
            return Ok(report);
        }
        Ok(self.finish(report, state).await)
    }

    /// Downloads everything after the local cursor. `false` when the cloud
    /// history no longer reaches this device (a re-download was requested).
    async fn pull_step(
        &self,
        state: &LocalState,
        report: &mut CycleReport,
    ) -> Result<bool, SyncError> {
        self.ctl.set_step(SyncStep::Downloading, 0);
        match self.pull_all(state.cloud_cursor).await {
            Ok(n) => {
                report.pulled = n;
                Ok(true)
            }
            Err(e) if e.kind == ErrorKind::CursorExpired => {
                self.ready.store(false, Ordering::SeqCst);
                self.ctl.require_bootstrap(
                    "the cloud history no longer reaches this device; a full re-download is needed",
                );
                report.bootstrap_required = true;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// End of a cycle: fresh counts, status, history and the cycle log line.
    async fn finish(&self, report: CycleReport, state: LocalState) -> CycleReport {
        let fresh = self.local.state().await.unwrap_or(state);
        let finished_at = chrono::Utc::now().to_rfc3339();
        self.ctl.finish_modules(
            &pairs(&fresh.pending_by_resource),
            &pairs(&fresh.conflicts_by_resource),
            &finished_at,
        );
        self.ctl.update(|s| {
            s.step = SyncStep::Idle;
            s.progress = None;
            s.last_cycle = Some(CycleSummary {
                at: finished_at.clone(),
                sent: report.pushed as u64,
                received: report.pulled as u64,
            });
            s.state = if self.ctl.is_paused() {
                SyncPhase::Paused
            } else {
                SyncPhase::Idle
            };
            s.last_sync_at = Some(chrono::Utc::now().to_rfc3339());
            s.last_error = None;
            s.pending_out = fresh.pending_out;
            s.conflicts_open = fresh.conflicts_open;
        });
        // Quiet cycles stay out of the activity list; only ones that moved data show.
        if report.pushed > 0 || report.pulled > 0 || report.refused > 0 {
            let message = (report.refused > 0).then(|| {
                format!(
                    "{} change(s) were refused. See Sync Conflicts.",
                    report.refused
                )
            });
            self.ctl.push_history(
                HistoryKind::Synced,
                report.pushed as u64,
                report.pulled as u64,
                message,
            );
        }
        LogEvent::shell("sync", "cycle.done")
            .level(Level::Debug)
            .data(json!({
                "pushed": report.pushed,
                "pulled": report.pulled,
                "blocks": report.blocks_reserved,
            }))
            .emit();
        report
    }

    /// Once this device has finished the shop's first-time setup, tells the cloud
    /// so the website's POS does not ask the same demo-vs-clean question again.
    /// Best effort: a failure is logged and retried on a later cycle; the cloud
    /// never lets this un-set a shop that is already set up.
    async fn sync_setup_flag(&self) {
        if self.cloud_setup_marked.load(Ordering::SeqCst) {
            return;
        }
        let outcome: Result<bool, SyncError> = async {
            let local = self.local.setup_status().await?;
            if !local.setup_completed {
                return Ok(false);
            }
            if !self.cloud.status().await?.setup_completed {
                self.cloud
                    .mark_setup_complete(local.sample_data_loaded)
                    .await?;
            }
            Ok(true)
        }
        .await;
        match outcome {
            Ok(true) => self.cloud_setup_marked.store(true, Ordering::SeqCst),
            Ok(false) => {}
            Err(e) => LogEvent::shell("sync", "setup_flag.failed")
                .level(Level::Warn)
                .data(json!({ "code": e.code }))
                .emit(),
        }
    }

    async fn sync_clock(&self, state: &LocalState) -> Result<(), SyncError> {
        if let Some(median) = self.clock.median() {
            if (median - state.clock_offset_ms).abs() > CLOCK_RESYNC_THRESHOLD_MS {
                self.local.set_clock_offset(median).await?;
            }
        }
        Ok(())
    }

    /// Decides what a device that has never synced does. `Ok(false)` means the
    /// cycle must stop: the shop already has cloud data and this device has its
    /// own, so nothing is merged or wiped until the user confirms.
    async fn first_sync(&self, state: &LocalState) -> Result<bool, SyncError> {
        let cloud = self.cloud.status().await?;
        if cloud.server_seq == 0 {
            // A shop created on the web has its owner's POS admin in the cloud
            // tables but no change history yet. A brand-new install that has not
            // been set up (nothing local to lose) joins it by downloading that,
            // instead of uploading an empty database and asking for a second admin.
            if !state.local_has_data
                && self.local.outbox(0, 1).await?.items.is_empty()
                && !self.local.setup_completed().await?
                && !self.cloud.snapshot(None).await?.changes.is_empty()
            {
                self.bootstrap().await?;
                return Ok(true);
            }
            // First device of the shop: upload what exists.
            self.local.seed().await?;
            return Ok(true);
        }
        let already_seeded = !self.local.outbox(0, 1).await?.items.is_empty();
        if already_seeded {
            return Ok(true);
        }
        if !state.local_has_data {
            self.bootstrap().await?;
            return Ok(true);
        }
        self.ctl.require_bootstrap(
            "this shop already has data in the cloud and this device has its own; replacing the local data needs confirmation",
        );
        Ok(false)
    }

    /// Replaces the local data with the cloud snapshot (empty device, or after
    /// the user confirmed). Pages go to the local backend one by one; the last
    /// one carries the snapshot's sequence as the new cursor.
    pub async fn bootstrap(&self) -> Result<(), SyncError> {
        // The final page tells the local backend whether the cloud shop is already
        // set up, so a freshly created shop still gets its demo-vs-clean choice.
        let cloud = self.cloud.status().await?;
        let mut page: Option<String> = None;
        let mut first = true;
        let mut as_of: Option<i64> = None;
        loop {
            let snap = self.cloud.snapshot(page.as_deref()).await?;
            let as_of_seq = *as_of.get_or_insert(snap.as_of_seq);
            let last = snap.next_page.is_none();
            let changes: Vec<Value> = snap.changes.into_iter().map(as_pulled_change).collect();
            let body = json!({
                "mode": if first { "bootstrap" } else { "incremental" },
                "changes": changes,
                "advanceCursorTo": if last { Some(as_of_seq) } else { None },
                "setupCompleted": if last { Some(cloud.setup_completed) } else { None },
                "sampleDataLoaded": if last { Some(cloud.sample_data_loaded) } else { None },
            });
            self.local.apply(&body).await?;
            first = false;
            if last {
                break;
            }
            page = snap.next_page;
        }
        self.ctl.clear_bootstrap();
        // Everything was replaced: every open screen should reload.
        self.ctl.applied(&["*".to_string()]);
        self.ready.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn push_all(&self, state: &LocalState) -> Result<(usize, usize), SyncError> {
        let mut after = state.last_pushed_outbox_seq;
        let mut total = 0;
        let mut refused_total = 0;
        self.ctl.set_step(SyncStep::Uploading, state.pending_out);
        self.ctl.begin_modules(
            &pairs(&state.pending_by_resource),
            &pairs(&state.conflicts_by_resource),
        );
        loop {
            let page = self.local.outbox(after, PUSH_PAGE).await?;
            let (Some(first), Some(last)) = (page.items.first(), page.items.last()) else {
                break;
            };
            let (_, last_seq) = (first.seq, last.seq);
            let count = page.items.len();
            let refused = self
                .push_page(&page.items, &page.epoch, state.cloud_cursor)
                .await?;
            after = last_seq;
            total += count;
            refused_total += refused;
            self.ctl
                .modules_sent(&tally(page.items.iter().map(|i| &i.record)), total as u64);
            if count < PUSH_PAGE {
                break;
            }
        }
        Ok((total, refused_total))
    }

    /// Uploads one outbox page. A page the cloud refuses outright (a 4xx that
    /// retrying cannot fix, such as one malformed record) is split in halves
    /// until the offending change is alone; that change is dropped and logged
    /// like any other refused change, so it can never hold back the changes
    /// queued behind it. Returns how many changes were refused.
    async fn push_page(
        &self,
        items: &[OutboxItem],
        epoch: &str,
        base_seq: i64,
    ) -> Result<usize, SyncError> {
        let mut queue: VecDeque<&[OutboxItem]> = VecDeque::from([items]);
        let mut refused = 0;
        while let Some(chunk) = queue.pop_front() {
            match self.push_chunk(chunk, epoch, base_seq).await {
                Ok(n) => refused += n,
                Err(e) if e.kind == ErrorKind::Invalid && chunk.len() > 1 => {
                    let (head, tail) = chunk.split_at(chunk.len() / 2);
                    queue.push_front(tail);
                    queue.push_front(head);
                }
                Err(e) if e.kind == ErrorKind::Invalid => {
                    let record = &chunk[0].record;
                    LogEvent::shell("sync", "push.dropped")
                        .level(Level::Warn)
                        .data(json!({
                            "resource": record.get("resource"),
                            "key": record.get("key"),
                            "code": e.code,
                            "status": e.status,
                        }))
                        .emit();
                    self.local.ack(chunk[0].seq).await?;
                    refused += 1;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(refused)
    }

    /// Sends one batch and, once the cloud confirmed it, drops it from the
    /// outbox. Returns how many of its changes the cloud refused.
    async fn push_chunk(
        &self,
        chunk: &[OutboxItem],
        epoch: &str,
        base_seq: i64,
    ) -> Result<usize, SyncError> {
        let (Some(first), Some(last)) = (chunk.first(), chunk.last()) else {
            return Ok(0);
        };
        let body = json!({
            "deviceId": self.device_id,
            "batchId": batch_id(&self.device_id, epoch, first.seq, last.seq),
            "baseSeq": base_seq,
            "changes": chunk.iter().map(|i| &i.record).collect::<Vec<_>>(),
            "outboxSeqs": chunk.iter().map(|i| i.seq).collect::<Vec<_>>(),
        });
        let started = Instant::now();
        let result = self.cloud.push(&body).await?;
        LogEvent::shell("sync", "push.page")
            .level(Level::Debug)
            .data(json!({ "count": chunk.len(), "ms": started.elapsed().as_millis() as u64 }))
            .emit();
        // Only now that the cloud confirmed does the outbox shrink.
        self.local.ack(last.seq).await?;
        let refused = result
            .acks
            .iter()
            .filter(|a| {
                matches!(
                    a.get("status").and_then(Value::as_str),
                    Some("rejected" | "conflict")
                )
            })
            .count();
        if refused > 0 {
            LogEvent::shell("sync", "push.refused")
                .level(Level::Warn)
                .data(json!({ "count": refused, "batch": chunk.len() }))
                .emit();
        }
        Ok(refused)
    }

    async fn pull_all(&self, cursor: i64) -> Result<usize, SyncError> {
        let mut cursor = cursor;
        let mut pulled = 0;
        let mut touched: Vec<String> = Vec::new();
        loop {
            let page = self.cloud.pull(cursor, PULL_PAGE).await?;
            let advance = page.next_seq.max(cursor);
            if page.changes.is_empty() && advance == cursor && !page.has_more {
                break;
            }
            let count = page.changes.len();
            let body = json!({
                "mode": "incremental",
                "changes": page.changes,
                "advanceCursorTo": advance,
            });
            self.local.apply(&body).await?;
            pulled += count;
            let counts = tally(page.changes.iter());
            for (resource, _) in &counts {
                if !touched.contains(resource) {
                    touched.push(resource.clone());
                }
            }
            self.ctl.modules_received(&counts, pulled as u64);
            cursor = advance;
            if !page.has_more {
                break;
            }
        }
        // Once per download, so open screens reload once, not per page.
        self.ctl.applied(&touched);
        Ok(pulled)
    }

    async fn top_up_blocks(&self, state: &LocalState) -> Result<usize, SyncError> {
        let periodic = {
            let last = self.last_block_check.lock().unwrap();
            last.map(|t| t.elapsed() >= BLOCK_CHECK_INTERVAL)
                .unwrap_or(true)
        };
        let mut reserved = 0;
        for (name, size) in blocks_due(&state.number_blocks, BLOCK_NAMES, periodic) {
            reserved += self.reserve_one_block(name, size).await?;
        }

        // SKU has no fixed name — discover which category+subcategory
        // prefixes the local catalog actually has, then top up each one the
        // same way. A failure to even list them (e.g. the local backend is
        // briefly unreachable) is logged and skipped, same as an individual
        // reservation failure below — it must never abort the other families.
        match self.local.sku_prefixes().await {
            Ok(prefixes) => {
                for prefix in skus_due(&state.number_blocks, &prefixes, periodic) {
                    let name = format!("sku:{prefix}");
                    reserved += self.reserve_one_block(&name, SKU_BLOCK_SIZE).await?;
                }
            }
            Err(e) => {
                LogEvent::shell("sync", "sku_prefixes.list_failed")
                    .level(Level::Warn)
                    .data(json!({ "code": e.code }))
                    .emit();
            }
        }

        if periodic {
            *self.last_block_check.lock().unwrap() = Some(Instant::now());
        }
        Ok(reserved)
    }

    /// Reserves one block by `name` from the cloud and stores it locally.
    /// Returns `1` on success, `0` on a failure worth only logging (an
    /// `Offline`/`Revoked`/`Unauthorized` failure instead propagates, since
    /// those mean the whole cycle should stop, not just this one block).
    async fn reserve_one_block(&self, name: &str, size: u64) -> Result<usize, SyncError> {
        match self.cloud.reserve_block(name, size, &self.device_id).await {
            Ok(block) => {
                self.local
                    .put_block(&json!({
                        "name": name,
                        "prefix": block.get("prefix"),
                        "padding": block.get("padding"),
                        "start": block.get("start"),
                        "end": block.get("end"),
                        "expiresAt": block.get("expiresAt"),
                    }))
                    .await?;
                Ok(1)
            }
            Err(e)
                if matches!(
                    e.kind,
                    ErrorKind::Offline | ErrorKind::Revoked | ErrorKind::Unauthorized
                ) =>
            {
                Err(e)
            }
            Err(e) => {
                LogEvent::shell("sync", "blocks.reserve_failed")
                    .level(Level::Warn)
                    .data(json!({ "name": name, "code": e.code }))
                    .emit();
                Ok(0)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::state::{Recorder, SyncStatus};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::http::{ClockFn, SecretFn, TokenFn};

    struct Fixture {
        agent: Agent,
        local: MockServer,
        cloud: MockServer,
        rec: Arc<Recorder>,
        forced: Arc<Mutex<Vec<bool>>>,
    }

    async fn fixture() -> Fixture {
        let local = MockServer::start().await;
        let cloud = MockServer::start().await;
        let rec = Arc::new(Recorder::default());
        let ctl = SyncControl::new(rec.clone());
        let clock = Arc::new(ClockSampler::default());
        let forced = Arc::new(Mutex::new(Vec::new()));
        let f2 = forced.clone();
        let token: TokenFn = Arc::new(move |force| {
            f2.lock().unwrap().push(force);
            Box::pin(async move { Ok(if force { "t2" } else { "t1" }.to_string()) })
        });
        let clock2 = clock.clone();
        let on_clock: ClockFn = Arc::new(move |ms| clock2.record(ms));
        let secret: SecretFn = Arc::new(|| Some("local-secret".to_string()));
        let http = reqwest::Client::new();
        let agent = Agent::new(
            LocalApi::new(http.clone(), local.uri(), secret),
            CloudSyncApi::new(http, cloud.uri(), token, on_clock),
            ctl,
            clock,
            "tnt_1".into(),
            "dev_cloud".into(),
            "Till 1".into(),
            "0.0.0".into(),
        );
        Fixture {
            agent,
            local,
            cloud,
            rec,
            forced,
        }
    }

    fn ok(data: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({ "success": true, "data": data }))
    }

    fn state(cursor: i64, pushed: i64) -> Value {
        json!({
            "deviceId": "dev_local", "linked": true, "captureEnabled": true,
            "cloudCursor": cursor, "lastPushedOutboxSeq": pushed, "clockOffsetMs": 0,
            "pendingOut": 0, "conflictsOpen": 2, "localHasData": true,
            "numberBlocks": [ { "name": "invoice", "remaining": 90, "blockSize": 100 } ],
            "pendingByResource": [ { "resource": "invoices", "count": 2 } ],
            "conflictsByResource": [ { "resource": "products", "count": 2 } ],
        })
    }

    fn outbox(seqs: &[i64]) -> Value {
        json!({
            "items": seqs.iter().map(|s| json!({ "seq": s, "record": { "resource": "products", "key": format!("prod_{s}") } })).collect::<Vec<_>>(),
            "lastSeq": seqs.last().copied().unwrap_or(0),
        })
    }

    async fn mount_quiet_cloud(f: &Fixture) {
        Mock::given(method("POST"))
            .and(path("/api/sync/devices/register"))
            .respond_with(ok(json!({})))
            .mount(&f.cloud)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/pull"))
            .respond_with(ok(json!({ "changes": [], "nextSeq": 5, "hasMore": false })))
            .mount(&f.cloud)
            .await;
        // Every other sequence is missing locally, so blocks get reserved.
        // `[^/]+` (not just `[A-Za-z]+`) so this also matches a `sku:<prefix>`
        // name, which contains a colon and a hyphen.
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex(r"^/api/sequences/[^/]+/reserve$"))
            .respond_with(ok(json!({ "prefix": "X-", "padding": 6, "start": 1, "end": 100, "expiresAt": "2030-01-01T00:00:00Z" })))
            .mount(&f.cloud)
            .await;
        // No mock for GET /api/sync/sku-prefixes here on purpose: an
        // unmatched wiremock request answers 404, which `top_up_blocks`
        // already treats as "no SKU prefixes to top up" (logged, not fatal) —
        // exactly the behaviour every OTHER test using this helper wants. A
        // test that cares about SKU blocks specifically mounts its own.
        Mock::given(method("POST"))
            .and(path("/api/sync/blocks"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/apply"))
            .respond_with(ok(json!({ "applied": 0, "cursor": 5 })))
            .mount(&f.local)
            .await;
    }

    async fn body_of(server: &MockServer, http_path: &str) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == http_path)
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect()
    }

    #[tokio::test]
    async fn push_happy_path_acks_after_cloud_and_reports_counts() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .and(query_param("after", "3"))
            .respond_with(ok(outbox(&[4, 6])))
            .mount(&f.local)
            .await;
        Mock::given(method("POST")).and(path("/api/sync/push"))
            .respond_with(ok(json!({ "acks": [ { "key": "prod_4", "status": "applied" }, { "key": "prod_6", "status": "duplicate" } ], "serverSeq": 9 })))
            .mount(&f.cloud).await;
        Mock::given(method("POST"))
            .and(path("/api/sync/outbox/ack"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;

        let report = f.agent.cycle().await.unwrap();
        assert_eq!((report.pushed, report.pulled), (2, 0));

        let push = &body_of(&f.cloud, "/api/sync/push").await[0];
        assert_eq!(push["deviceId"], "dev_cloud");
        assert_eq!(push["batchId"], "dev_cloud-4-6");
        assert_eq!(push["outboxSeqs"], json!([4, 6]));
        assert_eq!(push["changes"].as_array().unwrap().len(), 2);
        assert_eq!(
            body_of(&f.local, "/api/sync/outbox/ack").await[0]["upToSeq"],
            6
        );

        let status = f.agent.ctl.snapshot();
        assert_eq!(status.state, SyncPhase::Idle);
        assert!(status.last_sync_at.is_some());
        assert_eq!(status.conflicts_open, 2);
        assert!(f
            .rec
            .statuses
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.state == SyncPhase::Syncing));

        // The webview can follow the cycle: preparing, uploading with a total, then downloading.
        let steps: Vec<_> = f
            .rec
            .statuses
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.step)
            .collect();
        let pos = |x| steps.iter().position(|s| *s == x).unwrap();
        assert!(pos(SyncStep::Preparing) < pos(SyncStep::Uploading));
        assert!(pos(SyncStep::Uploading) < pos(SyncStep::Downloading));
        assert_eq!(status.step, SyncStep::Idle);
        assert_eq!(status.progress, None);

        // The activity list and per-module rows are filled after the cycle.
        assert_eq!(status.history[0].kind, HistoryKind::Synced);
        assert_eq!(status.history[0].sent, 2);
        assert_eq!(status.last_cycle.as_ref().unwrap().sent, 2);
        let modules: Vec<_> = status
            .modules
            .iter()
            .map(|m| (m.resource.as_str(), m.pending, m.conflicts))
            .collect();
        assert_eq!(modules, vec![("invoices", 2, 0), ("products", 0, 2)]);
    }

    #[tokio::test]
    async fn a_big_outbox_goes_up_in_small_confirmed_batches_with_live_progress() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        let mut waiting = state(5, 3);
        waiting["pendingOut"] = json!(27);
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(waiting))
            .mount(&f.local)
            .await;
        let first: Vec<i64> = (4..4 + PUSH_PAGE as i64).collect();
        let last_of_first = *first.last().unwrap();
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .and(query_param("after", "3"))
            .and(query_param("limit", PUSH_PAGE.to_string()))
            .respond_with(ok(outbox(&first)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .and(query_param("after", last_of_first.to_string()))
            .respond_with(ok(outbox(&[last_of_first + 1, last_of_first + 2])))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/push"))
            .respond_with(ok(json!({ "acks": [], "serverSeq": 9 })))
            .mount(&f.cloud)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/outbox/ack"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;

        let report = f.agent.cycle().await.unwrap();
        assert_eq!(report.pushed, PUSH_PAGE + 2);

        // Two requests, each confirmed before the outbox shrinks.
        let pushes = body_of(&f.cloud, "/api/sync/push").await;
        assert_eq!(pushes.len(), 2);
        assert_eq!(pushes[0]["changes"].as_array().unwrap().len(), PUSH_PAGE);
        assert_eq!(
            pushes[1]["outboxSeqs"],
            json!([last_of_first + 1, last_of_first + 2])
        );
        let acks: Vec<i64> = body_of(&f.local, "/api/sync/outbox/ack")
            .await
            .iter()
            .map(|b| b["upToSeq"].as_i64().unwrap())
            .collect();
        assert_eq!(acks, vec![last_of_first, last_of_first + 2]);

        // The webview sees the bar move after every confirmed batch, not just at the end.
        let done: Vec<u64> = f
            .rec
            .statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.step == SyncStep::Uploading)
            .filter_map(|s| s.progress.map(|p| p.done))
            .collect();
        assert!(done.windows(2).all(|w| w[0] <= w[1]), "{done:?}");
        assert!(done.contains(&(PUSH_PAGE as u64)), "{done:?}");
        assert_eq!(done.last().copied(), Some(PUSH_PAGE as u64 + 2));
    }

    #[tokio::test]
    async fn each_record_type_reports_what_was_sent_and_received_as_it_happens() {
        let f = fixture().await;
        // Registered before the quiet cloud's empty pull, so this one answers.
        Mock::given(method("GET"))
            .and(path("/api/sync/pull"))
            .respond_with(ok(json!({
                "changes": [
                    { "seq": 6, "resource": "customers", "key": "c1" },
                    { "seq": 7, "resource": "customers", "key": "c2" },
                    { "seq": 8, "resource": "customers", "key": "c3" }
                ],
                "nextSeq": 8,
                "hasMore": false
            })))
            .mount(&f.cloud)
            .await;
        mount_quiet_cloud(&f).await;

        let mut waiting = state(5, 3);
        waiting["pendingOut"] = json!(27);
        waiting["pendingByResource"] = json!([
            { "resource": "invoices", "count": 20 },
            { "resource": "products", "count": 7 }
        ]);
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(waiting))
            .mount(&f.local)
            .await;
        let item = |seq: i64, resource: &str| json!({ "seq": seq, "record": { "resource": resource, "key": format!("k{seq}") } });
        // First batch: 20 invoices then 5 products; second batch: the last 2 products.
        let first: Vec<Value> = (4..=23)
            .map(|s| item(s, "invoices"))
            .chain((24..=28).map(|s| item(s, "products")))
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .and(query_param("after", "3"))
            .respond_with(ok(json!({ "items": first, "lastSeq": 28 })))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .and(query_param("after", "28"))
            .respond_with(ok(json!({
                "items": [item(29, "products"), item(30, "products")],
                "lastSeq": 30
            })))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/push"))
            .respond_with(ok(json!({ "acks": [], "serverSeq": 9 })))
            .mount(&f.cloud)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/outbox/ack"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;

        let report = f.agent.cycle().await.unwrap();
        assert_eq!((report.pushed, report.pulled), (27, 3));

        let seen = f.rec.statuses.lock().unwrap().clone();
        let find = |s: &SyncStatus, r: &str| s.modules.iter().find(|m| m.resource == r).cloned();

        // The cycle opens with what is waiting per type, before anything is sent.
        assert!(seen.iter().any(|s| {
            s.step == SyncStep::Uploading
                && find(s, "invoices").is_some_and(|m| m.pending == 20 && m.sent == 0)
                && find(s, "products").is_some_and(|m| m.pending == 7 && m.sent == 0)
        }));
        // After batch one, invoices are finished while products still have 2 to go.
        assert!(seen.iter().any(|s| {
            find(s, "invoices").is_some_and(|m| m.pending == 0 && m.sent == 20)
                && find(s, "products").is_some_and(|m| m.pending == 2 && m.sent == 5)
        }));
        // Downloaded changes are counted for their own record type while downloading.
        assert!(seen.iter().any(|s| {
            s.step == SyncStep::Downloading && find(s, "customers").is_some_and(|m| m.received == 3)
        }));

        // When the cycle ends, each type that moved data remembers it.
        let end = f.agent.ctl.snapshot();
        let change = |r: &str| find(&end, r).and_then(|m| m.last_change);
        assert_eq!(
            change("invoices").map(|c| (c.sent, c.received)),
            Some((20, 0))
        );
        assert_eq!(
            change("products").map(|c| (c.sent, c.received)),
            Some((7, 0))
        );
        assert_eq!(
            change("customers").map(|c| (c.sent, c.received)),
            Some((0, 3))
        );
    }

    #[tokio::test]
    async fn linked_agent_tops_up_sku_and_barcode_blocks_alongside_the_fixed_families() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/sku-prefixes"))
            .respond_with(ok(json!({ "prefixes": ["PHO-SCR"] })))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            // No blocks held at all: every fixed family AND the discovered
            // SKU prefix must be reserved this cycle.
            .respond_with(ok(json!({
                "deviceId": "dev_local", "linked": true, "captureEnabled": true,
                "cloudCursor": 5, "lastPushedOutboxSeq": 3, "clockOffsetMs": 0,
                "pendingOut": 0, "conflictsOpen": 0, "localHasData": true,
                "numberBlocks": [],
            })))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;

        let report = f.agent.cycle().await.unwrap();
        assert_eq!(report.blocks_reserved, BLOCK_NAMES.len() + 1);

        let stored: Vec<String> = body_of(&f.local, "/api/sync/blocks")
            .await
            .iter()
            .map(|b| b["name"].as_str().unwrap().to_string())
            .collect();
        assert!(stored.contains(&"barcode".to_string()), "{stored:?}");
        assert!(stored.contains(&"sku:PHO-SCR".to_string()), "{stored:?}");
        assert!(stored.contains(&"invoice".to_string()), "{stored:?}");

        // The cloud actually saw both the plain and the colon-containing name.
        let reserved: Vec<String> = f
            .cloud
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.url.path().starts_with("/api/sequences/"))
            .map(|r| r.url.path().to_string())
            .collect();
        assert!(
            reserved.contains(&"/api/sequences/barcode/reserve".to_string()),
            "{reserved:?}"
        );
        assert!(
            reserved
                .iter()
                .any(|p| p.contains("sku") && p.contains("PHO-SCR")),
            "{reserved:?}"
        );
    }

    #[tokio::test]
    async fn outbox_is_never_acked_before_the_cloud_answers_2xx_and_retry_reuses_the_batch_id() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[4, 5])))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/outbox/ack"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;
        let failing = Mock::given(method("POST"))
            .and(path("/api/sync/push"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({ "message": "busy" })))
            .up_to_n_times(1)
            .mount_as_scoped(&f.cloud)
            .await;

        let err = f.agent.cycle().await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::Server);
        assert!(
            body_of(&f.local, "/api/sync/outbox/ack").await.is_empty(),
            "acked before cloud 2xx"
        );
        drop(failing);

        Mock::given(method("POST"))
            .and(path("/api/sync/push"))
            .respond_with(ok(json!({ "acks": [], "serverSeq": 9 })))
            .mount(&f.cloud)
            .await;
        f.agent.cycle().await.unwrap();

        let pushes = body_of(&f.cloud, "/api/sync/push").await;
        assert_eq!(pushes.len(), 2);
        assert_eq!(pushes[0]["batchId"], pushes[1]["batchId"]);
        assert_eq!(body_of(&f.local, "/api/sync/outbox/ack").await.len(), 1);
    }

    async fn count_of(server: &MockServer, method_name: &str, http_path: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == method_name && r.url.path() == http_path)
            .count()
    }

    /// A linked device with one change waiting and a cloud that accepts it.
    async fn ready_to_push(f: &Fixture) {
        mount_quiet_cloud(f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[4])))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/outbox/ack"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/push"))
            .respond_with(ok(json!({ "acks": [], "serverSeq": 9 })))
            .mount(&f.cloud)
            .await;
    }

    #[tokio::test]
    async fn a_realtime_wake_before_any_full_cycle_runs_the_full_cycle() {
        let f = fixture().await;
        ready_to_push(&f).await;

        f.agent.quick(true, false).await.unwrap();

        assert_eq!(
            count_of(&f.cloud, "POST", "/api/sync/devices/register").await,
            1
        );
        assert_eq!(count_of(&f.cloud, "GET", "/api/sync/pull").await, 1);
    }

    #[tokio::test]
    async fn a_local_write_only_uploads_and_a_cloud_change_only_downloads() {
        let f = fixture().await;
        ready_to_push(&f).await;
        f.agent.cycle().await.unwrap();
        let pulls = count_of(&f.cloud, "GET", "/api/sync/pull").await;
        let pushes = count_of(&f.cloud, "POST", "/api/sync/push").await;
        let reserves = f.cloud.received_requests().await.unwrap().len();

        let report = f.agent.quick(true, false).await.unwrap();
        assert_eq!(report.pushed, 1);
        assert_eq!(
            count_of(&f.cloud, "POST", "/api/sync/push").await,
            pushes + 1
        );
        assert_eq!(count_of(&f.cloud, "GET", "/api/sync/pull").await, pulls);

        f.agent.quick(false, true).await.unwrap();
        assert_eq!(count_of(&f.cloud, "GET", "/api/sync/pull").await, pulls + 1);
        assert_eq!(
            count_of(&f.cloud, "POST", "/api/sync/push").await,
            pushes + 1
        );
        // Neither fast path re-registers, re-syncs the clock or reserves blocks:
        // exactly the push and the pull above reached the cloud.
        assert_eq!(
            f.cloud.received_requests().await.unwrap().len(),
            reserves + 2
        );
    }

    #[tokio::test]
    async fn a_download_tells_the_screens_which_record_types_changed() {
        let f = fixture().await;
        Mock::given(method("GET"))
            .and(path("/api/sync/pull"))
            .respond_with(ok(json!({
                "changes": [
                    { "seq": 6, "resource": "customers", "key": "c1" },
                    { "seq": 7, "resource": "products", "key": "p1" },
                    { "seq": 8, "resource": "customers", "key": "c2" }
                ],
                "nextSeq": 8,
                "hasMore": false
            })))
            .up_to_n_times(1)
            .mount(&f.cloud)
            .await;
        ready_to_push(&f).await;
        Mock::given(method("POST"))
            .and(path("/api/sync/apply"))
            .respond_with(ok(json!({ "applied": 3 })))
            .mount(&f.local)
            .await;

        f.agent.cycle().await.unwrap();

        let applied = f.rec.applied.lock().unwrap().clone();
        assert_eq!(
            applied,
            vec![vec!["customers".to_string(), "products".to_string()]]
        );
        // A quiet download tells nobody anything.
        f.agent.quick(false, true).await.unwrap();
        assert_eq!(f.rec.applied.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn batch_ids_carry_the_outbox_epoch_so_a_recreated_database_never_reuses_one() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        let mut page = outbox(&[4, 5]);
        page["epoch"] = json!("ep_abc");
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(page))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/outbox/ack"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/push"))
            .respond_with(ok(json!({ "acks": [], "serverSeq": 9 })))
            .mount(&f.cloud)
            .await;

        f.agent.cycle().await.unwrap();

        let pushes = body_of(&f.cloud, "/api/sync/push").await;
        assert_eq!(pushes[0]["batchId"], "dev_cloud-ep_abc-4-5");
    }

    /// Refuses (400) any batch holding `prod_6`, accepts everything else.
    struct RefusesProd6;

    impl wiremock::Respond for RefusesProd6 {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let poisoned = body["changes"]
                .as_array()
                .is_some_and(|c| c.iter().any(|r| r["key"] == "prod_6"));
            if poisoned {
                ResponseTemplate::new(400)
                    .set_body_json(json!({ "code": "INVALID", "message": "bad record" }))
            } else {
                ok(json!({ "acks": [], "serverSeq": 9 }))
            }
        }
    }

    #[tokio::test]
    async fn one_refused_change_is_isolated_and_never_blocks_the_rest_of_the_queue() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[4, 5, 6, 7])))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/outbox/ack"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/push"))
            .respond_with(RefusesProd6)
            .mount(&f.cloud)
            .await;

        let report = f.agent.cycle().await.unwrap();

        // Every change but the refused one reached the cloud, in order.
        let accepted: Vec<i64> = body_of(&f.cloud, "/api/sync/push")
            .await
            .into_iter()
            .filter(|b| {
                !b["changes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["key"] == "prod_6")
            })
            .flat_map(|b| b["outboxSeqs"].as_array().unwrap().clone())
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(accepted, vec![4, 5, 7]);
        // The queue fully drained: the refused change was dropped, not retried.
        let acks: Vec<i64> = body_of(&f.local, "/api/sync/outbox/ack")
            .await
            .into_iter()
            .map(|b| b["upToSeq"].as_i64().unwrap())
            .collect();
        assert_eq!(acks, vec![5, 6, 7]);
        assert_eq!(report.pushed, 4);
    }

    #[tokio::test]
    async fn a_database_that_belongs_to_another_shop_never_syncs() {
        let f = fixture().await;
        let mut other = state(5, 3);
        other["tenantId"] = json!("tnt_other");
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(other))
            .mount(&f.local)
            .await;

        let err = f.agent.cycle().await.unwrap_err();
        assert_eq!(err.code, "LOCAL_TENANT_MISMATCH");
        // Nothing reached either side beyond reading the state.
        for server in [&f.local, &f.cloud] {
            let writes = server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|r| r.url.path() != "/api/sync/state")
                .count();
            assert_eq!(writes, 0, "no request may follow a tenant mismatch");
        }
    }

    #[tokio::test]
    async fn a_database_of_the_same_shop_syncs_as_before() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        let mut same = state(5, 3);
        same["tenantId"] = json!("tnt_1");
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(same))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;
        assert!(f.agent.cycle().await.is_ok());
    }

    #[tokio::test]
    async fn cursor_expired_pauses_and_asks_for_bootstrap_without_touching_local_data() {
        let f = fixture().await;
        Mock::given(method("POST"))
            .and(path("/api/sync/devices/register"))
            .respond_with(ok(json!({})))
            .mount(&f.cloud)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/pull"))
            .respond_with(
                ResponseTemplate::new(410)
                    .set_body_json(json!({ "code": "CURSOR_EXPIRED", "message": "too old" })),
            )
            .mount(&f.cloud)
            .await;

        let report = f.agent.cycle().await.unwrap();
        assert!(report.bootstrap_required);
        assert!(f.agent.ctl.is_paused());
        assert_eq!(f.rec.bootstrap.lock().unwrap().len(), 1);
        assert!(
            body_of(&f.local, "/api/sync/apply").await.is_empty(),
            "nothing may be applied or wiped"
        );
    }

    #[tokio::test]
    async fn first_device_of_an_empty_shop_seeds_the_outbox() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(0, 0)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/status"))
            .respond_with(ok(json!({ "serverSeq": 0, "compactedThroughSeq": 0 })))
            .mount(&f.cloud)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/outbox/seed"))
            .respond_with(ok(json!({})))
            .expect(1)
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;

        f.agent.cycle().await.unwrap();
        // `expect(1)` is verified when the mock server drops.
    }

    /// A fresh install, a web-created shop that has never synced (`serverSeq` 0).
    async fn fresh_install_joining_a_never_synced_shop(
        setup_completed: bool,
        snapshot_changes: Value,
        cloud_flags: Value,
    ) -> Fixture {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        let mut empty = state(0, 0);
        empty["localHasData"] = json!(false);
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(empty))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/status"))
            .respond_with(ok(json!({
                "serverSeq": 0, "compactedThroughSeq": 0,
                "setupCompleted": cloud_flags["setupCompleted"].as_bool().unwrap_or(false),
                "sampleDataLoaded": cloud_flags["sampleDataLoaded"].as_bool().unwrap_or(false),
            })))
            .mount(&f.cloud)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/system/setup-status"))
            .respond_with(ok(json!({ "setupCompleted": setup_completed })))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/snapshot"))
            .respond_with(ok(
                json!({ "asOfSeq": 0, "changes": snapshot_changes, "nextPage": null }),
            ))
            .mount(&f.cloud)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/apply"))
            .respond_with(ok(json!({ "applied": 1, "cursor": 0 })))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/outbox/seed"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;
        f
    }

    #[tokio::test]
    async fn fresh_install_downloads_a_web_created_shop_instead_of_uploading_nothing() {
        let f = fresh_install_joining_a_never_synced_shop(
            false,
            json!([{ "resource": "users", "key": "usr_owner" }]),
            json!({}),
        )
        .await;

        f.agent.cycle().await.unwrap();

        let applies = body_of(&f.local, "/api/sync/apply").await;
        assert_eq!(applies[0]["mode"], "bootstrap");
        assert_eq!(applies[0]["changes"][0]["key"], "usr_owner");
        assert!(body_of(&f.local, "/api/sync/outbox/seed").await.is_empty());
    }

    #[tokio::test]
    async fn an_install_that_is_already_set_up_still_seeds_a_never_synced_shop() {
        let f = fresh_install_joining_a_never_synced_shop(
            true,
            json!([{ "resource": "users", "key": "usr_owner" }]),
            json!({}),
        )
        .await;

        f.agent.cycle().await.unwrap();

        // The follow-up pull may post an empty incremental page; never a bootstrap.
        assert!(body_of(&f.local, "/api/sync/apply")
            .await
            .iter()
            .all(|b| b["mode"] != "bootstrap"));
        assert_eq!(body_of(&f.local, "/api/sync/outbox/seed").await.len(), 1);
    }

    #[tokio::test]
    async fn a_never_synced_shop_with_nothing_in_it_is_seeded_not_downloaded() {
        let f = fresh_install_joining_a_never_synced_shop(false, json!([]), json!({})).await;

        f.agent.cycle().await.unwrap();

        // The follow-up pull may post an empty incremental page; never a bootstrap.
        assert!(body_of(&f.local, "/api/sync/apply")
            .await
            .iter()
            .all(|b| b["mode"] != "bootstrap"));
        assert_eq!(body_of(&f.local, "/api/sync/outbox/seed").await.len(), 1);
    }

    #[tokio::test]
    async fn the_final_download_page_carries_whether_the_cloud_shop_is_set_up() {
        for (setup, sample) in [(true, true), (false, false)] {
            let f = fresh_install_joining_a_never_synced_shop(
                false,
                json!([{ "resource": "users", "key": "usr_owner" }]),
                json!({ "setupCompleted": setup, "sampleDataLoaded": sample }),
            )
            .await;

            f.agent.cycle().await.unwrap();

            let applies = body_of(&f.local, "/api/sync/apply").await;
            let last = applies
                .iter()
                .find(|b| b["mode"] == "bootstrap")
                .expect("a bootstrap page");
            assert_eq!(last["setupCompleted"], setup);
            assert_eq!(last["sampleDataLoaded"], sample);
        }
    }

    /// A linked device mid-life (cursor > 0), so no first sync runs.
    async fn linked_device_with_setup(local_setup: Value, cloud_setup: bool) -> Fixture {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/system/setup-status"))
            .respond_with(ok(local_setup))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/status"))
            .respond_with(ok(json!({ "setupCompleted": cloud_setup })))
            .mount(&f.cloud)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/setup-complete"))
            .respond_with(ok(json!({ "setupCompleted": true, "changed": true })))
            .mount(&f.cloud)
            .await;
        f
    }

    #[tokio::test]
    async fn finishing_setup_here_tells_the_cloud_exactly_once() {
        let f = linked_device_with_setup(
            json!({ "setupCompleted": true, "sampleDataLoaded": true }),
            false,
        )
        .await;

        f.agent.cycle().await.unwrap();
        f.agent.cycle().await.unwrap();

        let told = body_of(&f.cloud, "/api/sync/setup-complete").await;
        assert_eq!(told.len(), 1, "the second cycle has nothing left to tell");
        assert_eq!(told[0]["sampleDataLoaded"], true);
    }

    #[tokio::test]
    async fn a_cloud_shop_that_is_already_set_up_is_not_told_again() {
        let f = linked_device_with_setup(
            json!({ "setupCompleted": true, "sampleDataLoaded": false }),
            true,
        )
        .await;

        f.agent.cycle().await.unwrap();

        assert!(body_of(&f.cloud, "/api/sync/setup-complete")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_device_still_at_the_wizard_does_not_tell_the_cloud_anything() {
        let f = linked_device_with_setup(
            json!({ "setupCompleted": false, "sampleDataLoaded": false }),
            false,
        )
        .await;

        f.agent.cycle().await.unwrap();

        assert!(body_of(&f.cloud, "/api/sync/setup-complete")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn failing_to_tell_the_cloud_never_fails_the_sync_and_is_retried_later() {
        let f = linked_device_with_setup(
            json!({ "setupCompleted": true, "sampleDataLoaded": false }),
            false,
        )
        .await;
        // Replace the happy mock with a server error for the first attempt.
        f.cloud.reset().await;
        mount_quiet_cloud(&f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/status"))
            .respond_with(ok(json!({ "setupCompleted": false })))
            .mount(&f.cloud)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/setup-complete"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&f.cloud)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/setup-complete"))
            .respond_with(ok(json!({ "setupCompleted": true, "changed": true })))
            .mount(&f.cloud)
            .await;

        f.agent
            .cycle()
            .await
            .expect("the sync itself still succeeds");
        f.agent.cycle().await.unwrap();

        assert_eq!(body_of(&f.cloud, "/api/sync/setup-complete").await.len(), 2);
    }

    #[tokio::test]
    async fn device_with_data_joining_a_shop_with_data_waits_for_confirmation() {
        let f = fixture().await;
        Mock::given(method("POST"))
            .and(path("/api/sync/devices/register"))
            .respond_with(ok(json!({})))
            .mount(&f.cloud)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(0, 0)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/status"))
            .respond_with(ok(json!({ "serverSeq": 12 })))
            .mount(&f.cloud)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;

        let report = f.agent.cycle().await.unwrap();
        assert!(report.bootstrap_required);
        assert!(f.agent.ctl.snapshot().bootstrap_required);
        assert!(body_of(&f.local, "/api/sync/apply").await.is_empty());
    }

    #[tokio::test]
    async fn empty_device_joining_a_shop_bootstraps_from_the_snapshot_pages() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        let mut empty = state(0, 0);
        empty["localHasData"] = json!(false);
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(empty))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/status"))
            .respond_with(ok(json!({ "serverSeq": 12 })))
            .mount(&f.cloud)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;
        Mock::given(method("GET")).and(path("/api/sync/snapshot")).and(query_param("page", "p2"))
            .respond_with(ok(json!({ "asOfSeq": 99, "changes": [ { "resource": "customers", "key": "cust_2" } ], "nextPage": null }))).mount(&f.cloud).await;
        Mock::given(method("GET")).and(path("/api/sync/snapshot"))
            .respond_with(ok(json!({ "asOfSeq": 12, "changes": [ { "resource": "products", "key": "prod_1" } ], "nextPage": "p2" }))).mount(&f.cloud).await;
        Mock::given(method("POST"))
            .and(path("/api/sync/apply"))
            .respond_with(ok(json!({ "applied": 1, "cursor": 12 })))
            .mount(&f.local)
            .await;

        f.agent.cycle().await.unwrap();
        let applies = body_of(&f.local, "/api/sync/apply").await;
        // Two snapshot pages + the follow-up pull that advances nothing.
        assert_eq!(applies[0]["mode"], "bootstrap");
        assert_eq!(applies[0]["advanceCursorTo"], Value::Null);
        assert_eq!(applies[0]["changes"][0]["seq"], 0);
        assert_eq!(applies[1]["mode"], "incremental");
        assert_eq!(
            applies[1]["advanceCursorTo"], 12,
            "cursor is the FIRST page's asOfSeq"
        );
    }

    #[tokio::test]
    async fn a_401_refreshes_the_token_once_and_retries() {
        let f = fixture().await;
        mount_quiet_cloud(&f).await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;
        // The first pull is rejected, the retry (with the refreshed token) succeeds.
        Mock::given(method("GET"))
            .and(path("/api/sync/pull"))
            .and(wiremock::matchers::header("authorization", "Bearer t1"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(1)
            .mount(&f.cloud)
            .await;

        f.agent.cycle().await.unwrap();
        assert!(
            f.forced.lock().unwrap().contains(&true),
            "no forced refresh happened"
        );
    }

    #[tokio::test]
    async fn revoked_device_surfaces_as_revoked() {
        let f = fixture().await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/devices/register"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({ "code": "DEVICE_REVOKED", "message": "revoked" })),
            )
            .mount(&f.cloud)
            .await;
        assert_eq!(f.agent.cycle().await.unwrap_err().kind, ErrorKind::Revoked);
    }

    #[tokio::test]
    async fn clock_offset_from_server_time_reaches_the_backend() {
        let f = fixture().await;
        Mock::given(method("POST"))
            .and(path("/api/sync/devices/register"))
            .respond_with(ok(json!({})).insert_header("x-server-time", "2999-01-01T00:00:00.000Z"))
            .mount(&f.cloud)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/state"))
            .respond_with(ok(state(5, 3)))
            .mount(&f.local)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/state"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/outbox"))
            .respond_with(ok(outbox(&[])))
            .mount(&f.local)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sync/pull"))
            .respond_with(ok(json!({ "changes": [], "nextSeq": 5, "hasMore": false })))
            .mount(&f.cloud)
            .await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex(r"^/api/sequences/"))
            .respond_with(ok(json!({})))
            .mount(&f.cloud)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/sync/blocks"))
            .respond_with(ok(json!({})))
            .mount(&f.local)
            .await;

        f.agent.cycle().await.unwrap();
        let posted = body_of(&f.local, "/api/sync/state").await;
        assert!(posted[0]["clockOffsetMs"].as_i64().unwrap() > 1_000_000_000);
    }

    // ---- pure helpers -----------------------------------------------------

    #[test]
    fn backoff_follows_the_schedule_and_caps_at_five_minutes() {
        let mut b = Backoff::default();
        let secs: Vec<u64> = (0..7).map(|_| b.next_delay(0.0).as_secs()).collect();
        assert_eq!(secs, vec![2, 5, 15, 60, 300, 300, 300]);
        b.reset();
        assert_eq!(b.next_delay(0.0).as_secs(), 2);
        assert_eq!(
            Backoff::default().next_delay(0.2),
            Duration::from_secs_f64(2.4)
        );
    }

    #[test]
    fn clock_median_uses_the_last_five_samples() {
        let c = ClockSampler::default();
        assert_eq!(c.median(), None);
        for v in [1000, 10, 20, 30, 40, 50] {
            c.record(v);
        }
        // 1000 fell out of the window: median of 10,20,30,40,50.
        assert_eq!(c.median(), Some(30));
        let even = ClockSampler::default();
        even.record(10);
        even.record(20);
        assert_eq!(even.median(), Some(15));
    }

    #[test]
    fn blocks_are_due_when_missing_sixty_percent_used_or_half_on_the_periodic_check() {
        let have = vec![
            BlockInfo {
                name: "invoice".into(),
                remaining: 30,
                block_size: 100,
            },
            BlockInfo {
                name: "creditnote".into(),
                remaining: 45,
                block_size: 100,
            },
            BlockInfo {
                name: "repair".into(),
                remaining: 90,
                block_size: 100,
            },
        ];
        fn names(v: Vec<(&'static str, u64)>) -> Vec<&'static str> {
            v.into_iter().map(|(n, _)| n).collect()
        }
        // Every cycle: invoice is >= 60% used; printJob/purchase/barcode are missing.
        assert_eq!(
            names(blocks_due(&have, BLOCK_NAMES, false)),
            vec!["invoice", "printJob", "purchase", "barcode"]
        );
        // Periodic check also tops up creditNote (<= 50% left), not repair (90%).
        assert_eq!(
            names(blocks_due(&have, BLOCK_NAMES, true)),
            vec!["invoice", "creditNote", "printJob", "purchase", "barcode"]
        );
    }

    #[test]
    fn sku_blocks_are_due_the_same_way_but_keyed_by_prefix() {
        let have = vec![
            BlockInfo {
                name: "sku:PHO-SCR".into(),
                remaining: 15,
                block_size: 50,
            },
            BlockInfo {
                name: "sku:ELE-CAB".into(),
                remaining: 22,
                block_size: 50,
            },
        ];
        let prefixes = vec![
            "PHO-SCR".to_string(),
            "ELE-CAB".to_string(),
            "TVX-BAT".to_string(),
        ];
        // PHO-SCR is >= 60% used; TVX-BAT was never reserved; ELE-CAB isn't due yet.
        assert_eq!(
            skus_due(&have, &prefixes, false),
            vec!["PHO-SCR".to_string(), "TVX-BAT".to_string()]
        );
        // Periodic also tops up ELE-CAB (<= 50% left).
        assert_eq!(
            skus_due(&have, &prefixes, true),
            vec![
                "PHO-SCR".to_string(),
                "ELE-CAB".to_string(),
                "TVX-BAT".to_string()
            ]
        );
    }
}
