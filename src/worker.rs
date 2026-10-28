//! Executes `Command`s against Railway and sends the results back as `AppEvent::Data`.
//! Every `Load*` command gets exactly one reply, even when it fails, panics or is
//! refused by the rate budget, so the app never waits on a load forever.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::{FutureExt, StreamExt};
use graphql_ws_client::Client;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use url::Url;

use crate::api::subscribe::{connect_authed, deployment_status_stream};
use crate::api::{Api, ApiError};
use crate::app::{AppEvent, Command, DataEvent};
use crate::auth::TokenSource;
use crate::budget::{backstop_interval, RateBudget};
use crate::logs::follow::{follow_logs, FollowEvent, LogSource, RailwayLogSource, StreamState};
use crate::pending::{save_pending, Pending};
use crate::remember::save_remembered;

/// Railway's public GraphQL API over HTTP.
pub const HTTP_ENDPOINT: &str = "https://backboard.railway.com/graphql/v2";
/// The same API's `graphql-transport-ws` endpoint, for subscriptions.
pub const WS_ENDPOINT: &str = "wss://backboard.railway.com/graphql/v2";

/// How many deployments of a service's history to load.
const DEPLOYMENT_HISTORY: i64 = 50;
/// Buffer between one log follower and its forwarding loop.
const FOLLOW_BUFFER: usize = 64;

#[allow(clippy::too_many_arguments)]
pub async fn run_worker(
    api: Arc<Api>,
    ws: Url,
    tokens: Arc<dyn TokenSource>,
    remembered_path: PathBuf,
    pending_path: PathBuf,
    configured_interval: Option<u64>,
    mut cmds: mpsc::Receiver<Command>,
    events: mpsc::Sender<AppEvent>,
) {
    let save_pending = spawn_pending_saver(pending_path, events.clone());
    let mut worker = Worker {
        api,
        ws,
        tokens,
        remembered_path,
        save_pending,
        configured_interval,
        events,
        followers: HashMap::new(),
        watch: Watch::default(),
    };
    // The app starts with the default backstop; a configured one applies at once.
    let budget = budget_event(&worker.api.budget(), configured_interval);
    if worker.events.send(AppEvent::Data(budget)).await.is_err() {
        return;
    }
    while let Some(cmd) = cmds.recv().await {
        worker.handle(cmd);
    }
    worker.shutdown();
}

/// Runs `follow_logs` and forwards each `FollowEvent` as `DataEvent::Log`.
/// Both run on the returned task, so aborting it ends the session's events.
/// If the follower panics, the session is marked failed rather than left
/// showing its last state.
pub fn spawn_follower<S: LogSource + 'static>(
    session: u64,
    source: S,
    events: mpsc::Sender<AppEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel(FOLLOW_BUFFER);
        let forward_to = events.clone();
        let forward = async move {
            while let Some(event) = rx.recv().await {
                let data = DataEvent::Log { session, event };
                if forward_to.send(AppEvent::Data(data)).await.is_err() {
                    // Dropping `rx` here makes `follow_logs` stop too.
                    return;
                }
            }
        };
        let both = async { futures::join!(follow_logs(source, tx), forward) };
        if AssertUnwindSafe(both).catch_unwind().await.is_err() {
            let event = FollowEvent::State(StreamState::Failed("internal error".into()));
            let _ = events
                .send(AppEvent::Data(DataEvent::Log { session, event }))
                .await;
        }
    })
}

/// Saves pending snapshots one at a time, in the order sent, skipping any that a
/// newer one has already replaced. A failure comes back as `SaveFailed`, with a
/// message that names the error, never a value.
fn spawn_pending_saver(
    path: PathBuf,
    events: mpsc::Sender<AppEvent>,
) -> mpsc::UnboundedSender<Pending> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Pending>();
    tokio::spawn(async move {
        while let Some(mut latest) = rx.recv().await {
            while let Ok(newer) = rx.try_recv() {
                latest = newer;
            }
            let target = path.clone();
            let saved = tokio::task::spawn_blocking(move || save_pending(&target, &latest)).await;
            let failure = match saved {
                Ok(Ok(())) => continue,
                Ok(Err(e)) => format!("could not save pending changes: {e}"),
                Err(_) => "could not save pending changes".to_string(),
            };
            let event = AppEvent::Data(DataEvent::SaveFailed(failure));
            if events.send(event).await.is_err() {
                return;
            }
        }
    });
    tx
}

