// Tauri commands for the unified log: frontend ingestion, shared context,
// and the Settings → Logs viewer (query, stats, live tail, config, open
// folder, export). The viewer commands deliberately do not log their own
// outputs — logging query results would feed the log back into itself.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{json, Value};
use tauri::AppHandle;

use super::event::{Level, LogEvent};
use super::hub::{self, HubContext, LogConfig};
use super::{ingest, store};

/// Upper bound on one frontend batch, so a runaway page cannot flood the queue
/// in a single IPC call.
const MAX_BATCH: usize = 1_000;

fn logs_dir() -> Result<PathBuf, String> {
    hub::hub()
        .and_then(|h| h.logs_dir().cloned())
        .ok_or_else(|| "log folder is not ready yet".to_string())
}

/// Frontend batch ingestion. Each entry follows the unified schema; the
/// source is forced to `frontend` regardless of what the page sends.
#[tauri::command]
pub fn log_ingest(events: Vec<Value>) -> usize {
    let Some(hub) = hub::hub() else { return 0 };
    let total = events.len();
    let mut accepted = 0;
    for value in events.into_iter().take(MAX_BATCH) {
        if let Value::Object(obj) = value {
            hub.emit(ingest::from_json("frontend", obj));
            accepted += 1;
        }
    }
    if total > MAX_BATCH {
        LogEvent::shell("system", "log.batch_truncated")
            .level(Level::Warn)
            .data(json!({ "received": total, "accepted": accepted }))
            .emit();
    }
    accepted
}

/// Legacy bridge used by `public/webview-diagnostics.js` (boot-time errors
/// that happen before the bundled logger is up).
#[tauri::command]
pub fn log_webview(level: String, message: String) {
    LogEvent::new("frontend", "console", &level)
        .level(Level::parse(&level))
        .msg(message)
        .data(json!({ "via": "webview-diagnostics" }))
        .emit();
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogContextPayload {
    #[serde(flatten)]
    context: HubContext,
    config: LogConfig,
    logs_dir: Option<String>,
}

#[tauri::command]
pub fn log_context() -> Result<LogContextPayload, String> {
    let hub = hub::hub().ok_or("logging not initialised")?;
    Ok(LogContextPayload {
        context: hub.context(),
        config: hub.config(),
        logs_dir: hub.logs_dir().map(|p| p.to_string_lossy().into_owned()),
    })
}

#[tauri::command]
pub async fn logs_list_days() -> Result<Vec<store::DayInfo>, String> {
    let dir = logs_dir()?;
    tauri::async_runtime::spawn_blocking(move || store::list_days(&dir))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn logs_stats() -> Result<store::LogStats, String> {
    let dir = logs_dir()?;
    tauri::async_runtime::spawn_blocking(move || store::stats(&dir))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn logs_query(query: store::LogQuery) -> Result<store::LogPage, String> {
    let dir = logs_dir()?;
    // Flush first so the viewer sees events from the last 250 ms too.
    if let Some(hub) = hub::hub() {
        hub.flush_blocking(std::time::Duration::from_millis(500));
    }
    tauri::async_runtime::spawn_blocking(move || store::query(&dir, &query))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn logs_set_tail(enabled: bool) {
    if let Some(hub) = hub::hub() {
        hub.set_tail(enabled);
    }
}

/// The saved config — deliberately not the effective one, so a benchmark's
/// temporary mode never looks like the user's setting.
#[tauri::command]
pub fn logs_get_config() -> Result<LogConfig, String> {
    Ok(hub::hub()
        .ok_or("logging not initialised")?
        .persisted_config())
}

#[tauri::command]
pub fn logs_set_config(config: LogConfig) -> Result<LogConfig, String> {
    let hub = hub::hub().ok_or("logging not initialised")?;
    let before = hub.persisted_config();
    hub.set_config(config.clone())
        .map_err(|e| format!("save logging config: {e}"))?;
    LogEvent::shell("system", "log.config_changed")
        .msg("Logging settings changed (service body/SQL settings apply after restart)")
        .data(json!({ "before": before, "after": config }))
        .emit();
    Ok(config)
}

/// Reveal the log folder in Finder / Explorer / the file manager.
#[tauri::command]
pub fn logs_open_folder() -> Result<(), String> {
    let dir = logs_dir()?;
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(&dir)
        .spawn()
        .map_err(|e| format!("open {}: {e}", dir.display()))?;
    LogEvent::shell("system", "log.folder_opened")
        .data(json!({ "path": dir }))
        .emit();
    Ok(())
}

/// Ask where to save, then zip the selected days. `Ok(None)` = cancelled.
#[tauri::command]
pub async fn logs_export(
    app: AppHandle,
    from_day: Option<String>,
    to_day: Option<String>,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let dir = logs_dir()?;
    let suggested = format!(
        "jana2u-logs-{}.zip",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    );
    // Async command → runs off the main thread, so the blocking dialog is safe.
    let Some(picked) = app
        .dialog()
        .file()
        .set_file_name(&suggested)
        .add_filter("ZIP archive", &["zip"])
        .blocking_save_file()
    else {
        return Ok(None);
    };
    let dest = picked.into_path().map_err(|e| e.to_string())?;
    if let Some(hub) = hub::hub() {
        hub.flush_blocking(std::time::Duration::from_secs(2));
    }
    let dest_for_task = dest.clone();
    let count = tauri::async_runtime::spawn_blocking(move || {
        store::export_zip(&dir, from_day.as_deref(), to_day.as_deref(), &dest_for_task)
    })
    .await
    .map_err(|e| e.to_string())??;
    LogEvent::shell("system", "log.exported")
        .msg(format!("Exported {count} log files"))
        .data(json!({ "path": dest, "files": count }))
        .emit();
    Ok(Some(dest.to_string_lossy().into_owned()))
}
