// Thin client for the identity service (contract v1). Every call goes through
// the shell's `reqwest` client: the webview never talks to the cloud (its CSP
// only allows loopback), which is also why no cloud host appears in
// tauri.conf.json.

use std::fmt;
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(15);

/// Error returned to the webview (serialized as `{code,message,status}`).
#[derive(Debug, Clone, Serialize)]
pub struct CloudError {
    pub code: String,
    pub message: String,
    pub status: u16,
}

impl CloudError {
    pub fn new(code: &str, message: impl Into<String>, status: u16) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status,
        }
    }

    pub fn disabled() -> Self {
        Self::new(
            "CLOUD_DISABLED",
            "Cloud features are not enabled in this build",
            0,
        )
    }

    fn network(err: reqwest::Error) -> Self {
        Self::new("NETWORK_ERROR", err.without_url().to_string(), 0)
    }
}

impl fmt::Display for CloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CloudError {}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkStartResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    #[serde(default = "default_interval")]
    pub interval: u64,
    #[serde(default)]
    pub expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovedLink {
    pub device_id: String,
    pub tenant_id: String,
    #[serde(default)]
    pub shop_code: String,
    pub access_token: String,
    pub refresh_token: String,
}

pub enum PollResult {
    Pending,
    Approved(ApprovedLink),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Account {
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TenantRef {
    #[serde(alias = "id")]
    pub tenant_id: String,
    pub shop_code: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub account: Account,
    #[serde(default)]
    pub tenants: Vec<TenantRef>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPair {
    pub access_token: String,
    pub refresh_token: String,
}

/// One row of `GET /v1/devices` — every device linked to a tenant, including
/// this one (callers distinguish "this device" by comparing `deviceId` to the
/// id stored in `cloud.json`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub device_id: String,
    pub tenant_id: String,
    pub device_name: String,
    pub os: String,
    pub app_version: String,
    pub created_at: String,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DevicesListResponse {
    devices: Vec<DeviceView>,
}

pub struct Api<'a> {
    pub http: &'a reqwest::Client,
    pub base: &'a str,
}

/// Accepts both a bare object and the `{success,data}` envelope.
pub(crate) fn unwrap_envelope(value: Value) -> Value {
    match value {
        Value::Object(mut map) if map.contains_key("success") && map.contains_key("data") => {
            map.remove("data").unwrap_or(Value::Null)
        }
        other => other,
    }
}

async fn read_json<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, CloudError> {
    let status = resp.status();
    let text = resp.text().await.map_err(CloudError::network)?;
    if status.is_success() {
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| CloudError::new("BAD_RESPONSE", e.to_string(), status.as_u16()))?;
        return serde_json::from_value(unwrap_envelope(value))
            .map_err(|e| CloudError::new("BAD_RESPONSE", e.to_string(), status.as_u16()));
    }
    Err(error_from_body(status.as_u16(), &text))
}

pub(crate) fn error_from_body(status: u16, text: &str) -> CloudError {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let field = |name: &str| {
        parsed
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    CloudError::new(
        &field("code").unwrap_or_else(|| "CLOUD_ERROR".into()),
        field("message").unwrap_or_else(|| format!("cloud request failed ({status})")),
        status,
    )
}

impl Api<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn post_json<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
        bearer: Option<&str>,
    ) -> Result<T, CloudError> {
        let mut req = self.http.post(self.url(path)).timeout(TIMEOUT).json(body);
        if let Some(token) = bearer {
            req = req.bearer_auth(token);
        }
        read_json(req.send().await.map_err(CloudError::network)?).await
    }

    pub async fn register(
        &self,
        email: &str,
        password: &str,
        owner_name: &str,
        store_name: &str,
    ) -> Result<Value, CloudError> {
        self.post_json(
            "/v1/accounts",
            &json!({
                "email": email,
                "password": password,
                "ownerName": owner_name,
                "storeName": store_name,
            }),
            None,
        )
        .await
    }

