// The local document-server as seen by the package sync: list what it holds,
// install a downloaded archive, take a shop's grant away. Plain HTTP on
// loopback with the sidecar's shared secret; the sidecar sends no tenant, so
// its grants are stored under the empty tenant key.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cloud::api::{error_from_body, unwrap_envelope};

use super::http::{ErrorKind, SyncError};
use super::packages::{Local, LocalPackage};

const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct DocumentServerApi {
    http: reqwest::Client,
    base: String,
    api_key: String,
}

fn transport(err: reqwest::Error) -> SyncError {
    SyncError::new(
        ErrorKind::Local,
        "LOCAL_UNAVAILABLE",
        err.without_url().to_string(),
        0,
    )
}

impl DocumentServerApi {
    pub fn new(http: reqwest::Client, base: impl Into<String>, api_key: String) -> Self {
        Self {
            http,
            base: base.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }

    /// Sends `request` and returns the unwrapped JSON body of a success.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, SyncError> {
        let resp = request
            .timeout(TIMEOUT)
            .header("X-Internal-Api-Key", &self.api_key)
            .send()
            .await
            .map_err(transport)?;
        let status = resp.status();
        let text = resp.text().await.map_err(transport)?;
        if status.is_success() {
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
            return Ok(unwrap_envelope(value));
        }
        let e = error_from_body(status.as_u16(), &text);
        // A 4xx from the local server is a real answer (a conflict, a bad
        // archive); anything else means this computer's service is unwell.
        let kind = if matches!(
            status,
            StatusCode::CONFLICT | StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST
        ) {
            ErrorKind::Invalid
        } else {
            ErrorKind::Local
        };
        Err(SyncError::new(kind, &e.code, e.message, status.as_u16()))
    }
}

#[derive(Deserialize)]
struct Listing {
    packages: Vec<LocalPackage>,
}

impl Local for DocumentServerApi {
    async fn list(&self) -> Result<Vec<LocalPackage>, SyncError> {
        let value = self
            .send(self.http.get(format!("{}/api/packages", self.base)))
            .await?;
        serde_json::from_value::<Listing>(value)
            .map(|l| l.packages)
            .map_err(|e| SyncError::new(ErrorKind::Local, "BAD_RESPONSE", e.to_string(), 0))
    }

    async fn install(&self, name: &str, sha256: &str, archive: Vec<u8>) -> Result<(), SyncError> {
        self.send(
            self.http
                .put(format!("{}/api/packages/{name}", self.base))
                .query(&[("sha256", sha256)])
                .header(reqwest::header::CONTENT_TYPE, "application/zip")
                .body(archive),
        )
        .await
        .map(|_| ())
    }

    async fn revoke(&self, family: &str) -> Result<(), SyncError> {
        self.send(
            self.http
                .put(format!(
                    "{}/api/packages/families/{family}/grants",
                    self.base
                ))
                .json(&json!({ "tenantKeys": [] })),
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn api(server: &MockServer) -> DocumentServerApi {
        DocumentServerApi::new(reqwest::Client::new(), server.uri(), "local-key".into())
    }

    #[tokio::test]
    async fn lists_the_servers_packages_with_the_internal_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/packages"))
            .and(header("X-Internal-Api-Key", "local-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "data": { "packages": [{
                    "name": "doc_temp_aaaaaaaaaaaaaaaa", "family": "fam_a", "scope": "private",
                    "sha256": "abc", "status": "published", "grants": [""], "version": 1
                }]}
            })))
            .mount(&server)
            .await;
        let listed = api(&server).list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].grants, vec![String::new()]);
    }

    #[tokio::test]
    async fn installs_with_the_digest_and_the_zip_body() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/packages/doc_temp_aaaaaaaaaaaaaaaa"))
            .and(query_param("sha256", "abc"))
            .and(header("content-type", "application/zip"))
            .and(header("X-Internal-Api-Key", "local-key"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({"success": true, "data": {}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        api(&server)
            .install("doc_temp_aaaaaaaaaaaaaaaa", "abc", b"zip".to_vec())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_conflict_is_an_invalid_answer_and_a_500_is_a_local_failure() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/packages/doc_temp_aaaaaaaaaaaaaaaa"))
            .respond_with(ResponseTemplate::new(409).set_body_json(
                json!({"success": false, "code": "PACKAGE_CONFLICT", "message": "different"}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/packages/doc_temp_bbbbbbbbbbbbbbbb"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let api = api(&server);
        let conflict = api
            .install("doc_temp_aaaaaaaaaaaaaaaa", "x", vec![])
            .await
            .unwrap_err();
        assert_eq!(
            (conflict.kind, conflict.code.as_str()),
            (ErrorKind::Invalid, "PACKAGE_CONFLICT")
        );
        let broken = api
            .install("doc_temp_bbbbbbbbbbbbbbbb", "x", vec![])
            .await
            .unwrap_err();
        assert_eq!(broken.kind, ErrorKind::Local);
    }

    #[tokio::test]
    async fn revoking_replaces_the_families_grants_with_none() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/packages/families/fam_a/grants"))
            .and(body_json(json!({ "tenantKeys": [] })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        api(&server).revoke("fam_a").await.unwrap();
    }

    #[tokio::test]
    async fn an_unreachable_server_is_a_local_failure() {
        let api = DocumentServerApi::new(reqwest::Client::new(), "http://127.0.0.1:1", "k".into());
        assert_eq!(api.list().await.unwrap_err().kind, ErrorKind::Local);
    }
}
