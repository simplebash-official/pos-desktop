// The single log collector. Every producer (shell `log::` calls, explicit
// shell events, frontend batches, sidecar stdout, installer inbox files)
// funnels into one bounded channel drained by one dedicated writer thread,
// which owns every file handle. A plain OS thread — not a tokio task — so it
// runs before the async runtime exists and can still flush during a panic.
//
// Emitting never blocks: when the queue is full — by event count or by the
// byte budget below — the event is dropped and counted, and the count is
// itself logged. A POS must not stall on logging, and it must not grow
// without bound if the disk stalls either.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use super::event::{local_timezone_name, Level, LogEvent};
use super::{redact, retention};

const CHANNEL_CAPACITY: usize = 50_000;
/// Memory the queue may hold. Counting events alone is not enough: one event
/// can carry a 64 KB body, so 50k events could otherwise mean gigabytes.
const QUEUE_BYTE_BUDGET: usize = 32 * 1024 * 1024;
/// Events held in memory before `app_data_dir` is known (earliest startup).
const PRE_DIR_BUFFER: usize = 50_000;
const PRE_DIR_BYTE_BUDGET: usize = 16 * 1024 * 1024;
const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// Per source, per day. Past this the writer rolls to `<source>.1.jsonl`, …
const MAX_FILE_BYTES: u64 = 200 * 1024 * 1024;
/// A single line larger than this has its `data` replaced by a preview.
const MAX_LINE_BYTES: usize = 256 * 1024;
const MAX_MSG_BYTES: usize = 32 * 1024;
const MAX_TAIL_BATCH: usize = 500;
pub const TAIL_EVENT: &str = "log://entries";

/// User-tunable knobs, persisted at `logs/logging.json` and handed to the
/// frontend (`log_context`) and to sidecars (env at spawn).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LogConfig {
    /// Log request/response bodies (redacted + truncated) in every service.
    pub http_bodies: bool,
    /// `all` | `slow` | `off` — SQL statement logging in sidecars.
    pub sql: String,
    /// Byte cap applied to each logged body.
    pub body_cap_bytes: usize,
    /// Very noisy UI signals: focus/blur, scroll, pointer. Off by default.
    pub ui_trace: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            http_bodies: true,
            sql: "all".into(),
            body_cap_bytes: 32 * 1024,
            ui_trace: false,
        }
    }
}

/// Runtime logging level used by the System Benchmark. It overrides the
/// persisted config in memory only — `logs/logging.json` is never touched, so
/// a crash mid-benchmark cannot leave logging disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogMode {
    /// Record nothing (except `category = "benchmark"` markers).
    Off,
    /// Everything, bodies included, but only slow SQL.
    Standard,
    /// Everything, including every SQL statement.
    Full,
}

impl LogMode {
    pub fn parse(raw: &str) -> Option<LogMode> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" => Some(LogMode::Off),
            "standard" => Some(LogMode::Standard),
            "full" => Some(LogMode::Full),
            _ => None,
        }
    }

    /// The config a sidecar/frontend should apply for this mode.
    pub fn config(self, base: &LogConfig) -> LogConfig {
        match self {
            LogMode::Off => LogConfig {
                http_bodies: false,
                sql: "off".into(),
                ui_trace: false,
                ..base.clone()
            },
            LogMode::Standard => LogConfig {
                http_bodies: true,
                sql: "slow".into(),
                ui_trace: false,
                ..base.clone()
            },
            LogMode::Full => LogConfig {
                http_bodies: true,
                sql: "all".into(),
                ui_trace: false,
                ..base.clone()
            },
        }
    }
}

/// Writer-side totals, read by the benchmark to get disk bytes and event
/// counts for a phase.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogCounters {
    pub accepted: u64,
    pub events_written: u64,
    pub bytes_written: u64,
    pub dropped: u64,
    pub queued_bytes: u64,
}

/// Events that bypass `LogMode::Off` so a benchmark can mark its own phases.
pub const BENCHMARK_CATEGORY: &str = "benchmark";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HubContext {
    pub boot_id: String,
    pub installation_id: Option<String>,
    pub app_version: String,
    pub os: String,
    pub arch: String,
    pub tz: String,
}

