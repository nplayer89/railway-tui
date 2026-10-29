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
use crate::api::{Api, ApiError, BuildSnapshot};
use crate::auth::TokenSource;
use crate::model::{LogKind, LogLine};

/// How many lines of history to load on each (re)connect (the CLI's value).
const HISTORY_LIMIT: i64 = 500;
/// A stream that stays up this long counts as healthy even if it was quiet.
const HEALTHY_AFTER: Duration = Duration::from_secs(30);
/// Shown where a reconnect backfill may have been cut short.
pub const GAP_MARKER: &str = "[railway-tui] some log lines may be missing here";
/// Attribute that identifies the gap marker line, so filters never hide it.
pub const GAP_ATTR: (&str, &str) = ("railway-tui", "gap");
/// Shown in place of build logs for a deployment that had no build.
pub const NO_BUILD_LOGS: &str = "[railway-tui] no build logs for this deployment (image-based)";

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
    api: Arc<Api>,
    ws: Url,
    tokens: Arc<dyn TokenSource>,
    env_id: String,
    deployment_id: String,
    kind: LogKind,
    /// A deployment's build snapshot never changes, so reconnects reuse it.
    snapshot: tokio::sync::OnceCell<BuildSnapshot>,
}

impl RailwayLogSource {
    pub fn new(
        api: Arc<Api>,
        ws: Url,
        tokens: Arc<dyn TokenSource>,
        env_id: String,
        deployment_id: String,
        kind: LogKind,
    ) -> Self {
        Self {
            api,
            ws,
            tokens,
            env_id,
            deployment_id,
            kind,
            snapshot: tokio::sync::OnceCell::new(),
        }
    }

    /// The build snapshot, looked up until it is found and cached from then on.
    /// `None` (no build yet, or an image-based deployment) is not cached.
    async fn snapshot(&self) -> Result<Option<&BuildSnapshot>, ApiError> {
        if self.snapshot.get().is_none() {
            if let Some(found) = self.api.build_snapshot(&self.deployment_id).await? {
                let _ = self.snapshot.set(found);
            }
        }
        Ok(self.snapshot.get())
    }
}

impl LogSource for RailwayLogSource {
    async fn history(&self, since: Option<DateTime<Utc>>) -> Result<Vec<LogLine>, ApiError> {
        match self.kind {
            LogKind::Runtime => {
                self.api
                    .deployment_logs(&self.deployment_id, since, HISTORY_LIMIT)
                    .await
            }
            LogKind::Build => match self.snapshot().await? {
                Some(snapshot) => {
                    self.api
                        .snapshot_logs(&self.env_id, snapshot, since, HISTORY_LIMIT)
                        .await
                }
                None => Ok(Vec::new()),
            },
        }
    }

    async fn subscribe(
        &self,
    ) -> Result<BoxStream<'static, Result<Vec<LogLine>, ApiError>>, ApiError> {
        match self.kind {
            LogKind::Runtime => {
                let client = connect_authed(self.api.ws_http(), &self.ws, &self.tokens).await?;
                Ok(deployment_logs_stream(&client, &self.deployment_id))
            }
            LogKind::Build => {
                // No snapshot means no build: say so, then there is nothing to stream.
                let Some(snapshot) = self.snapshot().await? else {
                    let note = LogLine {
                        timestamp: Utc::now(),
                        message: NO_BUILD_LOGS.to_string(),
                        attributes: Vec::new(),
                    };
                    return Ok(futures::stream::once(async { Ok(vec![note]) })
                        .chain(futures::stream::pending())
                        .boxed());
                };
                let client = connect_authed(self.api.ws_http(), &self.ws, &self.tokens).await?;
                Ok(build_logs_stream(
                    &client,
                    &self.env_id,
                    &snapshot.id,
                    Utc::now(),
                ))
            }
        }
    }
}

/// How one connection ended.
enum Outcome {
    /// Dropped, ended or failed to start: reconnect, waiting at least
    /// `retry_after` if the API asked for that.
    Dropped { retry_after: Option<Duration> },
    /// The live stream reported an auth error. One reconnect is worth trying:
    /// it fetches the token again, which may have been refreshed meanwhile.
    StreamAuth(String),
    /// Retrying cannot help.
    Fatal(String),
    /// The receiver is gone.
    Closed,
}

