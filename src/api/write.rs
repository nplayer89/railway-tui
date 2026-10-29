//! The only code that changes anything on Railway: five allowlisted
//! mutations (`tests/no_mutations.rs` enforces the list).
//!
//! Every write goes through [`Api::send_write`], which shares the read path's
//! HTTP and rate-budget handling but never resends, not even after a 401: a
//! write that may have landed must not be applied twice behind the user's back.
//! Values are never put into an error; only Railway's own messages are kept.

use std::time::Instant;

use graphql_client::GraphQLQuery;

use super::{Api, ApiError, Attempt};
use crate::auth::AuthError;
use crate::model::{Variables, WriteFailure};

mod ops {
    use graphql_client::GraphQLQuery;

    type EnvironmentVariables = crate::model::Variables;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/write/VariableCollectionUpsert.graphql"
    )]
    pub struct VariableCollectionUpsert;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/write/VariableDelete.graphql"
    )]
    pub struct VariableDelete;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/write/ServiceInstanceRedeploy.graphql"
    )]
    pub struct ServiceInstanceRedeploy;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/write/ServiceInstanceDeploy.graphql"
    )]
    pub struct ServiceInstanceDeploy;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/write/DeploymentTriggerUpdate.graphql"
    )]
    pub struct DeploymentTriggerUpdate;
}

/// Why a write did not succeed, split by whether Railway may have applied it.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// Railway did not apply the write (4xx, GraphQL error, auth, rate limit, connect failure).
    #[error("Railway did not apply the change: {0}")]
    Refused(ApiError),
    /// Sent, but the outcome is unknown (timeout, connection lost, 5xx, undecodable reply).
    #[error("Railway may or may not have applied the change: {0}")]
    Unknown(ApiError),
}

impl WriteError {
    /// Railway's own error, and whether the write may have been applied anyway.
    pub fn into_parts(self) -> (ApiError, WriteFailure) {
        match self {
            WriteError::Refused(e) => (e, WriteFailure::Refused),
            WriteError::Unknown(e) => (e, WriteFailure::Unknown),
        }
    }
}

impl Api {
    /// Creates or updates `vars` on a service without touching any other
    /// variable (`replace: false`) and without triggering a deploy (`skipDeploys: true`).
    pub async fn upsert_variables(
        &self,
        project_id: &str,
        env_id: &str,
        service_id: &str,
        vars: &Variables,
    ) -> Result<(), WriteError> {
        use ops::variable_collection_upsert::{VariableCollectionUpsertInput, Variables as Vars};
        let data = self
            .send_write::<ops::VariableCollectionUpsert>(Vars {
                input: VariableCollectionUpsertInput {
                    project_id: project_id.to_string(),
                    environment_id: env_id.to_string(),
                    service_id: Some(service_id.to_string()),
                    variables: vars.clone(),
                    replace: Some(false),
                    skip_deploys: Some(true),
                },
            })
            .await?;
        applied(data.variable_collection_upsert)
    }

