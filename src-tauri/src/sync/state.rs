// What the webview sees of the sync agent, plus the pause / wake-up switches
// the commands flip. The agent never touches Tauri directly: it reports through
// a `StatusSink`, so the whole loop is testable without a running app.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncPhase {
    Idle,
    Syncing,
    Offline,
    Error,
    Paused,
}

/// What the agent is doing right now, in finer detail than `SyncPhase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncStep {
    Idle,
    Preparing,
    Uploading,
    Downloading,
    Finishing,
}

/// Progress of the current step. `total` is 0 when it is not known up front
/// (downloads), so the UI shows a count instead of a bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SyncProgress {
    pub done: u64,
    pub total: u64,
}

/// Waiting / conflicting changes for one resource, straight from the local backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleStatus {
    pub resource: String,
    pub pending: u64,
    pub conflicts: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CycleSummary {
    pub at: String,
    pub sent: u64,
    pub received: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HistoryKind {
    Synced,
    Offline,
    Online,
    Error,
    Paused,
    Resumed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub at: String,
    pub kind: HistoryKind,
    pub sent: u64,
    pub received: u64,
    pub message: Option<String>,
}

pub const HISTORY_LIMIT: usize = 20;

/// Payload of the `sync://status` event and of `sync_get_status`. Contains
/// counters and timestamps only - never a record body or a token.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub state: SyncPhase,
    /// False while cloud sync is disabled or this device is not linked; the UI
    /// hides the badge and the sync settings in that case.
    pub linked: bool,
    pub pending_out: u64,
    pub last_sync_at: Option<String>,
    pub conflicts_open: u64,
    pub last_error: Option<String>,
    /// The cloud change log no longer reaches back to this device (or the
    /// device joined a shop that already has data): nothing is wiped until the
    /// user confirms via `sync_bootstrap`.
    pub bootstrap_required: bool,
    pub step: SyncStep,
    pub progress: Option<SyncProgress>,
    pub modules: Vec<ModuleStatus>,
    pub last_cycle: Option<CycleSummary>,
    /// When the agent will next try on its own (RFC 3339).
    pub next_retry_at: Option<String>,
    /// Newest first, at most `HISTORY_LIMIT`.
    pub history: Vec<HistoryEntry>,
}

impl Default for SyncStatus {
    fn default() -> Self {
        Self {
            state: SyncPhase::Idle,
            linked: false,
            pending_out: 0,
            last_sync_at: None,
            conflicts_open: 0,
            last_error: None,
            bootstrap_required: false,
            step: SyncStep::Idle,
            progress: None,
            modules: Vec::new(),
            last_cycle: None,
            next_retry_at: None,
            history: Vec::new(),
        }
    }
}

/// Where status changes and one-off notices go (Tauri events in the app, a
/// recorder in tests).
pub trait StatusSink: Send + Sync {
    fn status(&self, status: &SyncStatus);
    fn bootstrap_required(&self, reason: &str);
}

pub struct SyncControl {
    status: Mutex<SyncStatus>,
    paused: AtomicBool,
    bootstrap_confirmed: AtomicBool,
    wake: Notify,
    sink: Arc<dyn StatusSink>,
}

