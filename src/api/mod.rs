//! Railway's public GraphQL API over HTTP (queries only).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use graphql_client::{GraphQLQuery, QueryBody};
use reqwest::StatusCode;
use serde::Serialize;
use url::Url;

use crate::auth::{AuthError, TokenSource};
use crate::budget::{parse_retry_after, RateBudget};
use crate::model::{
    DeploymentStatus, DeploymentSummary, Environment, Instance, InstanceStatus, LogLine,
    ProjectRef, ServiceInstance, Workspace,
};

pub mod subscribe;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Build events can be timestamped just before the deployment is created (CLI behaviour).
const BUILD_LOG_LEAD: chrono::Duration = chrono::Duration::minutes(5);

mod ops {
    use graphql_client::GraphQLQuery;

    // Custom scalars referenced by the queries below.
    type DateTime = chrono::DateTime<chrono::Utc>;
    type DeploymentMeta = serde_json::Value;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/Workspaces.graphql",
        response_derives = "Debug"
    )]
    pub struct Workspaces;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/Environments.graphql",
        response_derives = "Debug"
    )]
    pub struct Environments;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/EnvironmentInstances.graphql",
        response_derives = "Debug"
    )]
    pub struct EnvironmentInstances;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/Deployments.graphql",
        response_derives = "Debug"
    )]
    pub struct Deployments;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/DeploymentSnapshot.graphql",
        response_derives = "Debug"
    )]
    pub struct DeploymentSnapshot;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/DeploymentLogs.graphql",
        response_derives = "Debug"
    )]
    pub struct DeploymentLogs;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/EnvironmentLogs.graphql",
        response_derives = "Debug"
    )]
    pub struct EnvironmentLogs;
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    Auth(AuthError),
    #[error("Railway API rate limit reached; retry in {}s", retry_after.as_secs())]
    RateLimited { retry_after: Duration },
    #[error("Railway API returned HTTP {0}")]
    Http(u16),
    #[error("Railway API error: {}", .0.join("; "))]
    GraphQl(Vec<String>),
    #[error("Network error talking to Railway: {0}")]
    Network(String),
    #[error("Could not decode Railway API response: {0}")]
    Decode(String),
}

/// The build snapshot of a deployment; it never changes once it exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildSnapshot {
    pub id: String,
    pub deployment_created_at: DateTime<Utc>,
}

/// Outcome of one HTTP attempt; `Rejected` means the token was refused.
enum Attempt {
    Rejected,
    Failed(ApiError),
}

pub struct Api {
    http: reqwest::Client,
    endpoint: Url,
    tokens: Arc<dyn TokenSource>,
    budget: Arc<Mutex<RateBudget>>,
}

impl Api {
    pub fn new(
        endpoint: Url,
        tokens: Arc<dyn TokenSource>,
        budget: Arc<Mutex<RateBudget>>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client with a timeout builds");
        Self {
            http,
            endpoint,
            tokens,
            budget,
        }
    }

    pub fn budget(&self) -> Arc<Mutex<RateBudget>> {
        self.budget.clone()
    }

    /// The user's own workspaces plus any external workspaces they can see.
    pub async fn workspaces(&self) -> Result<Vec<Workspace>, ApiError> {
        let data = self
            .query::<ops::Workspaces>(ops::workspaces::Variables)
            .await?;
        let mut out: Vec<Workspace> = data
            .me
            .workspaces
            .into_iter()
            .map(|w| Workspace {
                id: w.id,
                name: w.name,
                projects: w
                    .projects
                    .edges
                    .into_iter()
                    .map(|e| ProjectRef {
                        id: e.node.id,
                        name: e.node.name,
                    })
                    .collect(),
            })
            .collect();
        for w in data.external_workspaces {
            if out.iter().any(|known| known.id == w.id) {
                continue;
            }
            out.push(Workspace {
                id: w.id,
                name: w.name,
                projects: w
                    .projects
                    .into_iter()
                    .map(|p| ProjectRef {
                        id: p.id,
                        name: p.name,
                    })
                    .collect(),
            });
        }
        Ok(out)
    }

    /// Non-ephemeral environments of a project.
    pub async fn environments(&self, project_id: &str) -> Result<Vec<Environment>, ApiError> {
        let data = self
            .query::<ops::Environments>(ops::environments::Variables {
                project_id: project_id.to_string(),
            })
            .await?;
        Ok(data
            .environments
            .edges
            .into_iter()
            .filter(|e| !e.node.is_ephemeral)
            .map(|e| Environment {
                id: e.node.id,
                name: e.node.name,
            })
            .collect())
    }

