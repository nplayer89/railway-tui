//! Follows a deployment's logs: history, then a live subscription, reconnecting
//! with backoff and backfilling the gap whenever the stream drops.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::mpsc;
use url::Url;

use crate::api::subscribe::{build_logs_stream, connect_authed, deployment_logs_stream};
use crate::api::{Api, ApiError};
use crate::auth::TokenSource;
use crate::model::{LogKind, LogLine};

/// How many lines of history to load on each (re)connect (the CLI's value).
const HISTORY_LIMIT: i64 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamState {
    Connecting,
    Live,
    Reconnecting { attempt: u32, retry_in: Duration },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowEvent {
    Lines(Vec<LogLine>),
    State(StreamState),
}

/// Delay before reconnect attempt `attempt` (1-based): 1, 2, 4, 8, 16s, then 30s.
pub fn backoff(attempt: u32) -> Duration {
    match attempt {
        0..=5 => Duration::from_secs(1 << attempt.saturating_sub(1)),
        _ => Duration::from_secs(30),
    }
}

pub trait LogSource: Send + Sync {
    fn history(
        &self,
        since: Option<DateTime<Utc>>,
    ) -> impl Future<Output = Result<Vec<LogLine>, ApiError>> + Send;

    fn subscribe(
        &self,
    ) -> impl Future<Output = Result<BoxStream<'static, Result<Vec<LogLine>, ApiError>>, ApiError>> + Send;
}

pub struct RailwayLogSource {
    pub api: Arc<Api>,
    pub ws: Url,
    pub tokens: Arc<dyn TokenSource>,
    pub env_id: String,
    pub deployment_id: String,
    pub kind: LogKind,
}

impl LogSource for RailwayLogSource {
    async fn history(&self, since: Option<DateTime<Utc>>) -> Result<Vec<LogLine>, ApiError> {
        match self.kind {
            LogKind::Runtime => {
                self.api
                    .deployment_logs(&self.deployment_id, since, HISTORY_LIMIT)
                    .await
            }
            LogKind::Build => {
                self.api
                    .build_logs(&self.env_id, &self.deployment_id, since, HISTORY_LIMIT)
                    .await
            }
        }
    }

    async fn subscribe(
        &self,
    ) -> Result<BoxStream<'static, Result<Vec<LogLine>, ApiError>>, ApiError> {
        match self.kind {
            LogKind::Runtime => {
                let client = connect_authed(&self.ws, &self.tokens).await?;
                Ok(deployment_logs_stream(&client, &self.deployment_id))
            }
            LogKind::Build => {
                // No snapshot means no build, so there is nothing to stream.
                let Some(snapshot_id) = self.api.snapshot_id(&self.deployment_id).await? else {
                    return Ok(futures::stream::pending().boxed());
                };
                let client = connect_authed(&self.ws, &self.tokens).await?;
                Ok(build_logs_stream(
                    &client,
                    &self.env_id,
                    &snapshot_id,
                    Utc::now(),
                ))
            }
        }
    }
}

/// How one connection ended.
enum Outcome {
    /// Dropped, ended or failed to start: reconnect.
    Dropped,
    /// Retrying cannot help.
    Fatal(String),
    /// The receiver is gone.
    Closed,
}

/// Runs until `out` is closed, or until an auth error makes retrying pointless.
pub async fn follow_logs<S: LogSource>(source: S, out: mpsc::Sender<FollowEvent>) {
    let mut attempt: u32 = 0;
    let mut last_seen: Option<DateTime<Utc>> = None;
    loop {
        let outcome = tokio::select! {
            _ = out.closed() => Outcome::Closed,
            outcome = connection(&source, &out, &mut last_seen, &mut attempt) => outcome,
        };
        match outcome {
            Outcome::Closed => return,
            Outcome::Fatal(msg) => {
                let _ = out.send(FollowEvent::State(StreamState::Failed(msg))).await;
                return;
            }
            Outcome::Dropped => {}
        }
        attempt = attempt.saturating_add(1);
        let retry_in = backoff(attempt);
        let state = StreamState::Reconnecting { attempt, retry_in };
        if out.send(FollowEvent::State(state)).await.is_err() {
            return;
        }
        tokio::select! {
            _ = out.closed() => return,
            _ = tokio::time::sleep(retry_in) => {}
        }
    }
}

