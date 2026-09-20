// Optional cloud account link (desktop side). The POS works fully offline
// without any of this; when a cloud URL is configured the owner may register or
// sign in, link this installation as a device, and send a minimal usage ping.
// The feature is OFF unless a base URL exists (`CLOUD_API_URL` at build time,
// or `apiUrl` in `cloud.json`), and every command then answers `CLOUD_DISABLED`.
// Only the shell talks to the cloud; tokens and the device key live in the OS
// keychain (`store::SecretStore`), never in a file, a log or the webview.

pub(crate) mod api;
mod store;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};

use crate::logging::{CommandLog, Level, LogEvent};
pub use api::CloudError;
use api::{Api, ApprovedLink, LinkStartResponse, PollResult};
use store::{CloudFile, KeyringStore, SecretStore, KEY_ACCESS, KEY_DEVICE, KEY_REFRESH};

/// Compile-time default; `None` disables the feature for builds without it.
const BUILD_CLOUD_URL: Option<&str> = option_env!("CLOUD_API_URL");
/// Optional separate host for the POS sync API (see `CloudFile::sync_api_url`).
const BUILD_SYNC_URL: Option<&str> = option_env!("CLOUD_SYNC_API_URL");

/// What the sync agent needs to talk to the POS cloud as this device.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncSession {
    pub sync_base: String,
    pub device_id: String,
    pub tenant_id: String,
}