struct Worker {
    api: Arc<Api>,
    ws: Url,
    tokens: Arc<dyn TokenSource>,
    remembered_path: PathBuf,
    save_pending: mpsc::UnboundedSender<Pending>,
    configured_interval: Option<u64>,
    events: mpsc::Sender<AppEvent>,
    followers: HashMap<u64, JoinHandle<()>>,
    watch: Watch,
}

/// Live deployment status: one websocket client, shared by successive watch sets.
#[derive(Default)]
struct Watch {
    ids: Vec<String>,
    task: Option<JoinHandle<()>>,
    client: Arc<tokio::sync::Mutex<Option<Client>>>,
}

impl Worker {
    fn handle(&mut self, cmd: Command) {
        let api = self.api.clone();
        match cmd {
            Command::LoadWorkspaces => {
                self.load(async move { api.workspaces().await }, DataEvent::Workspaces)
            }
            Command::LoadEnvironments { project_id } => {
                let id = project_id.clone();
                self.load(async move { api.environments(&id).await }, |result| {
                    DataEvent::Environments { project_id, result }
                })
            }
            Command::LoadServices { project_id, env_id } => {
                let (p, e) = (project_id.clone(), env_id.clone());
                self.load(
                    async move { api.service_instances(&p, &e).await },
                    |result| DataEvent::Services {
                        project_id,
                        env_id,
                        result,
                    },
                )
            }
            Command::LoadDeployments {
                project_id,
                env_id,
                service_id,
            } => {
                let (e, s) = (env_id.clone(), service_id.clone());
                let request = async move {
                    api.deployments(&project_id, &e, &s, DEPLOYMENT_HISTORY)
                        .await
                };
                self.load(request, |result| DataEvent::Deployments {
                    service_id,
                    env_id,
                    result,
                })
            }
            Command::LoadVariables {
                project_id,
                env_id,
                service_id,
            } => {
                let (e, s) = (env_id.clone(), service_id.clone());
                self.load(
                    async move { api.variables(&project_id, &e, &s).await },
                    |result| DataEvent::Variables {
                        env_id,
                        service_id,
                        result,
                    },
                )
            }
            // One task saves in order, so snapshots never race on the temp file.
            Command::SavePending(pending) => {
                let _ = self.save_pending.send(pending);
            }
            Command::WatchDeployments { ids } => self.watch(ids),
            Command::OpenLogs {
                session,
                env_id,
                deployment_id,
                kind,
            } => {
                self.followers.retain(|_, h| !h.is_finished());
                let source = RailwayLogSource::new(
                    api,
                    self.ws.clone(),
                    self.tokens.clone(),
                    env_id,
                    deployment_id,
                    kind,
                );
                let handle = spawn_follower(session, source, self.events.clone());
                if let Some(old) = self.followers.insert(session, handle) {
                    old.abort();
                }
            }
            Command::CloseLogs { session } => {
                if let Some(handle) = self.followers.remove(&session) {
                    handle.abort();
                }
            }
            Command::SaveRemembered(remembered) => {
                let path = self.remembered_path.clone();
                // Best effort: a lost write only means a default selection next start.
                tokio::task::spawn_blocking(move || {
                    let _ = save_remembered(&path, &remembered);
                });
            }
            // The main loop exits on Quit; it never reaches the worker.
            Command::Quit => {}
        }
    }

    fn load<T, F, W>(&self, request: F, reply: W)
    where
        T: Send + 'static,
        F: Future<Output = Result<T, ApiError>> + Send + 'static,
        W: FnOnce(Result<T, String>) -> DataEvent + Send + 'static,
    {
        spawn_load(
            self.api.budget(),
            self.configured_interval,
            self.events.clone(),
            request,
            reply,
        );
    }