/// One connection: backfill history since `last_seen`, then forward the live
/// stream until it drops. A successful subscribe resets `attempt`.
async fn connection<S: LogSource>(
    source: &S,
    out: &mpsc::Sender<FollowEvent>,
    last_seen: &mut Option<DateTime<Utc>>,
    attempt: &mut u32,
) -> Outcome {
    if out
        .send(FollowEvent::State(StreamState::Connecting))
        .await
        .is_err()
    {
        return Outcome::Closed;
    }
    let history = match source.history(*last_seen).await {
        Ok(lines) => lines,
        Err(e) => return failure(e),
    };
    if let Err(outcome) = forward(out, last_seen, history).await {
        return outcome;
    }
    let mut stream = match source.subscribe().await {
        Ok(stream) => stream,
        Err(e) => return failure(e),
    };
    if out
        .send(FollowEvent::State(StreamState::Live))
        .await
        .is_err()
    {
        return Outcome::Closed;
    }
    *attempt = 0;
    while let Some(batch) = stream.next().await {
        match batch {
            Ok(lines) => {
                if let Err(outcome) = forward(out, last_seen, lines).await {
                    return outcome;
                }
            }
            Err(e) => return failure(e),
        }
    }
    Outcome::Dropped
}

fn failure(e: ApiError) -> Outcome {
    match e {
        ApiError::Auth(_) => Outcome::Fatal(e.to_string()),
        _ => Outcome::Dropped,
    }
}