struct PendingLink {
    device_code: String,
    user_code: String,
    verification_url: String,
    interval: u64,
    expires_in: u64,
    account_email: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingLinkView {
    pub user_code: String,
    pub verification_url: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// What the webview sees. Never contains a token, device code or key.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudStateView {
    pub enabled: bool,
    pub linked: bool,
    pub account_email: Option<String>,
    pub tenant_id: Option<String>,
    pub shop_code: Option<String>,
    pub device_id: Option<String>,
    pub linked_at: Option<String>,
    pub telemetry_enabled: bool,
    pub pending_link: Option<PendingLinkView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterResult {
    pub email: String,
    pub verification_required: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum LinkPollView {
    Pending,
    Linked { state: CloudStateView },
}

pub struct CloudState {
    http: reqwest::Client,
    secrets: Arc<dyn SecretStore>,
    dir: PathBuf,
    version: String,
    build_url: Option<String>,
    pending: Mutex<Option<PendingLink>>,
}

/// Payload of the usage ping. Strictly these fields - nothing about the shop,
/// its customers or its data.
pub fn telemetry_payload(installation_id: &str, version: &str, tenant_id: Option<&str>) -> Value {
    let mut payload = json!({
        "installationId": installation_id,
        "version": version,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });
    if let Some(t) = tenant_id {
        payload["tenantId"] = json!(t);
    }
    payload
}

fn normalize_url(raw: &str) -> Option<String> {
    let url = raw.trim().trim_end_matches('/');
    (url.starts_with("https://") || url.starts_with("http://")).then(|| url.to_string())
}

fn keychain_err(e: String) -> CloudError {
    CloudError::new("KEYCHAIN_ERROR", e, 0)
}

impl CloudState {
    pub fn new(dir: PathBuf, version: String) -> Self {
        Self::with_parts(
            dir,
            version,
            BUILD_CLOUD_URL.map(str::to_string),
            Arc::new(KeyringStore),
        )
    }

    fn with_parts(
        dir: PathBuf,
        version: String,
        build_url: Option<String>,
        secrets: Arc<dyn SecretStore>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            secrets,
            dir,
            version,
            build_url,
            pending: Mutex::new(None),
        }
    }

    fn base_url(&self, file: &CloudFile) -> Option<String> {
        file.api_url
            .as_deref()
            .and_then(normalize_url)
            .or_else(|| self.build_url.as_deref().and_then(normalize_url))
    }

    /// Loads `cloud.json` and the base URL, or `CLOUD_DISABLED`.
    fn enabled(&self) -> Result<(String, CloudFile), CloudError> {
        let file = CloudFile::load(&self.dir);
        let base = self.base_url(&file).ok_or_else(CloudError::disabled)?;
        Ok((base, file))
    }

    fn installation_id(&self) -> Option<String> {
        installation_id_from(&self.dir)
    }

    pub fn view(&self) -> CloudStateView {
        let file = CloudFile::load(&self.dir);
        let pending = self.pending.lock().unwrap();
        CloudStateView {
            enabled: self.base_url(&file).is_some(),
            linked: file.is_linked(),
            telemetry_enabled: file.telemetry_enabled.unwrap_or(true),
            account_email: file.account_email,
            tenant_id: file.tenant_id,
            shop_code: file.shop_code,
            device_id: file.device_id,
            linked_at: file.linked_at,
            pending_link: pending.as_ref().map(|p| PendingLinkView {
                user_code: p.user_code.clone(),
                verification_url: p.verification_url.clone(),
                interval: p.interval,
                expires_in: p.expires_in,
            }),
        }
    }

    /// Where `/api/sync/*` lives, plus this device's identity; errors with
    /// `CLOUD_DISABLED` / `NOT_LINKED` so the agent simply stays idle.
    pub fn sync_session(&self) -> Result<SyncSession, CloudError> {
        let (base, file) = self.enabled()?;
        let sync_base = file
            .sync_api_url
            .as_deref()
            .and_then(normalize_url)
            .or_else(|| BUILD_SYNC_URL.and_then(normalize_url))
            .unwrap_or(base);
        match (file.device_id, file.tenant_id) {
            (Some(device_id), Some(tenant_id)) => Ok(SyncSession {
                sync_base,
                device_id,
                tenant_id,
            }),
            _ => Err(CloudError::new(
                "NOT_LINKED",
                "this device is not linked",
                0,
            )),
        }
    }

    /// A bearer token for the POS cloud. `force_refresh` (after a 401, or when
    /// none is stored) rotates it through the identity service first.
    pub async fn access_token(&self, force_refresh: bool) -> Result<String, CloudError> {
        if !force_refresh {
            if let Some(token) = self.secrets.get(KEY_ACCESS).map_err(keychain_err)? {
                return Ok(token);
            }
        }
        let (base, _) = self.enabled()?;
        let api = Api {
            http: &self.http,
            base: &base,
        };
        self.refresh_tokens(&api).await
    }

    fn store_tokens(&self, access: &str, refresh: &str) -> Result<(), CloudError> {
        self.secrets.set(KEY_ACCESS, access).map_err(keychain_err)?;
        self.secrets.set(KEY_REFRESH, refresh).map_err(keychain_err)
    }

    /// Base64 public half of this installation's Ed25519 key, creating and
    /// keychain-storing the key on first use.
    fn device_public_key(&self) -> Result<String, CloudError> {
        let secret = match self.secrets.get(KEY_DEVICE).map_err(keychain_err)? {
            Some(existing) => existing,
            None => {
                let key = SigningKey::generate(&mut OsRng);
                let encoded = B64.encode(key.to_bytes());
                self.secrets
                    .set(KEY_DEVICE, &encoded)
                    .map_err(keychain_err)?;
                encoded
            }
        };
        let bytes: [u8; 32] = B64
            .decode(secret)
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| keychain_err("stored device key is corrupt".into()))?;
        Ok(B64.encode(SigningKey::from_bytes(&bytes).verifying_key().to_bytes()))
    }

    pub async fn register(
        &self,
        email: &str,
        password: &str,
        owner_name: &str,
        store_name: &str,
    ) -> Result<RegisterResult, CloudError> {
        let (base, _) = self.enabled()?;
        let api = Api {
            http: &self.http,
            base: &base,
        };
        let response = api
            .register(email, password, owner_name, store_name)
            .await?;
        let verification_required = ["verificationRequired", "emailVerificationRequired"]
            .iter()
            .any(|k| response.get(k).and_then(Value::as_bool).unwrap_or(false));
        Ok(RegisterResult {
            email: email.to_string(),
            verification_required,
        })
    }

    /// Starts the link handshake and remembers the (secret) device code in memory.
    async fn begin_link(
        &self,
        api: &Api<'_>,
        tenant_id: Option<&str>,
        bearer: Option<&str>,
        account_email: Option<String>,
    ) -> Result<LinkStartResponse, CloudError> {
        let installation_id = self
            .installation_id()
            .ok_or_else(|| CloudError::new("NO_INSTALLATION", "installation record missing", 0))?;
        let public_key = self.device_public_key()?;
        let start = api
            .link_start(
                &installation_id,
                &device_name(),
                std::env::consts::OS,
                &self.version,
                &public_key,
                tenant_id,
                bearer,
            )
            .await?;
        *self.pending.lock().unwrap() = Some(PendingLink {
            device_code: start.device_code.clone(),
            user_code: start.user_code.clone(),
            verification_url: start.verification_url.clone(),
            interval: start.interval,
            expires_in: start.expires_in,
            account_email,
        });
        Ok(start)
    }

    fn finalize_link(
        &self,
        approved: ApprovedLink,
        account_email: Option<String>,
    ) -> Result<(), CloudError> {
        self.store_tokens(&approved.access_token, &approved.refresh_token)?;
        let mut file = CloudFile::load(&self.dir);
        file.device_id = Some(approved.device_id);
        file.tenant_id = Some(approved.tenant_id);
        file.shop_code = Some(approved.shop_code).filter(|s| !s.is_empty());
        file.account_email = account_email;
        file.linked_at = Some(chrono::Utc::now().to_rfc3339());
        file.save(&self.dir)
            .map_err(|e| CloudError::new("STATE_WRITE_FAILED", e, 0))?;
        *self.pending.lock().unwrap() = None;
        Ok(())
    }

    /// In-app path: sign in with email/password, then link this device. An
    /// authenticated start is normally approved immediately, so poll briefly;
    /// if it is still pending the UI keeps polling via `link_poll`.
    pub async fn login_and_link(
        &self,
        email: &str,
        password: &str,
        tenant_id: Option<String>,
    ) -> Result<CloudStateView, CloudError> {
        let (base, _) = self.enabled()?;
        let api = Api {
            http: &self.http,
            base: &base,
        };
        let login = api.login(email, password).await?;
        let tenant = match tenant_id {
            Some(id) => login.tenants.iter().find(|t| t.tenant_id == id),
            None => login.tenants.first(),
        }
        .cloned()
        .ok_or_else(|| CloudError::new("NO_TENANT", "this account has no shop to link", 0))?;
        self.store_tokens(&login.access_token, &login.refresh_token)?;

        let account_email = Some(if login.account.email.is_empty() {
            email.to_string()
        } else {
            login.account.email.clone()
        });
        let start = self
            .begin_link(
                &api,
                Some(&tenant.tenant_id),
                Some(&login.access_token),
                account_email.clone(),
            )
            .await?;
        for _ in 0..3 {
            match api.link_poll(&start.device_code).await? {
                PollResult::Approved(approved) => {
                    self.finalize_link(approved, account_email)?;
                    return Ok(self.view());
                }
                PollResult::Pending => {
                    tokio::time::sleep(Duration::from_secs(start.interval.min(3))).await;
                }
            }
        }
        Ok(self.view())
    }

    /// Browser-approval path: returns the code the owner enters on the site.
    pub async fn link_start(&self) -> Result<PendingLinkView, CloudError> {
        let (base, _) = self.enabled()?;
        let api = Api {
            http: &self.http,
            base: &base,
        };
        let start = self.begin_link(&api, None, None, None).await?;
        Ok(PendingLinkView {
            user_code: start.user_code,
            verification_url: start.verification_url,
            interval: start.interval,
            expires_in: start.expires_in,
        })
    }

    pub async fn link_poll(&self) -> Result<LinkPollView, CloudError> {
        let (base, _) = self.enabled()?;
        let (device_code, account_email) = {
            let pending = self.pending.lock().unwrap();
            let p = pending
                .as_ref()
                .ok_or_else(|| CloudError::new("NO_PENDING_LINK", "no link in progress", 0))?;
            (p.device_code.clone(), p.account_email.clone())
        };
        let api = Api {
            http: &self.http,
            base: &base,
        };
        match api.link_poll(&device_code).await? {
            PollResult::Pending => Ok(LinkPollView::Pending),
            PollResult::Approved(approved) => {
                self.finalize_link(approved, account_email)?;
                Ok(LinkPollView::Linked { state: self.view() })
            }
        }
    }

    async fn refresh_tokens(&self, api: &Api<'_>) -> Result<String, CloudError> {
        let refresh = self
            .secrets
            .get(KEY_REFRESH)
            .map_err(keychain_err)?
            .ok_or_else(|| CloudError::new("NOT_SIGNED_IN", "no refresh token", 0))?;
        let pair = api.refresh(&refresh).await?;
        self.store_tokens(&pair.access_token, &pair.refresh_token)?;
        Ok(pair.access_token)
    }

    /// Forgets the link locally (always) and asks the cloud to drop this device
    /// (best effort - an offline unlink must still work).
    pub async fn unlink(&self) -> Result<CloudStateView, CloudError> {
        let (base, mut file) = self.enabled()?;
        if let Some(device_id) = file.device_id.clone() {
            let api = Api {
                http: &self.http,
                base: &base,
            };
            let outcome = async {
                let token = self
                    .secrets
                    .get(KEY_ACCESS)
                    .map_err(keychain_err)?
                    .ok_or_else(|| CloudError::new("NOT_SIGNED_IN", "no access token", 0))?;
                if api.delete_device(&device_id, &token).await? {
                    return Ok(());
                }
                let fresh = self.refresh_tokens(&api).await?;
                api.delete_device(&device_id, &fresh).await.map(|_| ())
            }
            .await;
            if let Err(err) = outcome {
                LogEvent::shell("cloud", "unlink.remote_failed")
                    .level(Level::Warn)
                    .msg(err.to_string())
                    .emit();
            }
        }
        file.clear_link();
        file.save(&self.dir)
            .map_err(|e| CloudError::new("STATE_WRITE_FAILED", e, 0))?;
        for key in [KEY_ACCESS, KEY_REFRESH, KEY_DEVICE] {
            let _ = self.secrets.delete(key);
        }
        *self.pending.lock().unwrap() = None;
        Ok(self.view())
    }

    pub fn set_telemetry(&self, enabled: bool) -> Result<CloudStateView, CloudError> {
        self.enabled()?;
        let mut file = CloudFile::load(&self.dir);
        file.telemetry_enabled = Some(enabled);
        file.save(&self.dir)
            .map_err(|e| CloudError::new("STATE_WRITE_FAILED", e, 0))?;
        Ok(self.view())
    }

    /// One usage ping. Silent by design: never surfaces an error to the user.
    pub async fn ping(&self) {
        let file = CloudFile::load(&self.dir);
        let Some(base) = self.base_url(&file) else {
            return;
        };
        if !file.telemetry_enabled.unwrap_or(true) {
            return;
        }
        let Some(installation_id) = self.installation_id() else {
            return;
        };
        let payload = telemetry_payload(&installation_id, &self.version, file.tenant_id.as_deref());
        let api = Api {
            http: &self.http,
            base: &base,
        };
        let event = match api.telemetry_ping(&payload).await {
            Ok(()) => LogEvent::shell("cloud", "telemetry.sent").level(Level::Debug),
            Err(err) => LogEvent::shell("cloud", "telemetry.failed")
                .level(Level::Debug)
                .msg(err.to_string()),
        };
        event.emit();
    }
}

fn installation_id_from(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("installation.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    ["installation_id", "installationId"]
        .iter()
        .find_map(|k| value.get(k).and_then(Value::as_str))
        .map(str::to_string)
}

fn device_name() -> String {
    sysinfo::System::host_name().unwrap_or_else(|| "SimpleBash POS".to_string())
}

/// Fire-and-forget launch ping (5 s after start so it never competes with boot).
pub fn spawn_launch_ping(handle: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        handle.state::<CloudState>().ping().await;
    });
}

