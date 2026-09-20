// Desktop sync agent. Runs in the shell, links the LOCAL backend (SQLite, the
// shop's offline database) with the POS cloud for a linked, cloud-enabled
// install. It never opens the database: everything goes through HTTP, so the
// backend keeps owning every business rule.
//
//   local backend  <-- service token (HS256, scope "sync", <= 5 min) -->  agent
//   agent          <-- device access token (refreshed via identity) --->  POS cloud
//
// The agent starts only when cloud sync is enabled (a cloud URL exists); it
// then idles until this device is linked. The webview only ever sees counters
// (`sync://status`), never record bodies or tokens.
//
// ---------------------------------------------------------------------------
// LOCAL ENDPOINT CONTRACT (implemented by the backend, `TENANT_MODE=single`).
// All calls carry `Authorization: Bearer <service token>`; JSON is camelCase
// and may be wrapped in the usual `{success,data}` envelope.
//
//   GET  /api/sync/state
//        -> { deviceId, linked, captureEnabled, cloudCursor, lastPushedOutboxSeq,
//             clockOffsetMs, pendingOut, conflictsOpen, localHasData,
//             numberBlocks: [{ name, remaining, blockSize }] }
//        (every field optional; missing numbers read as 0 / false)
//   POST /api/sync/state          { clockOffsetMs }        median server-local ms
//   POST /api/sync/enable         { tenantId, deviceId }   turn capture on (idempotent)
//   POST /api/sync/outbox/seed                             enqueue every existing row
//   GET  /api/sync/outbox?after=<seq>&limit=200
//        -> { items: [{ seq, record: ChangeRecord }], lastSeq }
//   POST /api/sync/outbox/ack     { upToSeq }              drop items <= upToSeq
//   POST /api/sync/apply          { mode: "incremental"|"bootstrap",
//                                   changes: [PulledChange], advanceCursorTo? }
//        -> { applied, duplicates, conflicts, cursor }
//        Snapshot pages arrive as PulledChange with seq 0; the first bootstrap
//        page replaces the synced tables, the last one carries advanceCursorTo.
//   POST /api/sync/blocks         { name, prefix, padding, start, end, expiresAt }
//   GET  /api/sync/conflicts      -> { items: [...] } (or a bare array)
//   POST /api/sync/conflicts/{key}/resolve   { resolution }
//
// CLOUD CALLS (device access token; sync base = cloud.json `syncApiUrl`, or
// build-time `CLOUD_SYNC_API_URL`, else the identity `apiUrl`):
//   GET /api/sync/status | POST /api/sync/devices/register | POST /api/sync/push
//   GET /api/sync/pull?since=&limit= | GET /api/sync/snapshot?page=
//   POST /api/sequences/{name}/reserve
// ---------------------------------------------------------------------------

mod agent;
mod http;
mod state;
mod token;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};

use crate::cloud::{CloudState, SyncSession};
use crate::logging::{CommandLog, Level, LogEvent};
use crate::orchestrator::{read_jwt_secret, BACKEND_PORT, LOOPBACK};

use agent::{Agent, Backoff, ClockSampler, IDLE_INTERVAL, PUSH_PAGE};
use http::{ClockFn, CloudSyncApi, ErrorKind, LocalApi, SecretFn, SyncError, TokenFn};
pub use state::SyncStatus;
use state::{StatusSink, SyncControl, SyncPhase};

/// Local sync failures at start-up (backend still booting) are not shown as
/// errors until this many attempts in a row have failed.
const LOCAL_GRACE_ATTEMPTS: u32 = 3;
/// How long to wait before re-checking whether the device got linked.
const UNLINKED_POLL: Duration = Duration::from_secs(10);
/// Pause after a cycle that moved data, so a busy shop syncs promptly.
const BUSY_INTERVAL: Duration = Duration::from_secs(2);

pub struct SyncManager {
    ctl: Arc<SyncControl>,
    http: reqwest::Client,
    data_dir: PathBuf,
}

impl SyncManager {
    pub fn new(ctl: Arc<SyncControl>, data_dir: PathBuf) -> Self {
        Self {
            ctl,
            http: reqwest::Client::new(),
            data_dir,
        }
    }