/// Sends a non-empty batch and advances `last_seen` to its newest line.
async fn forward(
    out: &mpsc::Sender<FollowEvent>,
    last_seen: &mut Option<DateTime<Utc>>,
    lines: Vec<LogLine>,
) -> Result<(), Outcome> {
    let Some(newest) = lines.iter().map(|l| l.timestamp).max() else {
        return Ok(());
    };
    *last_seen = (*last_seen).max(Some(newest));
    out.send(FollowEvent::Lines(lines))
        .await
        .map_err(|_| Outcome::Closed)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use chrono::TimeZone;

    use super::*;
    use crate::auth::AuthError;

    fn line(secs: i64, msg: &str) -> LogLine {
        LogLine {
            timestamp: Utc.timestamp_opt(1_700_000_000 + secs, 0).unwrap(),
            message: msg.to_string(),
            attributes: Vec::new(),
        }
    }

    /// One scripted subscription: an error, or a stream of batches that then
    /// either ends or stays open forever.
    enum Sub {
        Fail(ApiError),
        Items(Vec<Result<Vec<LogLine>, ApiError>>, End),
    }

    enum End {
        Close,
        Hang,
    }

    /// Plays back scripted results; once a script runs out, history is empty
    /// and the subscription stays open without sending anything.
    #[derive(Default)]
    struct Script {
        histories: Mutex<VecDeque<Result<Vec<LogLine>, ApiError>>>,
        subs: Mutex<VecDeque<Sub>>,
        since_calls: Mutex<Vec<Option<DateTime<Utc>>>>,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Script>);

    impl Fake {
        fn history(self, result: Result<Vec<LogLine>, ApiError>) -> Self {
            self.0.histories.lock().unwrap().push_back(result);
            self
        }
        fn sub(self, sub: Sub) -> Self {
            self.0.subs.lock().unwrap().push_back(sub);
            self
        }
        fn since_calls(&self) -> Vec<Option<DateTime<Utc>>> {
            self.0.since_calls.lock().unwrap().clone()
        }
    }

    impl LogSource for Fake {
        fn history(
            &self,
            since: Option<DateTime<Utc>>,
        ) -> impl Future<Output = Result<Vec<LogLine>, ApiError>> + Send {
            self.0.since_calls.lock().unwrap().push(since);
            let result = self
                .0
                .histories
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(Vec::new()));
            async move { result }
        }

        fn subscribe(
            &self,
        ) -> impl Future<Output = Result<BoxStream<'static, Result<Vec<LogLine>, ApiError>>, ApiError>>
               + Send {
            let sub = self.0.subs.lock().unwrap().pop_front();
            async move {
                match sub {
                    None => Ok(futures::stream::pending().boxed()),
                    Some(Sub::Fail(e)) => Err(e),
                    Some(Sub::Items(items, End::Close)) => Ok(futures::stream::iter(items).boxed()),
                    Some(Sub::Items(items, End::Hang)) => Ok(futures::stream::iter(items)
                        .chain(futures::stream::pending())
                        .boxed()),
                }
            }
        }
    }

    /// Compile-time check: the real source's follower can be spawned too.
    #[allow(dead_code)]
    fn railway_follower_is_spawnable(source: RailwayLogSource, out: mpsc::Sender<FollowEvent>) {
        tokio::spawn(follow_logs(source, out));
    }

    /// Spawns the follower (which also proves its future is `Send`).
    fn start(fake: &Fake) -> (mpsc::Receiver<FollowEvent>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(64);
        let handle = tokio::spawn(follow_logs(fake.clone(), tx));
        (rx, handle)
    }

    /// Receives events until `n` state changes have arrived.
    async fn collect_states(rx: &mut mpsc::Receiver<FollowEvent>, n: usize) -> Vec<FollowEvent> {
        let mut events = Vec::new();
        let mut states = 0;
        while states < n {
            let ev = rx.recv().await.expect("follower ended early");
            if matches!(ev, FollowEvent::State(_)) {
                states += 1;
            }
            events.push(ev);
        }
        events
    }

    fn states(events: &[FollowEvent]) -> Vec<StreamState> {
        events
            .iter()
            .filter_map(|e| match e {
                FollowEvent::State(s) => Some(s.clone()),
                FollowEvent::Lines(_) => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn connect_then_live_forwards_lines() {
        let fake = Fake::default()
            .history(Ok(vec![line(1, "old")]))
            .sub(Sub::Items(vec![Ok(vec![line(2, "new")])], End::Hang));
        let (mut rx, _h) = start(&fake);

        let mut events = collect_states(&mut rx, 2).await;
        events.push(rx.recv().await.unwrap());
        assert_eq!(
            events,
            vec![
                FollowEvent::State(StreamState::Connecting),
                FollowEvent::Lines(vec![line(1, "old")]),
                FollowEvent::State(StreamState::Live),
                FollowEvent::Lines(vec![line(2, "new")]),
            ]
        );
        assert_eq!(fake.since_calls(), vec![None]);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_error_reconnects_and_backfills_from_last_timestamp() {
        let fake = Fake::default()
            .history(Ok(vec![line(1, "a")]))
            .sub(Sub::Items(
                vec![
                    Ok(vec![line(2, "b"), line(3, "c")]),
                    Err(ApiError::Network("socket dead".into())),
                ],
                End::Hang,
            ))
            .history(Ok(vec![line(3, "c"), line(4, "d")]));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 5).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                StreamState::Live,
                StreamState::Reconnecting {
                    attempt: 1,
                    retry_in: Duration::from_secs(1)
                },
                StreamState::Connecting,
                StreamState::Live,
            ]
        );
        assert_eq!(fake.since_calls(), vec![None, Some(line(3, "c").timestamp)]);
        assert!(events.contains(&FollowEvent::Lines(vec![line(3, "c"), line(4, "d")])));
    }

    #[tokio::test(start_paused = true)]
    async fn stream_end_counts_as_drop() {
        let fake = Fake::default().sub(Sub::Items(vec![Ok(vec![line(1, "a")])], End::Close));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 4).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                StreamState::Live,
                StreamState::Reconnecting {
                    attempt: 1,
                    retry_in: Duration::from_secs(1)
                },
                StreamState::Connecting,
            ]
        );
        assert_eq!(fake.since_calls()[1], Some(line(1, "a").timestamp));
    }

    #[tokio::test(start_paused = true)]
    async fn failed_attempts_back_off_until_live_resets() {
        let fake = Fake::default()
            .history(Err(ApiError::Network("down".into())))
            .sub(Sub::Fail(ApiError::Http(502)))
            .sub(Sub::Items(Vec::new(), End::Close));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 9).await;
        let reconnecting = |attempt, secs| StreamState::Reconnecting {
            attempt,
            retry_in: Duration::from_secs(secs),
        };
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                reconnecting(1, 1),
                StreamState::Connecting,
                reconnecting(2, 2),
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(1, 1),
                StreamState::Connecting,
                StreamState::Live,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn auth_error_fails_without_retry() {
        let fake = Fake::default().history(Err(ApiError::Auth(AuthError::Rejected)));
        let (mut rx, handle) = start(&fake);

        let events = collect_states(&mut rx, 2).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                StreamState::Failed(AuthError::Rejected.to_string()),
            ]
        );
        handle.await.unwrap();
        assert!(rx.recv().await.is_none());
        assert_eq!(fake.since_calls().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn auth_error_on_stream_fails_without_retry() {
        let fake = Fake::default().sub(Sub::Items(
            vec![Err(ApiError::Auth(AuthError::Rejected))],
            End::Hang,
        ));
        let (mut rx, handle) = start(&fake);

        let events = collect_states(&mut rx, 3).await;
        assert!(matches!(states(&events)[2], StreamState::Failed(_)));
        handle.await.unwrap();
    }

    #[test]
    fn backoff_sequence() {
        let secs: Vec<u64> = (1..=7).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(secs, vec![1, 2, 4, 8, 16, 30, 30]);
        assert_eq!(backoff(u32::MAX), Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn stops_when_receiver_dropped() {
        // Live with a silent stream: the follower must notice the closed channel.
        let fake = Fake::default();
        let (mut rx, handle) = start(&fake);
        collect_states(&mut rx, 2).await;
        drop(rx);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("follower did not stop")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn stops_when_receiver_dropped_during_backoff() {
        let fake = Fake::default().history(Err(ApiError::Network("down".into())));
        let (mut rx, handle) = start(&fake);
        collect_states(&mut rx, 2).await;
        drop(rx);
        tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("follower slept through the closed channel")
            .unwrap();
    }
}
