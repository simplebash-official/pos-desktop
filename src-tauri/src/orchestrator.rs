// Boots and supervises the bundled Rust services (backend on :8080,
// document-server on :8090) as sidecar child processes, wiring them at
// generated local secrets and an app-data SQLite location, then reveals the
// main window once every one answers its health check.
//
// Everything the services need that is normally supplied by `.env` /
// docker-compose is passed here as process environment on the sidecar
// `Command`. All writable state lives under the OS app-data directory; the
// bundled (read-only) Typst templates/fonts are copied there on first run.
//
// Sidecars are declared once in `SIDECARS`. Adding a service = one entry
// there (+ `externalBin`, capabilities, build script, NSIS taskkill). Its
// stdout is ingested into the unified log automatically.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, Instant};

use crate::branding;
use crate::logging::hub::{self, LogConfig};
use crate::logging::ingest::{self, Stream};
use crate::logging::{redact, Level, LogEvent};

pub(crate) const BACKEND_PORT: u16 = 8080;
const DOCUMENT_SERVER_PORT: u16 = 8090;
pub(crate) const LOOPBACK: &str = "127.0.0.1";

/// Child sidecar handles, killed when the app event loop exits.
#[derive(Default)]
pub struct Sidecars {
    children: Mutex<Vec<(&'static str, CommandChild)>>,
    /// Set once we start stopping them, so their exit is logged as expected
    /// rather than as a crash.
    stopping: AtomicBool,
}

impl Sidecars {
    fn push(&self, name: &'static str, child: CommandChild) {
        self.children.lock().unwrap().push((name, child));
    }