    /// Replaces the watch set. An unchanged set whose subscriptions are still
    /// running is left alone, so periodic service reloads do not resubscribe.
    fn watch(&mut self, ids: Vec<String>) {
        let running = self.watch.task.as_ref().is_some_and(|t| !t.is_finished());
        if running && ids == self.watch.ids {
            return;
        }
        if let Some(task) = self.watch.task.take() {
            task.abort();
        }
        self.watch.ids = ids.clone();
        if ids.is_empty() {
            return;
        }
        let task = watch_statuses(
            ids,
            self.watch.client.clone(),
            self.ws.clone(),
            self.tokens.clone(),
            self.events.clone(),
        );
        self.watch.task = Some(spawn_watch(task, self.events.clone()));
    }

    fn shutdown(&mut self) {
        for (_, handle) in self.followers.drain() {
            handle.abort();
        }
        if let Some(task) = self.watch.task.take() {
            task.abort();
        }
    }
}

/// Runs `request` on its own task and sends exactly one reply: its result, an
/// error if it failed or panicked, or a refusal while the rate budget is
/// blocked. A `DataEvent::Budget` follows the reply.
fn spawn_load<T, F, W>(
    budget: Arc<Mutex<RateBudget>>,
    configured_interval: Option<u64>,
    events: mpsc::Sender<AppEvent>,
    request: F,
    reply: W,
) where
    T: Send + 'static,
    F: Future<Output = Result<T, ApiError>> + Send + 'static,
    W: FnOnce(Result<T, String>) -> DataEvent + Send + 'static,
{
    let blocked = blocked_for(&budget, Instant::now());
    tokio::spawn(async move {
        let result = match blocked {
            Some(retry_after) => Err(ApiError::RateLimited { retry_after }.to_string()),
            None => match tokio::spawn(request).await {
                Ok(result) => result.map_err(|e| e.to_string()),
                Err(e) => Err(format!("Request failed: {e}")),
            },
        };
        if events.send(AppEvent::Data(reply(result))).await.is_ok() {
            let budget = budget_event(&budget, configured_interval);
            let _ = events.send(AppEvent::Data(budget)).await;
        }
    });
}

/// Time left on a rate-limit block, rounded up to whole seconds.
fn blocked_for(budget: &Mutex<RateBudget>, now: Instant) -> Option<Duration> {
    let until = lock(budget).blocked_until()?;
    let left = until.checked_duration_since(now).filter(|d| !d.is_zero())?;
    Some(Duration::from_secs(
        left.as_secs() + u64::from(left.subsec_nanos() > 0),
    ))
}

fn budget_event(budget: &Mutex<RateBudget>, configured_interval: Option<u64>) -> DataEvent {
    let budget = lock(budget);
    let (limit, _) = budget.hourly_limit();
    DataEvent::Budget {
        interval: backstop_interval(limit, configured_interval),
        blocked_until: budget.blocked_until(),
    }
}

fn lock(budget: &Mutex<RateBudget>) -> MutexGuard<'_, RateBudget> {
    budget.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs the status watch on its own task. If it panics, live updates are
/// reported off, so the polling backstop is seen to be all there is.
fn spawn_watch<F>(task: F, events: mpsc::Sender<AppEvent>) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        if AssertUnwindSafe(task).catch_unwind().await.is_err() {
            let _ = events
                .send(AppEvent::Data(DataEvent::LiveUpdates { ok: false }))
                .await;
        }
    })
}

