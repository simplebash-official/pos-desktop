//! A computer's first sync against the real staged backend (temporary SQLite), so the rows the
//! backend creates on every start are really there. Only the cloud is mocked, with the response
//! shapes of the multi-tenant backend: `/api/sync/status` carries no `serverSeq`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::agent::{Agent, ClockSampler, CycleReport};
use super::http::{ClockFn, CloudSyncApi, LocalApi, SecretFn, TokenFn};
use super::state::{Recorder, SyncControl};
use crate::orchestrator::tests::StagedBackend;

const SNAPSHOT_SEQ: i64 = 12;

/// One computer: its backend and the folder holding its database.
struct Computer {
    // Declared first so the process is gone before its folder is removed.
    backend: StagedBackend,
    _work: TempDir,
    http: reqwest::Client,
    api: LocalApi,
}

fn ok(data: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({ "success": true, "data": data }))
}

/// A freshly installed computer, as the shell starts it.
async fn new_computer(document_server: &MockServer) -> Computer {
    let work = tempfile::tempdir().unwrap();
    let backend = StagedBackend::spawn(
        work.path(),
        &[("DOCUMENT_SERVER_URL", document_server.uri())],
    );
    let http = reqwest::Client::new();
    let health = format!("{}/api/health", backend.base_url);
    let deadline = Instant::now() + Duration::from_secs(60);
    while !http
        .get(&health)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
    {
        assert!(
            Instant::now() < deadline,
            "the staged backend did not start"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let jwt_secret = backend.jwt_secret.clone();
    let secret: SecretFn = Arc::new(move || Some(jwt_secret.clone()));
    let api = LocalApi::new(http.clone(), backend.base_url.clone(), secret);
    Computer {
        backend,
        _work: work,
        http,
        api,
    }
}

impl Computer {
    /// Finishes the welcome wizard on this computer: it now has an admin of its own.
    async fn run_setup(&self) {
        let response = self
            .http
            .post(format!("{}/api/system/setup", self.backend.base_url))
            .json(&json!({ "loadSampleData": false, "adminPassword": "test-only-password" }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "{}", response.status());
    }
}

/// The records a set-up shop has in the cloud: everything its first computer uploads.
async fn shop_records(document_server: &MockServer) -> Vec<Value> {
    let first = new_computer(document_server).await;
    first.run_setup().await;
    first.api.enable("tnt_1", "dev_first").await.unwrap();
    let page = first.api.outbox(0, 500).await.unwrap();
    assert!(page.items.len() < 500, "the snapshot needs more pages");
    page.items.into_iter().map(|item| item.record).collect()
}

async fn mock_cloud(setup_completed: bool, snapshot: Vec<Value>) -> MockServer {
    let cloud = MockServer::start().await;
    let routes = [
        (
            "POST",
            "/api/sync/devices/register",
            json!({ "deviceId": "dev_new", "serverSeq": SNAPSHOT_SEQ }),
        ),
        (
            "GET",
            "/api/sync/status",
            json!({
                "serverTime": "2026-01-01T00:00:00Z", "resources": {},
                "setupCompleted": setup_completed, "sampleDataLoaded": false,
            }),
        ),
        (
            "GET",
            "/api/sync/snapshot",
            json!({ "asOfSeq": SNAPSHOT_SEQ, "changes": snapshot, "nextPage": null }),
        ),
        (
            "GET",
            "/api/sync/pull",
            json!({ "changes": [], "nextSeq": SNAPSHOT_SEQ, "hasMore": false }),
        ),
        (
            "POST",
            "/api/sync/push",
            json!({ "acks": [], "serverSeq": SNAPSHOT_SEQ }),
        ),
        (
            "POST",
            "/api/sync/setup-complete",
            json!({ "setupCompleted": true, "changed": true }),
        ),
    ];
    for (verb, route, data) in routes {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(ok(data))
            .mount(&cloud)
            .await;
    }
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/sequences/[^/]+/reserve$"))
        .respond_with(ok(json!({
            "prefix": "X-", "padding": 6, "start": 1, "end": 100,
            "expiresAt": "2030-01-01T00:00:00Z",
        })))
        .mount(&cloud)
        .await;
    cloud
}

async fn first_cycle(computer: &Computer, cloud: &MockServer) -> CycleReport {
    let token: TokenFn = Arc::new(|_| Box::pin(async { Ok("test-token".to_string()) }));
    let on_clock: ClockFn = Arc::new(|_| {});
    let agent = Agent::new(
        computer.api.clone(),
        CloudSyncApi::new(computer.http.clone(), cloud.uri(), token, on_clock),
        SyncControl::new(Arc::new(Recorder::default())),
        Arc::new(ClockSampler::default()),
        "tnt_1".into(),
        "dev_new".into(),
        "Till 2".into(),
        "0.0.0".into(),
    );
    agent.cycle().await.unwrap()
}

async fn uploads(cloud: &MockServer) -> usize {
    cloud
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/sync/push")
        .count()
}

async fn document_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(path("/api/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_new_computer_downloads_the_shop_and_uploads_nothing() {
    let docs = document_server().await;
    let cloud = mock_cloud(true, shop_records(&docs).await).await;
    let joining = new_computer(&docs).await;

    let report = first_cycle(&joining, &cloud).await;

    assert_eq!(uploads(&cloud).await, 0, "starter rows went to the cloud");
    assert!(!report.bootstrap_required);
    assert!(
        joining.api.setup_completed().await.unwrap(),
        "the welcome wizard would still show"
    );
    let state = joining.api.state().await.unwrap();
    assert_eq!((state.cloud_cursor, state.pending_out), (SNAPSHOT_SEQ, 0));
}

#[tokio::test]
async fn a_computer_with_its_own_data_asks_before_replacing_it() {
    let docs = document_server().await;
    let cloud = mock_cloud(true, shop_records(&docs).await).await;
    let joining = new_computer(&docs).await;
    joining.run_setup().await;

    let report = first_cycle(&joining, &cloud).await;

    assert!(report.bootstrap_required);
    assert_eq!(uploads(&cloud).await, 0);
    assert_eq!(joining.api.state().await.unwrap().cloud_cursor, 0);
}

#[tokio::test]
async fn the_first_computer_of_a_new_shop_uploads_its_data() {
    let docs = document_server().await;
    // A just-provisioned shop: rows exist in the cloud, but nobody has set it up.
    let cloud = mock_cloud(false, shop_records(&docs).await).await;
    let first = new_computer(&docs).await;
    first.run_setup().await;

    let report = first_cycle(&first, &cloud).await;

    assert!(!report.bootstrap_required);
    assert!(report.pushed > 0);
    assert!(uploads(&cloud).await > 0);
    assert_eq!(first.api.state().await.unwrap().pending_out, 0);
}
