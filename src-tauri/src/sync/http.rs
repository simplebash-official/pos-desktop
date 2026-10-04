// The two HTTP clients of the sync agent. Both are thin and typed only as far
// as the agent needs: change records travel as opaque `serde_json::Value`, so
// no code path here can log or reshape a record body.
//
//   LocalApi      -> http://127.0.0.1:<port>/api/sync/*   (service token)
//   CloudSyncApi  -> <sync base>/api/sync/*, /api/sequences/* (device token)
//
// Each also opens its server-sent event stream (`open_events`), which the
// listeners in `sync::live` read; those responses have no overall timeout.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::{header::HeaderMap, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cloud::api::{error_from_body, unwrap_envelope};

use super::token::mint_service_token;

const LOCAL_TIMEOUT: Duration = Duration::from_secs(60);
const CLOUD_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// No network / cloud unreachable.
    Offline,
    /// Token rejected even after a refresh.
    Unauthorized,
    /// The cloud revoked this device: stop until the user relinks.
    Revoked,
    /// The cloud change log no longer reaches this device's cursor (HTTP 410).
    CursorExpired,
    /// 5xx / 429 from the cloud: retry with backoff.
    Server,
    /// The local backend is unreachable or rejected a call.
    Local,
    /// A 4xx that retrying will not fix.
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SyncError {
    pub kind: ErrorKind,
    pub code: String,
    pub message: String,
    pub status: u16,
}

impl SyncError {
    pub fn new(kind: ErrorKind, code: &str, message: impl Into<String>, status: u16) -> Self {
        Self {
            kind,
            code: code.into(),
            message: message.into(),
            status,
        }
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SyncError {}

fn network_error(kind: ErrorKind, err: reqwest::Error) -> SyncError {
    let code = if kind == ErrorKind::Local {
        "LOCAL_UNAVAILABLE"
    } else {
        "NETWORK_ERROR"
    };
    SyncError::new(kind, code, err.without_url().to_string(), 0)
}

// ---------------------------------------------------------------------------
// Local backend
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BlockInfo {
    pub name: String,
    pub remaining: u64,
    pub block_size: u64,
}

/// Rows per resource, as grouped by the local backend.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResourceCount {
    pub resource: String,
    pub count: u64,
}

/// `GET /api/sync/state` (see the contract at the top of `sync/mod.rs`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LocalState {
    pub device_id: String,
    pub linked: bool,
    pub capture_enabled: bool,
    pub cloud_cursor: i64,
    pub last_pushed_outbox_seq: i64,
    pub clock_offset_ms: i64,
    pub pending_out: u64,
    pub conflicts_open: u64,
    /// True when the local database holds business data (invoices etc.).
    pub local_has_data: bool,
    pub number_blocks: Vec<BlockInfo>,
    pub pending_by_resource: Vec<ResourceCount>,
    pub conflicts_by_resource: Vec<ResourceCount>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxItem {
    pub seq: i64,
    pub record: Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OutboxPage {
    pub items: Vec<OutboxItem>,
    pub last_seq: i64,
    /// Identity of the local outbox numbering (empty from older backends).
    pub epoch: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ApplyResult {
    pub applied: u32,
    pub duplicates: u32,
    pub conflicts: u32,
    pub cursor: i64,
}

pub type SecretFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// What the local POS says about its own first-run setup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalSetup {
    pub setup_completed: bool,
    pub sample_data_loaded: bool,
}

#[derive(Clone)]
pub struct LocalApi {
    http: reqwest::Client,
    base: String,
    secret: SecretFn,
}

impl LocalApi {
    pub fn new(http: reqwest::Client, base: impl Into<String>, secret: SecretFn) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            secret,
        }
    }

