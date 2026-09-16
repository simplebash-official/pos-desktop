// Measurement support for Settings → System Benchmark's "logging overhead"
// phase. Two commands: one switches every source's logging level at runtime,
// the other samples per-process CPU time and memory plus the writer's own
// counters. The frontend takes a sample, runs a workload, samples again, and
// reports the deltas — so the numbers are real OS measurements, not estimates.
//
// The mode is in-memory only: `logs/logging.json` is never written, so a crash
// mid-benchmark can never leave a shop with logging turned off.

use std::sync::Mutex;

use serde::Serialize;
use serde_json::json;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tauri::Manager;

use super::hub::{self, LogCounters, LogMode};
use super::{CommandLog, LogEvent};
use crate::orchestrator::Sidecars;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSample {
    /// `shell`, a sidecar log source, or `webview`.
    pub name: String,
    pub pid: u32,
    /// CPU time used since the process started, in milliseconds.
    pub cpu_time_ms: u64,
    pub rss_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSample {
    /// Wall clock, so the caller can turn CPU time into a percentage.
    pub at_ms: u64,
    pub processes: Vec<ProcessSample>,
    pub log: LogCounters,
    /// Mode in force, for the report.
    pub mode: Option<LogMode>,
}

/// One `System` reused across samples: it keeps per-process bookkeeping, and
/// rebuilding it on every call would be slower and less accurate.
fn sampler() -> &'static Mutex<System> {
    static SAMPLER: std::sync::OnceLock<Mutex<System>> = std::sync::OnceLock::new();
    SAMPLER.get_or_init(|| Mutex::new(System::new()))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// CPU time + RSS for the shell, every sidecar, and (best effort) the webview
/// content process, which is a child of ours on macOS and Windows.
fn sample_processes(app: &tauri::AppHandle) -> Vec<ProcessSample> {
    let shell_pid = std::process::id();
    let mut wanted: Vec<(String, u32)> = vec![("shell".to_string(), shell_pid)];
    for (name, pid) in app.state::<Sidecars>().pids() {
        wanted.push((name.to_string(), pid));
    }

    let mut system = sampler().lock().unwrap_or_else(|e| e.into_inner());
    let refresh = ProcessRefreshKind::nothing()
        .with_cpu()
        .with_memory()
        .with_exe(UpdateKind::OnlyIfNotSet);
    // All processes: the webview children below are not known up front.
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

    // Webview content/GPU processes are separate OS processes whose work is
    // not in our own CPU time; include them so "frontend logging" is honest.
    let shell = Pid::from_u32(shell_pid);
    let mut webview_index = 0;
    for (pid, process) in system.processes() {
        if process.parent() == Some(shell) && *pid != shell {
            let name = process.name().to_string_lossy().to_string();
            if wanted.iter().any(|(_, known)| *known == pid.as_u32()) {
                continue;
            }
            webview_index += 1;
            wanted.push((format!("webview:{name}:{webview_index}"), pid.as_u32()));
        }
    }

    wanted
        .into_iter()
        .filter_map(|(name, pid)| {
            let process = system.process(Pid::from_u32(pid))?;
            Some(ProcessSample {
                name,
                pid,
                cpu_time_ms: process.accumulated_cpu_time(),
                rss_bytes: process.memory(),
            })
        })
        .collect()
}

/// Switch logging for every source: the shell hub, each sidecar (one stdin
/// control line), and — via the returned config — the frontend.
///
/// `mode` is `off` | `standard` | `full` | `restore`.
#[tauri::command]
pub fn benchmark_log_mode(app: tauri::AppHandle, mode: String) -> Result<hub::LogConfig, String> {
    let call = CommandLog::start("benchmark_log_mode", json!({ "mode": mode }));
    let result = apply_mode(&app, &mode);
    call.finish(result)
}

fn apply_mode(app: &tauri::AppHandle, mode: &str) -> Result<hub::LogConfig, String> {
    let hub = hub::hub().ok_or("logging is not initialised")?;
    let restore = mode.eq_ignore_ascii_case("restore");
    let parsed = if restore {
        None
    } else {
        Some(LogMode::parse(mode).ok_or_else(|| format!("unknown logging mode '{mode}'"))?)
    };

    // A marker (category `benchmark`, so it survives Off) bounds each phase in
    // the log itself, written while the previous mode is still in force.
    LogEvent::new("shell", hub::BENCHMARK_CATEGORY, "log_mode")
        .msg(format!("logging mode → {mode}"))
        .data(json!({ "mode": mode }))
        .emit();
    hub.flush_blocking(std::time::Duration::from_secs(2));

    hub.set_mode(parsed);
    let effective = hub.config();
    let enabled = parsed != Some(LogMode::Off);
    let line = json!({
        "cmd": "log_mode",
        "enabled": enabled,
        "http_bodies": enabled && effective.http_bodies,
        "sql": if enabled { effective.sql.clone() } else { "off".to_string() },
    });
    app.state::<Sidecars>()
        .broadcast(format!("{line}\n").as_bytes());
    Ok(effective)
}

/// One measurement point. Cheap enough to call every ~500 ms.
#[tauri::command]
pub fn benchmark_resource_sample(app: tauri::AppHandle) -> Result<ResourceSample, String> {
    let hub = hub::hub().ok_or("logging is not initialised")?;
    Ok(ResourceSample {
        at_ms: now_ms(),
        processes: sample_processes(&app),
        log: hub.counters(),
        mode: hub.mode(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_reports_cpu_time_and_memory() {
        let mut system = sampler().lock().unwrap();
        let me = Pid::from_u32(std::process::id());
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[me]),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
        let process = system.process(me).expect("own process");
        assert!(process.memory() > 0, "RSS should be known");
        // Accumulated CPU time is monotonic and non-zero once tests have run.
        assert!(process.accumulated_cpu_time() > 0);
    }
}