impl SyncControl {
    pub fn new(sink: Arc<dyn StatusSink>) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(SyncStatus::default()),
            paused: AtomicBool::new(false),
            bootstrap_confirmed: AtomicBool::new(false),
            wake: Notify::new(),
            sink,
        })
    }

    pub fn snapshot(&self) -> SyncStatus {
        self.status.lock().unwrap().clone()
    }

    /// Applies `f` and emits `sync://status` only when something changed.
    pub fn update(&self, f: impl FnOnce(&mut SyncStatus)) {
        let changed = {
            let mut guard = self.status.lock().unwrap();
            let before = guard.clone();
            f(&mut guard);
            (*guard != before).then(|| guard.clone())
        };
        if let Some(status) = changed {
            self.sink.status(&status);
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        let was = self.snapshot().state;
        self.update(|s| s.state = SyncPhase::Paused);
        if was != SyncPhase::Paused {
            self.push_history(HistoryKind::Paused, 0, 0, None);
        }
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        let was_paused = self.snapshot().state == SyncPhase::Paused;
        self.update(|s| {
            if s.state == SyncPhase::Paused {
                s.state = SyncPhase::Idle;
            }
        });
        if was_paused {
            self.push_history(HistoryKind::Resumed, 0, 0, None);
        }
        self.wake.notify_one();
    }

    /// Moves to a new step of the cycle; progress resets with it.
    pub fn set_step(&self, step: SyncStep, total: u64) {
        self.update(|s| {
            s.step = step;
            s.progress = match step {
                SyncStep::Uploading | SyncStep::Downloading => {
                    Some(SyncProgress { done: 0, total })
                }
                _ => None,
            };
        });
    }

    pub fn set_progress(&self, done: u64) {
        self.update(|s| {
            if let Some(p) = s.progress.as_mut() {
                p.done = done;
                if p.total > 0 && p.done > p.total {
                    p.total = p.done;
                }
            }
        });
    }

    pub fn set_next_retry(&self, after: Option<Duration>) {
        self.update(|s| {
            s.next_retry_at = after.map(|d| {
                (chrono::Utc::now() + chrono::Duration::milliseconds(d.as_millis() as i64))
                    .to_rfc3339()
            });
        });
    }

    /// Adds a line to the activity list (newest first, bounded). Identical
    /// consecutive offline / error lines are collapsed so a long outage is one line.
    pub fn push_history(
        &self,
        kind: HistoryKind,
        sent: u64,
        received: u64,
        message: Option<String>,
    ) {
        self.update(|s| {
            if matches!(kind, HistoryKind::Offline | HistoryKind::Error)
                && s.history
                    .first()
                    .map(|h| h.kind == kind && h.message == message)
                    .unwrap_or(false)
            {
                return;
            }
            s.history.insert(
                0,
                HistoryEntry {
                    at: chrono::Utc::now().to_rfc3339(),
                    kind,
                    sent,
                    received,
                    message,
                },
            );
            s.history.truncate(HISTORY_LIMIT);
        });
    }

    /// `sync_now`: cut the current wait short.
    pub fn wake_now(&self) {
        self.wake.notify_one();
    }

    /// Sleeps for `duration` or until `wake_now` / `resume`. Returns true if woken.
    pub async fn wait(&self, duration: Duration) -> bool {
        tokio::time::timeout(duration, self.wake.notified())
            .await
            .is_ok()
    }

    /// Marks the need for a user-confirmed bootstrap, pauses, and tells the UI
    /// once per transition.
    pub fn require_bootstrap(&self, reason: &str) {
        let already = self.snapshot().bootstrap_required;
        self.update(|s| s.bootstrap_required = true);
        self.pause();
        if !already {
            self.sink.bootstrap_required(reason);
        }
    }

    /// The user agreed to replace local data with the cloud copy.
    pub fn confirm_bootstrap(&self) {
        self.bootstrap_confirmed.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// Consumed by the agent loop: true exactly once per confirmation.
    pub fn take_bootstrap_confirmed(&self) -> bool {
        self.bootstrap_confirmed.swap(false, Ordering::SeqCst)
    }

    pub fn clear_bootstrap(&self) {
        self.update(|s| s.bootstrap_required = false);
    }
}

/// Test double that records everything the agent reports.
#[cfg(test)]
#[derive(Default)]
pub struct Recorder {
    pub statuses: Mutex<Vec<SyncStatus>>,
    pub bootstrap: Mutex<Vec<String>>,
}

#[cfg(test)]
impl StatusSink for Recorder {
    fn status(&self, status: &SyncStatus) {
        self.statuses.lock().unwrap().push(status.clone());
    }
    fn bootstrap_required(&self, reason: &str) {
        self.bootstrap.lock().unwrap().push(reason.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_emits_only_on_change() {
        let rec = Arc::new(Recorder::default());
        let ctl = SyncControl::new(rec.clone());
        ctl.update(|s| s.linked = true);
        ctl.update(|s| s.linked = true);
        ctl.update(|s| s.pending_out = 3);
        assert_eq!(rec.statuses.lock().unwrap().len(), 2);
    }

    #[test]
    fn pause_and_resume_flip_the_phase() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.pause();
        assert!(ctl.is_paused());
        assert_eq!(ctl.snapshot().state, SyncPhase::Paused);
        ctl.resume();
        assert!(!ctl.is_paused());
        assert_eq!(ctl.snapshot().state, SyncPhase::Idle);
    }

    #[test]
    fn bootstrap_notice_is_sent_once_per_transition() {
        let rec = Arc::new(Recorder::default());
        let ctl = SyncControl::new(rec.clone());
        ctl.require_bootstrap("cursor expired");
        ctl.require_bootstrap("cursor expired");
        assert_eq!(rec.bootstrap.lock().unwrap().len(), 1);
        assert!(ctl.is_paused() && ctl.snapshot().bootstrap_required);
        ctl.clear_bootstrap();
        ctl.require_bootstrap("again");
        assert_eq!(rec.bootstrap.lock().unwrap().len(), 2);
    }

    #[test]
    fn bootstrap_confirmation_is_consumed_once() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        assert!(!ctl.take_bootstrap_confirmed());
        ctl.confirm_bootstrap();
        assert!(ctl.take_bootstrap_confirmed());
        assert!(!ctl.take_bootstrap_confirmed());
    }

    #[test]
    fn history_is_newest_first_bounded_and_collapses_repeats() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        for i in 0..30 {
            ctl.push_history(HistoryKind::Synced, i, 0, None);
        }
        let h = ctl.snapshot().history;
        assert_eq!(h.len(), HISTORY_LIMIT);
        assert_eq!(h[0].sent, 29);
        ctl.push_history(HistoryKind::Offline, 0, 0, None);
        ctl.push_history(HistoryKind::Offline, 0, 0, None);
        assert_eq!(ctl.snapshot().history[0].kind, HistoryKind::Offline);
        assert_eq!(ctl.snapshot().history[1].kind, HistoryKind::Synced);
    }

    #[test]
    fn progress_only_exists_for_uploads_and_downloads() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.set_step(SyncStep::Uploading, 10);
        ctl.set_progress(4);
        assert_eq!(
            ctl.snapshot().progress,
            Some(SyncProgress { done: 4, total: 10 })
        );
        ctl.set_step(SyncStep::Finishing, 0);
        assert_eq!(ctl.snapshot().progress, None);
    }

    #[test]
    fn pause_and_resume_leave_history_lines() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.pause();
        ctl.pause();
        ctl.resume();
        let kinds: Vec<_> = ctl.snapshot().history.iter().map(|h| h.kind).collect();
        assert_eq!(kinds, vec![HistoryKind::Resumed, HistoryKind::Paused]);
    }

    #[tokio::test]
    async fn wake_now_cuts_the_wait_short() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.wake_now();
        let started = std::time::Instant::now();
        ctl.wait(Duration::from_secs(5)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