/// Subscribes to each deployment's status on the shared client, connecting it
/// first if needed. When the connection fails or dies, live updates go off
/// and the polling backstop carries on; the next watch set reconnects.
async fn watch_statuses(
    ids: Vec<String>,
    slot: Arc<tokio::sync::Mutex<Option<Client>>>,
    ws: Url,
    tokens: Arc<dyn TokenSource>,
    events: mpsc::Sender<AppEvent>,
) {
    let live = |ok| AppEvent::Data(DataEvent::LiveUpdates { ok });
    let client = {
        let mut slot = slot.lock().await;
        match slot.as_ref() {
            Some(client) => client.clone(),
            None => match connect_authed(&ws, &tokens).await {
                Ok(client) => slot.insert(client).clone(),
                Err(_) => {
                    drop(slot);
                    let _ = events.send(live(false)).await;
                    return;
                }
            },
        }
    };
    if events.send(live(true)).await.is_err() {
        return;
    }
    let streams = ids
        .into_iter()
        .map(|id| deployment_status_stream(&client, &id).map(move |item| (id.clone(), item)));
    let mut statuses = futures::stream::select_all(streams);
    while let Some((deployment_id, item)) = statuses.next().await {
        // One bad message does not end the subscription.
        let Ok(status) = item else { continue };
        let data = DataEvent::StatusChanged {
            deployment_id,
            status,
        };
        if events.send(AppEvent::Data(data)).await.is_err() {
            return;
        }
    }
    // Every subscription ended, so the connection is gone.
    slot.lock().await.take();
    let _ = events.send(live(false)).await;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use chrono::{DateTime, Utc};
    use futures::stream::BoxStream;
    use futures::StreamExt;
    use serde_json::json;
    use tokio::sync::mpsc::error::TryRecvError;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::api::ApiError;
    use crate::app::DataEvent;
    use crate::auth::EnvTokenSource;
    use crate::budget::RateBudget;
    use crate::logs::follow::FollowEvent;
    use crate::model::LogLine;
    use crate::remember::{load_remembered, Remembered};

    /// No history; the live stream sends one line every 100ms, forever.
    struct Ticker;

    impl LogSource for Ticker {
        async fn history(&self, _since: Option<DateTime<Utc>>) -> Result<Vec<LogLine>, ApiError> {
            Ok(Vec::new())
        }

        async fn subscribe(
            &self,
        ) -> Result<BoxStream<'static, Result<Vec<LogLine>, ApiError>>, ApiError> {
            let lines = futures::stream::unfold(0i64, |n| async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let line = LogLine {
                    timestamp: DateTime::from_timestamp(1_700_000_000 + n, 0).unwrap(),
                    message: format!("line {n}"),
                    attributes: Vec::new(),
                };
                Some((Ok(vec![line]), n + 1))
            });
            Ok(lines.boxed())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn aborted_follower_sends_nothing_more() {
        let (tx, mut rx) = mpsc::channel(64);
        let handle = spawn_follower(7, Ticker, tx);
        loop {
            match rx.recv().await {
                Some(AppEvent::Data(DataEvent::Log {
                    session: 7,
                    event: FollowEvent::Lines(lines),
                })) if !lines.is_empty() => break,
                Some(AppEvent::Data(DataEvent::Log { session: 7, .. })) => {}
                other => panic!("unexpected event: {other:?}"),
            }
        }
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        // Anything queued before the abort may still be read; nothing may follow it.
        while rx.try_recv().is_ok() {}
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Disconnected)));
    }

    /// A source whose code panics, standing in for a bug in the follower.
    struct Panicky;

    impl LogSource for Panicky {
        async fn history(&self, _since: Option<DateTime<Utc>>) -> Result<Vec<LogLine>, ApiError> {
            panic!("follower bug")
        }

        async fn subscribe(
            &self,
        ) -> Result<BoxStream<'static, Result<Vec<LogLine>, ApiError>>, ApiError> {
            Ok(futures::stream::pending().boxed())
        }
    }

    async fn next_event(rx: &mut mpsc::Receiver<AppEvent>) -> Option<AppEvent> {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("timed out waiting for an event")
    }

    #[tokio::test]
    async fn panicking_follower_reports_failed() {
        let (tx, mut rx) = mpsc::channel(8);
        let handle = spawn_follower(3, Panicky, tx);
        let mut states = Vec::new();
        while let Some(AppEvent::Data(DataEvent::Log { session, event })) =
            next_event(&mut rx).await
        {
            assert_eq!(session, 3);
            if let FollowEvent::State(s) = event {
                states.push(s);
            }
        }
        // Events queued before the panic may be lost with it; the last word is Failed.
        assert_eq!(
            states.last(),
            Some(&StreamState::Failed("internal error".into()))
        );
        assert!(handle.await.is_ok(), "the panic must not escape the task");
    }

    #[tokio::test]
    async fn panicking_watch_turns_live_updates_off() {
        let (tx, mut rx) = mpsc::channel(8);
        let handle = spawn_watch(async { panic!("watch bug") }, tx);
        assert!(matches!(
            next_event(&mut rx).await,
            Some(AppEvent::Data(DataEvent::LiveUpdates { ok: false }))
        ));
        assert!(handle.await.is_ok(), "the panic must not escape the task");
    }

    // ---- run_worker, against a wiremock server (never the live API) ----

    struct Harness {
        server: MockServer,
        budget: Arc<Mutex<RateBudget>>,
        cmds: mpsc::Sender<Command>,
        events: mpsc::Receiver<AppEvent>,
        state_dir: tempfile::TempDir,
    }

    impl Harness {
        async fn start(configured_interval: Option<u64>) -> Self {
            let server = MockServer::start().await;
            let budget = Arc::new(Mutex::new(RateBudget::new(None)));
            let tokens: Arc<dyn TokenSource> = Arc::new(EnvTokenSource("t".into()));
            let http = Url::parse(&format!("{}/graphql/v2", server.uri())).unwrap();
            let ws = Url::parse(&format!(
                "{}/graphql/v2",
                server.uri().replace("http", "ws")
            ))
            .unwrap();
            let api = Arc::new(Api::new(http, tokens.clone(), budget.clone()));
            let state_dir = tempfile::tempdir().unwrap();
            let (cmd_tx, cmd_rx) = mpsc::channel(16);
            let (ev_tx, ev_rx) = mpsc::channel(64);
            tokio::spawn(run_worker(
                api,
                ws,
                tokens,
                state_dir.path().join("state.toml"),
                state_dir.path().join("pending.toml"),
                configured_interval,
                cmd_rx,
                ev_tx,
            ));
            let mut h = Harness {
                server,
                budget,
                cmds: cmd_tx,
                events: ev_rx,
                state_dir,
            };
            // The worker announces its backstop before anything else.
            assert!(matches!(h.next().await, DataEvent::Budget { .. }));
            h
        }

        async fn send(&self, cmd: Command) {
            self.cmds.send(cmd).await.unwrap();
        }

        async fn next(&mut self) -> DataEvent {
            match tokio::time::timeout(Duration::from_secs(5), self.events.recv()).await {
                Ok(Some(AppEvent::Data(d))) => d,
                other => panic!("expected a data event, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn announces_the_configured_backstop_at_start() {
        let server = MockServer::start().await;
        let tokens: Arc<dyn TokenSource> = Arc::new(EnvTokenSource("t".into()));
        let budget = Arc::new(Mutex::new(RateBudget::new(None)));
        let api = Arc::new(Api::new(
            Url::parse(&server.uri()).unwrap(),
            tokens.clone(),
            budget,
        ));
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let (ev_tx, mut ev_rx) = mpsc::channel(4);
        let ws = Url::parse("ws://127.0.0.1:1/").unwrap();
        tokio::spawn(run_worker(
            api,
            ws,
            tokens,
            PathBuf::from("unused"),
            PathBuf::from("unused"),
            Some(42),
            cmd_rx,
            ev_tx,
        ));
        match ev_rx.recv().await {
            Some(AppEvent::Data(DataEvent::Budget {
                interval,
                blocked_until: None,
            })) => assert_eq!(interval, Duration::from_secs(42)),
            other => panic!("expected a budget event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_load_still_replies_then_reports_budget() {
        let mut h = Harness::start(None).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&h.server)
            .await;
        h.send(Command::LoadEnvironments {
            project_id: "p1".into(),
        })
        .await;
        match h.next().await {
            DataEvent::Environments { project_id, result } => {
                assert_eq!(project_id, "p1");
                assert_eq!(result, Err("Railway API returned HTTP 500".into()));
            }
            other => panic!("expected environments, got {other:?}"),
        }
        assert!(matches!(h.next().await, DataEvent::Budget { .. }));
    }

    #[tokio::test]
    async fn load_reply_carries_the_data() {
        let mut h = Harness::start(None).await;
        let body = json!({ "data": { "me": { "workspaces": [{
            "id": "w1", "name": "Mine",
            "projects": { "edges": [{ "node": { "id": "p1", "name": "api" } }] }
        }] }, "externalWorkspaces": [] } });
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&h.server)
            .await;
        h.send(Command::LoadWorkspaces).await;
        match h.next().await {
            DataEvent::Workspaces(Ok(ws)) => {
                assert_eq!(ws.len(), 1);
                assert_eq!(ws[0].projects[0].id, "p1");
            }
            other => panic!("expected workspaces, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn blocked_budget_refuses_the_load_but_still_replies() {
        let mut h = Harness::start(None).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&h.server)
            .await;
        let until = {
            let mut b = h.budget.lock().unwrap();
            b.on_rate_limited(Some(Duration::from_secs(30)), Instant::now());
            b.blocked_until()
        };
        h.send(Command::LoadServices {
            project_id: "p1".into(),
            env_id: "e1".into(),
        })
        .await;
        match h.next().await {
            DataEvent::Services { result, .. } => {
                let err = result.unwrap_err();
                assert!(err.contains("rate limit"), "{err}");
            }
            other => panic!("expected services, got {other:?}"),
        }
        match h.next().await {
            DataEvent::Budget { blocked_until, .. } => assert_eq!(blocked_until, until),
            other => panic!("expected budget, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn panicking_request_still_replies() {
        let (tx, mut rx) = mpsc::channel(4);
        let budget = Arc::new(Mutex::new(RateBudget::new(None)));
        spawn_load(
            budget,
            None,
            tx,
            async { panic!("boom") },
            |result: Result<Vec<crate::model::Workspace>, String>| DataEvent::Workspaces(result),
        );
        match rx.recv().await {
            Some(AppEvent::Data(DataEvent::Workspaces(Err(e)))) => assert!(e.contains("failed")),
            other => panic!("expected an error reply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failed_websocket_turns_live_updates_off() {
        let mut h = Harness::start(None).await;
        // The mock answers the upgrade with 404, so the connection fails.
        h.send(Command::WatchDeployments {
            ids: vec!["d1".into()],
        })
        .await;
        assert!(matches!(
            h.next().await,
            DataEvent::LiveUpdates { ok: false }
        ));
    }

    #[tokio::test]
    async fn save_remembered_writes_the_state_file() {
        let h = Harness::start(None).await;
        let path = h.state_dir.path().join("state.toml");
        let r = Remembered {
            last_project_id: Some("p1".into()),
            last_environment_id: Some("e1".into()),
            theme: None,
        };
        h.send(Command::SaveRemembered(r.clone())).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while load_remembered(&path) != r {
            assert!(Instant::now() < deadline, "state file never written");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn failed_pending_save_reports_without_values() {
        let (tx, mut rx) = mpsc::channel(4);
        let tokens: Arc<dyn TokenSource> = Arc::new(EnvTokenSource("t".into()));
        let budget = Arc::new(Mutex::new(RateBudget::new(None)));
        let api = Arc::new(Api::new(
            Url::parse("http://127.0.0.1:1/").unwrap(),
            tokens.clone(),
            budget,
        ));
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let dir = tempfile::tempdir().unwrap();
        // A file where the directory should be makes the save fail.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        tokio::spawn(run_worker(
            api,
            Url::parse("ws://127.0.0.1:1/").unwrap(),
            tokens,
            PathBuf::from("unused"),
            blocker.join("pending.toml"),
            None,
            cmd_rx,
            tx,
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AppEvent::Data(DataEvent::Budget { .. }))
        ));
        let target = crate::pending::Target {
            project_id: "p",
            env_id: "e",
            env_name: "prod",
            service_id: "s",
            service_name: "svc",
        };
        let mut pending = crate::pending::Pending::default();
        pending.set(&target, "KEY", "s3cret-value".into(), None);
        cmd_tx.send(Command::SavePending(pending)).await.unwrap();
        match rx.recv().await {
            Some(AppEvent::Data(DataEvent::SaveFailed(m))) => {
                assert!(m.starts_with("could not save pending changes"), "{m}");
                assert!(!m.contains("s3cret-value"));
            }
            other => panic!("expected SaveFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn save_pending_keeps_the_last_snapshot() {
        let h = Harness::start(None).await;
        let path = h.state_dir.path().join("pending.toml");
        let target = crate::pending::Target {
            project_id: "p",
            env_id: "e",
            env_name: "prod",
            service_id: "s",
            service_name: "svc",
        };
        let mut pending = Pending::default();
        for n in 0..20 {
            pending.set(&target, "KEY", format!("v{n}"), None);
            h.send(Command::SavePending(pending.clone())).await;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let loaded = crate::pending::load_pending(&path);
            if loaded.as_ref().is_ok_and(|p| *p == pending) {
                break;
            }
            assert!(Instant::now() < deadline, "last snapshot never written");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Let any trailing save finish; the file must still be the last one.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(crate::pending::load_pending(&path).unwrap(), pending);
    }
}