    pub async fn service_instances(
        &self,
        project_id: &str,
        env_id: &str,
    ) -> Result<Vec<ServiceInstance>, ApiError> {
        use ops::environment_instances::{DeploymentFields, Variables};
        let data = self
            .query::<ops::EnvironmentInstances>(Variables {
                environment_id: env_id.to_string(),
                project_id: project_id.to_string(),
            })
            .await?;
        let summary = |d: DeploymentFields| {
            summarize(
                d.id,
                &d.status,
                d.created_at,
                d.meta,
                d.instances.into_iter().map(|i| (i.id, raw_enum(&i.status))),
            )
        };
        Ok(data
            .environment
            .service_instances
            .edges
            .into_iter()
            .map(|e| {
                let n = e.node;
                ServiceInstance {
                    service_id: n.service_id,
                    service_name: n.service_name,
                    // Railway runs one replica when none is configured.
                    num_replicas: n
                        .num_replicas
                        .and_then(|r| u32::try_from(r).ok())
                        .unwrap_or(1),
                    latest: n.latest_deployment.map(summary),
                    active: n.active_deployments.into_iter().map(summary).collect(),
                }
            })
            .collect())
    }

    /// A service's deployment history, newest first.
    pub async fn deployments(
        &self,
        project_id: &str,
        env_id: &str,
        service_id: &str,
        first: i64,
    ) -> Result<Vec<DeploymentSummary>, ApiError> {
        let data = self
            .query::<ops::Deployments>(ops::deployments::Variables {
                project_id: project_id.to_string(),
                environment_id: env_id.to_string(),
                service_id: service_id.to_string(),
                first,
            })
            .await?;
        let mut out: Vec<DeploymentSummary> = data
            .deployments
            .edges
            .into_iter()
            .map(|e| {
                let d = e.node;
                summarize(
                    d.id,
                    &d.status,
                    d.created_at,
                    d.meta,
                    d.instances.into_iter().map(|i| (i.id, raw_enum(&i.status))),
                )
            })
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.created_at));
        Ok(out)
    }

    /// Runtime logs of one deployment.
    pub async fn deployment_logs(
        &self,
        deployment_id: &str,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<LogLine>, ApiError> {
        let data = self
            .query::<ops::DeploymentLogs>(ops::deployment_logs::Variables {
                deployment_id: deployment_id.to_string(),
                limit,
                start_date: since,
            })
            .await?;
        data.deployment_logs
            .into_iter()
            .map(|l| {
                let replica = l.tags.and_then(|t| t.deployment_instance_id);
                log_line(
                    &l.timestamp,
                    l.message,
                    l.attributes
                        .into_iter()
                        .map(|a| (a.key, a.value))
                        .chain(replica_attribute(replica)),
                )
            })
            .collect()
    }

    /// The snapshot a deployment was built from; build logs are scoped by it.
    /// `None` when the deployment had no build (an image-based deployment).
    pub async fn build_snapshot(
        &self,
        deployment_id: &str,
    ) -> Result<Option<BuildSnapshot>, ApiError> {
        let deployment = self
            .query::<ops::DeploymentSnapshot>(ops::deployment_snapshot::Variables {
                id: deployment_id.to_string(),
            })
            .await?
            .deployment;
        Ok(deployment.snapshot_id.map(|id| BuildSnapshot {
            id,
            deployment_created_at: deployment.created_at,
        }))
    }

    /// Build logs, scoped the way the Railway CLI and dashboard do it: by the
    /// deployment's snapshot id via `environmentLogs`. A deployment without a
    /// snapshot (no build) has no build logs.
    pub async fn build_logs(
        &self,
        env_id: &str,
        deployment_id: &str,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<LogLine>, ApiError> {
        match self.build_snapshot(deployment_id).await? {
            Some(snapshot) => self.snapshot_logs(env_id, &snapshot, since, limit).await,
            None => Ok(Vec::new()),
        }
    }

    /// Build logs of a known `snapshot` (see [`Api::build_logs`]).
    pub async fn snapshot_logs(
        &self,
        env_id: &str,
        snapshot: &BuildSnapshot,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<LogLine>, ApiError> {
        let snapshot_id = &snapshot.id;
        let start = since.unwrap_or(snapshot.deployment_created_at - BUILD_LOG_LEAD);
        let now = Utc::now();
        let stamp = |d: DateTime<Utc>| d.to_rfc3339_opts(SecondsFormat::Nanos, true);
        let data = self
            .query::<ops::EnvironmentLogs>(ops::environment_logs::Variables {
                environment_id: env_id.to_string(),
                filter: build_log_filter(snapshot_id),
                before_limit: limit,
                before_date: stamp(start),
                anchor_date: stamp(now),
                after_date: stamp(now),
                after_limit: 0,
            })
            .await?;
        let mut logs = data.environment_logs;
        // The API can return one more line than asked for; keep the newest `limit`.
        let keep = usize::try_from(limit.max(0)).unwrap_or(usize::MAX);
        if logs.len() > keep {
            logs.drain(..logs.len() - keep);
        }
        logs.into_iter()
            .map(|l| {
                log_line(
                    &l.timestamp,
                    l.message,
                    l.attributes.into_iter().map(|a| (a.key, a.value)),
                )
            })
            .collect()
    }

    /// Runs one query, refreshing the token and retrying once if it is rejected.
    async fn query<Q: GraphQLQuery>(
        &self,
        variables: Q::Variables,
    ) -> Result<Q::ResponseData, ApiError> {
        let body = Q::build_query(variables);
        let token = self.auth(|t| t.token()).await?;
        match self.attempt::<Q>(&body, &token).await {
            Ok(data) => return Ok(data),
            Err(Attempt::Failed(e)) => return Err(e),
            Err(Attempt::Rejected) => {}
        }
        let token = self
            .auth(move |t| t.refresh_after_rejection_of(&token))
            .await?;
        match self.attempt::<Q>(&body, &token).await {
            Ok(data) => Ok(data),
            Err(Attempt::Failed(e)) => Err(e),
            Err(Attempt::Rejected) => Err(ApiError::Auth(AuthError::Rejected)),
        }
    }

    async fn auth<F>(&self, f: F) -> Result<String, ApiError>
    where
        F: FnOnce(&dyn TokenSource) -> Result<String, AuthError> + Send + 'static,
    {
        blocking_token(&self.tokens, f).await
    }

    async fn attempt<Q: GraphQLQuery>(
        &self,
        body: &QueryBody<Q::Variables>,
        token: &str,
    ) -> Result<Q::ResponseData, Attempt> {
        let resp = self
            .http
            .post(self.endpoint.clone())
            .bearer_auth(token)
            .json(body)
            .send()
            .await
            .map_err(|e| Attempt::Failed(ApiError::Network(e.to_string())))?;
        let status = resp.status();
        {
            let mut budget = self.budget.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            budget.observe(resp.headers(), now);
            if status == StatusCode::TOO_MANY_REQUESTS {
                // The budget owns the default and the clamp, so the gate and the error agree.
                let retry_after = budget.on_rate_limited(parse_retry_after(resp.headers()), now);
                return Err(Attempt::Failed(ApiError::RateLimited { retry_after }));
            }
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(Attempt::Rejected);
        }
        if !status.is_success() {
            return Err(Attempt::Failed(ApiError::Http(status.as_u16())));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| Attempt::Failed(ApiError::Network(e.to_string())))?;
        let parsed: graphql_client::Response<Q::ResponseData> = serde_json::from_slice(&bytes)
            .map_err(|e| Attempt::Failed(ApiError::Decode(e.to_string())))?;
        if let Some(errors) = parsed.errors.filter(|e| !e.is_empty()) {
            if errors.iter().any(|e| e.message.contains("Not Authorized")) {
                return Err(Attempt::Rejected);
            }
            return Err(Attempt::Failed(ApiError::GraphQl(
                errors.into_iter().map(|e| e.message).collect(),
            )));
        }
        parsed
            .data
            .ok_or_else(|| Attempt::Failed(ApiError::Decode("response has no data".into())))
    }
}

/// Token calls may run a subprocess, so they go to the blocking pool.
async fn blocking_token<F>(tokens: &Arc<dyn TokenSource>, f: F) -> Result<String, ApiError>
where
    F: FnOnce(&dyn TokenSource) -> Result<String, AuthError> + Send + 'static,
{
    let tokens = tokens.clone();
    tokio::task::spawn_blocking(move || f(tokens.as_ref()))
        .await
        .map_err(|e| ApiError::Auth(AuthError::Io(e.to_string())))?
        .map_err(ApiError::Auth)
}

/// Scopes `environmentLogs` to one build, as the CLI and dashboard do.
fn build_log_filter(snapshot_id: &str) -> String {
    format!("(@snapshot:{snapshot_id} OR @replica:{snapshot_id})")
}

/// Attribute key under which a runtime log line carries its replica id.
pub const REPLICA_ATTR_KEY: &str = "deploymentInstanceId";

/// Railway reports the replica in `tags.deploymentInstanceId`, not in the
/// line's attributes; it is folded into the attributes under [`REPLICA_ATTR_KEY`].
fn replica_attribute(instance_id: Option<String>) -> Option<(String, String)> {
    instance_id.map(|id| (REPLICA_ATTR_KEY.to_string(), id))
}

/// The raw API spelling of a generated enum value, including its `Other(raw)` variant.
fn raw_enum<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn summarize<S: Serialize>(
    id: String,
    status: &S,
    created_at: DateTime<Utc>,
    meta: Option<serde_json::Value>,
    instances: impl Iterator<Item = (String, String)>,
) -> DeploymentSummary {
    let meta_str = |key: &str| {
        meta.as_ref()
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    DeploymentSummary {
        id,
        status: DeploymentStatus::from_api(&raw_enum(status)),
        created_at,
        commit_sha: meta_str("commitHash"),
        commit_message: meta_str("commitMessage"),
        image: meta_str("image"),
        instances: instances
            .map(|(id, status)| Instance {
                id,
                status: InstanceStatus::from_api(&status),
            })
            .collect(),
    }
}

fn log_line(
    timestamp: &str,
    message: String,
    attributes: impl Iterator<Item = (String, String)>,
) -> Result<LogLine, ApiError> {
    let timestamp = DateTime::parse_from_rfc3339(timestamp)
        .map_err(|e| ApiError::Decode(format!("log timestamp {timestamp:?}: {e}")))?
        .with_timezone(&Utc);
    Ok(LogLine {
        timestamp,
        message,
        attributes: attributes.collect(),
    })
}