    fn local_api(&self) -> LocalApi {
        let dir = self.data_dir.clone();
        let secret: SecretFn = Arc::new(move || read_jwt_secret(&dir));
        LocalApi::new(
            self.http.clone(),
            format!("http://{LOOPBACK}:{BACKEND_PORT}"),
            secret,
        )
    }
}

struct TauriSink(AppHandle);

impl StatusSink for TauriSink {
    fn status(&self, status: &SyncStatus) {
        let _ = self.0.emit("sync://status", status);
    }

    fn bootstrap_required(&self, reason: &str) {
        let _ = self
            .0
            .emit("sync://bootstrap-required", json!({ "reason": reason }));
    }
}

/// Builds the manager (always managed so the commands resolve) and starts the
/// agent when cloud sync is enabled in this build.
pub fn init(app: &tauri::App, handle: &AppHandle, data_dir: PathBuf) {
    let ctl = SyncControl::new(Arc::new(TauriSink(handle.clone())));
    app.manage(SyncManager::new(ctl, data_dir));
    if handle.state::<CloudState>().view().enabled {
        let handle = handle.clone();
        tauri::async_runtime::spawn(supervise(handle));
    }
}

fn cloud_error_to_sync(e: crate::cloud::CloudError) -> SyncError {
    let kind = if e.code == "NETWORK_ERROR" {
        ErrorKind::Offline
    } else {
        ErrorKind::Unauthorized
    };
    SyncError::new(kind, &e.code, e.message, e.status)
}

fn build_agent(handle: &AppHandle, session: &SyncSession) -> Agent {
    let mgr = handle.state::<SyncManager>();
    let clock = Arc::new(ClockSampler::default());
    let token_handle = handle.clone();
    let token: TokenFn = Arc::new(move |force| {
        let h = token_handle.clone();
        Box::pin(async move {
            h.state::<CloudState>()
                .access_token(force)
                .await
                .map_err(cloud_error_to_sync)
        })
    });
    let sampler = clock.clone();
    let on_clock: ClockFn = Arc::new(move |ms| sampler.record(ms));
    Agent::new(
        mgr.local_api(),
        CloudSyncApi::new(mgr.http.clone(), session.sync_base.clone(), token, on_clock),
        mgr.ctl.clone(),
        clock,
        session.tenant_id.clone(),
        session.device_id.clone(),
        sysinfo::System::host_name().unwrap_or_else(|| "SimpleBash POS".to_string()),
        handle.package_info().version.to_string(),
    )
}

