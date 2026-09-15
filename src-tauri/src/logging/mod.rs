// Unified desktop activity log. The shell is the single collector for every
// source (itself, the frontend, each sidecar, the installer) and writes
// append-only JSON Lines under `<app_data_dir>/logs/<day>/<source>.jsonl`.
// Contract for producers: `docs/logging.md`.

pub mod commands;
pub mod event;
pub mod hub;
pub mod ingest;
pub mod redact;
pub mod retention;
pub mod store;

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

pub use event::{Level, LogEvent};

/// A `log::` record → hub event. Existing `log::info!` calls throughout the
/// shell (and in Tauri/plugins) land in `shell.jsonl` with no rewrite.
fn record_to_event(record: &log::Record) -> LogEvent {
    let target = record.target();
    let category = if target.contains("orchestrator") {
        "sidecar"
    } else if target.starts_with("app_lib") {
        "system"
    } else {
        "runtime"
    };
    LogEvent::shell(category, "log")
        .level(Level::from_log(record.level()))
        .msg(record.args().to_string())
        .data(json!({ "target": target }))
}

/// The `log` facade dispatch: everything at `Info`+ goes to the hub; chatty
/// windowing internals are held at `Warn`.
pub fn log_dispatch() -> tauri_plugin_log::fern::Dispatch {
    tauri_plugin_log::fern::Dispatch::new()
        .level(log::LevelFilter::Info)
        .level_for("tao", log::LevelFilter::Warn)
        .level_for("wry", log::LevelFilter::Warn)
        .chain(tauri_plugin_log::fern::Output::call(|record| {
            hub::emit(record_to_event(record));
        }))
}

/// Record panics (with location and backtrace) and flush before the default
/// hook runs, so the cause of a crash is always on disk.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".into());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        LogEvent::shell("error", "panic")
            .level(Level::Fatal)
            .msg(format!("panic in thread '{thread}': {payload}"))
            .data(json!({
                "location": location,
                "thread": thread,
                "backtrace": std::backtrace::Backtrace::force_capture().to_string(),
            }))
            .emit();
        if let Some(hub) = hub::hub() {
            hub.flush_blocking(Duration::from_secs(2));
        }
        previous(info);
    }));
}

/// Times one Tauri command and records its (redacted) input and output.
///
/// ```ignore
/// let call = CommandLog::start("print_pdf_native", json!({ "title": title }));
/// let result = do_work().await;
/// call.finish(&result)
/// ```
pub struct CommandLog {
    name: &'static str,
    started: Instant,
}

/// Outputs larger than this are summarized instead of logged in full.
const MAX_OUTPUT_BYTES: usize = 32 * 1024;

impl CommandLog {
    pub fn start(name: &'static str, args: Value) -> Self {
        LogEvent::shell("command", "invoke")
            .msg(format!("{name} invoked"))
            .data(json!({ "command": name, "args": args }))
            .emit();
        CommandLog {
            name,
            started: Instant::now(),
        }
    }

    fn elapsed_ms(&self) -> f64 {
        (self.started.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0
    }

    /// Record the result and hand it back unchanged.
    pub fn finish<T: Serialize, E: std::fmt::Display>(self, result: Result<T, E>) -> Result<T, E> {
        match &result {
            Ok(output) => self.ok(output),
            Err(err) => {
                LogEvent::shell("command", "error")
                    .level(Level::Error)
                    .msg(format!("{} failed: {err}", self.name))
                    .data(json!({ "command": self.name, "duration_ms": self.elapsed_ms(), "error": err.to_string() }))
                    .emit();
            }
        }
        result
    }

    /// Record a successful output for commands that cannot fail.
    pub fn ok<T: Serialize>(&self, output: &T) {
        let mut value = hub::to_data(output);
        let size = value.to_string().len();
        if size > MAX_OUTPUT_BYTES {
            value = json!({ "truncated": true, "bytes": size });
        }
        LogEvent::shell("command", "result")
            .msg(format!("{} completed", self.name))
            .data(
                json!({ "command": self.name, "duration_ms": self.elapsed_ms(), "output": value }),
            )
            .emit();
    }
}