enum Cmd {
    /// The event plus the byte estimate charged to the queue budget.
    Event(Box<LogEvent>, usize),
    SetDir(PathBuf),
    Flush(SyncSender<()>),
}

pub struct LogHub {
    tx: SyncSender<Cmd>,
    dropped: AtomicU64,
    /// Events accepted onto the queue (monotonic; the queue depth is not).
    accepted: AtomicU64,
    queued_bytes: AtomicUsize,
    events_written: AtomicU64,
    bytes_written: AtomicU64,
    ctx: RwLock<HubContext>,
    config: RwLock<LogConfig>,
    mode: RwLock<Option<LogMode>>,
    dir: OnceLock<PathBuf>,
    app: OnceLock<AppHandle>,
    tail: AtomicBool,
}

static HUB: OnceLock<LogHub> = OnceLock::new();

/// Create the global hub and start its writer thread. Idempotent.
pub fn init() -> &'static LogHub {
    HUB.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        std::thread::Builder::new()
            .name("log-writer".into())
            .spawn(move || Writer::new(rx).run())
            .expect("spawn log writer thread");
        LogHub {
            tx,
            dropped: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            queued_bytes: AtomicUsize::new(0),
            events_written: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            ctx: RwLock::new(HubContext {
                boot_id: format!("boot_{}", random_hex(12)),
                installation_id: None,
                app_version: String::new(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                tz: local_timezone_name(),
            }),
            config: RwLock::new(LogConfig::default()),
            mode: RwLock::new(None),
            dir: OnceLock::new(),
            app: OnceLock::new(),
            tail: AtomicBool::new(false),
        }
    })
}

pub fn hub() -> Option<&'static LogHub> {
    HUB.get()
}

/// Queue an event on the global hub; a no-op before `init`.
pub fn emit(event: LogEvent) {
    if let Some(hub) = HUB.get() {
        hub.emit(event);
    }
}

