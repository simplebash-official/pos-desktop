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
use api::{
    Api, ApprovedLink, DeviceView, LinkStartResponse, OtpSendResult, OtpVerifyResult, PollResult,
};
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
    account_name: Option<String>,
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
    pub account_name: Option<String>,
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
    // Boxed: the view is ~200 bytes and `Pending` carries nothing. Serde
    // serializes a `Box` exactly like its contents, so the JSON is unchanged.
    Linked { state: Box<CloudStateView> },
}

pub struct CloudState {
    http: reqwest::Client,
    secrets: Arc<dyn SecretStore>,
    dir: PathBuf,
    version: String,
    build_url: Option<String>,
    pending: Mutex<Option<PendingLink>>,
    /// Serialises token refreshes: a refresh token is single-use, so two tasks
    /// refreshing at once would present the same one twice and the identity
    /// service would revoke the session as stolen.
    refresh_lock: tokio::sync::Mutex<()>,
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
            refresh_lock: tokio::sync::Mutex::new(()),
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
            account_name: file.account_name,
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
        let stale = self.secrets.get(KEY_ACCESS).map_err(keychain_err)?;
        if !force_refresh {
            if let Some(token) = stale {
                return Ok(token);
            }
        }
        let (base, _) = self.enabled()?;
        let api = Api {
            http: &self.http,
            base: &base,
        };
        self.refresh_tokens(&api, stale.as_deref()).await
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

    /// Texts a one-time code to `phone`. Nothing is stored: the webview keeps the challenge id.
    pub async fn otp_send(&self, phone: &str) -> Result<OtpSendResult, CloudError> {
        let (base, _) = self.enabled()?;
        Api {
            http: &self.http,
            base: &base,
        }
        .otp_send(phone)
        .await
    }

    /// Trades the right code for the one-time proof of the phone number.
    pub async fn otp_verify(
        &self,
        otp_id: &str,
        code: &str,
    ) -> Result<OtpVerifyResult, CloudError> {
        let (base, _) = self.enabled()?;
        Api {
            http: &self.http,
            base: &base,
        }
        .otp_verify(otp_id, code)
        .await
    }

