//! HTTP API client tests against a wiremock server (never the live API).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use railway_tui::api::write::WriteError;
use railway_tui::api::{Api, ApiError};
use railway_tui::auth::{AuthError, TokenSource};
use railway_tui::budget::{LimitSource, RateBudget, SharedBudget};
use railway_tui::model::{DeploymentStatus, Instance, InstanceStatus, Variables};
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
    fn refresh_after_rejection_of(&self, rejected_token: &str) -> Result<String, AuthError> {
        assert_eq!(
            rejected_token, self.token,
            "refresh must name the refused token"
        );
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(self.refreshed.clone())
    }
}

fn api_for(server: &MockServer, tokens: Arc<FakeTokens>) -> Api {
    let endpoint = Url::parse(&format!("{}/graphql/v2", server.uri())).unwrap();
    Api::new(endpoint, tokens, SharedBudget::new(RateBudget::new(None))).unwrap()
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
    assert!(api.budget().lock().blocked_until().is_some());
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
    assert!(api.budget().lock().blocked_until().is_some());
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
        api.budget().lock().hourly_limit(),
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

#[tokio::test]
async fn variables_request_is_unrendered_and_parses_map() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"variables": {"A": "1", "REF": "${{Postgres.DATABASE_URL}}"}}
        })))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let vars = api.variables("p1", "e1", "s1").await.unwrap();

    let mut want = std::collections::BTreeMap::new();
    want.insert("A".to_string(), "1".to_string());
    want.insert("REF".to_string(), "${{Postgres.DATABASE_URL}}".to_string());
    assert_eq!(vars, want);
    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let v = &body["variables"];
    assert_eq!(v["unrendered"], json!(true));
    assert_eq!(v["projectId"], json!("p1"));
    assert_eq!(v["environmentId"], json!("e1"));
    assert_eq!(v["serviceId"], json!("s1"));
}

#[tokio::test]
async fn variables_error_message_has_no_values() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": null, "errors": [{"message": "Problem processing request"}]}),
        ))
        .mount(&server)
        .await;
    let api = api_for(&server, FakeTokens::new("tok", "tok2"));

    let err = api.variables("p1", "e1", "s1").await.unwrap_err();

    let text = err.to_string();
    for secret in ["tok", "p1", "e1", "s1"] {
        assert!(!text.contains(secret), "{text}");
    }
}

// ---- Writes (mutations). Every write path is tested here, never against Railway. ----

fn write_api(server: &MockServer, tokens: Arc<FakeTokens>, timeout: Duration) -> Api {
    let endpoint = Url::parse(&format!("{}/graphql/v2", server.uri())).unwrap();
    Api::with_timeout(
        endpoint,
        tokens,
        SharedBudget::new(RateBudget::new(None)),
        timeout,
    )
    .unwrap()
}

fn ok_body(field: &str) -> serde_json::Value {
    json!({"data": {field: true}})
}

fn tricky_vars() -> Variables {
    let mut vars = Variables::new();
    for (k, v) in [
        ("EMPTY", ""),
        ("EQ", "a=b"),
        ("Q", "\"x'"),
        ("NL", "l1\nl2"),
        ("U", "héllo ✓"),
    ] {
        vars.insert(k.to_string(), v.to_string());
    }
    vars.insert("BIG".to_string(), "x".repeat(10240));
    vars
}

async fn sent_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn upsert_sends_skip_deploys_and_no_replace() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("variableCollectionUpsert"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("variableCollectionUpsert")))
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );
    let vars = tricky_vars();

    api.upsert_variables("p1", "e1", "s1", &vars).await.unwrap();

    let bodies = sent_bodies(&server).await;
    let input = &bodies[0]["variables"]["input"];
    assert_eq!(input["skipDeploys"], json!(true));
    assert_eq!(input["replace"], json!(false));
    assert_eq!(input["projectId"], json!("p1"));
    assert_eq!(input["environmentId"], json!("e1"));
    assert_eq!(input["serviceId"], json!("s1"));
    let sent: Variables = serde_json::from_value(input["variables"].clone()).unwrap();
    assert_eq!(sent, vars, "values must arrive byte-for-byte");
}

#[tokio::test]
async fn write_401_is_refused_and_not_resent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let tokens = FakeTokens::new("tok", "tok2");
    let api = write_api(&server, tokens.clone(), Duration::from_secs(5));

    let err = api
        .upsert_variables("p1", "e1", "s1", &tricky_vars())
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            WriteError::Refused(ApiError::Auth(AuthError::Rejected))
        ),
        "{err:?}"
    );
    assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn write_not_authorized_is_refused_and_not_resent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": null, "errors": [{"message": "Not Authorized"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let tokens = FakeTokens::new("tok", "tok2");
    let api = write_api(&server, tokens.clone(), Duration::from_secs(5));

    let err = api.redeploy_service("e1", "s1").await.unwrap_err();

    assert!(
        matches!(
            err,
            WriteError::Refused(ApiError::Auth(AuthError::Rejected))
        ),
        "{err:?}"
    );
    assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn write_500_is_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );

    let err = api
        .upsert_variables("p1", "e1", "s1", &tricky_vars())
        .await
        .unwrap_err();

    assert!(
        matches!(err, WriteError::Unknown(ApiError::Http(500))),
        "{err:?}"
    );
}