    pub async fn login(&self, email: &str, password: &str) -> Result<LoginResponse, CloudError> {
        self.post_json(
            "/v1/auth/login",
            &json!({ "email": email, "password": password }),
            None,
        )
        .await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenPair, CloudError> {
        self.post_json(
            "/v1/auth/refresh",
            &json!({ "refreshToken": refresh_token }),
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn link_start(
        &self,
        installation_id: &str,
        device_name: &str,
        os: &str,
        app_version: &str,
        public_key: &str,
        tenant_id: Option<&str>,
        bearer: Option<&str>,
    ) -> Result<LinkStartResponse, CloudError> {
        let mut body = json!({
            "installationId": installation_id,
            "deviceName": device_name,
            "os": os,
            "appVersion": app_version,
            "publicKey": public_key,
        });
        if let Some(t) = tenant_id {
            body["tenantId"] = json!(t);
        }
        self.post_json("/v1/devices/link/start", &body, bearer)
            .await
    }

    /// 202 `{status:"pending"}` while the user has not approved, 200 with the
    /// link once they have.
    pub async fn link_poll(&self, device_code: &str) -> Result<PollResult, CloudError> {
        let resp = self
            .http
            .post(self.url("/v1/devices/link/poll"))
            .timeout(TIMEOUT)
            .json(&json!({ "deviceCode": device_code }))
            .send()
            .await
            .map_err(CloudError::network)?;
        if resp.status().as_u16() == 202 {
            return Ok(PollResult::Pending);
        }
        Ok(PollResult::Approved(read_json(resp).await?))
    }

    /// `Ok(false)` means the access token was rejected (401) and the caller
    /// may refresh and retry.
    pub async fn delete_device(&self, device_id: &str, bearer: &str) -> Result<bool, CloudError> {
        let resp = self
            .http
            .delete(self.url(&format!("/v1/devices/{device_id}")))
            .timeout(TIMEOUT)
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(CloudError::network)?;
        let status = resp.status();
        if status.as_u16() == 401 {
            return Ok(false);
        }
        if status.is_success() || status.as_u16() == 404 {
            return Ok(true);
        }
        let text = resp.text().await.unwrap_or_default();
        Err(error_from_body(status.as_u16(), &text))
    }

    /// Devices linked to `tenant_id` (or every tenant the account belongs to,
    /// if `None`), newest first. `err.status == 401` signals a stale token the
    /// caller should refresh and retry, same convention as elsewhere here.
    pub async fn list_devices(
        &self,
        tenant_id: Option<&str>,
        bearer: &str,
    ) -> Result<Vec<DeviceView>, CloudError> {
        let mut req = self
            .http
            .get(self.url("/v1/devices"))
            .timeout(TIMEOUT)
            .bearer_auth(bearer);
        if let Some(t) = tenant_id {
            req = req.query(&[("tenantId", t)]);
        }
        let resp = req.send().await.map_err(CloudError::network)?;
        let parsed: DevicesListResponse = read_json(resp).await?;
        Ok(parsed.devices)
    }

    pub async fn telemetry_ping(&self, payload: &Value) -> Result<(), CloudError> {
        let resp = self
            .http
            .post(self.url("/v1/telemetry/ping"))
            .timeout(Duration::from_secs(8))
            .json(payload)
            .send()
            .await
            .map_err(CloudError::network)?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            Err(error_from_body(status, &text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_is_unwrapped_and_bare_objects_pass_through() {
        assert_eq!(
            unwrap_envelope(json!({"success": true, "data": {"a": 1}})),
            json!({"a": 1})
        );
        assert_eq!(unwrap_envelope(json!({"a": 1})), json!({"a": 1}));
    }

    #[test]
    fn error_envelope_maps_to_code_and_message() {
        let e = error_from_body(
            409,
            r#"{"success":false,"message":"exists","code":"EMAIL_TAKEN","statusCode":409}"#,
        );
        assert_eq!((e.code.as_str(), e.status), ("EMAIL_TAKEN", 409));
        assert_eq!(e.message, "exists");
        assert_eq!(error_from_body(500, "oops").code, "CLOUD_ERROR");
    }
}
