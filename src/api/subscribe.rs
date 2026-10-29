//! GraphQL subscriptions over a `graphql-transport-ws` websocket (read-only).
//! The connection follows the Railway CLI's `src/subscription.rs`.

use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use futures::stream::{self, BoxStream};
use futures::{SinkExt, StreamExt, TryStreamExt};
use graphql_client::GraphQLQuery;
use graphql_ws_client::graphql::StreamingOperation;
use graphql_ws_client::Client;
use reqwest::StatusCode;
use reqwest_websocket::{RequestBuilderExt, WebSocket};
use url::Url;

use super::{blocking_token, build_log_filter, log_line, raw_enum, replica_attribute, ApiError};
use crate::auth::{AuthError, TokenSource};
use crate::model::{DeploymentStatus, LogLine};

/// Covers the upgrade handshake (DNS + TCP + TLS + 101); the CLI uses the same.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on the whole connect, including waiting for `connection_ack`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A ping goes out after this long without hearing from the server.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// A frame that cannot be written in this long means the socket is dead.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// How many log lines a fresh subscription replays (the CLI's value).
const STREAM_LIMIT: i64 = 500;
/// How far back a build-log subscription starts, overlapping the history fetch.
const BUILD_STREAM_OVERLAP: chrono::Duration = chrono::Duration::minutes(1);

mod ops {
    use graphql_client::GraphQLQuery;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/DeploymentLogsSub.graphql",
        response_derives = "Debug"
    )]
    pub struct DeploymentLogsSub;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/EnvironmentLogsSub.graphql",
        response_derives = "Debug"
    )]
    pub struct EnvironmentLogsSub;

    #[derive(GraphQLQuery)]
    #[graphql(
        schema_path = "graphql/schema.json",
        query_path = "graphql/DeploymentStatusSub.graphql",
        response_derives = "Debug"
    )]
    pub struct DeploymentStatusSub;
}

/// Opens a `graphql-transport-ws` connection, authenticated on the upgrade
/// request. The connection actor runs on its own task; when the socket dies or
/// the keepalive gives up, the actor stops and every subscription on the
/// client ends, which the caller sees as its stream finishing.
///
/// Keepalive: after 15s of silence a ping is sent, and again 15s later; if
/// nothing at all has arrived 30s after the first unanswered ping, the
/// connection is closed.
///
/// `http` must be HTTP/1.1 only, as [`Api::ws_http`](super::Api::ws_http) is.
pub async fn connect(http: &reqwest::Client, ws: &Url, token: &str) -> Result<Client, ApiError> {
    tokio::time::timeout(CONNECT_TIMEOUT, open(http, ws, token))
        .await
        .map_err(|_| ApiError::Network("timed out connecting to the Railway websocket".into()))?
}

async fn open(http: &reqwest::Client, ws: &Url, token: &str) -> Result<Client, ApiError> {
    let resp = http
        .get(ws.clone())
        .timeout(UPGRADE_TIMEOUT)
        .bearer_auth(token)
        .upgrade()
        .protocols(["graphql-transport-ws"])
        .send()
        .await
        .map_err(|e| ApiError::Network(e.into()))?;
    let status = resp.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(ApiError::Auth(AuthError::Rejected));
    }
    if status.is_client_error() || status.is_server_error() {
        return Err(ApiError::Http(status.as_u16()));
    }
    let socket = resp
        .into_websocket()
        .await
        .map_err(|e| ApiError::Network(e.into()))?;
    handshake(Socket(socket), KEEPALIVE_INTERVAL).await
}

/// Runs `connection_init`/`connection_ack` and spawns the connection actor.
async fn handshake<C>(connection: C, keepalive: Duration) -> Result<Client, ApiError>
where
    C: graphql_ws_client::Connection + Send + 'static,
{
    let (client, actor) = Client::build(connection)
        .keep_alive_interval(keepalive)
        // The crate pings after one silent interval and gives up after
        // `1 + retries` more; 0 retries closes 2 intervals after the first ping.
        .keep_alive_retries(0)
        .await
        .map_err(ws_error)?;
    tokio::spawn(actor.into_future());
    Ok(client)
}

/// Connects with a token from `tokens`; if the upgrade is refused, asks the
/// token source for a fresh one and tries once more (as `Api` does for queries).
pub async fn connect_authed(
    http: &reqwest::Client,
    ws: &Url,
    tokens: &Arc<dyn TokenSource>,
) -> Result<Client, ApiError> {
    let token = blocking_token(tokens, |t| t.token()).await?;
    match connect(http, ws, &token).await {
        Err(ApiError::Auth(AuthError::Rejected)) => {}
        other => return other,
    }
    let token = blocking_token(tokens, move |t| t.refresh_after_rejection_of(&token)).await?;
    connect(http, ws, &token).await
}