    pub async fn register(
        &self,
        email: &str,
        password: &str,
        owner_name: &str,
        store_name: &str,
        phone: &str,
        phone_proof: &str,
    ) -> Result<RegisterResult, CloudError> {
        let (base, _) = self.enabled()?;
        let api = Api {
            http: &self.http,
            base: &base,
        };
        let response = api
            .register(email, password, owner_name, store_name, phone, phone_proof)
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
        account_name: Option<String>,
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
            account_name,
        });
        Ok(start)
    }

    fn finalize_link(
        &self,
        approved: ApprovedLink,
        account_email: Option<String>,
        account_name: Option<String>,
    ) -> Result<(), CloudError> {
        self.store_tokens(&approved.access_token, &approved.refresh_token)?;
        let mut file = CloudFile::load(&self.dir);
        file.device_id = Some(approved.device_id);
        file.tenant_id = Some(approved.tenant_id);
        file.shop_code = Some(approved.shop_code).filter(|s| !s.is_empty());
        // The identity server names the approver; prefer that over what we knew.
        file.account_email = approved.account_email.clone().or(account_email);
        file.account_name = approved.account_name.clone().or(account_name);
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
                login.account.name.clone(),
            )
            .await?;
        for _ in 0..3 {
            match api.link_poll(&start.device_code).await? {
                PollResult::Approved(approved) => {
                    self.finalize_link(approved, account_email, login.account.name.clone())?;
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
        let start = self.begin_link(&api, None, None, None, None).await?;
        Ok(PendingLinkView {
            user_code: start.user_code,
            verification_url: start.verification_url,
            interval: start.interval,
            expires_in: start.expires_in,
        })
    }

    pub async fn link_poll(&self) -> Result<LinkPollView, CloudError> {
        let (base, _) = self.enabled()?;
        let (device_code, account_email, account_name) = {
            let pending = self.pending.lock().unwrap();
            let p = pending
                .as_ref()
                .ok_or_else(|| CloudError::new("NO_PENDING_LINK", "no link in progress", 0))?;
            (
                p.device_code.clone(),
                p.account_email.clone(),
                p.account_name.clone(),
            )
        };
        let api = Api {
            http: &self.http,
            base: &base,
        };
        match api.link_poll(&device_code).await? {
            PollResult::Pending => Ok(LinkPollView::Pending),
            PollResult::Approved(approved) => {
                self.finalize_link(approved, account_email, account_name)?;
                Ok(LinkPollView::Linked {
                    state: Box::new(self.view()),
                })
            }
        }
    }

    /// Rotates the token pair. `stale` is the access token the caller found
    /// unusable: if another task already replaced it while this one waited for
    /// the lock, that fresh token is returned without a second refresh.
    async fn refresh_tokens(
        &self,
        api: &Api<'_>,
        stale: Option<&str>,
    ) -> Result<String, CloudError> {
        let _guard = self.refresh_lock.lock().await;
        if let (Some(stale), Some(current)) =
            (stale, self.secrets.get(KEY_ACCESS).map_err(keychain_err)?)
        {
            if current != stale {
                return Ok(current);
            }
        }
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
                let fresh = self.refresh_tokens(&api, Some(&token)).await?;
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

    /// Devices linked to this tenant, including this one — the caller
    /// distinguishes "this device" by comparing `deviceId` to `cloud.json`'s.
    pub async fn list_devices(&self) -> Result<Vec<DeviceView>, CloudError> {
        let (base, file) = self.enabled()?;
        let api = Api {
            http: &self.http,
            base: &base,
        };
        let token = self.access_token(false).await?;
        match api.list_devices(file.tenant_id.as_deref(), &token).await {
            Err(err) if err.status == 401 => {
                let fresh = self.access_token(true).await?;
                api.list_devices(file.tenant_id.as_deref(), &fresh).await
            }
            other => other,
        }
    }

    /// Revokes a device other than this one (unlinking this one is `unlink`).
    pub async fn revoke_device(&self, device_id: &str) -> Result<(), CloudError> {
        let (base, _) = self.enabled()?;
        let api = Api {
            http: &self.http,
            base: &base,
        };
        let token = self.access_token(false).await?;
        if api.delete_device(device_id, &token).await? {
            return Ok(());
        }
        let fresh = self.access_token(true).await?;
        api.delete_device(device_id, &fresh).await.map(|_| ())
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
    phone: String,
    phone_proof: String,
) -> Result<RegisterResult, CloudError> {
    // The proof is a credential (redacted by key) and is never passed to the log; the number is
    // personal data, so only its last digits are.
    let call = CommandLog::start(
        "cloud_register",
        json!({ "email": email, "storeName": store_name, "phone": mask_phone(&phone) }),
    );
    call.finish(
        state
            .register(
                &email,
                &password,
                &owner_name,
                &store_name,
                &phone,
                &phone_proof,
            )
            .await,
    )
}

/// `077 123 4567` -> `***4567`: enough to recognise a number in a log, not to reuse it.
fn mask_phone(phone: &str) -> String {
    let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 4 {
        return "***".to_string();
    }
    format!("***{}", &digits[digits.len() - 4..])
}

#[tauri::command]
pub async fn cloud_otp_send(
    state: State<'_, CloudState>,
    phone: String,
) -> Result<OtpSendResult, CloudError> {
    let call = CommandLog::start("cloud_otp_send", json!({ "phone": mask_phone(&phone) }));
    call.finish(state.otp_send(&phone).await)
}

#[tauri::command]
pub async fn cloud_otp_verify(
    state: State<'_, CloudState>,
    otp_id: String,
    code: String,
) -> Result<OtpVerifyResult, CloudError> {
    // Neither the code nor the proof it returns goes to the log.
    let call = CommandLog::start("cloud_otp_verify", json!({}));
    call.finish(state.otp_verify(&otp_id, &code).await)
}

#[tauri::command]
pub async fn cloud_login_and_link(
    app: AppHandle,
    state: State<'_, CloudState>,
    email: String,
    password: String,
    tenant_id: Option<String>,
) -> Result<CloudStateView, CloudError> {
    let call = CommandLog::start(
        "cloud_login_and_link",
        json!({ "email": email, "tenantId": tenant_id }),
    );
    let res = state.login_and_link(&email, &password, tenant_id).await;
    if res.is_ok() {
        if let Some(sync) = app.try_state::<crate::sync::SyncManager>() {
            sync.wake();
        }
    }
    call.finish(res)
}

#[tauri::command]
pub async fn cloud_link_start(state: State<'_, CloudState>) -> Result<PendingLinkView, CloudError> {
    let call = CommandLog::start("cloud_link_start", json!({}));
    call.finish(state.link_start().await)
}

#[tauri::command]
pub async fn cloud_link_poll(
    app: AppHandle,
    state: State<'_, CloudState>,
) -> Result<LinkPollView, CloudError> {
    let call = CommandLog::start("cloud_link_poll", json!({}));
    let res = state.link_poll().await;
    if let Ok(LinkPollView::Linked { .. }) = &res {
        if let Some(sync) = app.try_state::<crate::sync::SyncManager>() {
            sync.wake();
        }
    }
    call.finish(res)
}

#[tauri::command]
pub async fn cloud_unlink(
    app: AppHandle,
    state: State<'_, CloudState>,
) -> Result<CloudStateView, CloudError> {
    let call = CommandLog::start("cloud_unlink", json!({}));
    let res = state.unlink().await;
    if let Some(sync) = app.try_state::<crate::sync::SyncManager>() {
        sync.wake();
    }
    call.finish(res)
}

#[tauri::command]
pub async fn cloud_list_devices(
    state: State<'_, CloudState>,
) -> Result<Vec<DeviceView>, CloudError> {
    let call = CommandLog::start("cloud_list_devices", json!({}));
    call.finish(state.list_devices().await)
}

#[tauri::command]
pub async fn cloud_revoke_device(
    state: State<'_, CloudState>,
    device_id: String,
) -> Result<(), CloudError> {
    let call = CommandLog::start("cloud_revoke_device", json!({ "deviceId": device_id }));
    call.finish(state.revoke_device(&device_id).await)
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
    async fn concurrent_refreshes_present_the_refresh_token_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/refresh"))
            .and(body_json(json!({ "refreshToken": "ref_1" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "accessToken": "acc_2", "refreshToken": "ref_2", "expiresIn": 900
            })))
            // A second use of ref_1 is what makes the identity service revoke
            // the session, so exactly one request may reach it.
            .expect(1)
            .mount(&server)
            .await;

        let f = fixture(Some(server.uri()));
        f.secrets.set(KEY_ACCESS, "acc_1").unwrap();
        f.secrets.set(KEY_REFRESH, "ref_1").unwrap();

        let results = tokio::join!(
            f.state.access_token(true),
            f.state.access_token(true),
            f.state.access_token(true),
            f.state.access_token(true),
            f.state.access_token(true),
            f.state.access_token(true),
            f.state.access_token(true),
            f.state.access_token(true),
        );
        for token in [
            results.0, results.1, results.2, results.3, results.4, results.5, results.6, results.7,
        ] {
            assert_eq!(token.unwrap(), "acc_2");
        }
        assert_eq!(
            f.secrets.get(KEY_REFRESH).unwrap().as_deref(),
            Some("ref_2")
        );
    }

    #[tokio::test]
    async fn disabled_without_a_url_answers_cloud_disabled() {
        let f = fixture(None);
        assert!(!f.state.view().enabled);
        let err = f
            .state
            .register("a@b.c", "pw", "Owner", "Shop", "94771234567", "ovp_proof")
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
                "email": "o@shop.lk", "password": "pw-123456", "name": "Owner", "storeName": "Shop",
                "phone": "94771234567", "phoneProof": "ovp_proof"
            })))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({ "verificationRequired": true })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(Some(server.uri()));
        let out = f
            .state
            .register(
                "o@shop.lk",
                "pw-123456",
                "Owner",
                "Shop",
                "94771234567",
                "ovp_proof",
            )
            .await
            .unwrap();
        assert!(out.verification_required);
    }

    #[tokio::test]
    async fn otp_send_posts_the_phone_for_signup_and_returns_the_challenge() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/otp/send"))
            .and(body_partial_json(
                json!({ "phone": "0771234567", "purpose": "signup" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "otpId": "otp_abc", "expiresIn": 300, "resendAfter": 60, "deliveryUncertain": true
            })))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(Some(server.uri()));
        let out = f.state.otp_send("0771234567").await.unwrap();
        assert_eq!(
            (
                out.otp_id.as_str(),
                out.expires_in,
                out.resend_after,
                out.delivery_uncertain
            ),
            ("otp_abc", 300, 60, true)
        );
    }

    #[tokio::test]
    async fn otp_send_defaults_delivery_uncertain_to_false() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/otp/send"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "otpId": "otp_abc", "expiresIn": 300, "resendAfter": 60
            })))
            .mount(&server)
            .await;
        let f = fixture(Some(server.uri()));
        assert!(
            !f.state
                .otp_send("0771234567")
                .await
                .unwrap()
                .delivery_uncertain
        );
    }

    #[tokio::test]
    async fn otp_verify_posts_the_code_and_returns_the_proof() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/otp/verify"))
            .and(body_partial_json(
                json!({ "otpId": "otp_abc", "code": "042817" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "phoneProof": "ovp_proof", "expiresIn": 600
            })))
            .expect(1)
            .mount(&server)
            .await;
        let f = fixture(Some(server.uri()));
        let out = f.state.otp_verify("otp_abc", "042817").await.unwrap();
        assert_eq!(
            (out.phone_proof.as_str(), out.expires_in),
            ("ovp_proof", 600)
        );
    }

    #[tokio::test]
    async fn otp_errors_keep_the_identity_code_for_the_ui() {
        let server = MockServer::start().await;
        for (route, status, code) in [
            ("/v1/otp/verify", 400, "OTP_INVALID"),
            ("/v1/otp/send", 400, "PHONE_COUNTRY_UNSUPPORTED"),
        ] {
            Mock::given(method("POST"))
                .and(path(route))
                .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                    "success": false, "message": "x", "code": code, "statusCode": status
                })))
                .mount(&server)
                .await;
        }
        let f = fixture(Some(server.uri()));
        let err = f.state.otp_verify("otp_abc", "000000").await.unwrap_err();
        assert_eq!((err.code.as_str(), err.status), ("OTP_INVALID", 400));
        let err = f.state.otp_send("+1 415 555 0100").await.unwrap_err();
        assert_eq!(
            (err.code.as_str(), err.status),
            ("PHONE_COUNTRY_UNSUPPORTED", 400)
        );
    }

    #[test]
    fn logged_phone_numbers_keep_only_the_last_four_digits() {
        assert_eq!(mask_phone("077 123 4567"), "***4567");
        assert_eq!(mask_phone("+94 77 123 4567"), "***4567");
        assert_eq!(mask_phone("12"), "***");
    }

    #[tokio::test]
    async fn otp_calls_are_refused_when_cloud_is_off() {
        let f = fixture(None);
        assert_eq!(
            f.state.otp_send("0771234567").await.unwrap_err().code,
            "CLOUD_DISABLED"
        );
        assert_eq!(
            f.state.otp_verify("otp", "123456").await.unwrap_err().code,
            "CLOUD_DISABLED"
        );
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
            .register("o@shop.lk", "pw", "O", "S", "94771234567", "ovp_proof")
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

    fn linked_fixture(server_uri: &str) -> Fixture {
        let f = fixture(Some(server_uri.to_string()));
        CloudFile {
            device_id: Some("dev_1".into()),
            tenant_id: Some("tnt_1".into()),
            account_email: Some("o@shop.lk".into()),
            ..Default::default()
        }
        .save(f.dir.path())
        .unwrap();
        f.secrets.set(KEY_ACCESS, "acc_1").unwrap();
        f.secrets.set(KEY_REFRESH, "ref_1").unwrap();
        f
    }

    fn device_row(id: &str, revoked: bool) -> Value {
        json!({
            "deviceId": id, "tenantId": "tnt_1", "deviceName": "Front counter",
            "os": "macos", "appVersion": "0.7.0", "createdAt": "2026-01-01T00:00:00Z",
            "lastSeenAt": "2026-01-02T00:00:00Z", "revoked": revoked
        })
    }

    #[tokio::test]
    async fn list_devices_scopes_to_the_linked_tenant() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/devices"))
            .and(header("authorization", "Bearer acc_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "devices": [device_row("dev_1", false), device_row("dev_2", false)] }),
            ))
            .mount(&server)
            .await;
        let f = linked_fixture(&server.uri());

        let devices = f.state.list_devices().await.unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].device_id, "dev_1");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests[0].url.query(), Some("tenantId=tnt_1"));
    }

    #[tokio::test]
    async fn list_devices_refreshes_once_after_a_stale_token_and_retries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/devices"))
            .and(header("authorization", "Bearer acc_1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "accessToken": "acc_2", "refreshToken": "ref_2"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/devices"))
            .and(header("authorization", "Bearer acc_2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "devices": [device_row("dev_1", false)] })),
            )
            .mount(&server)
            .await;
        let f = linked_fixture(&server.uri());

        let devices = f.state.list_devices().await.unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(f.secrets.get(KEY_ACCESS).unwrap().as_deref(), Some("acc_2"));
    }

    #[tokio::test]
    async fn revoke_device_deletes_a_different_device_and_leaves_this_one_linked() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/devices/dev_2"))
            .and(header("authorization", "Bearer acc_1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let f = linked_fixture(&server.uri());

        f.state.revoke_device("dev_2").await.unwrap();
        // Revoking another device never touches this device's own link.
        assert!(f.state.view().linked);
        assert_eq!(f.secrets.get(KEY_ACCESS).unwrap().as_deref(), Some("acc_1"));
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