    /// Deletes one variable. A variable that is already gone counts as deleted.
    pub async fn delete_variable(
        &self,
        project_id: &str,
        env_id: &str,
        service_id: &str,
        name: &str,
    ) -> Result<(), WriteError> {
        use ops::variable_delete::{VariableDeleteInput, Variables as Vars};
        let result = self
            .send_write::<ops::VariableDelete>(Vars {
                input: VariableDeleteInput {
                    project_id: project_id.to_string(),
                    environment_id: env_id.to_string(),
                    service_id: Some(service_id.to_string()),
                    name: name.to_string(),
                },
            })
            .await;
        match result {
            Ok(data) => applied(data.variable_delete),
            // Unconfirmed guess at Railway's wording; Neil's first live run checks it.
            Err(WriteError::Refused(ApiError::GraphQl(messages)))
                if messages
                    .iter()
                    .any(|m| m.to_lowercase().contains("not found")) =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Redeploys a service's latest deployment in one environment.
    pub async fn redeploy_service(&self, env_id: &str, service_id: &str) -> Result<(), WriteError> {
        let data = self
            .send_write::<ops::ServiceInstanceRedeploy>(ops::service_instance_redeploy::Variables {
                environment_id: env_id.to_string(),
                service_id: service_id.to_string(),
            })
            .await?;
        applied(data.service_instance_redeploy)
    }

    /// Deploys the latest commit of a service's connected branch in one environment.
    pub async fn deploy_latest_commit(
        &self,
        env_id: &str,
        service_id: &str,
    ) -> Result<(), WriteError> {
        let data = self
            .send_write::<ops::ServiceInstanceDeploy>(ops::service_instance_deploy::Variables {
                environment_id: env_id.to_string(),
                service_id: service_id.to_string(),
            })
            .await?;
        applied(data.service_instance_deploy)
    }

    /// Points a deploy trigger at another branch: the service deploys from it
    /// from now on. Applied only when Railway answers with that branch.
    pub async fn switch_branch(&self, trigger_id: &str, branch: &str) -> Result<(), WriteError> {
        let data = self
            .send_write::<ops::DeploymentTriggerUpdate>(ops::deployment_trigger_update::Variables {
                id: trigger_id.to_string(),
                branch: branch.to_string(),
            })
            .await?;
        applied(data.deployment_trigger_update.branch == branch)
    }

    /// Sends one mutation exactly once. Refuses without sending while the rate
    /// budget is blocked; a rejected token is refreshed for next time, but the
    /// write itself is never resent.
    async fn send_write<Q: GraphQLQuery>(
        &self,
        variables: Q::Variables,
    ) -> Result<Q::ResponseData, WriteError> {
        if let Some(retry_after) = self.budget.blocked_for(Instant::now()) {
            return Err(WriteError::Refused(ApiError::RateLimited { retry_after }));
        }

        let body = Q::build_query(variables);
        let token = self
            .auth(|t| t.token())
            .await
            .map_err(WriteError::Refused)?;
        match self.attempt::<Q>(&body, &token).await {
            Ok(data) => Ok(data),
            Err(Attempt::Rejected) => {
                // Refresh so the user's next action has a working token. Its
                // outcome does not change this write's: it was refused.
                let _ = self
                    .auth(move |t| t.refresh_after_rejection_of(&token))
                    .await;
                Err(WriteError::Refused(ApiError::Auth(AuthError::Rejected)))
            }
            Err(Attempt::Unsent(e)) => Err(WriteError::Refused(e)),
            Err(Attempt::Failed(e)) => Err(classify(e)),
        }
    }
}

/// Splits a failure of a sent write by whether Railway may have applied it.
fn classify(e: ApiError) -> WriteError {
    match e {
        ApiError::Auth(_) | ApiError::RateLimited { .. } | ApiError::GraphQl(_) => {
            WriteError::Refused(e)
        }
        ApiError::Http(code) if (400..500).contains(&code) => WriteError::Refused(e),
        ApiError::Http(_) | ApiError::Network(_) => WriteError::Unknown(e),
        // A decoder message can quote the reply; keep it out of the error.
        ApiError::Decode(_) => {
            WriteError::Unknown(ApiError::Decode("unreadable reply to a write".into()))
        }
    }
}

/// Every mutation here answers `true` when it was applied. Anything else is
/// treated as unknown so the user checks rather than trusting either outcome.
fn applied(ok: bool) -> Result<(), WriteError> {
    if ok {
        Ok(())
    } else {
        Err(WriteError::Unknown(ApiError::Decode(
            "Railway answered false".into(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decode_error_on_a_write_keeps_no_trace_of_the_reply() {
        let quoted = serde_json::from_str::<u32>("\"s3cret-value\"").unwrap_err();
        assert!(quoted.to_string().contains("s3cret-value"), "{quoted}");
        let err = classify(ApiError::Decode(quoted.into()));
        let mut shown = vec![err.to_string(), format!("{err:?}")];
        let mut source = std::error::Error::source(&err);
        while let Some(e) = source {
            shown.push(e.to_string());
            source = e.source();
        }
        for text in shown {
            assert!(!text.contains("s3cret-value"), "{text}");
        }
    }
}
