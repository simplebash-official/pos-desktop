mod benchmark;
mod branding;
mod cloud;
// `pub` so `examples/log_pipe.rs` can reuse the real ingest + writer pipeline
// in the command-line logging benchmark. Not part of the app's public API.
#[doc(hidden)]
pub mod logging;
mod orchestrator;
mod printer;
mod sync;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use logging::{CommandLog, Level, LogEvent};
use orchestrator::Sidecars;
use serde_json::json;
use tauri::{Manager, RunEvent, WindowEvent};

/// Terminate all sidecar processes immediately prior to an updater relaunch
/// or installer execution so running binaries never lock files during extraction.
#[tauri::command]
fn prepare_for_update(app: tauri::AppHandle) {
    let call = CommandLog::start("prepare_for_update", json!({}));
    app.state::<Sidecars>().kill_all();
    orchestrator::reap_orphan_sidecars();
    if let Some(hub) = logging::hub::hub() {
        hub.flush_blocking(Duration::from_secs(2));
    }
    call.ok(&json!({ "sidecars_stopped": true }));
}

/// Query the local desktop installation record.
#[tauri::command]
fn get_installation_info(
    app: tauri::AppHandle,
) -> Result<orchestrator::InstallationRecord, String> {
    let call = CommandLog::start("get_installation_info", json!({}));
    let result = (|| {
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("no app data dir: {e}"))?;
        let version = app.package_info().version.to_string();
        orchestrator::load_or_create_installation(&data_dir.join("installation.json"), &version)
            .map_err(|e| format!("failed to load installation record: {e}"))
    })();
    call.finish(result)
}

/// Update the local desktop installation record upon completing onboarding.
#[tauri::command]
fn complete_installation_setup(
    app: tauri::AppHandle,
    sample_data_loaded: bool,
) -> Result<orchestrator::InstallationRecord, String> {
    let call = CommandLog::start(
        "complete_installation_setup",
        json!({ "sample_data_loaded": sample_data_loaded }),
    );
    let result = (|| {
        let data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| format!("no app data dir: {e}"))?;
        let version = app.package_info().version.to_string();
        orchestrator::update_installation_setup(
            &data_dir.join("installation.json"),
            &version,
            sample_data_loaded,
        )
        .map_err(|e| format!("failed to update installation record: {e}"))
    })();
    call.finish(result)
}

