// What the webview sees of the sync agent, plus the pause / wake-up switches
// the commands flip. The agent never touches Tauri directly: it reports through
// a `StatusSink`, so the whole loop is testable without a running app.

use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
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
    /// Still waiting to upload.
    pub pending: u64,
    pub conflicts: u64,
    /// Confirmed uploaded in the current cycle.
    pub sent: u64,
    /// Downloaded in the current cycle.
    pub received: u64,
    /// The last cycle that moved data for this record type; kept across quiet cycles.
    pub last_change: Option<ModuleChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleChange {
    pub at: String,
    pub sent: u64,
    pub received: u64,
}

impl ModuleStatus {
    fn new(resource: &str) -> Self {
        Self {
            resource: resource.to_string(),
            pending: 0,
            conflicts: 0,
            sent: 0,
            received: 0,
            last_change: None,
        }
    }
}

fn count_of(list: &[(String, u64)], resource: &str) -> u64 {
    list.iter()
        .filter(|(r, _)| r == resource)
        .map(|(_, n)| *n)
        .sum()
}

/// The row for `resource`, created (in alphabetical position) when missing.
fn row_mut<'a>(modules: &'a mut Vec<ModuleStatus>, resource: &str) -> &'a mut ModuleStatus {
    match modules.iter().position(|m| m.resource == resource) {
        Some(i) => &mut modules[i],
        None => {
            modules.push(ModuleStatus::new(resource));
            modules.sort_by(|a, b| a.resource.cmp(&b.resource));
            let i = modules
                .iter()
                .position(|m| m.resource == resource)
                .expect("row was just inserted");
            &mut modules[i]
        }
    }
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
    /// Changes from elsewhere were written into this computer's data, so open
    /// screens showing `resources` (`"*"` = everything) should reload.
    fn applied(&self, _resources: &[String]) {}
}

pub struct SyncControl {
    status: Mutex<SyncStatus>,
    paused: AtomicBool,
    bootstrap_confirmed: AtomicBool,
    wake: Notify,
    /// Why the agent was woken (see `Wakes`).
    wakes: Mutex<Wakes>,
    /// `LiveChannel` bits of the realtime streams that are connected.
    live: AtomicU8,
    sink: Arc<dyn StatusSink>,
}

/// What a wake asked the agent to do. A local write only needs an upload and
/// a cloud announcement only a download; everything else (and the timer) runs
/// the full cycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wakes {
    pub full: bool,
    pub local: bool,
    pub remote: bool,
}

/// The two realtime streams the agent listens to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LiveChannel {
    /// This computer's backend: "something was written".
    Local = 1,
    /// The cloud: "another device or the website changed something".
    Cloud = 2,
}

impl SyncControl {
    pub fn new(sink: Arc<dyn StatusSink>) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(SyncStatus::default()),
            paused: AtomicBool::new(false),
            bootstrap_confirmed: AtomicBool::new(false),
            wake: Notify::new(),
            wakes: Mutex::new(Wakes::default()),
            live: AtomicU8::new(0),
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
        self.wake_now();
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

    /// Start of a cycle's upload step: what is waiting per record type, with this
    /// cycle's sent / received counters back at zero. `last_change` is kept.
    pub fn begin_modules(&self, pending: &[(String, u64)], conflicts: &[(String, u64)]) {
        self.update(|s| {
            for (resource, _) in pending.iter().chain(conflicts.iter()) {
                row_mut(&mut s.modules, resource);
            }
            for m in s.modules.iter_mut() {
                m.pending = count_of(pending, &m.resource);
                m.conflicts = count_of(conflicts, &m.resource);
                m.sent = 0;
                m.received = 0;
            }
        });
    }

    /// A batch was confirmed by the cloud: move its counts from waiting to sent,
    /// and advance the bar, in one status update.
    pub fn modules_sent(&self, sent: &[(String, u64)], done: u64) {
        self.update(|s| {
            for (resource, n) in sent {
                let m = row_mut(&mut s.modules, resource);
                m.sent += n;
                m.pending = m.pending.saturating_sub(*n);
            }
            if let Some(p) = s.progress.as_mut() {
                p.done = done;
                if p.total > 0 && p.done > p.total {
                    p.total = p.done;
                }
            }
        });
    }

