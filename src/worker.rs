//! Executes `Command`s against Railway and sends the results back as `AppEvent::Data`.
//! Every `Load*` command gets exactly one reply, even when it fails, panics or is
//! refused by the rate budget, so the app never waits on a load forever.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures::{FutureExt, StreamExt};
use graphql_ws_client::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use url::Url;

use crate::api::subscribe::{connect_authed, deployment_status_stream};
use crate::api::write::WriteError;
use crate::api::{Api, ApiError};
use crate::app::{
    AppEvent, BranchReply, BranchSwitch, Command, DataEvent, DeployStep, LatestOutcome, Phase,
    Secret,
};
use crate::auth::TokenSource;
use crate::budget::{backstop_interval, SharedBudget};
use crate::github::GhError;
use crate::logs::follow::{follow_logs, FollowEvent, LogSource, RailwayLogSource, StreamState};
use crate::pending::{save_pending, Pending};
use crate::remember::{save_remembered, Remembered};

/// Railway's public GraphQL API over HTTP.
pub const HTTP_ENDPOINT: &str = "https://backboard.railway.com/graphql/v2";
/// The same API's `graphql-transport-ws` endpoint, for subscriptions.
pub const WS_ENDPOINT: &str = "wss://backboard.railway.com/graphql/v2";

/// How many deployments of a service's history to load.
const DEPLOYMENT_HISTORY: i64 = 50;
/// Buffer between one log follower and its forwarding loop.
const FOLLOW_BUFFER: usize = 64;

/// What the worker talks to and where it keeps its state.
pub struct WorkerConfig {
    pub api: Arc<Api>,
    /// The `graphql-transport-ws` endpoint, for subscriptions.
    pub ws: Url,
    pub tokens: Arc<dyn TokenSource>,
    /// Where the remembered selection is saved.
    pub remembered_path: PathBuf,
    /// Where pending changes are saved.
    pub pending_path: PathBuf,
    /// The configured polling backstop, in seconds.
    pub configured_interval: Option<u64>,
    /// The `gh` program used to list a repo's branches.
    pub gh: PathBuf,
}