    /// `(log source, pid)` for every running sidecar — used by the benchmark
    /// to attribute CPU and memory per service.
    pub fn pids(&self) -> Vec<(&'static str, u32)> {
        self.children
            .lock()
            .map(|children| {
                children
                    .iter()
                    .map(|(bin, child)| (source_for_bin(bin), child.pid()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Write one control line to every sidecar's stdin (see `docs/logging.md`).
    pub fn broadcast(&self, line: &[u8]) {
        if let Ok(mut children) = self.children.lock() {
            for (bin, child) in children.iter_mut() {
                if let Err(err) = child.write(line) {
                    LogEvent::shell("sidecar", "control_failed")
                        .level(Level::Warn)
                        .msg(format!("could not send a control line to {bin}: {err}"))
                        .data(json!({ "sidecar": bin }))
                        .emit();
                }
            }
        }
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    pub fn kill_all(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        if let Ok(mut children) = self.children.lock() {
            for (name, child) in children.drain(..) {
                let pid = child.pid();
                let result = child.kill();
                LogEvent::shell("sidecar", "kill")
                    .level(if result.is_ok() { Level::Info } else { Level::Warn })
                    .msg(format!("stopping {name} (pid {pid})"))
                    .data(json!({ "sidecar": name, "pid": pid, "error": result.err().map(|e| e.to_string()) }))
                    .emit();
            }
        }
    }
}

/// Locally generated secrets, persisted so logins and the backend <->
/// document-server handshake survive restarts. Plaintext on disk (these are
/// secrets for services that only ever listen on this machine's loopback),
/// but readable by the current user only — anyone holding `jwt_secret` can
/// mint an Admin token for the local backend.
#[derive(Serialize, Deserialize)]
struct Secrets {
    /// Backend `JWT_SECRET`. Never regenerated once written — a change logs
    /// every user out and would break a future cloud-sync agent.
    jwt_secret: String,
    /// document-server `INTERNAL_API_KEY` and backend `DOCUMENT_SERVER_API_KEY`.
    internal_api_key: String,
}

/// The backend `JWT_SECRET` from `config.json`, for the sync agent to mint its
/// short-lived service token. `None` until the orchestrator has created it.
pub fn read_jwt_secret(data_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(data_dir.join("config.json")).ok()?;
    serde_json::from_str::<Secrets>(&raw)
        .ok()
        .map(|s| s.jwt_secret)
        .filter(|s| !s.is_empty())
}

fn hex64() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Narrows `path` to owner read/write (0600). On Windows the file already
/// inherits the per-user ACL of the app-data directory, so there is nothing
/// to do. Best effort on an existing file: a failure is logged, not fatal.
fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn load_or_create_secrets(config_path: &Path) -> std::io::Result<Secrets> {
    if let Ok(raw) = fs::read_to_string(config_path) {
        if let Ok(secrets) = serde_json::from_str::<Secrets>(&raw) {
            // Installs from before this check wrote the file world-readable.
            if let Err(err) = restrict_to_owner(config_path) {
                LogEvent::shell("lifecycle", "secrets.chmod_failed")
                    .msg("could not restrict local secrets file to the current user")
                    .data(json!({ "path": config_path, "error": err.to_string() }))
                    .emit();
            }
            return Ok(secrets);
        }
    }
    let secrets = Secrets {
        jwt_secret: hex64(),
        internal_api_key: hex64(),
    };
    fs::write(config_path, serde_json::to_string_pretty(&secrets).unwrap())?;
    restrict_to_owner(config_path)?;
    LogEvent::shell("lifecycle", "secrets.generated")
        .msg("generated new local service secrets")
        .data(json!({ "path": config_path }))
        .emit();
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
    let now = chrono::Local::now().to_rfc3339();
    let record = InstallationRecord {
        installation_id: format!("inst_{}", &hex64()[..16]),
        app_version: app_version.to_string(),
        platform: std::env::consts::OS.to_string(),
        installed_at: now,
        initial_setup_completed: false,
        sample_data_loaded: None,
    };
    fs::write(install_path, serde_json::to_string_pretty(&record).unwrap())?;
    if let Some(hub) = hub::hub() {
        hub.set_installation_id(&record.installation_id);
    }
    // The very first moment this computer runs the app (on macOS/Linux there
    // is no installer step, so this is the earliest record there is).
    LogEvent::shell("lifecycle", "app.install.first_run")
        .msg(format!(
            "first run of {} {app_version} on this computer",
            branding::PRODUCT_NAME
        ))
        .data(json!({ "installation": record, "path": install_path }))
        .emit();
    Ok(record)
}

pub fn update_installation_setup(
    install_path: &Path,
    app_version: &str,
    sample_data_loaded: bool,
) -> std::io::Result<InstallationRecord> {
    let mut record = load_or_create_installation(install_path, app_version)?;
    record.initial_setup_completed = true;
    record.sample_data_loaded = Some(sample_data_loaded);
    fs::write(install_path, serde_json::to_string_pretty(&record).unwrap())?;
    LogEvent::shell("lifecycle", "app.install.setup_completed")
        .data(json!({ "installation_id": record.installation_id, "sample_data_loaded": sample_data_loaded }))
        .emit();
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
    let previous = fs::read_to_string(&stamp).ok();
    if previous.as_deref() == Some(version) {
        return Ok(());
    }
    let started = Instant::now();
    for name in ["templates", "fonts"] {
        let src = resource_dir.join("resources").join(name);
        if src.is_dir() {
            copy_dir_merge(&src, &assets_dir.join(name))?;
        }
    }
    fs::create_dir_all(assets_dir)?;
    fs::write(&stamp, version)?;
    LogEvent::shell("lifecycle", "assets.synced")
        .msg(format!("render assets refreshed for {version}"))
        .data(json!({ "from_version": previous, "to_version": version, "duration_ms": started.elapsed().as_millis() }))
        .emit();
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
    let started = Instant::now();
    let deadline = started + timeout;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        if http_ok(port, path).await {
            LogEvent::shell("sidecar", "ready")
                .msg(format!("{name} is ready on {LOOPBACK}:{port}"))
                .data(json!({
                    "sidecar": name, "port": port, "attempts": attempts,
                    "time_to_ready_ms": started.elapsed().as_millis(),
                }))
                .emit();
            return Ok(());
        }
        if Instant::now() >= deadline {
            LogEvent::shell("sidecar", "health_timeout")
                .level(Level::Error)
                .data(json!({ "sidecar": name, "port": port, "attempts": attempts, "timeout_s": timeout.as_secs() }))
                .emit();
            return Err(format!(
                "{name} did not become healthy on {LOOPBACK}:{port} within {}s",
                timeout.as_secs()
            ));
        }
        sleep(Duration::from_millis(400)).await;
    }
}

/// Best-effort kill of sidecar processes left behind by a previous run that
/// died without running its exit handler (SIGKILL, power loss, a panic).
/// Runs before we spawn, so a stale backend can't hold `:8080` and make this
/// launch fail. Safe because every binary name is unique to this app.
pub fn reap_orphan_sidecars() {
    for spec in SIDECARS {
        let name = spec.bin;
        #[cfg(windows)]
        let output = std::process::Command::new("taskkill")
            .args(["/F", "/IM", &format!("{name}.exe")])
            .output();
        #[cfg(not(windows))]
        let output = std::process::Command::new("pkill")
            .args(["-x", name])
            .output();
        // pkill exits 0 only when it matched (= an orphan really was left).
        if let Ok(out) = output {
            if out.status.success() {
                LogEvent::shell("sidecar", "orphan_reaped")
                    .level(Level::Warn)
                    .msg(format!("killed a leftover {name} from a previous run"))
                    .data(json!({ "sidecar": name }))
                    .emit();
            }
        }
    }
}

/// Kill the sidecars and quit when the OS asks the process to stop
/// (SIGINT/SIGTERM on Unix, Ctrl-C on Windows) — the `RunEvent::ExitRequested`
/// path only covers a window close / `app.exit()`.
fn install_signal_handlers(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        #[cfg(unix)]
        let signal_name = {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = int.recv() => "SIGINT",
            }
        };
        #[cfg(not(unix))]
        let signal_name = {
            let _ = tokio::signal::ctrl_c().await;
            "CTRL_C"
        };
        LogEvent::shell("lifecycle", "signal")
            .msg(format!("{signal_name} received — shutting down sidecars"))
            .data(json!({ "signal": signal_name }))
            .emit();
        app.state::<Sidecars>().kill_all();
        if let Some(hub) = hub::hub() {
            hub.flush_blocking(Duration::from_secs(2));
        }
        app.exit(0);
    });
}

// ---------------------------------------------------------------------------
// Sidecar table
// ---------------------------------------------------------------------------

/// Log source label for a binary name, from the `SIDECARS` table.
fn source_for_bin(bin: &str) -> &'static str {
    SIDECARS
        .iter()
        .find(|spec| spec.bin == bin)
        .map_or("sidecar", |spec| spec.source)
}

/// Everything a sidecar's environment may depend on.
pub struct SidecarCtx {
    db_dir: PathBuf,
    assets_dir: PathBuf,
    generated_docs: PathBuf,
    secrets: Secrets,
    installation: InstallationRecord,
    version: String,
    log: LogConfig,
    boot_id: String,
}

pub struct SidecarSpec {
    /// Binary name in `externalBin` (without the target-triple suffix).
    pub bin: &'static str,
    /// Log `source` label — also its file name under `logs/<day>/`.
    pub source: &'static str,
    pub port: u16,
    pub health_path: &'static str,
    pub health_timeout: Duration,
    /// Working directory under `db/`.
    pub cwd: &'static str,
    envs: fn(&SidecarCtx) -> Vec<(&'static str, String)>,
}

/// Start order matters: the backend checks the document-server on boot.
pub const SIDECARS: &[SidecarSpec] = &[
    SidecarSpec {
        bin: "simplebash-document-server",
        source: "document-server",
        port: DOCUMENT_SERVER_PORT,
        health_path: "/api/health",
        health_timeout: Duration::from_secs(45),
        cwd: "document-server",
        envs: |c| {
            vec![
                ("DATABASE_URL", "sqlite://document_server.db".into()),
                (
                    "TEMPLATES_DIR",
                    c.assets_dir
                        .join("templates")
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "FONTS_DIR",
                    c.assets_dir.join("fonts").to_string_lossy().into_owned(),
                ),
                ("INTERNAL_API_KEY", c.secrets.internal_api_key.clone()),
                ("REMOTE_IMAGE_FETCH_ENABLED", "false".into()),
            ]
        },
    },
    SidecarSpec {
        bin: "simplebash-backend",
        source: "backend",
        port: BACKEND_PORT,
        health_path: "/api/health",
        health_timeout: Duration::from_secs(60),
        cwd: "backend",
        envs: |c| {
            vec![
                ("DATABASE_TYPE", "sqlite".into()),
                ("DATABASE_URL", "sqlite://pos.db?mode=rwc".into()),
                ("JWT_SECRET", c.secrets.jwt_secret.clone()),
                ("JWT_EXPIRY_HOURS", "12".into()),
                (
                    "DOCUMENT_SERVER_URL",
                    format!("http://{LOOPBACK}:{DOCUMENT_SERVER_PORT}"),
                ),
                (
                    "DOCUMENT_SERVER_API_KEY",
                    c.secrets.internal_api_key.clone(),
                ),
                (
                    "GENERATED_DOCUMENTS_DIR",
                    c.generated_docs.to_string_lossy().into_owned(),
                ),
                ("RETURN_WINDOW_DAYS", "30".into()),
                ("AUTO_SEED", "false".into()),
                ("INSTALLATION_ID", c.installation.installation_id.clone()),
                ("APP_VERSION", c.version.clone()),
                ("PLATFORM", std::env::consts::OS.to_string()),
            ]
        },
    },
];

/// Environment every sidecar gets: loopback binding plus the unified-log
/// contract (JSON lines on stdout, shared boot id, body/SQL verbosity).
fn common_envs(spec: &SidecarSpec, ctx: &SidecarCtx) -> Vec<(&'static str, String)> {
    vec![
        ("BIND_ADDR", LOOPBACK.into()),
        ("PORT", spec.port.to_string()),
        ("LOG_FORMAT", "json".into()),
        ("LOG_SOURCE", spec.source.into()),
        ("LOG_BOOT_ID", ctx.boot_id.clone()),
        ("LOG_HTTP_BODIES", ctx.log.http_bodies.to_string()),
        ("LOG_BODY_CAP_BYTES", ctx.log.body_cap_bytes.to_string()),
        ("LOG_SQL", ctx.log.sql.clone()),
        ("NO_COLOR", "1".into()),
        // `tower_http`'s own trace lines duplicate the services' structured
        // `http/request` + `http/response` events, so hold it at warn here.
        ("RUST_LOG", "info,tower_http=warn".into()),
    ]
}

/// Spawn one bundled service, ingesting its stdout/stderr into the unified log.
fn spawn_sidecar(
    app: &AppHandle,
    spec: &'static SidecarSpec,
    ctx: &SidecarCtx,
) -> Result<CommandChild, String> {
    let cwd = ctx.db_dir.join(spec.cwd);
    fs::create_dir_all(&cwd).map_err(|e| format!("create {}: {e}", cwd.display()))?;
    let mut envs = common_envs(spec, ctx);
    envs.extend((spec.envs)(ctx));

    let mut command = app
        .shell()
        .sidecar(spec.bin)
        .map_err(|e| format!("sidecar {}: {e}", spec.bin))?
        .current_dir(&cwd);
    let mut env_log = serde_json::Map::new();
    for (k, v) in envs {
        let shown = if redact::is_sensitive_key(k) {
            redact::REDACTED.to_string()
        } else {
            v.clone()
        };
        env_log.insert(k.to_string(), shown.into());
        command = command.env(k, v);
    }
    let (mut rx, child) = command.spawn().map_err(|e| {
        LogEvent::shell("sidecar", "spawn_failed")
            .level(Level::Error)
            .data(json!({ "sidecar": spec.bin, "error": e.to_string() }))
            .emit();
        format!("spawn {}: {e}", spec.bin)
    })?;
    LogEvent::shell("sidecar", "spawned")
        .msg(format!("started {} (pid {})", spec.bin, child.pid()))
        .data(json!({ "sidecar": spec.bin, "pid": child.pid(), "cwd": cwd, "port": spec.port, "env": env_log }))
        .emit();

    let app_for_task = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line) => {
                    if let Some(e) = ingest::from_sidecar_line(spec.source, Stream::Stdout, &line) {
                        e.emit();
                    }
                }
                CommandEvent::Stderr(line) => {
                    if let Some(e) = ingest::from_sidecar_line(spec.source, Stream::Stderr, &line) {
                        e.emit();
                    }
                }
                CommandEvent::Error(err) => LogEvent::shell("sidecar", "io_error")
                    .level(Level::Error)
                    .msg(format!("[{}] {err}", spec.bin))
                    .data(json!({ "sidecar": spec.bin }))
                    .emit(),
                CommandEvent::Terminated(payload) => {
                    let expected = app_for_task.state::<Sidecars>().is_stopping();
                    LogEvent::shell("sidecar", "exited")
                        .level(if expected { Level::Info } else { Level::Error })
                        .msg(if expected {
                            format!("{} stopped", spec.bin)
                        } else {
                            format!("{} exited unexpectedly", spec.bin)
                        })
                        .data(json!({
                            "sidecar": spec.bin, "code": payload.code,
                            "signal": payload.signal, "expected": expected,
                        }))
                        .emit();
                }
                _ => {}
            }
        }
    });
    Ok(child)
}