#[tokio::test]
async fn write_400_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );

    let err = api.redeploy_service("e1", "s1").await.unwrap_err();

    assert!(
        matches!(err, WriteError::Refused(ApiError::Http(400))),
        "{err:?}"
    );
}

#[tokio::test]
async fn write_graphql_error_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": null, "errors": [{"message": "Problem processing request"}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );
    let vars = tricky_vars();

    let err = api
        .upsert_variables("p1", "e1", "s1", &vars)
        .await
        .unwrap_err();

    assert!(
        matches!(err, WriteError::Refused(ApiError::GraphQl(_))),
        "{err:?}"
    );
    let shown = format!("{err} {err:?}");
    for value in ["a=b", "l1", "héllo"] {
        assert!(!shown.contains(value), "value leaked: {shown}");
    }
}

#[tokio::test]
async fn write_timeout_is_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_body("variableCollectionUpsert"))
                .set_delay(Duration::from_secs(2)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_millis(200),
    );

    let err = api
        .upsert_variables("p1", "e1", "s1", &tricky_vars())
        .await
        .unwrap_err();

    assert!(
        matches!(err, WriteError::Unknown(ApiError::Network(_))),
        "{err:?}"
    );
}

#[tokio::test]
async fn write_429_is_refused_and_blocks_the_budget() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "30"))
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );

    let err = api.redeploy_service("e1", "s1").await.unwrap_err();

    assert!(
        matches!(err, WriteError::Refused(ApiError::RateLimited { .. })),
        "{err:?}"
    );
    assert!(api.budget().lock().blocked_until().is_some());
}

#[tokio::test]
async fn write_is_refused_without_sending_while_budget_blocked() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("serviceInstanceRedeploy")))
        .expect(0)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );
    api.budget()
        .lock()
        .on_rate_limited(Some(Duration::from_secs(60)), std::time::Instant::now());

    let err = api.redeploy_service("e1", "s1").await.unwrap_err();

    assert!(
        matches!(err, WriteError::Refused(ApiError::RateLimited { .. })),
        "{err:?}"
    );
}

#[tokio::test]
async fn write_connect_failure_is_refused() {
    // A local port with nothing listening: the request is never sent.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let endpoint = Url::parse(&format!("http://127.0.0.1:{port}/graphql/v2")).unwrap();
    let api = Api::with_timeout(
        endpoint,
        FakeTokens::new("tok", "tok2"),
        SharedBudget::new(RateBudget::new(None)),
        Duration::from_secs(5),
    )
    .unwrap();

    let err = api.redeploy_service("e1", "s1").await.unwrap_err();

    assert!(
        matches!(err, WriteError::Refused(ApiError::Network(_))),
        "{err:?}"
    );
}

#[tokio::test]
async fn delete_sends_name_and_ids() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("variableDelete"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("variableDelete")))
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );

    api.delete_variable("p1", "e1", "s1", "OLD").await.unwrap();

    let bodies = sent_bodies(&server).await;
    let input = &bodies[0]["variables"]["input"];
    assert_eq!(input["name"], json!("OLD"));
    assert_eq!(input["projectId"], json!("p1"));
    assert_eq!(input["environmentId"], json!("e1"));
    assert_eq!(input["serviceId"], json!("s1"));
}

#[tokio::test]
async fn delete_not_found_is_ok() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"data": null, "errors": [{"message": "Variable Not Found"}]}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );

    api.delete_variable("p1", "e1", "s1", "GONE").await.unwrap();
}

#[tokio::test]
async fn delete_other_graphql_error_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data": null, "errors": [{"message": "Problem processing request"}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );

    let err = api
        .delete_variable("p1", "e1", "s1", "OLD")
        .await
        .unwrap_err();

    assert!(
        matches!(err, WriteError::Refused(ApiError::GraphQl(_))),
        "{err:?}"
    );
}

#[tokio::test]
async fn redeploy_sends_env_and_service() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("serviceInstanceRedeploy"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body("serviceInstanceRedeploy")))
        .expect(1)
        .mount(&server)
        .await;
    let api = write_api(
        &server,
        FakeTokens::new("tok", "tok2"),
        Duration::from_secs(5),
    );

    api.redeploy_service("e1", "s1").await.unwrap();

    let bodies = sent_bodies(&server).await;
    assert_eq!(bodies[0]["variables"]["environmentId"], json!("e1"));
    assert_eq!(bodies[0]["variables"]["serviceId"], json!("s1"));
}
