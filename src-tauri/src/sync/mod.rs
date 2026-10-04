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
use state::{HistoryKind, StatusSink, SyncControl, SyncPhase, SyncStep};

/// Local sync failures at start-up (backend still booting) are not shown as
/// errors until this many attempts in a row have failed.
const LOCAL_GRACE_ATTEMPTS: u32 = 3;
/// How long to wait before re-checking whether the device got linked.
const UNLINKED_POLL: Duration = Duration::from_secs(10);
/// Pause after a cycle that moved data, so a busy shop syncs promptly.
const BUSY_INTERVAL: Duration = Duration::from_secs(2);
/// Longest wait before retrying a failure of this computer's own backend.
const LOCAL_RETRY_CAP: Duration = Duration::from_secs(15);

pub struct SyncManager {
    ctl: Arc<SyncControl>,
    /// Shared (pooled) client for the cloud calls.
    http: reqwest::Client,
    /// Client for this computer's own backend: see `local_client`.
    local_http: reqwest::Client,
    data_dir: PathBuf,
}

/// HTTP client for the local backend on loopback. It never keeps a connection
/// between calls and never uses a system proxy, so every call reaches whatever
/// owns the port *now*. A kept-alive connection opened in the first second after
/// launch (before the backend has bound its port) could otherwise stay glued to
/// another program on the same port, such as a Docker backend, and make every
/// later cycle fail even though the right service is up.
fn local_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .pool_max_idle_per_host(0)
        .build()
        .expect("static local client configuration is valid")
}

/// What the Sync screen says about a failure, in plain words. The technical
/// code stays in the activity log (`sync/cycle.failed`).
fn friendly_error(e: &SyncError) -> String {
    match e.kind {
        ErrorKind::Local => {
            "This computer's data service did not answer correctly. Trying again…".to_string()
        }
        ErrorKind::Unauthorized => {
            "Your cloud sign-in has expired. Link this computer again under Cloud Account."
                .to_string()
        }
        _ => e.to_string(),
    }
}

/// Local problems (the backend is still starting) heal in seconds, so they
/// never wait out the long cloud backoff.
fn retry_delay(kind: ErrorKind, backoff: Duration) -> Duration {
    if kind == ErrorKind::Local {
        backoff.min(LOCAL_RETRY_CAP)
    } else {
        backoff
    }
}

impl SyncManager {
    pub fn new(ctl: Arc<SyncControl>, data_dir: PathBuf) -> Self {
        Self {
            ctl,
            http: reqwest::Client::new(),
            local_http: local_client(),
            data_dir,
        }
    }

    pub fn wake(&self) {
        self.ctl.wake_now();
    }