/// Full startup sequence, run on a background task from `setup`.
pub async fn run(app: AppHandle) -> Result<(), String> {
    let boot_started = Instant::now();
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
    let hub = hub::init();
    hub.set_installation_id(&installation.installation_id);

    sync_render_assets(&resource_dir, &assets_dir, &version)
        .map_err(|e| format!("render assets: {e}"))?;

    let ctx = SidecarCtx {
        db_dir,
        assets_dir,
        generated_docs,
        secrets,
        installation,
        version,
        log: hub.config(),
        boot_id: hub.context().boot_id,
    };

    for spec in SIDECARS {
        let child = spawn_sidecar(&app, spec, &ctx)?;
        app.state::<Sidecars>().push(spec.bin, child);
        wait_healthy(
            spec.source,
            spec.port,
            spec.health_path,
            spec.health_timeout,
        )
        .await?;
    }

    reveal_main_window(&app)?;
    LogEvent::shell("lifecycle", "app.ready")
        .msg("all services healthy, main window shown")
        .data(json!({ "startup_ms": boot_started.elapsed().as_millis() }))
        .emit();
    Ok(())
}

/// Create the main window only now — after every service answers its health
/// check — so the frontend's startup auth probe never races an unready backend.
fn reveal_main_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window("main").is_none() {
        let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
            .title(app.package_info().name.clone())
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
    let logs_dir = hub::hub()
        .and_then(|h| h.logs_dir().cloned())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "the app data folder".into());
    LogEvent::shell("lifecycle", "startup_failed")
        .level(Level::Fatal)
        .msg(format!("startup failed: {message}"))
        .emit();
    if let Some(hub) = hub::hub() {
        hub.flush_blocking(Duration::from_secs(2));
    }
    use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
    let product_name = &app.package_info().name;
    app.dialog()
        .message(format!(
            "{product_name} could not start.\n\n{message}\n\nDetails are in the log folder:\n{logs_dir}"
        ))
        .kind(MessageDialogKind::Error)
        .title(product_name.clone())
        .blocking_show();
    app.state::<Sidecars>().kill_all();
    app.exit(1);
}
