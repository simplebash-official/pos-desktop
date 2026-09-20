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

use super::http::{BlockInfo, CloudSyncApi, ErrorKind, LocalApi, LocalState, SyncError};
use super::state::{SyncControl, SyncPhase};

pub const PUSH_PAGE: usize = 200;
pub const PULL_PAGE: usize = 200;
/// Pause between cycles when everything is quiet.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(15);
/// Full number-block top-up check.
pub const BLOCK_CHECK_INTERVAL: Duration = Duration::from_secs(20 * 60);
/// Only push a new clock offset to the backend when it moved this much.
pub const CLOCK_RESYNC_THRESHOLD_MS: i64 = 500;

/// Sequences the cloud can reserve blocks for, with the block size to request.
pub const BLOCK_NAMES: &[(&str, u64)] = &[
    ("invoice", 100),
    ("creditNote", 100),
    ("repair", 100),
    ("printJob", 100),
    ("purchase", 100),
];

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
    pub bootstrap_required: bool,
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
        });

        let mut state = self.local.state().await?;
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

        report.pushed = self.push_all(&state).await?;
        match self.pull_all(state.cloud_cursor).await {
            Ok(n) => report.pulled = n,
            Err(e) if e.kind == ErrorKind::CursorExpired => {
                self.ctl.require_bootstrap(
                    "the cloud history no longer reaches this device; a full re-download is needed",
                );
                report.bootstrap_required = true;
                return Ok(report);
            }
            Err(e) => return Err(e),
        }
        report.blocks_reserved = self.top_up_blocks(&state).await?;

        let fresh = self.local.state().await.unwrap_or(state);
        self.ctl.update(|s| {
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
        LogEvent::shell("sync", "cycle.done")
            .level(Level::Debug)
            .data(json!({
                "pushed": report.pushed,
                "pulled": report.pulled,
                "blocks": report.blocks_reserved,
            }))
            .emit();
        Ok(report)
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
            });
            self.local.apply(&body).await?;
            first = false;
            if last {
                break;
            }
            page = snap.next_page;
        }
        self.ctl.clear_bootstrap();
        Ok(())
    }

    async fn push_all(&self, state: &LocalState) -> Result<usize, SyncError> {
        let mut after = state.last_pushed_outbox_seq;
        let mut total = 0;
        loop {
            let page = self.local.outbox(after, PUSH_PAGE).await?;
            let (Some(first), Some(last)) = (page.items.first(), page.items.last()) else {
                break;
            };
            let (first_seq, last_seq) = (first.seq, last.seq);
            let count = page.items.len();
            // Deterministic per outbox range: a retry after a failure re-sends
            // the very same batch id, which the cloud dedups.
            let body = json!({
                "deviceId": self.device_id,
                "batchId": format!("{}-{first_seq}-{last_seq}", self.device_id),
                "baseSeq": state.cloud_cursor,
                "changes": page.items.iter().map(|i| &i.record).collect::<Vec<_>>(),
                "outboxSeqs": page.items.iter().map(|i| i.seq).collect::<Vec<_>>(),
            });
            let result = self.cloud.push(&body).await?;
            // Only now that the cloud confirmed does the outbox shrink.
            self.local.ack(last_seq).await?;
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
                    .data(json!({ "count": refused, "batch": count }))
                    .emit();
            }
            after = last_seq;
            total += count;
            if count < PUSH_PAGE {
                break;
            }
        }
        Ok(total)
    }

    async fn pull_all(&self, cursor: i64) -> Result<usize, SyncError> {
        let mut cursor = cursor;
        let mut pulled = 0;
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
            cursor = advance;
            if !page.has_more {
                break;
            }
        }
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
                    reserved += 1;
                }
                Err(e)
                    if matches!(
                        e.kind,
                        ErrorKind::Offline | ErrorKind::Revoked | ErrorKind::Unauthorized
                    ) =>
                {
                    return Err(e)
                }
                Err(e) => {
                    LogEvent::shell("sync", "blocks.reserve_failed")
                        .level(Level::Warn)
                        .data(json!({ "name": name, "code": e.code }))
                        .emit();
                }
            }
        }
        if periodic {
            *self.last_block_check.lock().unwrap() = Some(Instant::now());
        }
        Ok(reserved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::state::Recorder;
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
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex(r"^/api/sequences/[A-Za-z]+/reserve$"))
            .respond_with(ok(json!({ "prefix": "X-", "padding": 6, "start": 1, "end": 100, "expiresAt": "2030-01-01T00:00:00Z" })))
            .mount(&f.cloud)
            .await;
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
        // Every cycle: invoice is >= 60% used; printJob/purchase are missing.
        assert_eq!(
            names(blocks_due(&have, BLOCK_NAMES, false)),
            vec!["invoice", "printJob", "purchase"]
        );
        // Periodic check also tops up creditNote (<= 50% left), not repair (90%).
        assert_eq!(
            names(blocks_due(&have, BLOCK_NAMES, true)),
            vec!["invoice", "creditNote", "printJob", "purchase"]
        );
    }
}