/// Live status of one deployment.
pub fn deployment_status_stream(
    client: &Client,
    deployment_id: &str,
) -> BoxStream<'static, Result<DeploymentStatus, ApiError>> {
    let op = StreamingOperation::<ops::DeploymentStatusSub>::new(
        ops::deployment_status_sub::Variables {
            id: deployment_id.to_string(),
        },
    );
    subscription(client, op)
        .map(|item| {
            Ok(DeploymentStatus::from_api(&raw_enum(
                &item?.deployment.status,
            )))
        })
        .boxed()
}

/// Runtime log lines of one deployment, in batches as the server sends them.
pub fn deployment_logs_stream(
    client: &Client,
    deployment_id: &str,
) -> BoxStream<'static, Result<Vec<LogLine>, ApiError>> {
    let op =
        StreamingOperation::<ops::DeploymentLogsSub>::new(ops::deployment_logs_sub::Variables {
            deployment_id: deployment_id.to_string(),
            filter: Some(String::new()),
            limit: Some(STREAM_LIMIT),
        });
    subscription(client, op)
        .map(|item| {
            item?
                .deployment_logs
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
        })
        .boxed()
}

/// Build log lines of the build `snapshot_id`, scoped like `Api::build_logs`.
pub fn build_logs_stream(
    client: &Client,
    env_id: &str,
    snapshot_id: &str,
    now: DateTime<Utc>,
) -> BoxStream<'static, Result<Vec<LogLine>, ApiError>> {
    let op =
        StreamingOperation::<ops::EnvironmentLogsSub>::new(ops::environment_logs_sub::Variables {
            environment_id: env_id.to_string(),
            filter: Some(build_log_filter(snapshot_id)),
            before_limit: Some(STREAM_LIMIT),
            before_date: Some(
                (now - BUILD_STREAM_OVERLAP).to_rfc3339_opts(SecondsFormat::Nanos, true),
            ),
            anchor_date: None,
            after_date: None,
            after_limit: Some(0),
        });
    subscription(client, op)
        .map(|item| {
            item?
                .environment_logs
                .into_iter()
                .map(|l| {
                    log_line(
                        &l.timestamp,
                        l.message,
                        l.attributes.into_iter().map(|a| (a.key, a.value)),
                    )
                })
                .collect()
        })
        .boxed()
}

/// Starts `op` on `client` and yields each response's data. A GraphQL error
/// ends the operation server-side, so it is surfaced as an `Err` item for the
/// caller to treat as a dropped stream.
fn subscription<Q>(
    client: &Client,
    op: StreamingOperation<Q>,
) -> BoxStream<'static, Result<Q::ResponseData, ApiError>>
where
    Q: GraphQLQuery + Send + Sync + Unpin + 'static,
    Q::Variables: Send + Sync + Unpin,
    Q::ResponseData: Send + 'static,
{
    let client = client.clone();
    stream::once(async move { client.subscribe(op).await.map_err(ws_error) })
        .map_ok(|sub| {
            sub.map(|item| {
                let resp = item.map_err(ws_error)?;
                if let Some(errors) = resp.errors.filter(|e| !e.is_empty()) {
                    if errors.iter().any(|e| e.message.contains("Not Authorized")) {
                        return Err(ApiError::Auth(AuthError::Rejected));
                    }
                    return Err(ApiError::GraphQl(
                        errors.into_iter().map(|e| e.message).collect(),
                    ));
                }
                resp.data
                    .ok_or_else(|| ApiError::Decode("subscription message has no data".into()))
            })
        })
        .try_flatten()
        .boxed()
}

fn ws_error(e: graphql_ws_client::Error) -> ApiError {
    match e {
        graphql_ws_client::Error::Decode(msg) => ApiError::Decode(msg.into()),
        other => ApiError::Network(other.into()),
    }
}

/// Adapts `reqwest_websocket` to `graphql_ws_client` (as the CLI does), with a
/// write timeout so a wedged socket ends the connection instead of stalling it.
struct Socket(WebSocket);