pub async fn run_worker(
    config: WorkerConfig,
    mut cmds: mpsc::Receiver<Command>,
    events: mpsc::Sender<AppEvent>,
) {
    let WorkerConfig {
        api,
        ws,
        tokens,
        remembered_path,
        pending_path,
        configured_interval,
        gh,
    } = config;
    let pending_failed = (events.clone(), "could not save pending changes");
    let save_pending = spawn_saver(
        pending_path,
        |path, pending: &Arc<Pending>| save_pending(path, pending),
        Some(pending_failed),
    );
    // Best effort: a lost write only means a default selection next start.
    let save_remembered = spawn_saver(remembered_path, save_remembered, None);
    let mut worker = Worker {
        api,
        ws,
        tokens,
        save_remembered,
        save_pending,
        configured_interval,
        gh,
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

/// Saves each value sent on the returned channel to `path`, one at a time and in
/// order, skipping any that a newer one replaced before its turn: two saves at
/// once would race on the same temp file. Once the sender is dropped, a value
/// not yet saved is still saved, then the loop ends.
///
/// The loop runs on the blocking pool, not as an async task, so the runtime's
/// shutdown grace covers it: at exit the worker (and with it the sender) is
/// dropped, and the last value is written before the runtime finishes.
///
/// With `report`, a failure comes back as `SaveFailed`, its message starting
/// with the given text and naming the error, never a value; without it saving
/// is best effort.
fn spawn_saver<T>(
    path: PathBuf,
    save: fn(&Path, &T) -> std::io::Result<()>,
    report: Option<(mpsc::Sender<AppEvent>, &'static str)>,
) -> watch::Sender<Option<T>>
where
    T: Clone + Send + Sync + 'static,
{
    let (tx, mut rx) = watch::channel(None);
    tokio::task::spawn_blocking(move || {
        // `changed` needs no runtime driver, so this keeps working during shutdown.
        while futures::executor::block_on(rx.changed()).is_ok() {
            let Some(latest) = rx.borrow_and_update().clone() else {
                continue;
            };
            let saved = std::panic::catch_unwind(AssertUnwindSafe(|| save(&path, &latest)));
            let Some((events, what)) = &report else {
                continue;
            };
            let failure = match saved {
                Ok(Ok(())) => continue,
                Ok(Err(e)) => format!("{what}: {e}"),
                Err(_) => what.to_string(),
            };
            let event = AppEvent::Data(DataEvent::SaveFailed(failure));
            if events.blocking_send(event).is_err() {
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
    save_remembered: watch::Sender<Option<Remembered>>,
    save_pending: watch::Sender<Option<Arc<Pending>>>,
    configured_interval: Option<u64>,
    /// The `gh` program used to list a repo's branches.
    gh: PathBuf,
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
            Command::LoadBranches {
                project_id,
                env_id,
                service_id,
                repo,
            } => {
                let (e, s) = (env_id.clone(), service_id.clone());
                let gh = self.gh.clone();
                let request = async move {
                    let (triggers, names) = tokio::join!(
                        api.triggers(&project_id, &e, &s),
                        list_branches(&api, &gh, &repo)
                    );
                    Ok(BranchReply {
                        triggers: triggers?,
                        names: names.map_err(|e| e.to_string()),
                    })
                };
                self.load(request, |result| DataEvent::Branches {
                    env_id,
                    service_id,
                    result,
                })
            }
            Command::LoadVariables {
                project_id,
                env_id,
                service_id,
                purpose,
            } => {
                let (e, s) = (env_id.clone(), service_id.clone());
                let view = purpose == crate::app::VarsPurpose::View;
                self.load(
                    async move {
                        if !view {
                            return api.variables(&project_id, &e, &s).await.map(|v| (v, None));
                        }
                        let (vars, sealed) = tokio::join!(
                            api.variables(&project_id, &e, &s),
                            api.sealed_variables(&e, &s)
                        );
                        // A failed sealed lookup must not hide the variables.
                        vars.map(|v| (v, Some(sealed.map_err(|e| e.to_string()))))
                    },
                    move |result| match result {
                        Ok((vars, sealed)) => DataEvent::Variables {
                            env_id,
                            service_id,
                            purpose,
                            result: Ok(Secret::new(vars)),
                            sealed,
                        },
                        Err(e) => DataEvent::Variables {
                            env_id,
                            service_id,
                            purpose,
                            result: Err(e),
                            sealed: None,
                        },
                    },
                )
            }
            Command::DeployService {
                project_id,
                env_id,
                service_id,
                upserts,
                deletes,
                redeploy_only,
            } => {
                let target = DeployTarget {
                    project_id,
                    env_id,
                    service_id,
                };
                let (budget, interval) = (api.budget(), self.configured_interval);
                let events = self.events.clone();
                tokio::spawn(async move {
                    let writes = (!redeploy_only).then_some((upserts.into_inner(), deletes));
                    if run_deploy(&api, &target, writes, &events).await.is_ok() {
                        let budget = budget_event(&budget, interval);
                        let _ = events.send(AppEvent::Data(budget)).await;
                    }
                });
            }
            Command::DeployLatest {
                env_id,
                service_id,
                switch,
            } => {
                let (budget, interval) = (api.budget(), self.configured_interval);
                let events = self.events.clone();
                tokio::spawn(async move {
                    let outcome = deploy_latest(&api, &env_id, &service_id, switch).await;
                    let data = DataEvent::DeployedLatest {
                        env_id,
                        service_id,
                        outcome,
                    };
                    if events.send(AppEvent::Data(data)).await.is_ok() {
                        let budget = budget_event(&budget, interval);
                        let _ = events.send(AppEvent::Data(budget)).await;
                    }
                });
            }
            // One task saves in order, so snapshots never race on the temp file.
            Command::SavePending(pending) => {
                self.save_pending.send_replace(Some(pending));
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
            // Like pending snapshots: one task, so selections never race on the temp file.
            Command::SaveRemembered(remembered) => {
                self.save_remembered.send_replace(Some(remembered));
            }
            // The main loop exits on Quit; it never reaches the worker.
            // Both are handled by the main loop before commands reach the worker.
            Command::EditExternal { .. } | Command::Copy { .. } | Command::Quit => {}
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
            self.api.ws_http().clone(),
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
    budget: SharedBudget,
    configured_interval: Option<u64>,
    events: mpsc::Sender<AppEvent>,
    request: F,
    reply: W,
) where
    T: Send + 'static,
    F: Future<Output = Result<T, ApiError>> + Send + 'static,
    W: FnOnce(Result<T, String>) -> DataEvent + Send + 'static,
{
    let blocked = budget.blocked_for(Instant::now());
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

struct DeployTarget {
    project_id: String,
    env_id: String,
    service_id: String,
}

/// The app has gone, so nobody hears about the rest.
struct Closed;

/// Runs one deploy in order — the upsert (skipped when empty), each delete,
/// then the redeploy — sending a `Deploy` event after each step and stopping
/// at the first failure, so the redeploy never follows a failed write. Nothing
/// is resent.
async fn run_deploy(
    api: &Api,
    t: &DeployTarget,
    writes: Option<(crate::model::Variables, Vec<String>)>,
    events: &mpsc::Sender<AppEvent>,
) -> Result<(), Closed> {
    // Railway's error text may quote what was sent; it is shown unrevealed.
    let sent = writes.as_ref().map(|(u, _)| u.clone()).unwrap_or_default();
    let failed = |during: Phase, e: WriteError| {
        let (error, failure) = e.into_parts();
        DeployStep::Failed {
            during,
            error: mask_sent(error.to_string(), &sent),
            failure,
        }
    };
    let (project, env, service) = (&t.project_id, &t.env_id, &t.service_id);
    if let Some((upserts, deletes)) = writes {
        if !upserts.is_empty() {
            let step = match api.upsert_variables(project, env, service, &upserts).await {
                Ok(()) => DeployStep::Upserted,
                Err(e) => failed(Phase::Upsert, e),
            };
            if !emit(events, t, step).await? {
                return Ok(());
            }
        }
        for name in deletes {
            let step = match api.delete_variable(project, env, service, &name).await {
                Ok(()) => DeployStep::Deleted(name),
                Err(e) => failed(Phase::Delete(name), e),
            };
            if !emit(events, t, step).await? {
                return Ok(());
            }
        }
    }
    let step = match api.redeploy_service(env, service).await {
        Ok(()) => DeployStep::Redeployed,
        Err(e) => failed(Phase::Redeploy, e),
    };
    emit(events, t, step).await?;
    Ok(())
}

/// Sends one deploy step; false when it was a failure, which ends the deploy.
async fn emit(
    events: &mpsc::Sender<AppEvent>,
    t: &DeployTarget,
    step: DeployStep,
) -> Result<bool, Closed> {
    let carry_on = !matches!(step, DeployStep::Failed { .. });
    let data = DataEvent::Deploy {
        env_id: t.env_id.clone(),
        service_id: t.service_id.clone(),
        step,
    };
    events
        .send(AppEvent::Data(data))
        .await
        .map_err(|_| Closed)?;
    Ok(carry_on)
}

/// Why neither `gh` nor Railway could list a repo's branches.
#[derive(Debug, thiserror::Error)]
#[error("{gh}; Railway: {railway}")]
struct BranchesError {
    gh: GhError,
    railway: ApiError,
}

/// A repo's branches from `gh` (the user's GitHub login), else from Railway,
/// which refuses `railway login` tokens; the error says why both failed.
async fn list_branches(api: &Api, gh: &Path, repo: &str) -> Result<Vec<String>, BranchesError> {
    let gh_error = match crate::github::branches(gh, repo).await {
        Ok(names) => return Ok(names),
        Err(e) => e,
    };
    api.branches(repo).await.map_err(|railway| BranchesError {
        gh: gh_error,
        railway,
    })
}

/// Switches the service's branch when asked, then deploys the latest commit;
/// a switch that did not go through stops before the deploy. Nothing is resent.
async fn deploy_latest(
    api: &Api,
    env_id: &str,
    service_id: &str,
    switch: Option<BranchSwitch>,
) -> LatestOutcome {
    if let Some(sw) = switch {
        if let Err(e) = api.switch_branch(&sw.trigger_id, &sw.to).await {
            let (error, failure) = e.into_parts();
            return LatestOutcome::SwitchFailed {
                error: error.to_string(),
                failure,
            };
        }
    }
    match api.deploy_latest_commit(env_id, service_id).await {
        Ok(()) => LatestOutcome::Started,
        Err(e) => {
            let (error, failure) = e.into_parts();
            LatestOutcome::Failed {
                error: error.to_string(),
                failure,
            }
        }
    }
}

/// Replaces every non-empty sent value in `error` with the hidden marker, the
/// longest first so a value containing another is masked whole.
fn mask_sent(mut error: String, sent: &crate::model::Variables) -> String {
    let mut values: Vec<&str> = sent
        .values()
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for value in values {
        error = error.replace(value, "••••••");
    }
    error
}

fn budget_event(budget: &SharedBudget, configured_interval: Option<u64>) -> DataEvent {
    let budget = budget.lock();
    let (limit, _) = budget.hourly_limit();
    DataEvent::Budget {
        interval: backstop_interval(limit, configured_interval),
        blocked_until: budget.blocked_until(),
    }
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
    http: reqwest::Client,
    ws: Url,
    tokens: Arc<dyn TokenSource>,
    events: mpsc::Sender<AppEvent>,
) {
    let live = |ok| AppEvent::Data(DataEvent::LiveUpdates { ok });
    let client = {
        let mut slot = slot.lock().await;
        match slot.as_ref() {
            Some(client) => client.clone(),
            None => match connect_authed(&http, &ws, &tokens).await {
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
    use crate::model::{LogLine, WriteFailure};
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
        budget: SharedBudget,
        cmds: mpsc::Sender<Command>,
        events: mpsc::Receiver<AppEvent>,
        state_dir: tempfile::TempDir,
    }

    impl Harness {
        async fn start(configured_interval: Option<u64>) -> Self {
            Self::start_with_gh(configured_interval, PathBuf::from("/nonexistent/gh")).await
        }

        async fn start_with_gh(configured_interval: Option<u64>, gh: PathBuf) -> Self {
            let server = MockServer::start().await;
            let budget = SharedBudget::new(RateBudget::new(None));
            let tokens: Arc<dyn TokenSource> = Arc::new(EnvTokenSource("t".into()));
            let http = Url::parse(&format!("{}/graphql/v2", server.uri())).unwrap();
            let ws = Url::parse(&format!(
                "{}/graphql/v2",
                server.uri().replace("http", "ws")
            ))
            .unwrap();
            let api = Arc::new(Api::new(http, tokens.clone(), budget.clone()).unwrap());
            let state_dir = tempfile::tempdir().unwrap();
            let (cmd_tx, cmd_rx) = mpsc::channel(16);
            let (ev_tx, ev_rx) = mpsc::channel(64);
            tokio::spawn(run_worker(
                WorkerConfig {
                    api,
                    ws,
                    tokens,
                    remembered_path: state_dir.path().join("state.toml"),
                    pending_path: state_dir.path().join("pending.toml"),
                    configured_interval,
                    gh,
                },
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
        let budget = SharedBudget::new(RateBudget::new(None));
        let api =
            Arc::new(Api::new(Url::parse(&server.uri()).unwrap(), tokens.clone(), budget).unwrap());
        let (_cmd_tx, cmd_rx) = mpsc::channel(1);
        let (ev_tx, mut ev_rx) = mpsc::channel(4);
        let ws = Url::parse("ws://127.0.0.1:1/").unwrap();
        tokio::spawn(run_worker(
            WorkerConfig {
                api,
                ws,
                tokens,
                remembered_path: PathBuf::from("unused"),
                pending_path: PathBuf::from("unused"),
                configured_interval: Some(42),
                gh: PathBuf::from("unused"),
            },
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
            let mut b = h.budget.lock();
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
        let budget = SharedBudget::new(RateBudget::new(None));
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
    async fn save_remembered_keeps_the_last_selection() {
        let h = Harness::start(None).await;
        let path = h.state_dir.path().join("state.toml");
        let remembered = |n: usize| Remembered {
            last_project_id: Some(format!("p{n}")),
            last_environment_id: Some(format!("e{n}")),
            theme: None,
        };
        for n in 0..100 {
            h.send(Command::SaveRemembered(remembered(n))).await;
        }
        let last = remembered(99);
        let deadline = Instant::now() + Duration::from_secs(5);
        while load_remembered(&path) != last {
            assert!(Instant::now() < deadline, "last selection never written");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Let any trailing save finish; the file must still be the last one.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(load_remembered(&path), last);
    }

    #[tokio::test]
    async fn saver_still_saves_the_last_value_after_its_sender_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let save = spawn_saver(path.clone(), save_remembered, None);
        let r = Remembered {
            last_project_id: Some("p1".into()),
            ..Remembered::default()
        };
        save.send_replace(Some(r.clone()));
        drop(save);
        let deadline = Instant::now() + Duration::from_secs(5);
        while load_remembered(&path) != r {
            assert!(Instant::now() < deadline, "last value never saved");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Quit drops the command sender and waits for the worker: a save still
    /// queued at that moment must be handled, and the worker must then return.
    #[tokio::test]
    async fn worker_handles_queued_commands_after_its_sender_is_dropped() {
        let state_dir = tempfile::tempdir().unwrap();
        let path = state_dir.path().join("state.toml");
        let tokens: Arc<dyn TokenSource> = Arc::new(EnvTokenSource("t".into()));
        let http = Url::parse("http://127.0.0.1:9/graphql/v2").unwrap();
        let budget = SharedBudget::new(RateBudget::new(None));
        let api = Arc::new(Api::new(http, tokens.clone(), budget).unwrap());
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let (ev_tx, _ev_rx) = mpsc::channel(64);
        let r = Remembered {
            last_project_id: Some("p1".into()),
            ..Remembered::default()
        };
        cmd_tx
            .send(Command::SaveRemembered(r.clone()))
            .await
            .unwrap();
        drop(cmd_tx);
        let config = WorkerConfig {
            api,
            ws: Url::parse("ws://127.0.0.1:9/graphql/v2").unwrap(),
            tokens,
            remembered_path: path.clone(),
            pending_path: state_dir.path().join("pending.toml"),
            configured_interval: None,
            gh: PathBuf::from("/nonexistent/gh"),
        };
        tokio::time::timeout(Duration::from_secs(5), run_worker(config, cmd_rx, ev_tx))
            .await
            .expect("worker returns once its queue is drained");
        let deadline = Instant::now() + Duration::from_secs(5);
        while load_remembered(&path) != r {
            assert!(Instant::now() < deadline, "queued save never written");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Like `save_remembered`, but slow enough to still be writing when a
    /// newer value arrives.
    fn slow_save(path: &Path, r: &Remembered) -> std::io::Result<()> {
        std::thread::sleep(Duration::from_millis(100));
        save_remembered(path, r)
    }

    /// As at exit in `main`: the sender goes, then the runtime shuts down with
    /// a grace period. The last value must land, without using up the grace.
    #[test]
    fn runtime_shutdown_waits_for_the_last_save_but_not_longer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let save = runtime.block_on(async { spawn_saver(path.clone(), slow_save, None) });
        let remembered = |id: &str| Remembered {
            last_project_id: Some(id.into()),
            ..Remembered::default()
        };
        save.send_replace(Some(remembered("first")));
        // Let the first save start, so the last one queues behind it.
        std::thread::sleep(Duration::from_millis(30));
        save.send_replace(Some(remembered("last")));
        drop(save);
        let start = Instant::now();
        runtime.shutdown_timeout(Duration::from_secs(5));
        assert_eq!(load_remembered(&path), remembered("last"));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "{:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn failed_pending_save_reports_without_values() {
        let (tx, mut rx) = mpsc::channel(4);
        let tokens: Arc<dyn TokenSource> = Arc::new(EnvTokenSource("t".into()));
        let budget = SharedBudget::new(RateBudget::new(None));
        let api = Arc::new(
            Api::new(
                Url::parse("http://127.0.0.1:1/").unwrap(),
                tokens.clone(),
                budget,
            )
            .unwrap(),
        );
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let dir = tempfile::tempdir().unwrap();
        // A file where the directory should be makes the save fail.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        tokio::spawn(run_worker(
            WorkerConfig {
                api,
                ws: Url::parse("ws://127.0.0.1:1/").unwrap(),
                tokens,
                remembered_path: PathBuf::from("unused"),
                pending_path: blocker.join("pending.toml"),
                configured_interval: None,
                gh: PathBuf::from("unused"),
            },
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
        cmd_tx
            .send(Command::SavePending(Arc::new(pending)))
            .await
            .unwrap();
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
            h.send(Command::SavePending(Arc::new(pending.clone())))
                .await;
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

    // ---- deploy: the app and the worker together, against wiremock ----

    use crate::app::{App, Confirm, DeployConfirm, DeployStep, Modal, Phase, StartPrefs};
    use crate::pending::{Change, Target};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use wiremock::matchers::body_string_contains;

    const TARGET: Target<'static> = Target {
        project_id: "p1",
        env_id: "e1",
        env_name: "production",
        service_id: "s1",
        service_name: "api",
    };

    /// Pending edits on (e1, s1): A and B set, X and Y deleted.
    fn deploy_app() -> App {
        let mut pending = Pending::default();
        pending.set(&TARGET, "A", "new-a".into(), Some("old-a"));
        pending.set(&TARGET, "B", "new-b".into(), None);
        pending.delete(&TARGET, "X", Some("x"));
        pending.delete(&TARGET, "Y", Some("y"));
        let (app, _) = App::new(StartPrefs {
            pending,
            ..StartPrefs::default()
        });
        app
    }

    /// Answers `y` to the deploy confirm of (e1, s1).
    fn confirm_deploy(app: &mut App) -> Vec<Command> {
        app.modal = Some(Modal::Confirm(Confirm::Deploy(DeployConfirm {
            env_id: "e1".into(),
            service_id: "s1".into(),
            drift: Vec::new(),
        })));
        let y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
        app.update(AppEvent::Key(y), Instant::now())
    }

    /// Forwards the app's commands to the worker and its replies to the app
    /// until every deploy and variables load has finished; returns the steps.
    async fn drive(h: &mut Harness, app: &mut App, mut cmds: Vec<Command>) -> Vec<DeployStep> {
        let mut steps = Vec::new();
        let mut outstanding = 0;
        loop {
            for cmd in cmds.drain(..) {
                match cmd {
                    Command::DeployService { .. } | Command::LoadVariables { .. } => {
                        outstanding += 1;
                        h.send(cmd).await;
                    }
                    Command::SavePending(_) => h.send(cmd).await,
                    _ => {}
                }
            }
            if outstanding == 0 {
                return steps;
            }
            let event = h.next().await;
            match &event {
                DataEvent::Budget { .. } => continue,
                DataEvent::Deploy { step, .. } => {
                    if matches!(step, DeployStep::Redeployed | DeployStep::Failed { .. }) {
                        outstanding -= 1;
                    }
                    steps.push(step.clone());
                }
                DataEvent::Variables { .. } => outstanding -= 1,
                _ => {}
            }
            cmds = app.update(AppEvent::Data(event), Instant::now());
        }
    }

    #[tokio::test]
    async fn deploy_latest_sends_one_latest_commit_deploy() {
        let mut h = Harness::start(None).await;
        mount(
            &h.server,
            "serviceInstanceDeploy",
            200,
            ok("serviceInstanceDeploy"),
        )
        .await;
        h.send(Command::DeployLatest {
            env_id: "e1".into(),
            service_id: "s1".into(),
            switch: None,
        })
        .await;
        let event = loop {
            match h.next().await {
                DataEvent::Budget { .. } => continue,
                event => break event,
            }
        };
        assert!(
            matches!(
                &event,
                DataEvent::DeployedLatest { env_id, service_id, outcome: LatestOutcome::Started }
                    if env_id == "e1" && service_id == "s1"
            ),
            "{event:?}"
        );
        let requests = h.server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["operationName"], "ServiceInstanceDeploy");
        assert_eq!(
            body["variables"],
            json!({"environmentId": "e1", "serviceId": "s1"})
        );
        assert!(body["query"]
            .as_str()
            .unwrap()
            .contains("latestCommit: true"));
    }

    #[tokio::test]
    async fn deploy_latest_refusal_is_reported_not_resent() {
        let mut h = Harness::start(None).await;
        mount(&h.server, "serviceInstanceDeploy", 400, json!({})).await;
        h.send(Command::DeployLatest {
            env_id: "e1".into(),
            service_id: "s1".into(),
            switch: None,
        })
        .await;
        let event = loop {
            match h.next().await {
                DataEvent::Budget { .. } => continue,
                event => break event,
            }
        };
        assert!(
            matches!(
                &event,
                DataEvent::DeployedLatest {
                    outcome: LatestOutcome::Failed {
                        failure: WriteFailure::Refused,
                        ..
                    },
                    ..
                }
            ),
            "{event:?}"
        );
        assert_eq!(h.server.received_requests().await.unwrap().len(), 1);
    }

    fn switch() -> Option<crate::app::BranchSwitch> {
        Some(crate::app::BranchSwitch {
            trigger_id: "t1".into(),
            from: "main".into(),
            to: "feat/x".into(),
        })
    }

    async fn deployed_latest(h: &mut Harness) -> LatestOutcome {
        loop {
            match h.next().await {
                DataEvent::DeployedLatest { outcome, .. } => return outcome,
                DataEvent::Budget { .. } => continue,
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_picked_branch_is_switched_to_then_deployed() {
        let mut h = Harness::start(None).await;
        let switched =
            json!({"data": {"deploymentTriggerUpdate": {"id": "t1", "branch": "feat/x"}}});
        mount(&h.server, "deploymentTriggerUpdate", 200, switched).await;
        mount(
            &h.server,
            "serviceInstanceDeploy",
            200,
            ok("serviceInstanceDeploy"),
        )
        .await;
        h.send(Command::DeployLatest {
            env_id: "e1".into(),
            service_id: "s1".into(),
            switch: switch(),
        })
        .await;
        assert_eq!(deployed_latest(&mut h).await, LatestOutcome::Started);
        let requests = h.server.received_requests().await.unwrap();
        let bodies: Vec<serde_json::Value> = requests
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        let ops: Vec<&str> = bodies
            .iter()
            .map(|b| b["operationName"].as_str().unwrap())
            .collect();
        assert_eq!(ops, ["DeploymentTriggerUpdate", "ServiceInstanceDeploy"]);
        assert_eq!(
            bodies[0]["variables"],
            json!({"id": "t1", "branch": "feat/x"})
        );
    }

    #[tokio::test]
    async fn a_refused_switch_never_deploys() {
        let mut h = Harness::start(None).await;
        mount(&h.server, "deploymentTriggerUpdate", 400, json!({})).await;
        Mock::given(method("POST"))
            .and(body_string_contains("serviceInstanceDeploy"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok("serviceInstanceDeploy")))
            .expect(0)
            .mount(&h.server)
            .await;
        h.send(Command::DeployLatest {
            env_id: "e1".into(),
            service_id: "s1".into(),
            switch: switch(),
        })
        .await;
        assert!(matches!(
            deployed_latest(&mut h).await,
            LatestOutcome::SwitchFailed {
                failure: WriteFailure::Refused,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_switch_answered_with_another_branch_is_unknown() {
        let mut h = Harness::start(None).await;
        let other = json!({"data": {"deploymentTriggerUpdate": {"id": "t1", "branch": "main"}}});
        mount(&h.server, "deploymentTriggerUpdate", 200, other).await;
        h.send(Command::DeployLatest {
            env_id: "e1".into(),
            service_id: "s1".into(),
            switch: switch(),
        })
        .await;
        assert!(matches!(
            deployed_latest(&mut h).await,
            LatestOutcome::SwitchFailed {
                failure: WriteFailure::Unknown,
                ..
            }
        ));
        assert_eq!(h.server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn load_branches_returns_triggers_and_branch_names() {
        let mut h = Harness::start(None).await;
        let triggers = json!({"data": {"deploymentTriggers": {"edges": [
            {"node": {"id": "t1", "branch": "main", "repository": "acme/api"}}
        ]}}});
        mount(&h.server, "deploymentTriggers", 200, triggers).await;
        let branches =
            json!({"data": {"githubRepoBranches": [{"name": "main"}, {"name": "feat/x"}]}});
        mount(&h.server, "githubRepoBranches", 200, branches).await;
        h.send(Command::LoadBranches {
            project_id: "p1".into(),
            env_id: "e1".into(),
            service_id: "s1".into(),
            repo: "acme/api".into(),
        })
        .await;
        let result = loop {
            match h.next().await {
                DataEvent::Branches { result, .. } => break result,
                DataEvent::Budget { .. } => continue,
                other => panic!("unexpected {other:?}"),
            }
        };
        let BranchReply { triggers, names } = result.unwrap();
        assert_eq!(triggers[0].id, "t1");
        assert_eq!(names.unwrap(), ["main", "feat/x"]);
        let requests = h.server.received_requests().await.unwrap();
        let branch_req = requests
            .iter()
            .map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap())
            .find(|b| b["operationName"] == "GitHubRepoBranches")
            .unwrap();
        assert_eq!(
            branch_req["variables"],
            json!({"owner": "acme", "repo": "api"})
        );
    }

    async fn mount_trigger(server: &MockServer) {
        let triggers = json!({"data": {"deploymentTriggers": {"edges": [
            {"node": {"id": "t1", "branch": "main", "repository": "acme/api"}}
        ]}}});
        mount(server, "deploymentTriggers", 200, triggers).await;
    }

    fn load_branches() -> Command {
        Command::LoadBranches {
            project_id: "p1".into(),
            env_id: "e1".into(),
            service_id: "s1".into(),
            repo: "acme/api".into(),
        }
    }

    async fn branches_reply(h: &mut Harness) -> BranchReply {
        loop {
            match h.next().await {
                DataEvent::Branches { result, .. } => return result.unwrap(),
                DataEvent::Budget { .. } => continue,
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn branches_come_from_gh_first() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\nprintf 'main\\nfeat/from-gh\\n'\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut h = Harness::start_with_gh(None, gh).await;
        mount_trigger(&h.server).await;
        Mock::given(method("POST"))
            .and(body_string_contains("githubRepoBranches"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&h.server)
            .await;
        h.send(load_branches()).await;
        let BranchReply { triggers, names } = branches_reply(&mut h).await;
        assert_eq!(triggers[0].branch, "main");
        assert_eq!(names.unwrap(), ["main", "feat/from-gh"]);
    }

    #[tokio::test]
    async fn railway_refusing_branches_is_reported_once_without_a_refresh() {
        let mut h = Harness::start(None).await;
        mount_trigger(&h.server).await;
        let refused = json!({"data": null, "errors": [{"message": "Not Authorized"}]});
        mount(&h.server, "githubRepoBranches", 200, refused).await;
        h.send(load_branches()).await;
        let BranchReply { triggers, names } = branches_reply(&mut h).await;
        assert_eq!(triggers.len(), 1, "the trigger still loads");
        assert_eq!(
            names.unwrap_err(),
            "gh is not installed; Railway: Railway API error: Not Authorized"
        );
        let asked = h
            .server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("githubRepoBranches"))
            .count();
        assert_eq!(asked, 1, "a refusal is not retried");
    }

    async fn mount(server: &MockServer, op: &str, status: u16, body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(body_string_contains(op))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    async fn mount_once(server: &MockServer, op: &str, status: u16) {
        Mock::given(method("POST"))
            .and(body_string_contains(op))
            .respond_with(ResponseTemplate::new(status))
            .up_to_n_times(1)
            .mount(server)
            .await;
    }

    fn ok(field: &str) -> serde_json::Value {
        json!({ "data": { field: true } })
    }

    /// Every request so far as (operation, deleted name if any), in order.
    async fn sent(h: &Harness) -> Vec<(String, Option<String>)> {
        h.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                let op = body["operationName"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let name = body["variables"]["input"]["name"]
                    .as_str()
                    .map(String::from);
                (op, name)
            })
            .collect()
    }

    fn op(name: &str, deleted: Option<&str>) -> (String, Option<String>) {
        (name.to_string(), deleted.map(String::from))
    }

    fn applied(app: &App) -> Vec<String> {
        app.pending
            .service("e1", "s1")
            .map(|sp| {
                sp.vars
                    .iter()
                    .filter(|(_, v)| v.applied)
                    .map(|(n, _)| n.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn upsert_then_deletes_then_redeploy_order() {
        let mut h = Harness::start(None).await;
        mount(
            &h.server,
            "variableCollectionUpsert",
            200,
            ok("variableCollectionUpsert"),
        )
        .await;
        mount(&h.server, "variableDelete", 200, ok("variableDelete")).await;
        mount(
            &h.server,
            "serviceInstanceRedeploy",
            200,
            ok("serviceInstanceRedeploy"),
        )
        .await;
        let mut app = deploy_app();
        let cmds = confirm_deploy(&mut app);
        let steps = drive(&mut h, &mut app, cmds).await;
        assert_eq!(
            steps,
            [
                DeployStep::Upserted,
                DeployStep::Deleted("X".into()),
                DeployStep::Deleted("Y".into()),
                DeployStep::Redeployed,
            ]
        );
        assert_eq!(
            sent(&h).await,
            [
                op("VariableCollectionUpsert", None),
                op("VariableDelete", Some("X")),
                op("VariableDelete", Some("Y")),
                op("ServiceInstanceRedeploy", None),
            ]
        );
        assert!(app.pending.service("e1", "s1").is_none());
        assert_eq!(app.message.as_deref(), Some("deployed api to production"));
    }

    #[tokio::test]
    async fn failed_delete_stops_before_redeploy() {
        let mut h = Harness::start(None).await;
        mount(
            &h.server,
            "variableCollectionUpsert",
            200,
            ok("variableCollectionUpsert"),
        )
        .await;
        mount(&h.server, "variableDelete", 400, json!({})).await;
        Mock::given(method("POST"))
            .and(body_string_contains("serviceInstanceRedeploy"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok("serviceInstanceRedeploy")))
            .expect(0)
            .mount(&h.server)
            .await;
        let mut app = deploy_app();
        let cmds = confirm_deploy(&mut app);
        let steps = drive(&mut h, &mut app, cmds).await;
        assert_eq!(steps.len(), 2, "{steps:?}");
        assert_eq!(steps[0], DeployStep::Upserted);
        assert!(matches!(
            &steps[1],
            DeployStep::Failed { during: Phase::Delete(n), failure: WriteFailure::Refused, .. } if n == "X"
        ));
        assert_eq!(applied(&app), ["A", "B"]);
        let msg = app.message.clone().unwrap();
        assert!(msg.starts_with("deploy failed during delete X: "), "{msg}");
        assert!(!msg.contains("new-a") && !msg.contains("new-b"), "{msg}");
    }

    #[tokio::test]
    async fn railway_error_echoing_a_sent_value_is_masked() {
        let mut h = Harness::start(None).await;
        // Railway's own message quotes what was submitted.
        let reply = json!({
            "data": null,
            "errors": [{ "message": "invalid value \"new-a\" for A; new-b too" }]
        });
        mount(&h.server, "variableCollectionUpsert", 200, reply).await;
        let mut app = deploy_app();
        let cmds = confirm_deploy(&mut app);
        let steps = drive(&mut h, &mut app, cmds).await;
        let [DeployStep::Failed {
            during: Phase::Upsert,
            error,
            failure: WriteFailure::Refused,
        }] = steps.as_slice()
        else {
            panic!("{steps:?}");
        };
        for shown in [error.clone(), app.message.clone().unwrap()] {
            assert!(
                !shown.contains("new-a") && !shown.contains("new-b"),
                "{shown}"
            );
            assert!(shown.contains("invalid value \"••••••\" for A"), "{shown}");
        }
    }

    #[test]
    fn mask_sent_values_longest_first_and_skips_empty() {
        let sent: crate::model::Variables = [("A", "abc"), ("B", "abcdef"), ("C", "")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(
            mask_sent("got abcdef and abc".into(), &sent),
            "got •••••• and ••••••"
        );
    }

    #[tokio::test]
    async fn retry_sends_only_unapplied_then_redeploys() {
        let mut h = Harness::start(None).await;
        mount(
            &h.server,
            "variableCollectionUpsert",
            200,
            ok("variableCollectionUpsert"),
        )
        .await;
        mount_once(&h.server, "variableDelete", 400).await;
        mount(&h.server, "variableDelete", 200, ok("variableDelete")).await;
        mount(
            &h.server,
            "serviceInstanceRedeploy",
            200,
            ok("serviceInstanceRedeploy"),
        )
        .await;
        let mut app = deploy_app();
        let cmds = confirm_deploy(&mut app);
        drive(&mut h, &mut app, cmds).await;
        assert_eq!(applied(&app), ["A", "B"]);
        let first = sent(&h).await.len();

        let cmds = confirm_deploy(&mut app);
        let steps = drive(&mut h, &mut app, cmds).await;
        assert_eq!(
            steps,
            [
                DeployStep::Deleted("X".into()),
                DeployStep::Deleted("Y".into()),
                DeployStep::Redeployed,
            ]
        );
        assert_eq!(
            sent(&h).await[first..],
            [
                op("VariableDelete", Some("X")),
                op("VariableDelete", Some("Y")),
                op("ServiceInstanceRedeploy", None),
            ]
        );
        assert!(app.pending.service("e1", "s1").is_none());
    }

    #[tokio::test]
    async fn redeploy_failure_retry_is_redeploy_only() {
        let mut h = Harness::start(None).await;
        mount(
            &h.server,
            "variableCollectionUpsert",
            200,
            ok("variableCollectionUpsert"),
        )
        .await;
        mount(&h.server, "variableDelete", 200, ok("variableDelete")).await;
        mount_once(&h.server, "serviceInstanceRedeploy", 400).await;
        mount(
            &h.server,
            "serviceInstanceRedeploy",
            200,
            ok("serviceInstanceRedeploy"),
        )
        .await;
        let mut app = deploy_app();
        let cmds = confirm_deploy(&mut app);
        let steps = drive(&mut h, &mut app, cmds).await;
        assert!(matches!(
            steps.last(),
            Some(DeployStep::Failed {
                during: Phase::Redeploy,
                failure: WriteFailure::Refused,
                ..
            })
        ));
        let sp = app.pending.service("e1", "s1").unwrap();
        assert!(sp.written_not_deployed);
        assert_eq!(applied(&app), ["A", "B", "X", "Y"]);
        let first = sent(&h).await.len();

        let cmds = confirm_deploy(&mut app);
        assert!(
            matches!(
                cmds.as_slice(),
                [Command::DeployService {
                    redeploy_only: true,
                    ..
                }]
            ),
            "{cmds:?}"
        );
        let steps = drive(&mut h, &mut app, cmds).await;
        assert_eq!(steps, [DeployStep::Redeployed]);
        assert_eq!(
            sent(&h).await[first..],
            [op("ServiceInstanceRedeploy", None)]
        );
        assert!(app.pending.service("e1", "s1").is_none());
    }

    #[tokio::test]
    async fn unknown_write_reconciles_from_railway() {
        let mut h = Harness::start(None).await;
        // Sent, but the reply is a 500: Railway may or may not have applied it.
        mount(&h.server, "variableCollectionUpsert", 500, json!({})).await;
        // Railway holds A's new value, but not B; X is gone, Y is still there.
        let current = json!({ "data": { "variables": { "A": "new-a", "Y": "y" } } });
        mount(&h.server, "query Variables", 200, current).await;
        Mock::given(method("POST"))
            .and(body_string_contains("variableDelete"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&h.server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains("serviceInstanceRedeploy"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&h.server)
            .await;
        let mut app = deploy_app();
        let cmds = confirm_deploy(&mut app);
        let steps = drive(&mut h, &mut app, cmds).await;
        assert!(matches!(
            steps.as_slice(),
            [DeployStep::Failed {
                during: Phase::Upsert,
                failure: WriteFailure::Unknown,
                ..
            }]
        ));
        assert_eq!(applied(&app), ["A", "X"]);
        let ops: Vec<String> = sent(&h).await.into_iter().map(|(o, _)| o).collect();
        assert_eq!(
            ops,
            ["VariableCollectionUpsert", "Variables"],
            "never resent"
        );
    }

    async fn variables_event(h: &mut Harness) -> DataEvent {
        loop {
            match h.next().await {
                DataEvent::Budget { .. } => continue,
                event => return event,
            }
        }
    }

    fn load_variables(purpose: crate::app::VarsPurpose) -> Command {
        Command::LoadVariables {
            project_id: "p1".into(),
            env_id: "e1".into(),
            service_id: "svc".into(),
            purpose,
        }
    }

    #[tokio::test]
    async fn view_load_reports_sealed_names_for_the_service() {
        let mut h = Harness::start(None).await;
        let vars = json!({"data": {"variables": {"A": "1", "S": ""}}});
        mount(&h.server, "query Variables", 200, vars).await;
        let node = |name: &str, service: &str, sealed: bool| json!({"node": {"name": name, "serviceId": service, "isSealed": sealed}});
        let sealed = json!({"data": {"environment": {"variables": {
            "edges": [node("S", "svc", true), node("A", "svc", false), node("S2", "other", true)],
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        }}}});
        mount(&h.server, "query SealedVariables", 200, sealed).await;
        h.send(load_variables(crate::app::VarsPurpose::View)).await;
        let DataEvent::Variables { sealed, .. } = variables_event(&mut h).await else {
            panic!("expected Variables");
        };
        assert_eq!(sealed, Some(Ok(["S".to_string()].into())));
    }

    fn sealed_page(names: &[&str], has_next: bool, cursor: Option<&str>) -> serde_json::Value {
        let edges: Vec<_> = names
            .iter()
            .map(|n| json!({"node": {"name": n, "serviceId": "svc", "isSealed": true}}))
            .collect();
        json!({"data": {"environment": {"variables": {
            "edges": edges,
            "pageInfo": {"hasNextPage": has_next, "endCursor": cursor}
        }}}})
    }

    async fn sealed_requests(h: &Harness) -> usize {
        sent(h)
            .await
            .iter()
            .filter(|(op, _)| op == "SealedVariables")
            .count()
    }

    #[tokio::test]
    async fn sealed_names_are_collected_across_pages() {
        let mut h = Harness::start(None).await;
        let vars = json!({"data": {"variables": {"A": "1"}}});
        mount(&h.server, "query Variables", 200, vars).await;
        // Only the request for page 2 carries the cursor; its mock outranks page 1's.
        mount(
            &h.server,
            "query SealedVariables",
            200,
            sealed_page(&["P1"], true, Some("c1")),
        )
        .await;
        Mock::given(method("POST"))
            .and(body_string_contains("query SealedVariables"))
            .and(body_string_contains(r#""after":"c1""#))
            .respond_with(ResponseTemplate::new(200).set_body_json(sealed_page(
                &["P2"],
                false,
                None,
            )))
            .with_priority(1)
            .mount(&h.server)
            .await;
        h.send(load_variables(crate::app::VarsPurpose::View)).await;
        let DataEvent::Variables { sealed, .. } = variables_event(&mut h).await else {
            panic!("expected Variables");
        };
        assert_eq!(
            sealed,
            Some(Ok(["P1".to_string(), "P2".to_string()].into()))
        );
        assert_eq!(sealed_requests(&h).await, 2);
    }

    #[tokio::test]
    async fn sealed_paging_stops_without_a_cursor() {
        let mut h = Harness::start(None).await;
        let vars = json!({"data": {"variables": {"A": "1"}}});
        mount(&h.server, "query Variables", 200, vars).await;
        // More pages claimed, but no cursor to ask for them with.
        mount(
            &h.server,
            "query SealedVariables",
            200,
            sealed_page(&["P1"], true, None),
        )
        .await;
        h.send(load_variables(crate::app::VarsPurpose::View)).await;
        let DataEvent::Variables { sealed, .. } = variables_event(&mut h).await else {
            panic!("expected Variables");
        };
        assert_eq!(sealed, Some(Ok(["P1".to_string()].into())));
        assert_eq!(sealed_requests(&h).await, 1);
    }

    #[tokio::test]
    async fn deploy_load_skips_the_sealed_query() {
        let mut h = Harness::start(None).await;
        let vars = json!({"data": {"variables": {"A": "1"}}});
        mount(&h.server, "query Variables", 200, vars).await;
        h.send(load_variables(crate::app::VarsPurpose::Deploy))
            .await;
        let DataEvent::Variables { sealed, .. } = variables_event(&mut h).await else {
            panic!("expected Variables");
        };
        assert_eq!(sealed, None);
        let ops: Vec<String> = sent(&h).await.into_iter().map(|(o, _)| o).collect();
        assert_eq!(ops, ["Variables"]);
    }

    #[tokio::test]
    async fn rate_limited_deploy_sends_nothing() {
        let mut h = Harness::start(None).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&h.server)
            .await;
        h.budget
            .lock()
            .on_rate_limited(Some(Duration::from_secs(30)), Instant::now());
        let mut app = deploy_app();
        let cmds = confirm_deploy(&mut app);
        let steps = drive(&mut h, &mut app, cmds).await;
        assert!(matches!(
            steps.as_slice(),
            [DeployStep::Failed {
                during: Phase::Upsert,
                failure: WriteFailure::Refused,
                ..
            }]
        ));
        assert!(applied(&app).is_empty());
        let sp = app.pending.service("e1", "s1").unwrap();
        assert!(!sp.written_not_deployed);
        assert_eq!(sp.vars.len(), 4);
        assert!(matches!(sp.vars["X"].change, Change::Delete));
    }
}