/// Throttle for high-frequency window events (resize/move), per window label.
static LAST_WINDOW_EVENT: Mutex<Option<(String, &'static str, Instant)>> = Mutex::new(None);

fn window_event_throttled(label: &str, kind: &'static str) -> bool {
    let mut last = LAST_WINDOW_EVENT.lock().unwrap();
    if let Some((l, k, at)) = last.as_ref() {
        if l == label && *k == kind && at.elapsed() < Duration::from_millis(500) {
            return true;
        }
    }
    *last = Some((label.to_string(), kind, Instant::now()));
    false
}

fn log_window_event(window: &tauri::Window, event: &WindowEvent) {
    let label = window.label();
    let (name, data, level) = match event {
        WindowEvent::Focused(focused) => (
            if *focused { "focused" } else { "blurred" },
            json!({}),
            Level::Info,
        ),
        WindowEvent::Resized(size) => {
            if window_event_throttled(label, "resized") {
                return;
            }
            (
                "resized",
                json!({ "width": size.width, "height": size.height }),
                Level::Info,
            )
        }
        WindowEvent::Moved(pos) => {
            if window_event_throttled(label, "moved") {
                return;
            }
            ("moved", json!({ "x": pos.x, "y": pos.y }), Level::Debug)
        }
        WindowEvent::CloseRequested { .. } => ("close_requested", json!({}), Level::Info),
        WindowEvent::Destroyed => ("destroyed", json!({}), Level::Info),
        WindowEvent::ScaleFactorChanged { scale_factor, .. } => (
            "scale_factor_changed",
            json!({ "scale_factor": scale_factor }),
            Level::Info,
        ),
        WindowEvent::ThemeChanged(theme) => (
            "theme_changed",
            json!({ "theme": format!("{theme:?}") }),
            Level::Info,
        ),
        WindowEvent::DragDrop(drop) => (
            "drag_drop",
            json!({ "event": format!("{drop:?}") }),
            Level::Info,
        ),
        _ => return,
    };
    let mut payload = data;
    payload["window"] = json!(label);
    LogEvent::shell("window", name)
        .level(level)
        .data(payload)
        .emit();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Logging comes first: everything from here on is recorded (buffered in
    // memory until the app data folder is known in `setup`).
    let hub = logging::hub::init();
    logging::install_panic_hook();
    LogEvent::shell("lifecycle", "process.start")
        .msg(format!("{} process started", branding::PRODUCT_NAME))
        .data(json!({
            "pid": std::process::id(),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "exe": std::env::current_exe().ok(),
            "args": std::env::args().collect::<Vec<_>>(),
            "debug_build": cfg!(debug_assertions),
        }))
        .emit();

    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }

    let mut builder = tauri::Builder::default();

    // Route the `log` facade (shell code, Tauri, plugins) into the hub. In
    // debug builds the devtools plugin owns the global logger, so ours is
    // attached to it; release builds install it directly.
    #[cfg(debug_assertions)]
    {
        let mut devtools_builder = tauri_plugin_devtools::Builder::default();
        let (_, logger) = logging::log_dispatch().into_log();
        devtools_builder.attach_logger(logger);
        builder = builder.plugin(devtools_builder.init());
    }
    #[cfg(not(debug_assertions))]
    {
        let (level, logger) = logging::log_dispatch().into_log();
        if log::set_boxed_logger(logger).is_ok() {
            log::set_max_level(level);
        }
    }

    // Must be the first non-devtools plugin: focuses the running window
    // instead of letting a second launch spawn rival copies fighting over
    // :8080/:8090.
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            LogEvent::shell("lifecycle", "second_instance")
                .msg("another launch was redirected to the running window")
                .data(json!({ "args": args, "cwd": cwd }))
                .emit();
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.show();
                let _ = main.set_focus();
            }
        }));
    }

    let app = builder
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        // Desktop auto-update: the Settings → Updates panel calls the updater
        // JS API; `tauri_plugin_process` supplies the relaunch after install.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            prepare_for_update,
            get_installation_info,
            complete_installation_setup,
            printer::print_pdf_native,
            cloud::cloud_get_state,
            cloud::cloud_register,
            cloud::cloud_otp_send,
            cloud::cloud_otp_verify,
            cloud::cloud_login_and_link,
            cloud::cloud_link_start,
            cloud::cloud_link_poll,
            cloud::cloud_unlink,
            cloud::cloud_list_devices,
            cloud::cloud_revoke_device,
            cloud::cloud_set_telemetry,
            cloud::cloud_ping,
            sync::sync_get_status,
            sync::sync_now,
            sync::sync_pause,
            sync::sync_resume,
            sync::sync_list_conflicts,
            sync::sync_resolve_conflict,
            sync::sync_bootstrap,
            benchmark::get_system_specs,
            benchmark::benchmark_disk_io,
            benchmark::benchmark_native_compute,
            logging::commands::log_webview,
            logging::commands::log_ingest,
            logging::commands::log_context,
            logging::commands::logs_list_days,
            logging::commands::logs_stats,
            logging::commands::logs_query,
            logging::commands::logs_set_tail,
            logging::commands::logs_get_config,
            logging::commands::logs_set_config,
            logging::commands::logs_open_folder,
            logging::commands::logs_export,
            logging::bench::benchmark_log_mode,
            logging::bench::benchmark_resource_sample,
        ])
        .manage(Sidecars::default())
        .on_window_event(log_window_event)
        .setup(move |app| {
            let handle = app.handle().clone();
            let version = handle.package_info().version.to_string();
            match handle.path().app_data_dir() {
                Ok(data_dir) => {
                    let logs_dir = data_dir.join("logs");
                    hub.attach(&handle, logs_dir.clone(), &version);
                    LogEvent::shell("lifecycle", "app.boot")
                        .msg(format!("{} {version} starting", handle.package_info().name))
                        .data(json!({
                            "version": version,
                            "tauri_version": tauri::VERSION,
                            "identifier": handle.config().identifier,
                            "context": hub.context(),
                            "data_dir": data_dir,
                            "logs_dir": logs_dir,
                            "locale": std::env::var("LANG").ok(),
                            "cpu_cores": std::thread::available_parallelism().map(|n| n.get()).ok(),
                        }))
                        .emit();
                }
                Err(err) => LogEvent::shell("lifecycle", "app.boot")
                    .level(Level::Error)
                    .msg(format!("no app data dir, logs stay in memory: {err}"))
                    .emit(),
            }
            // Optional cloud link. Always managed so its commands resolve; it
            // reports `enabled: false` (and does nothing) without a cloud URL.
            let cloud_dir = handle
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("simplebash-pos"));
            app.manage(cloud::CloudState::new(cloud_dir.clone(), version.clone()));
            // Sync agent: managed always (its commands resolve), runs only when
            // cloud sync is enabled and the device is linked.
            sync::init(app, &handle, cloud_dir);
            cloud::spawn_launch_ping(handle.clone());
            tauri::async_runtime::spawn(async move {
                if let Err(err) = orchestrator::run(handle.clone()).await {
                    orchestrator::fatal(&handle, &err);
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| match event {
        RunEvent::ExitRequested { code, .. } => {
            LogEvent::shell("lifecycle", "app.exit_requested")
                .data(json!({ "code": code }))
                .emit();
            app_handle.state::<Sidecars>().kill_all();
        }
        RunEvent::Exit => {
            app_handle.state::<Sidecars>().kill_all();
            LogEvent::shell("lifecycle", "app.exit")
                .msg(format!("{} exiting", app_handle.package_info().name))
                .emit();
            if let Some(hub) = logging::hub::hub() {
                hub.flush_blocking(Duration::from_secs(2));
            }
        }
        RunEvent::Resumed => LogEvent::shell("lifecycle", "app.resumed").emit(),
        _ => {}
    });
}