impl graphql_ws_client::Connection for Socket {
    fn receive(&mut self) -> impl Future<Output = Option<graphql_ws_client::Message>> + Send {
        use graphql_ws_client::Message as Out;
        use reqwest_websocket::Message as In;
        async {
            loop {
                let message = self.0.next().await?.ok()?;
                return Some(match message {
                    In::Text(t) => Out::Text(t),
                    In::Binary(_) => continue,
                    In::Ping(_) => Out::Ping,
                    In::Pong(_) => Out::Pong,
                    In::Close { code, reason } => Out::Close {
                        code: Some(code.into()),
                        reason: Some(reason),
                    },
                });
            }
        }
    }

    fn send(
        &mut self,
        message: graphql_ws_client::Message,
    ) -> impl Future<Output = Result<(), graphql_ws_client::Error>> + Send {
        use graphql_ws_client::Message as In;
        use reqwest_websocket::Message as Out;
        async {
            let message = match message {
                In::Text(t) => Out::Text(t),
                In::Close { code, reason } => Out::Close {
                    code: code.unwrap_or(0).into(),
                    reason: reason.unwrap_or_default(),
                },
                In::Ping => Out::Ping(Default::default()),
                In::Pong => Out::Pong(Default::default()),
            };
            match tokio::time::timeout(SEND_TIMEOUT, self.0.send(message)).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(graphql_ws_client::Error::Send(e.to_string())),
                Err(_) => Err(graphql_ws_client::Error::Send(
                    "websocket write timed out".into(),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use super::*;

    /// An in-memory socket: acks the handshake, then either goes silent or
    /// answers every ping with a pong. Records what the client sends.
    struct FakeSocket {
        acked: bool,
        answer_pings: bool,
        pings: tokio::sync::mpsc::UnboundedReceiver<()>,
        ping_tx: tokio::sync::mpsc::UnboundedSender<()>,
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl FakeSocket {
        fn new(answer_pings: bool) -> (Self, Arc<Mutex<Vec<String>>>) {
            let (ping_tx, pings) = tokio::sync::mpsc::unbounded_channel();
            let sent = Arc::new(Mutex::new(Vec::new()));
            let socket = Self {
                acked: false,
                answer_pings,
                pings,
                ping_tx,
                sent: sent.clone(),
            };
            (socket, sent)
        }
    }

    impl graphql_ws_client::Connection for FakeSocket {
        async fn receive(&mut self) -> Option<graphql_ws_client::Message> {
            if !self.acked {
                self.acked = true;
                return Some(graphql_ws_client::Message::Text(
                    r#"{"type":"connection_ack"}"#.into(),
                ));
            }
            // A silent socket never queues a pong, so this waits forever.
            self.pings.recv().await?;
            Some(graphql_ws_client::Message::Text(
                r#"{"type":"pong"}"#.into(),
            ))
        }

        async fn send(
            &mut self,
            message: graphql_ws_client::Message,
        ) -> Result<(), graphql_ws_client::Error> {
            if let graphql_ws_client::Message::Text(text) = message {
                if self.answer_pings && text.contains(r#""type":"ping""#) {
                    let _ = self.ping_tx.send(());
                }
                self.sent.lock().unwrap().push(text);
            }
            Ok(())
        }
    }

    fn pings(sent: &Mutex<Vec<String>>) -> usize {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|m| m.contains(r#""type":"ping""#))
            .count()
    }

    // Real time: the crate's keepalive timer does not run on tokio's clock.
    #[tokio::test]
    async fn silent_socket_ends_the_subscription() {
        let interval = Duration::from_millis(100);
        let (socket, sent) = FakeSocket::new(false);
        let client = handshake(socket, interval).await.unwrap();
        let mut stream = deployment_logs_stream(&client, "dep");
        drop(client);

        let started = Instant::now();
        let next = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("dead socket was never detected");
        assert!(next.is_none(), "expected the stream to end, got {next:?}");
        // First ping after 1 silent interval, closed after 2 more.
        let elapsed = started.elapsed();
        assert!(elapsed >= interval * 3, "closed too early: {elapsed:?}");
        assert_eq!(pings(&sent), 2);
        let sent = sent.lock().unwrap();
        assert!(sent[0].contains("connection_init"));
        assert!(sent[1].contains("deploymentLogs"));
    }

    #[tokio::test]
    async fn answered_pings_keep_the_subscription_open() {
        let interval = Duration::from_millis(50);
        let (socket, sent) = FakeSocket::new(true);
        let client = handshake(socket, interval).await.unwrap();
        let mut stream = deployment_logs_stream(&client, "dep");
        drop(client);

        let next = tokio::time::timeout(interval * 10, stream.next()).await;
        assert!(next.is_err(), "live socket was closed: {next:?}");
        assert!(pings(&sent) >= 3, "expected regular pings");
    }
}
