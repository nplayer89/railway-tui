//! HTTP API client tests against a wiremock server (never the live API).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use railway_tui::api::{Api, ApiError};
use railway_tui::auth::{AuthError, TokenSource};
use railway_tui::budget::{LimitSource, RateBudget};
use railway_tui::model::{DeploymentStatus, Instance, InstanceStatus};
use serde_json::json;
use url::Url;
use wiremock::matchers::{body_string_contains, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct FakeTokens {
    token: String,
    refreshed: String,
    refreshes: AtomicUsize,
}

impl FakeTokens {
    fn new(token: &str, refreshed: &str) -> Arc<Self> {
        Arc::new(Self {
            token: token.into(),
            refreshed: refreshed.into(),
            refreshes: AtomicUsize::new(0),
        })
    }
}

impl TokenSource for FakeTokens {
    fn token(&self) -> Result<String, AuthError> {
        Ok(self.token.clone())
    }
    fn refresh_after_rejection(&self) -> Result<String, AuthError> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(self.refreshed.clone())
    }
}

fn api_for(server: &MockServer, tokens: Arc<FakeTokens>) -> Api {
    let endpoint = Url::parse(&format!("{}/graphql/v2", server.uri())).unwrap();
    Api::new(
        endpoint,
        tokens,
        Arc::new(Mutex::new(RateBudget::new(None))),
    )
}

fn instances_body(latest_status: &str) -> serde_json::Value {
    json!({
        "data": {
            "environment": {
                "serviceInstances": {
                    "edges": [{
                        "node": {
                            "serviceId": "svc1",
                            "serviceName": "web",
                            "numReplicas": 2,
                            "latestDeployment": {
                                "id": "dep2",
                                "status": latest_status,
                                "createdAt": "2026-10-28T10:00:00Z",
                                "meta": {
                                    "commitHash": "abc123",
                                    "commitMessage": "fix things",
                                    "image": null
                                },
                                "instances": [
                                    {"id": "i1", "status": "RUNNING"},
                                    {"id": "i2", "status": "TELEPORTING"}
                                ]
                            },
                            "activeDeployments": [{
                                "id": "dep1",
                                "status": "SUCCESS",
                                "createdAt": "2026-10-27T10:00:00Z",
                                "meta": null,
                                "instances": []
                            }]
                        }
                    }]
                }
            }
        }
    })
}

#[tokio::test]
async fn service_instances_maps_latest_active_and_instances() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(instances_body("DEPLOYING")))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let services = api.service_instances("proj1", "env1").await.unwrap();

    assert_eq!(services.len(), 1);
    let s = &services[0];
    assert_eq!(s.service_id, "svc1");
    assert_eq!(s.service_name, "web");
    assert_eq!(s.num_replicas, 2);
    let latest = s.latest.as_ref().unwrap();
    assert_eq!(latest.id, "dep2");
    assert_eq!(latest.status, DeploymentStatus::Deploying);
    assert_eq!(latest.created_at.to_rfc3339(), "2026-10-28T10:00:00+00:00");
    assert_eq!(latest.commit_sha.as_deref(), Some("abc123"));
    assert_eq!(latest.commit_message.as_deref(), Some("fix things"));
    assert_eq!(latest.image, None);
    assert_eq!(
        latest.instances,
        vec![
            Instance {
                id: "i1".into(),
                status: InstanceStatus::Running
            },
            Instance {
                id: "i2".into(),
                status: InstanceStatus::Unknown("TELEPORTING".into())
            },
        ]
    );
    assert_eq!(s.active.len(), 1);
    assert_eq!(s.active[0].id, "dep1");
    assert_eq!(s.active[0].status, DeploymentStatus::Success);
    assert_eq!(s.active[0].commit_sha, None);
}

#[tokio::test]
async fn unknown_enum_value_decodes_to_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(instances_body("HIBERNATING")))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let services = api.service_instances("proj1", "env1").await.unwrap();

    assert_eq!(
        services[0].latest.as_ref().unwrap().status,
        DeploymentStatus::Unknown("HIBERNATING".into())
    );
}

#[tokio::test]
async fn sends_bearer_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(instances_body("SUCCESS")))
        .expect(1)
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    api.service_instances("proj1", "env1").await.unwrap();
}

#[tokio::test]
async fn http_401_triggers_one_refresh_and_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(401))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer tok2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(instances_body("SUCCESS")))
        .expect(1)
        .mount(&server)
        .await;
    let tokens = FakeTokens::new("tok", "tok2");
    let api = api_for(&server, tokens.clone());

    let services = api.service_instances("proj1", "env1").await.unwrap();

    assert_eq!(services.len(), 1);
    assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persistent_401_is_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .expect(2)
        .mount(&server)
        .await;
    let tokens = FakeTokens::new("tok", "tok2");
    let api = api_for(&server, tokens.clone());

    let err = api.service_instances("proj1", "env1").await.unwrap_err();

    assert!(
        matches!(err, ApiError::Auth(AuthError::Rejected)),
        "{err:?}"
    );
    assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn not_authorized_graphql_error_triggers_refresh() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": null, "errors": [{"message": "Not Authorized"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer tok2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(instances_body("SUCCESS")))
        .mount(&server)
        .await;
    let tokens = FakeTokens::new("tok", "tok2");
    let api = api_for(&server, tokens.clone());

    api.service_instances("proj1", "env1").await.unwrap();

    assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http_429_is_rate_limited_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "30"))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let err = api.service_instances("proj1", "env1").await.unwrap_err();

    match err {
        ApiError::RateLimited { retry_after } => assert_eq!(retry_after, Duration::from_secs(30)),
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert!(api.budget().lock().unwrap().blocked_until().is_some());
}

