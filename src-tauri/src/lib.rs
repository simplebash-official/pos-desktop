mod orchestrator;
mod printer;

use orchestrator::Sidecars;
use tauri::{Manager, RunEvent};

/// Bridge for `public/webview-diagnostics.js` — surfaces webview
/// console.warn/error and uncaught exceptions in the app log.
#[tauri::command]
fn log_webview(level: String, message: String) {
    match level.as_str() {
        "error" => log::error!("[webview] {message}"),
        "warn" => log::warn!("[webview] {message}"),
        _ => log::info!("[webview] {message}"),
    }
}

/// Terminate all sidecar processes immediately prior to an updater relaunch
/// or installer execution so running binaries never lock files during extraction.
#[tauri::command]
fn prepare_for_update(app: tauri::AppHandle) {
    log::info!("prepare_for_update: shutting down sidecars before installer runs");
    app.state::<Sidecars>().kill_all();
    orchestrator::reap_orphan_sidecars();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();

    // Must be the first plugin: focuses the running window instead of
    // letting a second launch spawn rival copies fighting over :8080/:8090.
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(main) = app.get_webview_window("main") {
                let _ = main.show();
                let _ = main.set_focus();
            }
        }));
    }

    let app = builder
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        // Desktop auto-update: the Settings → Updates panel calls the updater
        // JS API; `tauri_plugin_process` supplies the relaunch after install.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            log_webview,
            prepare_for_update,
            printer::print_pdf_native
        ])
        .manage(Sidecars::default())
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = orchestrator::run(handle.clone()).await {
                    orchestrator::fatal(&handle, &err);
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        if let RunEvent::ExitRequested { .. } | RunEvent::Exit = event {
            app_handle.state::<Sidecars>().kill_all();
        }
    });
}