impl LogHub {
    pub fn emit(&self, event: LogEvent) {
        if self.mode() == Some(LogMode::Off) && event.category != BENCHMARK_CATEGORY {
            return;
        }
        let size = approx_size(&event);
        // Reserve the bytes first; a queue that is full by count *or* by bytes
        // drops the event rather than growing memory.
        let mut reserved = self.queued_bytes.load(Ordering::Relaxed);
        loop {
            if reserved + size > QUEUE_BYTE_BUDGET {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            match self.queued_bytes.compare_exchange_weak(
                reserved,
                reserved + size,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => reserved = current,
            }
        }
        match self.tx.try_send(Cmd::Event(Box::new(event), size)) {
            Ok(()) => {
                self.accepted.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.queued_bytes.fetch_sub(size, Ordering::Relaxed);
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn mode(&self) -> Option<LogMode> {
        *self.mode.read().unwrap()
    }

    /// Apply a runtime mode (benchmark). `None` restores the persisted config.
    pub fn set_mode(&self, mode: Option<LogMode>) {
        *self.mode.write().unwrap() = mode;
    }

    pub fn counters(&self) -> LogCounters {
        LogCounters {
            accepted: self.accepted.load(Ordering::Relaxed),
            events_written: self.events_written.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            queued_bytes: self.queued_bytes.load(Ordering::Relaxed) as u64,
        }
    }

    /// Point the writer at `<app_data_dir>/logs` and load the persisted config.
    /// Everything buffered so far is written out in order.
    pub fn attach(&self, app: &AppHandle, logs_dir: PathBuf, app_version: &str) {
        let _ = self.app.set(app.clone());
        self.attach_dir(logs_dir, app_version);
    }

    /// `attach` without a Tauri app: used by the `log_pipe` example, which
    /// runs the same writer for the command-line logging benchmark.
    pub fn attach_dir(&self, logs_dir: PathBuf, app_version: &str) {
        if let Ok(mut ctx) = self.ctx.write() {
            ctx.app_version = app_version.to_string();
        }
        let _ = fs::create_dir_all(logs_dir.join("inbox"));
        *self.config.write().unwrap() = load_config(&logs_dir);
        let _ = self.dir.set(logs_dir.clone());
        let _ = self.tx.send(Cmd::SetDir(logs_dir));
    }

    pub fn set_installation_id(&self, id: &str) {
        if let Ok(mut ctx) = self.ctx.write() {
            ctx.installation_id = Some(id.to_string());
        }
    }

    pub fn context(&self) -> HubContext {
        self.ctx.read().unwrap().clone()
    }

    /// The config in force right now: the benchmark's mode when one is set,
    /// otherwise what the user saved.
    pub fn config(&self) -> LogConfig {
        let base = self.config.read().unwrap().clone();
        match self.mode() {
            Some(mode) => mode.config(&base),
            None => base,
        }
    }

    /// What is saved on disk, ignoring any benchmark mode.
    pub fn persisted_config(&self) -> LogConfig {
        self.config.read().unwrap().clone()
    }

    pub fn set_config(&self, config: LogConfig) -> std::io::Result<()> {
        if let Some(dir) = self.dir.get() {
            fs::write(
                dir.join("logging.json"),
                serde_json::to_string_pretty(&config).unwrap_or_default(),
            )?;
        }
        *self.config.write().unwrap() = config;
        Ok(())
    }

    pub fn logs_dir(&self) -> Option<&PathBuf> {
        self.dir.get()
    }

    pub fn set_tail(&self, enabled: bool) {
        self.tail.store(enabled, Ordering::Relaxed);
    }

    /// Block (bounded) until everything queued so far is on disk. Used on
    /// exit and from the panic hook.
    pub fn flush_blocking(&self, timeout: Duration) {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if self.tx.try_send(Cmd::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(timeout);
        }
    }
}

fn load_config(logs_dir: &Path) -> LogConfig {
    fs::read_to_string(logs_dir.join("logging.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Cheap upper-ish estimate of an event's in-memory cost, used for the queue
/// byte budget. Walks `data` only to a bounded depth/width — it must never
/// cost more than the write it is protecting.
pub fn approx_size(event: &LogEvent) -> usize {
    const BASE: usize = 320; // fixed fields: ids, timestamps, source, level…
    BASE + event.msg.as_ref().map_or(0, String::len)
        + event.category.len()
        + event.event.len()
        + approx_value_size(&event.data, 0)
}

fn approx_value_size(value: &Value, depth: u32) -> usize {
    if depth > 6 {
        return 16;
    }
    match value {
        Value::Null | Value::Bool(_) => 8,
        Value::Number(_) => 12,
        Value::String(s) => s.len() + 4,
        Value::Array(items) => {
            // A long array is sampled, not walked: the first few entries are
            // representative enough for a memory budget.
            let sampled: usize = items
                .iter()
                .take(16)
                .map(|v| approx_value_size(v, depth + 1))
                .sum();
            match items.len() {
                0 => 8,
                n if n <= 16 => sampled + 8,
                n => sampled * n / 16 + 8,
            }
        }
        Value::Object(map) => {
            8 + map
                .iter()
                .take(64)
                .map(|(k, v)| k.len() + 4 + approx_value_size(v, depth + 1))
                .sum::<usize>()
        }
    }
}

pub fn random_hex(len: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; len.div_ceil(2)];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    hex[..len].to_string()
}

// ---------------------------------------------------------------------------
// Writer thread
// ---------------------------------------------------------------------------

struct OpenFile {
    out: BufWriter<File>,
    bytes: u64,
    index: u32,
}

struct Writer {
    rx: Receiver<Cmd>,
    dir: Option<PathBuf>,
    pending: Vec<LogEvent>,
    pending_bytes: usize,
    files: HashMap<(String, String), OpenFile>,
    seq: u64,
    tail_batch: Vec<LogEvent>,
    last_flush: Instant,
    compacted_for_day: String,
}

impl Writer {
    fn new(rx: Receiver<Cmd>) -> Self {
        Writer {
            rx,
            dir: None,
            pending: Vec::new(),
            pending_bytes: 0,
            files: HashMap::new(),
            seq: 0,
            tail_batch: Vec::new(),
            last_flush: Instant::now(),
            compacted_for_day: String::new(),
        }
    }

    fn run(mut self) {
        loop {
            match self.rx.recv_timeout(FLUSH_EVERY) {
                Ok(Cmd::Event(event, size)) => {
                    let urgent = event.level >= Level::Error;
                    self.accept(*event);
                    // The event is no longer queued: release its reservation.
                    if let Some(hub) = HUB.get() {
                        hub.queued_bytes.fetch_sub(size, Ordering::Relaxed);
                    }
                    if urgent {
                        self.flush();
                    }
                }
                Ok(Cmd::SetDir(dir)) => {
                    self.dir = Some(dir.clone());
                    for event in retention::drain_inbox(&dir) {
                        self.accept(event);
                    }
                    self.pending_bytes = 0;
                    for event in std::mem::take(&mut self.pending) {
                        self.write(event);
                    }
                    self.flush();
                }
                Ok(Cmd::Flush(ack)) => {
                    self.flush();
                    let _ = ack.send(());
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.flush();
                    return;
                }
            }
            if self.last_flush.elapsed() >= FLUSH_EVERY {
                self.tick();
            }
        }
    }

    /// Periodic housekeeping: drop count, flush, live tail, day compaction.
    fn tick(&mut self) {
        if let Some(hub) = HUB.get() {
            let dropped = hub.dropped.swap(0, Ordering::Relaxed);
            if dropped > 0 {
                self.accept(
                    LogEvent::shell("system", "log.dropped")
                        .level(Level::Warn)
                        .msg(format!("{dropped} log events dropped: log queue was full"))
                        .data(json!({ "count": dropped })),
                );
            }
        }
        self.flush();
        self.emit_tail();
        self.maybe_compact();
    }

    fn accept(&mut self, event: LogEvent) {
        if self.dir.is_some() {
            self.write(event);
            return;
        }
        let size = approx_size(&event);
        if self.pending.len() < PRE_DIR_BUFFER && self.pending_bytes + size <= PRE_DIR_BYTE_BUDGET {
            self.pending_bytes += size;
            self.pending.push(event);
        } else if let Some(hub) = HUB.get() {
            hub.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn finalize(&mut self, event: &mut LogEvent) {
        self.seq += 1;
        event.v = super::event::SCHEMA_VERSION;
        event.seq = self.seq;
        event.normalize_time();
        if let Some(hub) = HUB.get() {
            let ctx = hub.ctx.read().unwrap();
            event.tz = ctx.tz.clone();
            if event.boot_id.is_empty() {
                event.boot_id = ctx.boot_id.clone();
            }
            if event.installation_id.is_none() {
                event.installation_id = ctx.installation_id.clone();
            }
            if event.app_version.is_empty() {
                event.app_version = ctx.app_version.clone();
            }
            if event.os.is_empty() {
                event.os = ctx.os.clone();
            }
        }
        if let Some(msg) = event.msg.as_mut() {
            if msg.len() > MAX_MSG_BYTES {
                let mut cut = MAX_MSG_BYTES;
                while !msg.is_char_boundary(cut) {
                    cut -= 1;
                }
                msg.truncate(cut);
                msg.push_str("…[truncated]");
            }
            *msg = redact::redact_text(msg);
        }
        redact::redact_value(&mut event.data);
    }

    fn write(&mut self, mut event: LogEvent) {
        self.finalize(&mut event);
        let mut line = serde_json::to_string(&event).unwrap_or_default();
        if line.len() > MAX_LINE_BYTES {
            let original = line.len();
            let preview: String = event.data.to_string().chars().take(8 * 1024).collect();
            event.data = json!({ "truncated": true, "bytes": original, "preview": preview });
            line = serde_json::to_string(&event).unwrap_or_default();
        }
        line.push('\n');

        #[cfg(debug_assertions)]
        println!(
            "{} {:<5} [{}] {}/{} {}",
            event.ts,
            format!("{:?}", event.level).to_uppercase(),
            event.source,
            event.category,
            event.event,
            event.msg.as_deref().unwrap_or("")
        );

        let Some(dir) = self.dir.clone() else { return };
        let day = event.day();
        let source = sanitize_source(&event.source);
        if let Err(err) = self.append(&dir, &day, &source, line.as_bytes()) {
            eprintln!("log writer: {err}");
        }

        if HUB.get().is_some_and(|h| h.tail.load(Ordering::Relaxed))
            && self.tail_batch.len() < MAX_TAIL_BATCH
        {
            self.tail_batch.push(event);
        }
    }

    fn append(&mut self, dir: &Path, day: &str, source: &str, bytes: &[u8]) -> std::io::Result<()> {
        let key = (day.to_string(), source.to_string());
        if !self.files.contains_key(&key) {
            if self.files.len() >= 32 {
                self.flush();
                self.files.clear();
            }
            let file = open_log_file(dir, day, source, 0)?;
            self.files.insert(key.clone(), file);
        }
        let file = self.files.get_mut(&key).expect("just inserted");
        if file.bytes + bytes.len() as u64 > MAX_FILE_BYTES {
            file.out.flush()?;
            let next = open_log_file(dir, day, source, file.index + 1)?;
            *file = next;
        }
        file.out.write_all(bytes)?;
        file.bytes += bytes.len() as u64;
        if let Some(hub) = HUB.get() {
            hub.events_written.fetch_add(1, Ordering::Relaxed);
            hub.bytes_written
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(())
    }

    fn flush(&mut self) {
        for file in self.files.values_mut() {
            let _ = file.out.flush();
        }
        self.last_flush = Instant::now();
    }

    fn emit_tail(&mut self) {
        if self.tail_batch.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.tail_batch);
        if let Some(app) = HUB.get().and_then(|h| h.app.get()) {
            let _ = app.emit(TAIL_EVENT, &batch);
        }
    }

    fn maybe_compact(&mut self) {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        if self.compacted_for_day == today {
            return;
        }
        let Some(dir) = self.dir.clone() else { return };
        self.compacted_for_day = today;
        // gzip can take a while on a large day; keep the writer responsive.
        std::thread::spawn(move || retention::compact_old_days(&dir));
    }
}

/// Opens (appending) the first file at or after `start_index` that still has
/// room, so a restart continues the current day's file instead of a new one.
fn open_log_file(
    dir: &Path,
    day: &str,
    source: &str,
    start_index: u32,
) -> std::io::Result<OpenFile> {
    let day_dir = dir.join(day);
    fs::create_dir_all(&day_dir)?;
    let mut index = start_index;
    loop {
        let path = day_dir.join(log_file_name(source, index));
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if bytes < MAX_FILE_BYTES {
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            return Ok(OpenFile {
                out: BufWriter::with_capacity(64 * 1024, file),
                bytes,
                index,
            });
        }
        index += 1;
    }
}

pub fn log_file_name(source: &str, index: u32) -> String {
    if index == 0 {
        format!("{source}.jsonl")
    } else {
        format!("{source}.{index}.jsonl")
    }
}

/// File-name-safe source label (`document-server`, `frontend`, …).
pub fn sanitize_source(source: &str) -> String {
    let cleaned: String = source
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".into()
    } else {
        cleaned
    }
}

/// Convenience for commands: serialize any value into log `data`.
pub fn to_data<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_are_file_safe() {
        assert_eq!(sanitize_source("Document Server"), "document-server");
        assert_eq!(sanitize_source("../etc"), "---etc");
        assert_eq!(sanitize_source(""), "unknown");
    }

    #[test]
    fn rollover_file_names() {
        assert_eq!(log_file_name("backend", 0), "backend.jsonl");
        assert_eq!(log_file_name("backend", 2), "backend.2.jsonl");
    }

    /// The global hub is process-wide; these tests must not run concurrently.
    fn hub_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn big_event(kb: usize) -> LogEvent {
        LogEvent::shell("system", "flood").data(json!({ "blob": "x".repeat(kb * 1024) }))
    }

    #[test]
    fn approx_size_tracks_payload_without_walking_all_of_it() {
        assert!(approx_size(&LogEvent::shell("system", "tiny")) < 400);
        let one_kb = approx_size(&big_event(1));
        assert!((1024..2048).contains(&one_kb), "1 KB event: {one_kb}");
        // A long array is sampled and scaled, not walked item by item.
        let wide = LogEvent::shell("system", "wide")
            .data(json!({ "items": (0..10_000).map(|i| json!({ "k": i })).collect::<Vec<_>>() }));
        let estimated = approx_size(&wide);
        assert!(estimated > 100_000, "scaled estimate: {estimated}");
    }

    #[test]
    fn queue_is_bounded_by_bytes_and_drops_the_excess() {
        let _guard = hub_test_lock();
        let hub = init();
        hub.set_mode(None);
        let before = hub.counters().dropped;
        // 40k x 64 KB ≈ 2.5 GB of events with nowhere to write them yet.
        for _ in 0..40_000 {
            hub.emit(big_event(64));
            assert!(
                hub.counters().queued_bytes as usize <= QUEUE_BYTE_BUDGET,
                "queue exceeded its byte budget"
            );
        }
        let counters = hub.counters();
        assert!(counters.dropped > before, "expected drops under flood");
        assert!((counters.queued_bytes as usize) <= QUEUE_BYTE_BUDGET);
    }

    #[test]
    fn off_mode_records_nothing_except_benchmark_markers() {
        let _guard = hub_test_lock();
        let hub = init();
        hub.set_mode(Some(LogMode::Off));
        let before = hub.counters();
        for _ in 0..100 {
            hub.emit(big_event(16));
        }
        let after = hub.counters();
        assert_eq!(after.dropped, before.dropped, "Off must not count drops");
        assert_eq!(
            after.queued_bytes, before.queued_bytes,
            "Off must not queue"
        );

        assert_eq!(
            after.accepted, before.accepted,
            "Off must not accept events"
        );

        hub.emit(LogEvent::new("shell", BENCHMARK_CATEGORY, "phase.start"));
        assert_eq!(
            hub.counters().accepted,
            before.accepted + 1,
            "markers still pass"
        );
        hub.set_mode(None);
    }

    #[test]
    fn mode_overrides_config_without_touching_the_saved_one() {
        let _guard = hub_test_lock();
        let hub = init();
        hub.set_mode(Some(LogMode::Standard));
        assert_eq!(hub.config().sql, "slow");
        assert!(hub.config().http_bodies);
        hub.set_mode(Some(LogMode::Full));
        assert_eq!(hub.config().sql, "all");
        hub.set_mode(None);
        assert_eq!(hub.config().sql, hub.persisted_config().sql);
    }

    #[test]
    fn writer_appends_to_day_and_source_file() {
        let tmp = std::env::temp_dir().join(format!("myrologic-logtest-{}", random_hex(8)));
        let (_tx, rx) = mpsc::sync_channel(1);
        let mut writer = Writer::new(rx);
        writer.dir = Some(tmp.clone());
        let mut e = LogEvent::new("frontend", "ui", "click");
        e.ts = "2026-09-15T10:00:00+05:30".into();
        e.data = json!({"password": "x", "label": "Save"});
        writer.write(e);
        writer.flush();
        let raw = fs::read_to_string(tmp.join("2026-09-15").join("frontend.jsonl")).unwrap();
        // The writer's counters moved by exactly this one line.
        if let Some(hub) = HUB.get() {
            assert!(hub.counters().bytes_written >= raw.len() as u64);
        }
        let line: Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(line["data"]["password"], redact::REDACTED);
        assert_eq!(line["data"]["label"], "Save");
        assert_eq!(line["seq"], 1);
        let _ = fs::remove_dir_all(tmp);
    }
}