// ---------------------------------------------------------------------------
// Tauri commands. Secrets (passwords) are redacted by key in `CommandLog`, and
// only non-secret arguments are passed to it at all.
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn cloud_get_state(state: State<'_, CloudState>) -> Result<CloudStateView, CloudError> {
    let call = CommandLog::start("cloud_get_state", json!({}));
    call.finish(Ok::<_, CloudError>(state.view()))
}

#[tauri::command]
pub async fn cloud_register(
    state: State<'_, CloudState>,
    email: String,
    password: String,
    owner_name: String,
    store_name: String,
) -> Result<RegisterResult, CloudError> {
    let call = CommandLog::start(
        "cloud_register",
        json!({ "email": email, "storeName": store_name }),
    );
    call.finish(
        state
            .register(&email, &password, &owner_name, &store_name)
            .await,
    )
}

#[tauri::command]
pub async fn cloud_login_and_link(
    state: State<'_, CloudState>,
    email: String,
    password: String,
    tenant_id: Option<String>,
) -> Result<CloudStateView, CloudError> {
    let call = CommandLog::start(
        "cloud_login_and_link",
        json!({ "email": email, "tenantId": tenant_id }),
    );
    call.finish(state.login_and_link(&email, &password, tenant_id).await)
}

#[tauri::command]
pub async fn cloud_link_start(state: State<'_, CloudState>) -> Result<PendingLinkView, CloudError> {
    let call = CommandLog::start("cloud_link_start", json!({}));
    call.finish(state.link_start().await)
}