#[tokio::test]
async fn http_429_without_retry_after_defaults_to_60s() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let err = api.service_instances("proj1", "env1").await.unwrap_err();

    assert!(
        matches!(err, ApiError::RateLimited { retry_after } if retry_after == Duration::from_secs(60)),
        "{err:?}"
    );
    assert!(api.budget().lock().unwrap().blocked_until().is_some());
}

#[tokio::test]
async fn http_429_with_huge_retry_after_is_clamped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "999999999"))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let err = api.service_instances("proj1", "env1").await.unwrap_err();

    assert!(
        matches!(err, ApiError::RateLimited { retry_after } if retry_after <= Duration::from_secs(3600)),
        "{err:?}"
    );
}

#[tokio::test]
async fn policy_header_updates_budget() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("RateLimit-Policy", "1000;w=3600")
                .set_body_json(instances_body("SUCCESS")),
        )
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    api.service_instances("proj1", "env1").await.unwrap();

    assert_eq!(
        api.budget().lock().unwrap().hourly_limit(),
        (1000, LimitSource::Detected)
    );
}

#[tokio::test]
async fn graphql_errors_surface() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"errors": [{"message": "boom"}]})),
        )
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let err = api.service_instances("proj1", "env1").await.unwrap_err();

    assert!(
        matches!(&err, ApiError::GraphQl(msgs) if msgs == &vec!["boom".to_string()]),
        "{err:?}"
    );
}

#[tokio::test]
async fn http_500_is_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let err = api.service_instances("proj1", "env1").await.unwrap_err();

    assert!(matches!(err, ApiError::Http(500)), "{err:?}");
}

#[tokio::test]
async fn build_logs_uses_snapshot_filter() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("DeploymentSnapshot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"deployment": {"snapshotId": "snap1", "createdAt": "2026-10-28T10:00:00Z"}}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("EnvironmentLogs"))
        .and(body_string_contains("@snapshot:snap1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"environmentLogs": [{
                "timestamp": "2026-10-28T10:00:01.5Z",
                "message": "building",
                "attributes": [{"key": "level", "value": "info"}]
            }]}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let logs = api.build_logs("env1", "dep1", None, 100).await.unwrap();

    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "building");
    assert_eq!(
        logs[0].attributes,
        vec![("level".to_string(), "info".to_string())]
    );
}

#[tokio::test]
async fn deployment_logs_carry_replica_tag_as_attribute() {
    // Railway puts the replica id in `tags.deploymentInstanceId`, not in `attributes`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("deploymentInstanceId"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"deploymentLogs": [
                {"timestamp": "2026-10-28T10:00:00Z", "message": "a",
                 "attributes": [{"key": "level", "value": "info"}],
                 "tags": {"deploymentInstanceId": "inst-1"}},
                {"timestamp": "2026-10-28T10:00:01Z", "message": "b",
                 "attributes": [], "tags": null}
            ]}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let logs = api.deployment_logs("dep1", None, 100).await.unwrap();

    assert_eq!(
        logs[0].attributes,
        vec![
            ("level".to_string(), "info".to_string()),
            ("deploymentInstanceId".to_string(), "inst-1".to_string()),
        ]
    );
    assert!(logs[1].attributes.is_empty());
}

#[tokio::test]
async fn deployments_are_newest_first() {
    let server = MockServer::start().await;
    let node = |id: &str, at: &str| json!({"node": {"id": id, "status": "SUCCESS", "createdAt": at, "meta": {"image": "nginx"}, "instances": []}});
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"deployments": {"edges": [
                node("old", "2026-10-01T00:00:00Z"),
                node("new", "2026-10-02T00:00:00Z")
            ]}}
        })))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let deps = api.deployments("p", "e", "s", 10).await.unwrap();

    let ids: Vec<_> = deps.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["new", "old"]);
    assert_eq!(deps[0].image.as_deref(), Some("nginx"));
}

#[tokio::test]
async fn workspaces_merge_external_workspaces() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "me": {"workspaces": [{
                    "id": "w1", "name": "Mine",
                    "projects": {"edges": [{"node": {"id": "p1", "name": "app"}}]}
                }]},
                "externalWorkspaces": [
                    {"id": "w1", "name": "Mine", "projects": []},
                    {"id": "w2", "name": "Theirs", "projects": [{"id": "p2", "name": "shared"}]}
                ]
            }
        })))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let ws = api.workspaces().await.unwrap();

    assert_eq!(ws.len(), 2);
    assert_eq!(ws[0].id, "w1");
    assert_eq!(ws[0].projects[0].name, "app");
    assert_eq!(ws[1].id, "w2");
    assert_eq!(ws[1].projects[0].id, "p2");
}

#[tokio::test]
async fn environments_skip_ephemeral() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"environments": {"edges": [
                {"node": {"id": "e1", "name": "production", "isEphemeral": false}},
                {"node": {"id": "e2", "name": "pr-12", "isEphemeral": true}}
            ]}}
        })))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let envs = api.environments("p").await.unwrap();

    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].name, "production");
}

#[tokio::test]
async fn deployment_logs_map_lines() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("DeploymentLogs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"deploymentLogs": [
                {"timestamp": "2026-10-28T10:00:00Z", "message": "hello", "attributes": []}
            ]}
        })))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let logs = api.deployment_logs("dep1", None, 50).await.unwrap();

    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "hello");
}
