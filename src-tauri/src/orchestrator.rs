// Boots and supervises the two bundled Rust services (backend on :8080,
// document-server on :8090) as sidecar child processes, wiring them at
// generated local secrets and an app-data SQLite location, then reveals the
// main window once both answer their health check.
//
// Everything the services need that is normally supplied by `.env` /
// docker-compose is passed here as process environment on the sidecar
// `Command`. All writable state lives under the OS app-data directory; the
// bundled (read-only) Typst templates/fonts are copied there on first run.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, Instant};

const BACKEND_PORT: u16 = 8080;
const DOCUMENT_SERVER_PORT: u16 = 8090;
const LOOPBACK: &str = "127.0.0.1";

/// Child sidecar handles, killed when the app event loop exits.
#[derive(Default)]
pub struct Sidecars(pub Mutex<Vec<CommandChild>>);

impl Sidecars {
    pub fn kill_all(&self) {
        if let Ok(mut children) = self.0.lock() {
            for child in children.drain(..) {
                let _ = child.kill();
            }
        }
    }
}

/// Locally generated secrets, persisted so logins and the backend <->
/// document-server handshake survive restarts. Plaintext on disk: these are
/// secrets for services that only ever listen on this machine's loopback.
#[derive(Serialize, Deserialize)]
struct Secrets {
    /// Backend `JWT_SECRET`. Never regenerated once written — a change logs
    /// every user out and would break a future cloud-sync agent.
    jwt_secret: String,
    /// document-server `INTERNAL_API_KEY` and backend `DOCUMENT_SERVER_API_KEY`.
    internal_api_key: String,
}

fn hex64() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn load_or_create_secrets(config_path: &Path) -> std::io::Result<Secrets> {
    if let Ok(raw) = fs::read_to_string(config_path) {
        if let Ok(secrets) = serde_json::from_str::<Secrets>(&raw) {
            return Ok(secrets);
        }
    }
    let secrets = Secrets {
        jwt_secret: hex64(),
        internal_api_key: hex64(),
    };
    fs::write(config_path, serde_json::to_string_pretty(&secrets).unwrap())?;
    Ok(secrets)
}

/// Installation metadata tracking when the app was first installed/run on this computer.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InstallationRecord {
    pub installation_id: String,
    pub app_version: String,
    pub platform: String,
    pub installed_at: String,
    pub initial_setup_completed: bool,
    pub sample_data_loaded: Option<bool>,
}

pub fn load_or_create_installation(
    install_path: &Path,
    app_version: &str,
) -> std::io::Result<InstallationRecord> {
    if let Ok(raw) = fs::read_to_string(install_path) {
        if let Ok(record) = serde_json::from_str::<InstallationRecord>(&raw) {
            return Ok(record);
        }
    }
    let now = chrono::Utc::now().to_rfc3339();
    let record = InstallationRecord {
        installation_id: format!("inst_{}", &hex64()[..16]),
        app_version: app_version.to_string(),
        platform: std::env::consts::OS.to_string(),
        installed_at: now,
        initial_setup_completed: false,
        sample_data_loaded: None,
    };
    fs::write(install_path, serde_json::to_string_pretty(&record).unwrap())?;
    Ok(record)
}

pub fn update_installation_setup(
    install_path: &Path,
    sample_data_loaded: bool,
) -> std::io::Result<InstallationRecord> {
    let mut record = load_or_create_installation(install_path, "0.5.0")?;
    record.initial_setup_completed = true;
    record.sample_data_loaded = Some(sample_data_loaded);
    fs::write(install_path, serde_json::to_string_pretty(&record).unwrap())?;
    Ok(record)
}