/// Runs until `out` is closed, or until an auth error makes retrying pointless.
/// An auth error inside a running stream gets one reconnect; it is final only
/// if it recurs before the stream has proved healthy again.
pub async fn follow_logs<S: LogSource>(source: S, out: mpsc::Sender<FollowEvent>) {
    let mut attempt: u32 = 0;
    let mut last_seen: Option<DateTime<Utc>> = None;
    let mut auth_retry_spent = false;
    loop {
        let outcome = tokio::select! {
            _ = out.closed() => Outcome::Closed,
            outcome = connection(&source, &out, &mut last_seen, &mut attempt) => outcome,
        };
        if attempt == 0 {
            // Healthy since the last auth retry (or never retried).
            auth_retry_spent = false;
        }
        let retry_after = match outcome {
            Outcome::Closed => return,
            Outcome::StreamAuth(_) if !auth_retry_spent => {
                auth_retry_spent = true;
                None
            }
            Outcome::Fatal(msg) | Outcome::StreamAuth(msg) => {
                let _ = out.send(FollowEvent::State(StreamState::Failed(msg))).await;
                return;
            }
            Outcome::Dropped { retry_after } => retry_after,
        };
        attempt = attempt.saturating_add(1);
        let retry_in = backoff(attempt).max(retry_after.unwrap_or_default());
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
/// stream until it drops. `attempt` resets only once the stream proves
/// healthy: a line newer than everything the backfill brought (a fresh
/// subscription replays recent lines, which proves nothing), or
/// `HEALTHY_AFTER` of uptime. So a subscription that replays and then ends
/// at once still backs off.
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
    let gap_start = *last_seen;
    let mut history = match source.history(gap_start).await {
        Ok(lines) => lines,
        Err(e) => return failure(e),
    };
    // Both sources return the newest `HISTORY_LIMIT` lines, so a full page
    // after a drop means the oldest part of the gap was cut off.
    if let Some(gap_start) = gap_start {
        if history.len() >= HISTORY_LIMIT as usize {
            history.insert(
                0,
                LogLine {
                    timestamp: gap_start,
                    message: GAP_MARKER.to_string(),
                    attributes: vec![(GAP_ATTR.0.to_string(), GAP_ATTR.1.to_string())],
                },
            );
        }
    }
    if let Err(outcome) = forward(out, last_seen, history).await {
        return outcome;
    }
    let backfilled_to = *last_seen;
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
    let live_since = tokio::time::Instant::now();
    let outcome = loop {
        match stream.next().await {
            Some(Ok(lines)) => {
                if let Err(outcome) = forward(out, last_seen, lines).await {
                    break outcome;
                }
                if *last_seen > backfilled_to {
                    *attempt = 0;
                }
            }
            Some(Err(ApiError::Auth(e))) => break Outcome::StreamAuth(e.to_string()),
            Some(Err(e)) => break failure(e),
            None => break Outcome::Dropped { retry_after: None },
        }
    };
    if live_since.elapsed() >= HEALTHY_AFTER {
        *attempt = 0;
    }
    outcome
}

fn failure(e: ApiError) -> Outcome {
    match e {
        ApiError::Auth(_) => Outcome::Fatal(e.to_string()),
        ApiError::RateLimited { retry_after } => Outcome::Dropped {
            retry_after: Some(retry_after),
        },
        _ => Outcome::Dropped { retry_after: None },
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
        /// Stays open, silent, for this long and then ends.
        After(Duration),
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
                    Some(Sub::Items(items, End::After(d))) => {
                        let wait = futures::stream::once(tokio::time::sleep(d))
                            .filter_map(|()| async { None });
                        Ok(futures::stream::iter(items).chain(wait).boxed())
                    }
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

    fn reconnecting(attempt: u32, secs: u64) -> StreamState {
        StreamState::Reconnecting {
            attempt,
            retry_in: Duration::from_secs(secs),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn failed_attempts_back_off() {
        let fake = Fake::default()
            .history(Err(ApiError::Network("down".into())))
            .sub(Sub::Fail(ApiError::Http(502)));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 5).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                reconnecting(1, 1),
                StreamState::Connecting,
                reconnecting(2, 2),
                StreamState::Connecting,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stream_ending_at_once_keeps_backing_off() {
        let fake = Fake::default()
            .sub(Sub::Items(Vec::new(), End::Close))
            .sub(Sub::Items(vec![Ok(Vec::new())], End::Close))
            .sub(Sub::Items(Vec::new(), End::Close));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 9).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(1, 1),
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(2, 2),
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(3, 4),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn new_lines_reset_backoff() {
        // The second connection replays the backfilled line, then sends a newer one.
        let fake = Fake::default()
            .sub(Sub::Fail(ApiError::Http(502)))
            .history(Ok(vec![line(1, "a")]))
            .sub(Sub::Items(
                vec![Ok(vec![line(1, "a")]), Ok(vec![line(2, "b")])],
                End::Close,
            ));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 6).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                reconnecting(1, 1),
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(1, 1),
                StreamState::Connecting,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn replayed_lines_do_not_reset_backoff() {
        // Every subscription replays lines already seen, then closes at once.
        let replay = || Sub::Items(vec![Ok(vec![line(1, "a"), line(2, "b")])], End::Close);
        let fake = Fake::default()
            .history(Ok(vec![line(1, "a"), line(2, "b")]))
            .sub(replay())
            .sub(replay())
            .sub(replay());
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 9).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(1, 1),
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(2, 2),
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(3, 4),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_stream_up_for_30s_resets_backoff() {
        let fake = Fake::default()
            .sub(Sub::Fail(ApiError::Http(502)))
            .sub(Sub::Items(Vec::new(), End::After(Duration::from_secs(29))))
            .sub(Sub::Items(Vec::new(), End::After(Duration::from_secs(30))));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 8).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                reconnecting(1, 1),
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(2, 2),
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(1, 1),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_waits_for_retry_after() {
        let fake = Fake::default()
            .history(Err(ApiError::RateLimited {
                retry_after: Duration::from_secs(60),
            }))
            .history(Err(ApiError::RateLimited {
                retry_after: Duration::from_millis(500),
            }));
        let (mut rx, _h) = start(&fake);

        let started = tokio::time::Instant::now();
        let events = collect_states(&mut rx, 5).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                reconnecting(1, 60),
                StreamState::Connecting,
                // backoff(2) is longer than the 0.5s asked for, so it wins.
                reconnecting(2, 2),
                StreamState::Connecting,
            ]
        );
        assert!(started.elapsed() >= Duration::from_secs(62));
    }

    fn full_page(from_secs: i64) -> Vec<LogLine> {
        (0..HISTORY_LIMIT)
            .map(|i| line(from_secs + i, &format!("l{i}")))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn full_backfill_page_marks_a_possible_gap() {
        let fake = Fake::default()
            .history(Ok(Vec::new()))
            .sub(Sub::Items(
                vec![Ok(vec![line(1, "a")]), Err(ApiError::Network("x".into()))],
                End::Hang,
            ))
            .history(Ok(full_page(1000)));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 5).await;
        let backfill = events
            .iter()
            .filter_map(|e| match e {
                FollowEvent::Lines(lines) if lines.len() > 1 => Some(lines),
                _ => None,
            })
            .next()
            .expect("backfill batch");
        assert_eq!(backfill.len(), HISTORY_LIMIT as usize + 1);
        assert_eq!(
            backfill[0],
            LogLine {
                timestamp: line(1, "a").timestamp,
                message: GAP_MARKER.to_string(),
                attributes: vec![(GAP_ATTR.0.to_string(), GAP_ATTR.1.to_string())],
            }
        );
        assert_eq!(backfill[1], line(1000, "l0"));
    }

    #[tokio::test(start_paused = true)]
    async fn full_first_page_and_short_backfill_have_no_marker() {
        let fake = Fake::default()
            .history(Ok(full_page(0)))
            .sub(Sub::Items(
                vec![Err(ApiError::Network("x".into()))],
                End::Hang,
            ))
            .history(Ok(vec![line(600, "b")]));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 5).await;
        let marked = events.iter().any(|e| {
            matches!(e, FollowEvent::Lines(lines) if lines.iter().any(|l| l.message == GAP_MARKER))
        });
        assert!(!marked);
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

    fn stream_auth_error() -> Sub {
        Sub::Items(vec![Err(ApiError::Auth(AuthError::Rejected))], End::Hang)
    }

    #[tokio::test(start_paused = true)]
    async fn auth_error_on_stream_reconnects_once() {
        let fake = Fake::default().sub(stream_auth_error());
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 5).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(1, 1),
                StreamState::Connecting,
                StreamState::Live,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn auth_error_on_stream_after_its_reconnect_fails() {
        let fake = Fake::default()
            .sub(stream_auth_error())
            .sub(stream_auth_error());
        let (mut rx, handle) = start(&fake);

        let events = collect_states(&mut rx, 6).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(1, 1),
                StreamState::Connecting,
                StreamState::Live,
                StreamState::Failed(AuthError::Rejected.to_string()),
            ]
        );
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn auth_error_on_the_reconnect_itself_fails() {
        let fake = Fake::default()
            .sub(stream_auth_error())
            .history(Ok(Vec::new()))
            .history(Err(ApiError::Auth(AuthError::Rejected)));
        let (mut rx, handle) = start(&fake);

        let events = collect_states(&mut rx, 5).await;
        assert_eq!(
            states(&events)[4],
            StreamState::Failed(AuthError::Rejected.to_string())
        );
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_stream_earns_a_new_auth_retry() {
        let fake = Fake::default().sub(stream_auth_error()).sub(Sub::Items(
            vec![
                Ok(vec![line(1, "new")]),
                Err(ApiError::Auth(AuthError::Rejected)),
            ],
            End::Hang,
        ));
        let (mut rx, _h) = start(&fake);

        let events = collect_states(&mut rx, 8).await;
        assert_eq!(
            states(&events),
            vec![
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(1, 1),
                StreamState::Connecting,
                StreamState::Live,
                reconnecting(1, 1),
                StreamState::Connecting,
                StreamState::Live,
            ]
        );
    }

    // ---- RailwayLogSource, against a wiremock server (never the live API) ----

    mod railway {
        use serde_json::json;
        use wiremock::matchers::{body_string_contains, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use super::*;
        use crate::auth::EnvTokenSource;
        use crate::budget::{RateBudget, SharedBudget};

        fn build_source(server: &MockServer) -> RailwayLogSource {
            let tokens: Arc<dyn TokenSource> = Arc::new(EnvTokenSource("t".into()));
            let http = Url::parse(&format!("{}/graphql/v2", server.uri())).unwrap();
            let ws = Url::parse(&server.uri().replace("http", "ws")).unwrap();
            let budget = SharedBudget::new(RateBudget::new(None));
            let api = Arc::new(Api::new(http, tokens.clone(), budget).unwrap());
            RailwayLogSource::new(
                api,
                ws,
                tokens,
                "env1".into(),
                "dep1".into(),
                LogKind::Build,
            )
        }

        async fn snapshot_mock(server: &MockServer, snapshot_id: Option<&str>, times: u64) {
            Mock::given(method("POST"))
                .and(body_string_contains("DeploymentSnapshot"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": {"deployment": {
                        "snapshotId": snapshot_id,
                        "createdAt": "2026-10-28T10:00:00Z"
                    }}
                })))
                .expect(times)
                .named("DeploymentSnapshot")
                .mount(server)
                .await;
        }

        #[tokio::test]
        async fn build_snapshot_is_looked_up_once_across_reconnects() {
            let server = MockServer::start().await;
            snapshot_mock(&server, Some("snap1"), 1).await;
            Mock::given(method("POST"))
                .and(body_string_contains("EnvironmentLogs"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data": {"environmentLogs": []}})),
                )
                .mount(&server)
                .await;
            let source = build_source(&server);

            // First connection, then a reconnect. The websocket upgrade is
            // refused by the mock server, which is fine here.
            source.history(None).await.unwrap();
            let _ = source.subscribe().await;
            source.history(Some(Utc::now())).await.unwrap();
            let _ = source.subscribe().await;
            // `expect(1)` on the snapshot mock is verified when the server drops.
        }

        #[tokio::test]
        async fn image_based_deployment_says_it_has_no_build_logs() {
            let server = MockServer::start().await;
            snapshot_mock(&server, None, 2).await;
            let (tx, mut rx) = mpsc::channel(16);
            let _h = tokio::spawn(follow_logs(build_source(&server), tx));

            let mut events = Vec::new();
            while events.len() < 3 {
                let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("timed out")
                    .expect("follower ended");
                events.push(ev);
            }
            assert_eq!(events[0], FollowEvent::State(StreamState::Connecting));
            assert_eq!(events[1], FollowEvent::State(StreamState::Live));
            match &events[2] {
                FollowEvent::Lines(lines) => {
                    assert_eq!(lines.len(), 1);
                    assert_eq!(lines[0].message, NO_BUILD_LOGS);
                }
                other => panic!("expected the explanatory line, got {other:?}"),
            }
        }
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