    fn local_api(&self) -> LocalApi {
        let dir = self.data_dir.clone();
        let secret: SecretFn = Arc::new(move || read_jwt_secret(&dir));
        LocalApi::new(
            self.local_http.clone(),
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
    let mut was_offline = false;

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

        let result = agent.cycle().await;
        // A cycle that stopped early (bootstrap needed, error) must not leave
        // the UI showing "uploading".
        ctl.update(|s| {
            s.step = SyncStep::Idle;
            s.progress = None;
        });
        match result {
            Ok(report) => {
                backoff.reset();
                failures = 0;
                if was_offline {
                    was_offline = false;
                    ctl.push_history(HistoryKind::Online, 0, 0, None);
                }
                let pending = ctl.snapshot().pending_out;
                let busy = report.pushed >= PUSH_PAGE
                    || report.pulled > 0
                    || report.pushed > 0
                    || pending > 0;
                let delay = if busy { BUSY_INTERVAL } else { IDLE_INTERVAL };
                ctl.set_next_retry(Some(delay));
                ctl.wait(delay).await;
            }
            Err(e) => {
                failures += 1;
                LogEvent::shell("sync", "cycle.failed")
                    .level(Level::Warn)
                    .data(json!({ "code": e.code, "kind": e.kind, "status": e.status, "attempt": failures }))
                    .emit();
                match e.kind {
                    ErrorKind::Revoked => {
                        ctl.push_history(HistoryKind::Error, 0, 0, Some(e.message.clone()));
                        ctl.update(|s| s.last_error = Some(e.message.clone()));
                        ctl.pause();
                        continue;
                    }
                    ErrorKind::Offline => {
                        ctl.update(|s| {
                            s.state = SyncPhase::Offline;
                            s.last_error = None;
                        });
                        if !was_offline {
                            was_offline = true;
                            ctl.push_history(HistoryKind::Offline, 0, 0, None);
                        }
                        let jitter: f64 = rand::thread_rng().gen_range(-0.2..0.2);
                        let delay = backoff.next_delay(jitter);
                        ctl.set_next_retry(Some(delay));
                        let start = std::time::Instant::now();
                        while start.elapsed() < delay {
                            let remaining = delay.saturating_sub(start.elapsed());
                            let tick = Duration::from_secs(3).min(remaining);
                            let woken = ctl.wait(tick).await;
                            if woken || agent.cloud.check_reachability().await {
                                LogEvent::shell("sync", "internet.restored")
                                    .level(Level::Info)
                                    .emit();
                                backoff.reset();
                                failures = 0;
                                break;
                            }
                        }
                        continue;
                    }
                    ErrorKind::Local if failures <= LOCAL_GRACE_ATTEMPTS => {}
                    _ => {
                        let message = friendly_error(&e);
                        ctl.push_history(HistoryKind::Error, 0, 0, Some(message.clone()));
                        ctl.update(|s| {
                            s.state = SyncPhase::Error;
                            s.last_error = Some(message);
                        });
                    }
                }
                let jitter: f64 = rand::thread_rng().gen_range(-0.2..0.2);
                let delay = retry_delay(e.kind, backoff.next_delay(jitter));
                ctl.set_next_retry(Some(delay));
                ctl.wait(delay).await;
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

/// The individual changes still waiting to upload (names and times only), for
/// the per-module list on the Sync screen. Capped by the backend.
#[tauri::command]
pub async fn sync_list_pending(
    state: State<'_, SyncManager>,
    resource: Option<String>,
    limit: Option<usize>,
) -> Result<Value, SyncError> {
    let call = CommandLog::start(
        "sync_list_pending",
        json!({ "resource": resource, "limit": limit }),
    );
    call.finish(
        state
            .local_api()
            .pending(resource.as_deref(), limit.unwrap_or(50).min(200))
            .await,
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn err(kind: ErrorKind) -> SyncError {
        SyncError::new(kind, "CODE", "boom", 401)
    }

    #[test]
    fn local_and_expired_sign_in_errors_read_in_plain_words() {
        assert!(friendly_error(&err(ErrorKind::Local)).contains("data service"));
        assert!(!friendly_error(&err(ErrorKind::Local)).contains("log in"));
        assert!(friendly_error(&err(ErrorKind::Unauthorized)).contains("Cloud Account"));
        assert_eq!(friendly_error(&err(ErrorKind::Server)), "CODE: boom");
    }

    #[test]
    fn only_local_failures_cap_the_retry_delay() {
        let long = Duration::from_secs(300);
        assert_eq!(retry_delay(ErrorKind::Local, long), LOCAL_RETRY_CAP);
        assert_eq!(
            retry_delay(ErrorKind::Local, Duration::from_secs(2)),
            Duration::from_secs(2)
        );
        assert_eq!(retry_delay(ErrorKind::Server, long), long);
    }

    /// The local client must open a fresh connection per call, so it can never
    /// stay stuck on another program that answered first on the same port.
    #[tokio::test]
    async fn the_local_client_does_not_reuse_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    // Keep-alive on purpose: a pooling client would reuse this socket.
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                        )
                        .await;
                    let mut rest = [0u8; 1];
                    let _ = sock.read(&mut rest).await;
                });
            }
        });

        let client = local_client();
        let url = format!("http://{addr}/");
        for _ in 0..2 {
            let body = client.get(&url).send().await.unwrap().text().await.unwrap();
            assert_eq!(body, "ok");
        }
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