    /// Opens `GET /api/sync/local/events` (an endless stream).
    pub async fn open_events(&self) -> Result<reqwest::Response, SyncError> {
        let secret = (self.secret)().ok_or_else(|| {
            SyncError::new(
                ErrorKind::Local,
                "LOCAL_SECRET_MISSING",
                "local service secret is not available yet",
                0,
            )
        })?;
        let token = mint_service_token(&secret, Utc::now().timestamp());
        let resp = self
            .http
            .get(format!("{}/api/sync/local/events", self.base))
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| network_error(ErrorKind::Local, e))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let e = error_from_body(status.as_u16(), &text);
            return Err(SyncError::new(
                ErrorKind::Local,
                &e.code,
                e.message,
                status.as_u16(),
            ));
        }
        Ok(resp)
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, SyncError> {
        let secret = (self.secret)().ok_or_else(|| {
            SyncError::new(
                ErrorKind::Local,
                "LOCAL_SECRET_MISSING",
                "local service secret is not available yet",
                0,
            )
        })?;
        let token = mint_service_token(&secret, Utc::now().timestamp());
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .timeout(LOCAL_TIMEOUT)
            .bearer_auth(token)
            .query(query);
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| network_error(ErrorKind::Local, e))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| network_error(ErrorKind::Local, e))?;
        if !status.is_success() {
            let e = error_from_body(status.as_u16(), &text);
            return Err(SyncError::new(
                ErrorKind::Local,
                &e.code,
                e.message,
                status.as_u16(),
            ));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        let value: Value = serde_json::from_str(&text).map_err(|e| {
            SyncError::new(
                ErrorKind::Local,
                "BAD_RESPONSE",
                e.to_string(),
                status.as_u16(),
            )
        })?;
        Ok(unwrap_envelope(value))
    }

    async fn typed<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<T, SyncError> {
        let value = self.call(method, path, query, body).await?;
        serde_json::from_value(value)
            .map_err(|e| SyncError::new(ErrorKind::Local, "BAD_RESPONSE", e.to_string(), 0))
    }

    /// The local POS's first-run state. A fresh install that is still waiting at
    /// the wizard answers `setup_completed: false`.
    pub async fn setup_status(&self) -> Result<LocalSetup, SyncError> {
        let value = self
            .call(Method::GET, "/api/system/setup-status", &[], None)
            .await?;
        let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
        Ok(LocalSetup {
            setup_completed: flag("setupCompleted"),
            sample_data_loaded: flag("sampleDataLoaded"),
        })
    }

    /// Whether the local POS has finished its first-run setup.
    pub async fn setup_completed(&self) -> Result<bool, SyncError> {
        Ok(self.setup_status().await?.setup_completed)
    }

    pub async fn state(&self) -> Result<LocalState, SyncError> {
        self.typed(Method::GET, "/api/sync/state", &[], None).await
    }

    /// The distinct SKU block families (e.g. "PHO-SCR") this device's local
    /// catalog currently needs a cloud-reserved number block for — the agent
    /// has no other way to discover which `sku:<prefix>` names to keep
    /// topped up, since SKU has no single fixed name the way invoice does.
    pub async fn sku_prefixes(&self) -> Result<Vec<String>, SyncError> {
        let value = self
            .call(Method::GET, "/api/sync/sku-prefixes", &[], None)
            .await?;
        serde_json::from_value(
            value
                .get("prefixes")
                .cloned()
                .unwrap_or(Value::Array(vec![])),
        )
        .map_err(|e| SyncError::new(ErrorKind::Local, "BAD_RESPONSE", e.to_string(), 0))
    }

    pub async fn set_clock_offset(&self, offset_ms: i64) -> Result<(), SyncError> {
        self.call(
            Method::POST,
            "/api/sync/state",
            &[],
            Some(&json!({ "clockOffsetMs": offset_ms })),
        )
        .await
        .map(|_| ())
    }

    /// Turns capture on for this device (idempotent).
    pub async fn enable(&self, tenant_id: &str, cloud_device_id: &str) -> Result<(), SyncError> {
        self.call(
            Method::POST,
            "/api/sync/enable",
            &[],
            Some(&json!({ "tenantId": tenant_id, "deviceId": cloud_device_id })),
        )
        .await
        .map(|_| ())
    }

    /// Enqueues every existing row (first device of a shop).
    pub async fn seed(&self) -> Result<(), SyncError> {
        self.call(Method::POST, "/api/sync/outbox/seed", &[], None)
            .await
            .map(|_| ())
    }

    pub async fn outbox(&self, after: i64, limit: usize) -> Result<OutboxPage, SyncError> {
        self.typed(
            Method::GET,
            "/api/sync/outbox",
            &[("after", after.to_string()), ("limit", limit.to_string())],
            None,
        )
        .await
    }

    pub async fn ack(&self, up_to_seq: i64) -> Result<(), SyncError> {
        self.call(
            Method::POST,
            "/api/sync/outbox/ack",
            &[],
            Some(&json!({ "upToSeq": up_to_seq })),
        )
        .await
        .map(|_| ())
    }

    pub async fn apply(&self, body: &Value) -> Result<ApplyResult, SyncError> {
        self.typed(Method::POST, "/api/sync/apply", &[], Some(body))
            .await
    }

    /// Changes still waiting to upload, with readable names (for the Sync screen).
    pub async fn pending(&self, resource: Option<&str>, limit: usize) -> Result<Value, SyncError> {
        let mut query = vec![("limit", limit.to_string())];
        if let Some(r) = resource {
            query.push(("resource", r.to_string()));
        }
        self.typed(Method::GET, "/api/sync/outbox/pending", &query, None)
            .await
    }

    pub async fn put_block(&self, block: &Value) -> Result<(), SyncError> {
        self.call(Method::POST, "/api/sync/blocks", &[], Some(block))
            .await
            .map(|_| ())
    }

    pub async fn conflicts(&self) -> Result<Value, SyncError> {
        self.call(Method::GET, "/api/sync/conflicts", &[], None)
            .await
    }

    pub async fn resolve_conflict(&self, key: &str, resolution: &str) -> Result<Value, SyncError> {
        self.call(
            Method::POST,
            &format!("/api/sync/conflicts/{key}/resolve"),
            &[],
            Some(&json!({ "resolution": resolution })),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// POS cloud
// ---------------------------------------------------------------------------

pub type TokenFuture = Pin<Box<dyn Future<Output = Result<String, SyncError>> + Send>>;
/// Returns a bearer token; `true` forces a refresh through the identity service.
pub type TokenFn = Arc<dyn Fn(bool) -> TokenFuture + Send + Sync>;
/// Receives `server_time - local_time` in ms measured on every cloud response.
pub type ClockFn = Arc<dyn Fn(i64) + Send + Sync>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CloudStatus {
    pub server_seq: i64,
    pub compacted_through_seq: i64,
    /// The shop has finished its own first-time setup (demo vs clean data).
    pub setup_completed: bool,
    /// That setup loaded the demo data.
    pub sample_data_loaded: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PushResult {
    pub acks: Vec<Value>,
    pub server_seq: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PullResult {
    pub changes: Vec<Value>,
    pub next_seq: i64,
    pub has_more: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SnapshotResult {
    pub as_of_seq: i64,
    pub changes: Vec<Value>,
    pub next_page: Option<String>,
}

#[derive(Clone)]
pub struct CloudSyncApi {
    http: reqwest::Client,
    base: String,
    token: TokenFn,
    on_clock: ClockFn,
}

fn server_offset_ms(
    headers: &HeaderMap,
    sent: DateTime<Utc>,
    received: DateTime<Utc>,
) -> Option<i64> {
    let server = headers.get("x-server-time")?.to_str().ok()?;
    let server = DateTime::parse_from_rfc3339(server)
        .ok()?
        .with_timezone(&Utc);
    let midpoint = sent.timestamp_millis() + (received - sent).num_milliseconds() / 2;
    Some(server.timestamp_millis() - midpoint)
}

impl CloudSyncApi {
    pub fn new(
        http: reqwest::Client,
        base: impl Into<String>,
        token: TokenFn,
        on_clock: ClockFn,
    ) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            token,
            on_clock,
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, SyncError> {
        for attempt in 0..2 {
            let token = (self.token)(attempt == 1).await?;
            let mut req = self
                .http
                .request(method.clone(), format!("{}{}", self.base, path))
                .timeout(CLOUD_TIMEOUT)
                .bearer_auth(token)
                .query(query);
            if let Some(body) = body {
                req = req.json(body);
            }
            let sent = Utc::now();
            let resp = req
                .send()
                .await
                .map_err(|e| network_error(ErrorKind::Offline, e))?;
            let received = Utc::now();
            if let Some(offset) = server_offset_ms(resp.headers(), sent, received) {
                (self.on_clock)(offset);
            }
            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| network_error(ErrorKind::Offline, e))?;
            if status.is_success() {
                if text.trim().is_empty() {
                    return Ok(Value::Null);
                }
                let value: Value = serde_json::from_str(&text).map_err(|e| {
                    SyncError::new(
                        ErrorKind::Invalid,
                        "BAD_RESPONSE",
                        e.to_string(),
                        status.as_u16(),
                    )
                })?;
                return Ok(unwrap_envelope(value));
            }
            if status.as_u16() == 401 && attempt == 0 {
                continue;
            }
            return Err(classify(status.as_u16(), &text));
        }
        Err(SyncError::new(
            ErrorKind::Unauthorized,
            "UNAUTHORIZED",
            "cloud rejected the device token",
            401,
        ))
    }

    async fn typed<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<T, SyncError> {
        let value = self.call(method, path, query, body).await?;
        serde_json::from_value(value)
            .map_err(|e| SyncError::new(ErrorKind::Invalid, "BAD_RESPONSE", e.to_string(), 0))
    }

    pub async fn status(&self) -> Result<CloudStatus, SyncError> {
        self.typed(Method::GET, "/api/sync/status", &[], None).await
    }

    /// Lightweight reachability check against the cloud sync endpoint.
    /// Does not require authorization or a valid token; any response (even 4xx) proves
    /// internet connectivity and route liveness to the cloud.
    /// Opens `GET /api/sync/events` (an endless stream), refreshing the device
    /// token once if the cloud rejects it.
    pub async fn open_events(&self) -> Result<reqwest::Response, SyncError> {
        for attempt in 0..2 {
            let token = (self.token)(attempt == 1).await?;
            let resp = self
                .http
                .get(format!("{}/api/sync/events", self.base))
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .bearer_auth(token)
                .send()
                .await
                .map_err(|e| network_error(ErrorKind::Offline, e))?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            if status.as_u16() == 401 && attempt == 0 {
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            return Err(classify(status.as_u16(), &text));
        }
        Err(SyncError::new(
            ErrorKind::Unauthorized,
            "UNAUTHORIZED",
            "cloud rejected the device token",
            401,
        ))
    }

    pub async fn check_reachability(&self) -> bool {
        let url = format!("{}/api/sync/status", self.base);
        match self
            .http
            .get(&url)
            .timeout(Duration::from_secs(3))
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                resp.status().is_success() || (400..500).contains(&status)
            }
            Err(_) => false,
        }
    }

    /// Idempotent per-device registration, done once per agent start.
    pub async fn register_device(
        &self,
        device_id: &str,
        device_name: &str,
        app_version: &str,
    ) -> Result<(), SyncError> {
        self.call(
            Method::POST,
            "/api/sync/devices/register",
            &[],
            Some(&json!({
                "deviceId": device_id,
                "deviceName": device_name,
                "appVersion": app_version,
            })),
        )
        .await
        .map(|_| ())
    }

    /// Tells the cloud this shop's first-time setup is done (idempotent, one-way:
    /// the cloud never un-sets it).
    pub async fn mark_setup_complete(&self, sample_data_loaded: bool) -> Result<(), SyncError> {
        self.call(
            Method::POST,
            "/api/sync/setup-complete",
            &[],
            Some(&json!({ "sampleDataLoaded": sample_data_loaded })),
        )
        .await
        .map(|_| ())
    }

    pub async fn push(&self, body: &Value) -> Result<PushResult, SyncError> {
        self.typed(Method::POST, "/api/sync/push", &[], Some(body))
            .await
    }

    pub async fn pull(&self, since: i64, limit: usize) -> Result<PullResult, SyncError> {
        self.typed(
            Method::GET,
            "/api/sync/pull",
            &[("since", since.to_string()), ("limit", limit.to_string())],
            None,
        )
        .await
    }

    pub async fn snapshot(&self, page: Option<&str>) -> Result<SnapshotResult, SyncError> {
        let query: Vec<(&str, String)> =
            page.map(|p| ("page", p.to_string())).into_iter().collect();
        self.typed(Method::GET, "/api/sync/snapshot", &query, None)
            .await
    }

    pub async fn reserve_block(
        &self,
        name: &str,
        block_size: u64,
        device_id: &str,
    ) -> Result<Value, SyncError> {
        self.call(
            Method::POST,
            &format!("/api/sequences/{name}/reserve"),
            &[],
            Some(&json!({ "blockSize": block_size, "deviceId": device_id })),
        )
        .await
    }
}

/// Maps a non-2xx cloud answer to what the agent should do about it.
fn classify(status: u16, body: &str) -> SyncError {
    let e = error_from_body(status, body);
    let kind = match status {
        410 => ErrorKind::CursorExpired,
        401 => ErrorKind::Unauthorized,
        403 if e.code == "DEVICE_REVOKED" => ErrorKind::Revoked,
        403 => ErrorKind::Unauthorized,
        // A concurrent retry of the same batch is still running on the cloud.
        409 if e.code == "IDEMPOTENCY_IN_PROGRESS" => ErrorKind::Server,
        408 | 429 | 500..=599 => ErrorKind::Server,
        _ => ErrorKind::Invalid,
    };
    let code = if status == 410 && e.code == "CLOUD_ERROR" {
        "CURSOR_EXPIRED".to_string()
    } else {
        e.code
    };
    SyncError::new(kind, &code, e.message, status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_the_intended_actions() {
        assert_eq!(classify(410, "").kind, ErrorKind::CursorExpired);
        assert_eq!(classify(410, "").code, "CURSOR_EXPIRED");
        assert_eq!(classify(401, "").kind, ErrorKind::Unauthorized);
        assert_eq!(
            classify(403, r#"{"code":"DEVICE_REVOKED","message":"gone"}"#).kind,
            ErrorKind::Revoked
        );
        assert_eq!(
            classify(403, r#"{"code":"NOPE"}"#).kind,
            ErrorKind::Unauthorized
        );
        assert_eq!(classify(503, "").kind, ErrorKind::Server);
        assert_eq!(classify(429, "").kind, ErrorKind::Server);
        assert_eq!(classify(422, "").kind, ErrorKind::Invalid);
    }

    #[test]
    fn server_time_header_yields_an_offset_from_the_request_midpoint() {
        let sent = DateTime::parse_from_rfc3339("2026-01-01T00:00:00.000Z")
            .unwrap()
            .with_timezone(&Utc);
        let received = sent + chrono::Duration::milliseconds(200);
        let mut headers = HeaderMap::new();
        headers.insert("x-server-time", "2026-01-01T00:00:05.100Z".parse().unwrap());
        // server 5100 ms vs local midpoint 100 ms -> +5000 ms.
        assert_eq!(server_offset_ms(&headers, sent, received), Some(5000));
        assert_eq!(server_offset_ms(&HeaderMap::new(), sent, received), None);
    }
}