/// The agent loop: link check, one cycle, then wait (idle interval, busy
/// interval, or the backoff schedule after a failure).
async fn supervise(handle: AppHandle) {
    let ctl = handle.state::<SyncManager>().ctl.clone();
    let mut backoff = Backoff::default();
    let mut failures: u32 = 0;
    let mut current: Option<(SyncSession, Agent)> = None;

    loop {
        let session = match handle.state::<CloudState>().sync_session() {
            Ok(session) => session,
            Err(_) => {
                current = None;
                ctl.update(|s| {
                    s.linked = false;
                    s.state = SyncPhase::Idle;
                });
                ctl.wait(UNLINKED_POLL).await;
                continue;
            }
        };
        if current.as_ref().map(|(s, _)| s != &session).unwrap_or(true) {
            current = Some((session.clone(), build_agent(&handle, &session)));
        }
        let agent = &current.as_ref().expect("agent was just built").1;

        // A user-confirmed re-download runs even while paused.
        if ctl.take_bootstrap_confirmed() {
            match agent.bootstrap().await {
                Ok(()) => {
                    ctl.clear_bootstrap();
                    ctl.resume();
                }
                Err(e) => {
                    ctl.update(|s| s.last_error = Some(e.to_string()));
                    LogEvent::shell("sync", "bootstrap.failed")
                        .level(Level::Warn)
                        .data(json!({ "code": e.code }))
                        .emit();
                }
            }
            continue;
        }
        if ctl.is_paused() {
            ctl.wait(IDLE_INTERVAL).await;
            continue;
        }

        match agent.cycle().await {
            Ok(report) => {
                backoff.reset();
                failures = 0;
                let busy = report.pushed >= PUSH_PAGE || report.pulled > 0 || report.pushed > 0;
                ctl.wait(if busy { BUSY_INTERVAL } else { IDLE_INTERVAL })
                    .await;
            }
            Err(e) => {
                failures += 1;
                LogEvent::shell("sync", "cycle.failed")
                    .level(Level::Warn)
                    .data(json!({ "code": e.code, "kind": e.kind, "status": e.status, "attempt": failures }))
                    .emit();
                match e.kind {
                    ErrorKind::Revoked => {
                        ctl.update(|s| s.last_error = Some(e.message.clone()));
                        ctl.pause();
                        continue;
                    }
                    ErrorKind::Offline => ctl.update(|s| {
                        s.state = SyncPhase::Offline;
                        s.last_error = None;
                    }),
                    ErrorKind::Local if failures <= LOCAL_GRACE_ATTEMPTS => {}
                    _ => ctl.update(|s| {
                        s.state = SyncPhase::Error;
                        s.last_error = Some(e.to_string());
                    }),
                }
                let jitter: f64 = rand::thread_rng().gen_range(-0.2..0.2);
                ctl.wait(backoff.next_delay(jitter)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Commands. Arguments are logged only when they carry no secret; nothing here
// ever receives or returns a record body except the conflict list, which is
// the user's own data shown back to them.
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sync_get_status(state: State<'_, SyncManager>) -> Result<SyncStatus, SyncError> {
    let call = CommandLog::start("sync_get_status", json!({}));
    call.finish(Ok::<_, SyncError>(state.ctl.snapshot()))
}

#[tauri::command]
pub async fn sync_now(state: State<'_, SyncManager>) -> Result<SyncStatus, SyncError> {
    let call = CommandLog::start("sync_now", json!({}));
    state.ctl.wake_now();
    call.finish(Ok::<_, SyncError>(state.ctl.snapshot()))
}

#[tauri::command]
pub async fn sync_pause(state: State<'_, SyncManager>) -> Result<SyncStatus, SyncError> {
    let call = CommandLog::start("sync_pause", json!({}));
    state.ctl.pause();
    call.finish(Ok::<_, SyncError>(state.ctl.snapshot()))
}

#[tauri::command]
pub async fn sync_resume(state: State<'_, SyncManager>) -> Result<SyncStatus, SyncError> {
    let call = CommandLog::start("sync_resume", json!({}));
    state.ctl.resume();
    call.finish(Ok::<_, SyncError>(state.ctl.snapshot()))
}

#[tauri::command]
pub async fn sync_list_conflicts(state: State<'_, SyncManager>) -> Result<Value, SyncError> {
    let call = CommandLog::start("sync_list_conflicts", json!({}));
    call.finish(state.local_api().conflicts().await)
}

#[tauri::command]
pub async fn sync_resolve_conflict(
    state: State<'_, SyncManager>,
    key: String,
    resolution: String,
) -> Result<Value, SyncError> {
    let call = CommandLog::start(
        "sync_resolve_conflict",
        json!({ "key": key, "resolution": resolution }),
    );
    call.finish(state.local_api().resolve_conflict(&key, &resolution).await)
}

/// Confirms a full re-download of the cloud data over this device's data. The
/// agent performs it on its next loop turn; nothing is wiped without `confirm`.
#[tauri::command]
pub async fn sync_bootstrap(
    state: State<'_, SyncManager>,
    confirm: bool,
) -> Result<SyncStatus, SyncError> {
    let call = CommandLog::start("sync_bootstrap", json!({ "confirm": confirm }));
    let result = if confirm {
        state.ctl.confirm_bootstrap();
        Ok(state.ctl.snapshot())
    } else {
        Err(SyncError::new(
            ErrorKind::Invalid,
            "CONFIRMATION_REQUIRED",
            "replacing the local data needs explicit confirmation",
            0,
        ))
    };
    call.finish(result)
}
