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
        self.update(|s| s.state = SyncPhase::Paused);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.update(|s| {
            if s.state == SyncPhase::Paused {
                s.state = SyncPhase::Idle;
            }
        });
        self.wake.notify_one();
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

    #[tokio::test]
    async fn wake_now_cuts_the_wait_short() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.wake_now();
        let started = std::time::Instant::now();
        ctl.wait(Duration::from_secs(5)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