/// Copy `src` into `dst` recursively, overwriting files that already exist
/// but never deleting extra files in `dst` (so a shop-added template
/// survives an app update that re-lays-down the bundled ones).
fn copy_dir_merge(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_merge(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Lay down (or refresh, on version change) the writable copy of the Typst
/// templates + fonts. document-server writes into its templates dir at
/// runtime, so it cannot point at the read-only resource bundle.
fn sync_render_assets(
    resource_dir: &Path,
    assets_dir: &Path,
    version: &str,
) -> std::io::Result<()> {
    let stamp = assets_dir.join(".version");
    if fs::read_to_string(&stamp).ok().as_deref() == Some(version) {
        return Ok(());
    }
    for name in ["templates", "fonts"] {
        let src = resource_dir.join("resources").join(name);
        if src.is_dir() {
            copy_dir_merge(&src, &assets_dir.join(name))?;
        }
    }
    fs::create_dir_all(assets_dir)?;
    fs::write(&stamp, version)?;
    Ok(())
}

/// One HTTP GET over a fresh TCP connection; true only on a `200` status
/// line. Both services bind their port only after they are fully ready
/// (document-server after the Typst warm-up, backend after seeding), so
/// this doubles as a "startup finished" signal.
async fn http_ok(port: u16, path: &str) -> bool {
    let Ok(mut stream) = tokio::net::TcpStream::connect((LOOPBACK, port)).await else {
        return false;
    };
    let req =
        format!("GET {path} HTTP/1.0\r\nHost: {LOOPBACK}:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 128];
    match stream.read(&mut buf).await {
        Ok(n) if n > 0 => {
            let head = String::from_utf8_lossy(&buf[..n]);
            head.starts_with("HTTP/1.") && head.split_whitespace().nth(1) == Some("200")
        }
        _ => false,
    }
}

async fn wait_healthy(name: &str, port: u16, path: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if http_ok(port, path).await {
            log::info!("{name} is ready on 127.0.0.1:{port}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{name} did not become healthy on 127.0.0.1:{port} within {}s",
                timeout.as_secs()
            ));
        }
        sleep(Duration::from_millis(400)).await;
    }
}

/// Best-effort kill of sidecar processes left behind by a previous run that
/// died without running its exit handler (SIGKILL, power loss, a panic).
/// Runs before we spawn, so a stale backend can't hold `:8080` and make this
/// launch fail. Safe because both binary names are unique to this app.
pub fn reap_orphan_sidecars() {
    let names = ["jana2u-backend", "jana2u-document-server"];
    for name in names {
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", &format!("{name}.exe")])
            .output();
        #[cfg(not(windows))]
        let _ = std::process::Command::new("pkill")
            .args(["-x", name])
            .output();
    }
}

/// Kill the sidecars and quit when the OS asks the process to stop
/// (SIGINT/SIGTERM on Unix, Ctrl-C on Windows) — the `RunEvent::ExitRequested`
/// path only covers a window close / `app.exit()`.
fn install_signal_handlers(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {},
                _ = int.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        log::info!("stop signal received — shutting down sidecars");
        app.state::<Sidecars>().kill_all();
        app.exit(0);
    });
}

/// Spawn one bundled service, forwarding its stdout/stderr into the app log.
fn spawn_sidecar(
    app: &AppHandle,
    bin: &str,
    cwd: PathBuf,
    envs: Vec<(&str, String)>,
) -> Result<CommandChild, String> {
    fs::create_dir_all(&cwd).map_err(|e| format!("create {}: {e}", cwd.display()))?;
    let mut command = app
        .shell()
        .sidecar(bin)
        .map_err(|e| format!("sidecar {bin}: {e}"))?
        .current_dir(cwd);
    for (k, v) in envs {
        command = command.env(k, v);
    }
    let (mut rx, child) = command.spawn().map_err(|e| format!("spawn {bin}: {e}"))?;
    let tag = bin.to_string();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line) => {
                    log::info!("[{tag}] {}", String::from_utf8_lossy(&line).trim_end())
                }
                CommandEvent::Stderr(line) => {
                    log::warn!("[{tag}] {}", String::from_utf8_lossy(&line).trim_end())
                }
                CommandEvent::Error(err) => log::error!("[{tag}] {err}"),
                CommandEvent::Terminated(payload) => {
                    log::error!("[{tag}] exited: {:?}", payload.code)
                }
                _ => {}
            }
        }
    });
    Ok(child)
}

