//! The only code that changes anything on Railway: three allowlisted
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
use crate::model::Variables;

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

    /// Sends one mutation exactly once. Refuses without sending while the rate
    /// budget is blocked; a rejected token is refreshed for next time, but the
    /// write itself is never resent.
    async fn send_write<Q: GraphQLQuery>(
        &self,
        variables: Q::Variables,
    ) -> Result<Q::ResponseData, WriteError> {
        let blocked_for = {
            let budget = self.budget.lock().unwrap_or_else(|e| e.into_inner());
            budget
                .blocked_until()
                .and_then(|until| until.checked_duration_since(Instant::now()))
                .filter(|d| !d.is_zero())
        };
        if let Some(retry_after) = blocked_for {
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
            WriteError::Unknown(ApiError::Decode("unreadable reply to a write".to_string()))
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
            "Railway answered false".to_string(),
        )))
    }
}