    /// A page of changes from other devices was applied here.
    pub fn modules_received(&self, received: &[(String, u64)], done: u64) {
        self.update(|s| {
            for (resource, n) in received {
                row_mut(&mut s.modules, resource).received += n;
            }
            if let Some(p) = s.progress.as_mut() {
                p.done = done;
                if p.total > 0 && p.done > p.total {
                    p.total = p.done;
                }
            }
        });
    }

    /// End of a cycle: the real waiting / conflict counts, and `last_change` for
    /// every record type that moved data this cycle.
    pub fn finish_modules(&self, pending: &[(String, u64)], conflicts: &[(String, u64)], at: &str) {
        self.update(|s| {
            for (resource, _) in pending.iter().chain(conflicts.iter()) {
                row_mut(&mut s.modules, resource);
            }
            for m in s.modules.iter_mut() {
                m.pending = count_of(pending, &m.resource);
                m.conflicts = count_of(conflicts, &m.resource);
                if m.sent > 0 || m.received > 0 {
                    m.last_change = Some(ModuleChange {
                        at: at.to_string(),
                        sent: m.sent,
                        received: m.received,
                    });
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
    /// Asks for a full cycle at once (manual "sync now", link, resume).
    pub fn wake_now(&self) {
        self.request(|w| w.full = true);
    }

    /// Something was written locally: upload it at once.
    pub fn wake_local(&self) {
        self.request(|w| w.local = true);
    }

    /// The cloud announced a change from elsewhere: download it at once.
    pub fn wake_remote(&self) {
        self.request(|w| w.remote = true);
    }

    fn request(&self, f: impl FnOnce(&mut Wakes)) {
        f(&mut self.wakes.lock().unwrap());
        self.wake.notify_one();
    }

    /// What was asked for since the last call, cleared.
    pub fn take_wakes(&self) -> Wakes {
        std::mem::take(&mut *self.wakes.lock().unwrap())
    }

    /// A realtime channel connected or dropped.
    pub fn set_live(&self, channel: LiveChannel, up: bool) {
        if up {
            self.live.fetch_or(channel as u8, Ordering::SeqCst);
        } else {
            self.live.fetch_and(!(channel as u8), Ordering::SeqCst);
        }
    }

    /// Both realtime channels are up, so the timer is only a safety net.
    pub fn live_connected(&self) -> bool {
        self.live.load(Ordering::SeqCst) == LiveChannel::Local as u8 | LiveChannel::Cloud as u8
    }

    /// Sleeps for `duration` or until a wake request. Returns true if woken.
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

    /// Tells the UI which record types just received changes from elsewhere.
    pub fn applied(&self, resources: &[String]) {
        if !resources.is_empty() {
            self.sink.applied(resources);
        }
    }

    /// The user agreed to replace local data with the cloud copy.
    pub fn confirm_bootstrap(&self) {
        self.bootstrap_confirmed.store(true, Ordering::SeqCst);
        self.wake_now();
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
    pub applied: Mutex<Vec<Vec<String>>>,
}

#[cfg(test)]
impl StatusSink for Recorder {
    fn status(&self, status: &SyncStatus) {
        self.statuses.lock().unwrap().push(status.clone());
    }
    fn bootstrap_required(&self, reason: &str) {
        self.bootstrap.lock().unwrap().push(reason.to_string());
    }

    fn applied(&self, resources: &[String]) {
        self.applied.lock().unwrap().push(resources.to_vec());
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
        ctl.modules_sent(&[], 4);
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

    fn counts(items: &[(&str, u64)]) -> Vec<(String, u64)> {
        items.iter().map(|(r, n)| (r.to_string(), *n)).collect()
    }

    fn row<'a>(s: &'a SyncStatus, resource: &str) -> &'a ModuleStatus {
        s.modules.iter().find(|m| m.resource == resource).unwrap()
    }

    #[test]
    fn modules_follow_a_cycle_from_waiting_to_sent_to_last_change() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.set_step(SyncStep::Uploading, 10);
        ctl.begin_modules(&counts(&[("invoices", 6), ("products", 4)]), &[]);
        let s = ctl.snapshot();
        assert_eq!(
            (row(&s, "invoices").pending, row(&s, "invoices").sent),
            (6, 0)
        );

        ctl.modules_sent(&counts(&[("invoices", 4), ("products", 1)]), 5);
        let s = ctl.snapshot();
        assert_eq!(
            (row(&s, "invoices").pending, row(&s, "invoices").sent),
            (2, 4)
        );
        assert_eq!(
            (row(&s, "products").pending, row(&s, "products").sent),
            (3, 1)
        );
        assert_eq!(s.progress.map(|p| p.done), Some(5));

        ctl.modules_sent(&counts(&[("invoices", 2), ("products", 3)]), 10);
        ctl.set_step(SyncStep::Downloading, 0);
        ctl.modules_received(&counts(&[("customers", 7)]), 7);
        ctl.finish_modules(&[], &[], "2026-10-04T12:00:00Z");
        let s = ctl.snapshot();
        assert_eq!(row(&s, "invoices").pending, 0);
        assert_eq!(row(&s, "customers").received, 7);
        assert_eq!(
            row(&s, "invoices")
                .last_change
                .as_ref()
                .map(|c| (c.sent, c.received)),
            Some((6, 0))
        );
        assert_eq!(
            row(&s, "customers")
                .last_change
                .as_ref()
                .map(|c| (c.sent, c.received)),
            Some((0, 7))
        );
    }

    #[test]
    fn a_quiet_cycle_resets_the_counters_but_keeps_last_change() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.begin_modules(&counts(&[("invoices", 2)]), &[]);
        ctl.modules_sent(&counts(&[("invoices", 2)]), 2);
        ctl.finish_modules(&[], &[], "t1");
        // Next cycle finds nothing to do.
        ctl.begin_modules(&[], &[]);
        ctl.finish_modules(&[], &[], "t2");
        let s = ctl.snapshot();
        let m = row(&s, "invoices");
        assert_eq!((m.sent, m.received, m.pending), (0, 0, 0));
        assert_eq!(m.last_change.as_ref().map(|c| c.at.as_str()), Some("t1"));
    }

    #[test]
    fn sent_never_drives_waiting_below_zero_and_rows_stay_sorted() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.begin_modules(&counts(&[("products", 1)]), &counts(&[("customers", 2)]));
        ctl.modules_sent(&counts(&[("products", 5), ("invoices", 1)]), 6);
        let s = ctl.snapshot();
        assert_eq!(row(&s, "products").pending, 0);
        assert_eq!(row(&s, "customers").conflicts, 2);
        let names: Vec<_> = s.modules.iter().map(|m| m.resource.as_str()).collect();
        assert_eq!(names, vec!["customers", "invoices", "products"]);
    }

    #[tokio::test]
    async fn wakes_say_what_was_asked_and_are_taken_once() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.wake_local();
        ctl.wake_remote();
        assert!(ctl.wait(Duration::from_secs(5)).await);
        assert_eq!(
            ctl.take_wakes(),
            Wakes {
                full: false,
                local: true,
                remote: true
            }
        );
        assert_eq!(ctl.take_wakes(), Wakes::default());
        ctl.resume();
        assert!(ctl.take_wakes().full);
    }

    #[test]
    fn live_counts_as_connected_only_with_both_streams() {
        let ctl = SyncControl::new(Arc::new(Recorder::default()));
        ctl.set_live(LiveChannel::Local, true);
        assert!(!ctl.live_connected());
        ctl.set_live(LiveChannel::Cloud, true);
        assert!(ctl.live_connected());
        ctl.set_live(LiveChannel::Local, false);
        assert!(!ctl.live_connected());
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