/// Full startup sequence, run on a background task from `setup`.
pub async fn run(app: AppHandle) -> Result<(), String> {
    reap_orphan_sidecars();
    install_signal_handlers(app.clone());

    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("no app data dir: {e}"))?;
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|e| format!("no resource dir: {e}"))?;

    let db_dir = data_dir.join("db");
    let assets_dir = data_dir.join("assets");
    let generated_docs = data_dir.join("generated_documents");
    for dir in [&db_dir, &assets_dir, &generated_docs] {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }

    let secrets = load_or_create_secrets(&data_dir.join("config.json"))
        .map_err(|e| format!("secrets: {e}"))?;
    // App version is the single source of truth in package.json (tauri.conf.json
    // -> "version": "../package.json"); read it at runtime rather than baking in
    // CARGO_PKG_VERSION, which is no longer release-bumped.
    let version = app.package_info().version.to_string();
    let installation = load_or_create_installation(&data_dir.join("installation.json"), &version)
        .map_err(|e| format!("installation record: {e}"))?;

    sync_render_assets(&resource_dir, &assets_dir, &version)
        .map_err(|e| format!("render assets: {e}"))?;

    // --- document-server -------------------------------------------------
    let doc_child = spawn_sidecar(
        &app,
        "jana2u-document-server",
        db_dir.join("document-server"),
        vec![
            ("BIND_ADDR", LOOPBACK.into()),
            ("PORT", DOCUMENT_SERVER_PORT.to_string()),
            ("DATABASE_URL", "sqlite://document_server.db".into()),
            (
                "TEMPLATES_DIR",
                assets_dir.join("templates").to_string_lossy().into_owned(),
            ),
            (
                "FONTS_DIR",
                assets_dir.join("fonts").to_string_lossy().into_owned(),
            ),
            ("INTERNAL_API_KEY", secrets.internal_api_key.clone()),
            ("REMOTE_IMAGE_FETCH_ENABLED", "false".into()),
            (
                "RUST_LOG",
                "document_server=info,tower_http=warn,warn".into(),
            ),
        ],
    )?;
    app.state::<Sidecars>().0.lock().unwrap().push(doc_child);
    wait_healthy(
        "document-server",
        DOCUMENT_SERVER_PORT,
        "/api/health",
        Duration::from_secs(45),
    )
    .await?;

    // --- backend -------------------------------------------------------
    let backend_child = spawn_sidecar(
        &app,
        "jana2u-backend",
        db_dir.join("backend"),
        vec![
            ("BIND_ADDR", LOOPBACK.into()),
            ("PORT", BACKEND_PORT.to_string()),
            ("DATABASE_TYPE", "sqlite".into()),
            ("DATABASE_URL", "sqlite://pos.db?mode=rwc".into()),
            ("JWT_SECRET", secrets.jwt_secret.clone()),
            ("JWT_EXPIRY_HOURS", "12".into()),
            (
                "DOCUMENT_SERVER_URL",
                format!("http://{LOOPBACK}:{DOCUMENT_SERVER_PORT}"),
            ),
            ("DOCUMENT_SERVER_API_KEY", secrets.internal_api_key.clone()),
            (
                "GENERATED_DOCUMENTS_DIR",
                generated_docs.to_string_lossy().into_owned(),
            ),
            ("RETURN_WINDOW_DAYS", "30".into()),
            ("AUTO_SEED", "false".into()),
            ("INSTALLATION_ID", installation.installation_id.clone()),
            ("APP_VERSION", version.clone()),
            ("PLATFORM", std::env::consts::OS.to_string()),
            (
                "RUST_LOG",
                "jana2u_pos_backend=info,tower_http=warn,warn".into(),
            ),
        ],
    )?;
    app.state::<Sidecars>()
        .0
        .lock()
        .unwrap()
        .push(backend_child);
    wait_healthy(
        "backend",
        BACKEND_PORT,
        "/api/health",
        Duration::from_secs(60),
    )
    .await?;

    reveal_main_window(&app)?;
    Ok(())
}

/// Create the main window only now — after both services answer their health
/// check — so the frontend's startup auth probe never races an unready backend.
fn reveal_main_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window("main").is_none() {
        let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
            .title("Jana2U POS")
            .inner_size(1400.0, 900.0)
            .min_inner_size(1024.0, 640.0)
            .center()
            .resizable(true);

        #[cfg(debug_assertions)]
        let builder = builder.devtools(true);

        builder
            .build()
            .map_err(|e| format!("create main window: {e}"))?;
    }
    if let Some(splash) = app.get_webview_window("splashscreen") {
        let _ = splash.close();
    }
    Ok(())
}

/// Startup failed: surface it, then quit (a half-started POS is worse than a
/// clear error).
pub fn fatal(app: &AppHandle, message: &str) {
    log::error!("startup failed: {message}");
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
    app.dialog()
        .message(format!(
            "Jana2U POS could not start.\n\n{message}\n\nSee the log for details."
        ))
        .kind(MessageDialogKind::Error)
        .title("Jana2U POS")
        .blocking_show();
    app.state::<Sidecars>().kill_all();
    app.exit(1);
}