#[tauri::command]
pub async fn cloud_link_poll(state: State<'_, CloudState>) -> Result<LinkPollView, CloudError> {
    let call = CommandLog::start("cloud_link_poll", json!({}));
    call.finish(state.link_poll().await)
}

#[tauri::command]
pub async fn cloud_unlink(state: State<'_, CloudState>) -> Result<CloudStateView, CloudError> {
    let call = CommandLog::start("cloud_unlink", json!({}));
    call.finish(state.unlink().await)
}

#[tauri::command]
pub async fn cloud_set_telemetry(
    state: State<'_, CloudState>,
    enabled: bool,
) -> Result<CloudStateView, CloudError> {
    let call = CommandLog::start("cloud_set_telemetry", json!({ "enabled": enabled }));
    call.finish(state.set_telemetry(enabled))
}

/// Called by the webview after an update check.
#[tauri::command]
pub async fn cloud_ping(state: State<'_, CloudState>) -> Result<(), CloudError> {
    state.ping().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::MemoryStore;
    use wiremock::{
        matchers::{body_json, body_partial_json, header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    struct Fixture {
        state: CloudState,
        secrets: Arc<MemoryStore>,
        dir: tempfile::TempDir,
    }

    fn fixture(build_url: Option<String>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("installation.json"),
            r#"{"installation_id":"inst_test1234"}"#,
        )
        .unwrap();
        let secrets = Arc::new(MemoryStore::default());
        let state = CloudState::with_parts(
            dir.path().to_path_buf(),
            "9.9.9".into(),
            build_url,
            secrets.clone(),
        );
        Fixture {
            state,
            secrets,
            dir,
        }
    }

    fn start_body() -> Value {
        json!({ "deviceCode": "dc_1", "userCode": "ABCD-EFGH",
                "verificationUrl": "https://cloud.test/link", "interval": 0, "expiresIn": 600 })
    }

    fn approved_body() -> Value {
        json!({ "deviceId": "dev_1", "tenantId": "tnt_1", "shopCode": "myshop",
                "accessToken": "acc_1", "refreshToken": "ref_1" })
    }

    #[tokio::test]
    async fn disabled_without_a_url_answers_cloud_disabled() {
        let f = fixture(None);
        assert!(!f.state.view().enabled);
        let err = f
            .state
            .register("a@b.c", "pw", "Owner", "Shop")
            .await
            .unwrap_err();
        assert_eq!(err.code, "CLOUD_DISABLED");
        assert_eq!(
            f.state.link_start().await.unwrap_err().code,
            "CLOUD_DISABLED"
        );
        assert_eq!(
            f.state.set_telemetry(false).unwrap_err().code,
            "CLOUD_DISABLED"
        );
        f.state.ping().await; // must not panic or send anything
    }

    #[tokio::test]
    async fn cloud_json_api_url_overrides_and_enables() {
        let f = fixture(None);
        let file = CloudFile {
            api_url: Some("https://example.test/".into()),
            ..Default::default()
        };
        file.save(f.dir.path()).unwrap();
        assert!(f.state.view().enabled);
    }

    #[tokio::test]
    async fn register_posts_the_expected_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts"))
            .and(body_partial_json(json!({
                "email": "o@shop.lk", "password": "pw-123456", "ownerName": "Owner", "storeName": "Shop"
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "verificationRequired": true })))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(Some(server.uri()));
        let out = f
            .state
            .register("o@shop.lk", "pw-123456", "Owner", "Shop")
            .await
            .unwrap();
        assert!(out.verification_required);
    }

    #[tokio::test]
    async fn register_surfaces_the_error_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/accounts"))
            .respond_with(ResponseTemplate::new(409).set_body_json(json!({
                "success": false, "message": "email taken", "code": "EMAIL_TAKEN", "statusCode": 409
            })))
            .mount(&server)
            .await;
        let f = fixture(Some(server.uri()));
        let err = f
            .state
            .register("o@shop.lk", "pw", "O", "S")
            .await
            .unwrap_err();
        assert_eq!((err.code.as_str(), err.status), ("EMAIL_TAKEN", 409));
    }

    #[tokio::test]
    async fn link_flow_goes_pending_then_linked_and_keeps_tokens_out_of_files() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/devices/link/start"))
            .and(body_partial_json(json!({
                "installationId": "inst_test1234", "appVersion": "9.9.9"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(start_body()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/devices/link/poll"))
            .and(body_json(json!({ "deviceCode": "dc_1" })))
            .respond_with(ResponseTemplate::new(202).set_body_json(json!({ "status": "pending" })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/devices/link/poll"))
            .respond_with(ResponseTemplate::new(200).set_body_json(approved_body()))
            .mount(&server)
            .await;

        let f = fixture(Some(server.uri()));
        let pending = f.state.link_start().await.unwrap();
        assert_eq!(pending.user_code, "ABCD-EFGH");
        assert!(f.state.view().pending_link.is_some());
        // The device key was created in the keychain and only its public half was sent.
        assert!(f.secrets.get(KEY_DEVICE).unwrap().is_some());

        assert!(matches!(
            f.state.link_poll().await.unwrap(),
            LinkPollView::Pending
        ));
        match f.state.link_poll().await.unwrap() {
            LinkPollView::Linked { state } => {
                assert!(state.linked);
                assert_eq!(state.tenant_id.as_deref(), Some("tnt_1"));
                assert_eq!(state.shop_code.as_deref(), Some("myshop"));
                assert!(state.pending_link.is_none());
            }
            LinkPollView::Pending => panic!("expected linked"),
        }
        assert_eq!(f.secrets.get(KEY_ACCESS).unwrap().as_deref(), Some("acc_1"));
        assert_eq!(
            f.secrets.get(KEY_REFRESH).unwrap().as_deref(),
            Some("ref_1")
        );
        let raw = std::fs::read_to_string(CloudFile::path(f.dir.path())).unwrap();
        assert!(!raw.contains("acc_1") && !raw.contains("ref_1") && !raw.contains("dc_1"));
    }

    #[tokio::test]
    async fn link_start_sends_a_32_byte_public_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/devices/link/start"))
            .respond_with(ResponseTemplate::new(200).set_body_json(start_body()))
            .mount(&server)
            .await;
        let f = fixture(Some(server.uri()));
        f.state.link_start().await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let key = B64.decode(body["publicKey"].as_str().unwrap()).unwrap();
        assert_eq!(key.len(), 32);
        assert!(body.get("deviceCode").is_none());
    }

    #[tokio::test]
    async fn login_and_link_uses_bearer_and_links_when_auto_approved() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "accessToken": "login_acc", "refreshToken": "login_ref",
                "account": { "email": "o@shop.lk" },
                "tenants": [{ "tenantId": "tnt_1", "shopCode": "myshop" }, { "tenantId": "tnt_2" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/devices/link/start"))
            .and(header("authorization", "Bearer login_acc"))
            .and(body_partial_json(json!({ "tenantId": "tnt_1" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(start_body()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/devices/link/poll"))
            .respond_with(ResponseTemplate::new(200).set_body_json(approved_body()))
            .mount(&server)
            .await;

        let f = fixture(Some(server.uri()));
        let view = f
            .state
            .login_and_link("o@shop.lk", "pw", None)
            .await
            .unwrap();
        assert!(view.linked);
        assert_eq!(view.account_email.as_deref(), Some("o@shop.lk"));
        assert_eq!(f.secrets.get(KEY_ACCESS).unwrap().as_deref(), Some("acc_1"));
    }

    #[tokio::test]
    async fn unlink_clears_local_state_even_when_the_cloud_fails() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/devices/dev_1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let f = fixture(Some(server.uri()));
        CloudFile {
            device_id: Some("dev_1".into()),
            tenant_id: Some("tnt_1".into()),
            telemetry_enabled: Some(false),
            ..Default::default()
        }
        .save(f.dir.path())
        .unwrap();
        f.secrets.set(KEY_ACCESS, "a").unwrap();
        f.secrets.set(KEY_REFRESH, "r").unwrap();
        f.secrets.set(KEY_DEVICE, "k").unwrap();

        let view = f.state.unlink().await.unwrap();
        assert!(!view.linked);
        assert!(f.secrets.get(KEY_ACCESS).unwrap().is_none());
        assert!(f.secrets.get(KEY_REFRESH).unwrap().is_none());
        assert!(f.secrets.get(KEY_DEVICE).unwrap().is_none());
        // The user's telemetry choice survives an unlink.
        assert_eq!(CloudFile::load(f.dir.path()).telemetry_enabled, Some(false));
    }

    #[test]
    fn telemetry_payload_has_exactly_the_allowed_fields() {
        let with_tenant = telemetry_payload("inst_1", "1.2.3", Some("tnt_1"));
        let mut keys: Vec<_> = with_tenant.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["arch", "installationId", "os", "tenantId", "version"]
        );
        let anonymous = telemetry_payload("inst_1", "1.2.3", None);
        assert!(anonymous.get("tenantId").is_none());
    }

    #[tokio::test]
    async fn ping_sends_the_payload_and_respects_opt_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/telemetry/ping"))
            .and(body_json(telemetry_payload("inst_test1234", "9.9.9", None)))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(Some(server.uri()));
        f.state.ping().await; // default on

        f.state.set_telemetry(false).unwrap();
        f.state.ping().await; // opted out: the mock still expects exactly 1 request
    }

    #[tokio::test]
    async fn ping_failures_are_silent() {
        let f = fixture(Some("http://127.0.0.1:1".into()));
        f.state.ping().await; // unreachable server must not panic or error out
    }
}
