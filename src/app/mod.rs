//! App state machine: `update(event, now) -> commands`. No IO, no rendering.

mod deploy;
mod keys;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::KeyEvent;

use crate::budget::{backstop_interval, DEFAULT_HOURLY_LIMIT};
use crate::logs::buffer::{LogBuffer, LOG_CAP};
use crate::logs::follow::{FollowEvent, StreamState};
use crate::model::{
    DeploymentStatus, DeploymentSummary, Environment, Instance, LogKind, ProjectRef,
    ServiceInstance, Trigger, Variables, Workspace, WriteFailure,
};
use crate::pending::{Pending, PendingVar};
use crate::remember::Remembered;
use crate::status::{default_log_kind, deployment_rows, DeploymentRow};
use crate::theme::{self, Theme, THEMES};

/// A fetched value with its freshness. A failed reload keeps the old value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded<T> {
    pub value: Option<T>,
    pub fetched_at: Option<Instant>,
    pub error: Option<String>,
    pub loading: bool,
}

impl<T> Default for Loaded<T> {
    fn default() -> Self {
        Self {
            value: None,
            fetched_at: None,
            error: None,
            loading: false,
        }
    }
}

/// The stack holds a handful of views, so the large `Logs` variant is not worth boxing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum View {
    Projects,
    Services,
    Deployments { service_id: String },
    Instances { service_id: String },
    Logs(LogsView),
    Variables(VarsView),
    Pending(PendingView),
}

/// The Variables view of one service. Dropping it re-hides every value.
#[derive(Debug)]
pub struct VarsView {
    pub service_id: String,
    /// Names whose value is shown.
    pub revealed: BTreeSet<String>,
    pub reveal_all: bool,
    /// Names Railway reports as sealed (their values are never readable).
    pub sealed: BTreeSet<String>,
    /// Where a visual selection started.
    pub anchor: Option<String>,
}

/// The review of every pending change in the open project.
#[derive(Debug, Default)]
pub struct PendingView {
    pub reveal_all: bool,
}

/// One service in one environment: what a pending group or a deploy is of.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupKey {
    pub env_id: String,
    pub service_id: String,
}

impl GroupKey {
    pub fn new(env_id: impl Into<String>, service_id: impl Into<String>) -> Self {
        Self {
            env_id: env_id.into(),
            service_id: service_id.into(),
        }
    }

    fn is(&self, env_id: &str, service_id: &str) -> bool {
        self.env_id == env_id && self.service_id == service_id
    }
}

/// One service's pending changes in one environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGroup {
    /// Also its selection key.
    pub key: GroupKey,
    /// What no longer exists, so nothing can be written to it.
    pub orphaned: Option<Orphan>,
}

/// Why a pending group can only be discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orphan {
    /// The environment is not in the loaded list.
    Environment,
    /// The environment is the current one and its loaded services lack this one.
    Service,
}

/// A question waiting for `y`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    Discard { env_id: String, service_id: String },
    Deploy(DeployConfirm),
    DeployLatest(Box<LatestConfirm>),
}

/// Deploying one pending group. The ids come from the group, never from the
/// open project or environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployConfirm {
    pub env_id: String,
    pub service_id: String,
    /// Names changed on Railway since they were edited.
    pub drift: Vec<String>,
}

/// Deploying the latest commit of a service's branch, with what is deployed
/// now and where it goes, as known when `D` was pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestConfirm {
    pub project_id: String,
    pub env_id: String,
    pub env_name: String,
    pub service_id: String,
    pub service_name: String,
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// The deployment running now, else the latest one.
    pub current: Option<DeploymentSummary>,
    /// Variables written to Railway but not deployed: they ship with this deploy.
    pub written: usize,
    /// Set when a branch other than the connected one was picked with `d`.
    pub switch: Option<BranchSwitch>,
}

/// Points the service's deploy trigger at another branch before deploying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchSwitch {
    pub trigger_id: String,
    pub from: String,
    pub to: String,
}

/// The branch picker (`d` on Services or Deployments): type to filter,
/// arrows to move, Enter to pick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchPicker {
    /// The deploy as it would go to the connected branch.
    pub target: LatestConfirm,
    /// `None` while loading.
    pub choices: Option<Result<BranchChoices, String>>,
    pub query: String,
    /// Index into `filtered()`.
    pub index: usize,
}

/// A service's deploy triggers and its repo's branch names, or why the names
/// could not be listed (the trigger is still usable then).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchReply {
    pub triggers: Vec<Trigger>,
    pub names: Result<Vec<String>, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchChoices {
    pub trigger_id: String,
    /// The branch the service deploys from now.
    pub connected: String,
    pub names: Vec<String>,
    /// Why no branch list could be had; the typed name is picked instead.
    pub unlisted: Option<String>,
}

impl BranchPicker {
    /// Branch names containing the query (any case), the connected one first.
    pub fn filtered(&self) -> Vec<&str> {
        let Some(Ok(c)) = &self.choices else {
            return Vec::new();
        };
        let query = self.query.to_lowercase();
        let mut names: Vec<&str> = c
            .names
            .iter()
            .map(String::as_str)
            .filter(|n| n.to_lowercase().contains(&query))
            .collect();
        if let Some(i) = names.iter().position(|n| *n == c.connected) {
            let connected = names.remove(i);
            names.insert(0, connected);
        }
        names
    }
}

/// What Railway said to a deploy of the latest commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatestOutcome {
    Started,
    /// The deploy was refused, or sent with no answer as to whether it started.
    Failed {
        error: String,
        failure: WriteFailure,
    },
    /// The branch switch did not go through (or may not have), so nothing was deployed.
    SwitchFailed {
        error: String,
        failure: WriteFailure,
    },
}

/// Why variables were loaded: for the Variables view, to check a deploy
/// before confirming it, or to find out what an interrupted write applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarsPurpose {
    View,
    Deploy,
    Reconcile,
}

/// One finished step of a deploy, reported by the worker as it goes.
/// Error texts are Railway's messages with every sent value masked by the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployStep {
    Upserted,
    Deleted(String),
    Redeployed,
    Failed {
        during: Phase,
        error: String,
        failure: WriteFailure,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Upsert,
    Delete(String),
    Redeploy,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Phase::Upsert => f.write_str("upsert"),
            Phase::Delete(name) => write!(f, "delete {name}"),
            Phase::Redeploy => f.write_str("redeploy"),
        }
    }
}

#[derive(Debug)]
pub struct LogsView {
    pub session: u64,
    pub deployment_id: String,
    pub service_name: String,
    pub kind: LogKind,
    pub replica: Option<String>,
    pub buffer: LogBuffer,
    pub follow: bool,
    /// Lines scrolled up from the bottom; 0 while following.
    pub scroll: usize,
    pub filter: Option<String>,
    pub state: StreamState,
}

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Tick(Instant),
    Data(DataEvent),
}

/// Values travel as `Secret`, so `Debug` never prints them.
#[derive(Debug)]
pub enum DataEvent {
    Workspaces(Result<Vec<Workspace>, String>),
    Environments {
        project_id: String,
        result: Result<Vec<Environment>, String>,
    },
    Services {
        project_id: String,
        env_id: String,
        result: Result<Vec<ServiceInstance>, String>,
    },
    Deployments {
        service_id: String,
        env_id: String,
        result: Result<Vec<DeploymentSummary>, String>,
    },
    Variables {
        env_id: String,
        service_id: String,
        purpose: VarsPurpose,
        result: Result<Secret<Variables>, String>,
        /// Sealed names or why they are unknown; `Some` only for `VarsPurpose::View`.
        sealed: Option<Result<BTreeSet<String>, String>>,
    },
    Deploy {
        env_id: String,
        service_id: String,
        step: DeployStep,
    },
    DeployedLatest {
        env_id: String,
        service_id: String,
        outcome: LatestOutcome,
    },
    /// The service's deploy triggers and its repo's branches (or why they
    /// could not be listed), for the picker.
    Branches {
        env_id: String,
        service_id: String,
        result: Result<BranchReply, String>,
    },
    /// Saving the pending file failed; the text never holds a value.
    SaveFailed(String),
    /// The external editor finished: the new value, or why it could not run.
    Edited {
        name: String,
        result: Result<Secret<String>, String>,
    },
    StatusChanged {
        deployment_id: String,
        status: DeploymentStatus,
    },
    Log {
        session: u64,
        event: FollowEvent,
    },
    Budget {
        interval: Duration,
        blocked_until: Option<Instant>,
    },
    LiveUpdates {
        ok: bool,
    },
}

/// Values travel as `Secret`, so `Debug` never prints them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    LoadWorkspaces,
    LoadEnvironments {
        project_id: String,
    },
    LoadServices {
        project_id: String,
        env_id: String,
    },
    LoadDeployments {
        project_id: String,
        env_id: String,
        service_id: String,
    },
    LoadVariables {
        project_id: String,
        env_id: String,
        service_id: String,
        purpose: VarsPurpose,
    },
    /// A snapshot of the pending store, shared until the app next changes it.
    SavePending(Arc<Pending>),
    /// Writes one group's unapplied changes, then redeploys: upsert (skipped
    /// when `upserts` is empty), each delete, then the redeploy, stopping at
    /// the first failure. `redeploy_only` skips the writes.
    DeployService {
        project_id: String,
        env_id: String,
        service_id: String,
        upserts: Secret<Variables>,
        deletes: Vec<String>,
        redeploy_only: bool,
    },
    /// Deploys the latest commit of the service's connected branch, after
    /// switching that branch when `switch` is set (stopping if it fails).
    DeployLatest {
        env_id: String,
        service_id: String,
        switch: Option<BranchSwitch>,
    },
    /// The service's deploy triggers and the branches of `repo` (`owner/name`).
    LoadBranches {
        project_id: String,
        env_id: String,
        service_id: String,
        repo: String,
    },
    /// Handled by the main loop, not the worker: edit `initial` in `$VISUAL`/`$EDITOR`.
    EditExternal {
        name: String,
        initial: Secret<String>,
    },
    /// Handled by the main loop, not the worker: put `text` on the clipboard via OSC 52.
    Copy {
        text: Secret<String>,
    },
    /// Replaces the previous watch set; empty means stop.
    WatchDeployments {
        ids: Vec<String>,
    },
    OpenLogs {
        session: u64,
        env_id: String,
        deployment_id: String,
        kind: LogKind,
    },
    CloseLogs {
        session: u64,
    },
    SaveRemembered(Remembered),
    Quit,
}

/// A one-line variable input. `name: None` means the name of a new variable is being typed;
/// otherwise `text` is the value of `name`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VarInput {
    pub name: Option<String>,
    pub text: Secret<String>,
}

/// A variable value: `Debug` prints `<redacted>`, so it never reaches a log or
/// panic message. Reading it takes an explicit `expose`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &T {
        &self.0
    }

    pub fn expose_mut(&mut self) -> &mut T {
        &mut self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The one input that takes the next key: at most one is open at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// The question being asked, answered with `y` or any other key.
    Confirm(Confirm),
    /// The variable name or value being typed in the Variables view.
    VarInput(VarInput),
    /// Command text being typed after `:`.
    Command(String),
    /// Filter text being typed after `/` (table or logs, by the top view).
    Filter(String),
    /// The environment picker, at its highlighted index into `environments`.
    EnvPicker(usize),
    BranchPicker(Box<BranchPicker>),
    /// The theme picker; `restore` is the theme to go back to on Esc.
    ThemePicker {
        restore: usize,
    },
}

/// Start-up preferences: config defaults (id or case-insensitive name) and remembered ids.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartPrefs {
    pub workspace: Option<String>,
    pub project: Option<String>,
    pub environment: Option<String>,
    /// Configured theme id or name.
    pub theme: Option<String>,
    pub remembered: Remembered,
    pub pending: Pending,
    /// Why the pending file could not be read, shown at start.
    pub pending_error: Option<String>,
}

/// Selected row per table view, by id, so it survives reloads and filtering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub project: Option<String>,
    pub service: Option<String>,
    pub deployment: Option<String>,
    pub instance: Option<String>,
    pub variable: Option<String>,
    pub pending: Option<GroupKey>,
}

pub struct App {
    pub(crate) prefs: StartPrefs,
    /// Never empty; the root is always `View::Projects`.
    pub(crate) stack: Vec<View>,
    pub(crate) workspace_id: Option<String>,
    pub(crate) project: Option<ProjectRef>,
    pub(crate) environment: Option<Environment>,
    pub(crate) workspaces: Loaded<Vec<Workspace>>,
    pub(crate) environments: Loaded<Vec<Environment>>,
    pub(crate) services: Loaded<Vec<ServiceInstance>>,
    /// History for the service of the `Deployments` view in the stack.
    pub(crate) deployments: Loaded<Vec<DeploymentSummary>>,
    /// Railway's variables for the service of the open `Variables` view.
    pub(crate) variables: Loaded<Variables>,
    /// Edits not deployed yet, for every service. Shared with the saver;
    /// changed through `pending_mut`.
    pub(crate) pending: Arc<Pending>,
    pub(crate) selection: Selection,
    /// The service under the cursor in the Services view and when it got there,
    /// so a long name scrolls from its start.
    pub(crate) service_focus: Option<(String, Instant)>,
    /// The input that takes the next key, if any.
    pub(crate) modal: Option<Modal>,
    /// Deploys sent to the worker and not finished.
    deploys: BTreeMap<GroupKey, deploy::Sent>,
    /// Latest-commit deploys sent to the worker and not answered.
    latest_deploys: BTreeMap<GroupKey, deploy::LatestSent>,
    /// Ctrl-C was pressed once while deploys are in flight; a second one quits.
    /// Cleared once no deploy is in flight.
    force_quit_armed: bool,
    /// Applied name filter for the current table view (cleared on view change).
    pub(crate) table_filter: Option<String>,
    /// Index into `THEMES` of the active (or previewed) theme.
    pub(crate) theme: usize,
    /// Stays open over a modal, which still takes the keys.
    pub(crate) help: bool,
    /// One-shot notice (unknown command, nothing to open); cleared on the next key.
    pub(crate) message: Option<String>,
    pub(crate) backstop: Duration,
    pub(crate) blocked_until: Option<Instant>,
    pub(crate) live_updates: bool,
    next_session: u64,
    started: bool,
    requested: Requested,
}

/// When each load was last requested, so a failed first load is still retried on the backstop.
#[derive(Debug, Default)]
struct Requested {
    workspaces: Option<Instant>,
    environments: Option<Instant>,
    services: Option<Instant>,
    deployments: Option<Instant>,
    variables: Option<Instant>,
}

impl App {
    pub fn new(prefs: StartPrefs) -> (App, Vec<Command>) {
        let (theme, theme_message) = start_theme(&prefs);
        let message = prefs.pending_error.clone().or(theme_message);
        let pending = Arc::new(prefs.pending.clone());
        let mut app = App {
            prefs,
            stack: vec![View::Projects],
            workspace_id: None,
            project: None,
            environment: None,
            workspaces: Loaded::default(),
            environments: Loaded::default(),
            services: Loaded::default(),
            deployments: Loaded::default(),
            variables: Loaded::default(),
            pending,
            selection: Selection::default(),
            service_focus: None,
            modal: None,
            deploys: BTreeMap::new(),
            latest_deploys: BTreeMap::new(),
            force_quit_armed: false,
            table_filter: None,
            theme,
            help: false,
            message,
            backstop: backstop_interval(DEFAULT_HOURLY_LIMIT, None),
            blocked_until: None,
            live_updates: true,
            next_session: 0,
            started: false,
            requested: Requested::default(),
        };
        app.workspaces.loading = true;
        (app, vec![Command::LoadWorkspaces])
    }

    pub fn update(&mut self, ev: AppEvent, now: Instant) -> Vec<Command> {
        let mut out = Vec::new();
        match ev {
            AppEvent::Key(key) => self.on_key(key, now, &mut out),
            AppEvent::Tick(_) => self.on_tick(now, &mut out),
            AppEvent::Data(data) => self.on_data(data, now, &mut out),
        }
        if !self.deploy_in_flight() {
            self.force_quit_armed = false;
        }
        self.track_service_focus(now);
        out
    }

    fn track_service_focus(&mut self, now: Instant) {
        let focused = match self.kind() {
            Kind::Services => self.selected_service().map(|s| s.service_id.clone()),
            _ => None,
        };
        match focused {
            Some(id) if self.service_focus.as_ref().is_some_and(|(f, _)| *f == id) => {}
            Some(id) => self.service_focus = Some((id, now)),
            None => self.service_focus = None,
        }
    }

    // ---- rows for the table views (filtered by `table_filter`) ----

    /// Projects grouped by workspace, the preferred workspace first.
    pub fn project_rows(&self) -> Vec<(&Workspace, &ProjectRef)> {
        self.ordered_workspaces()
            .into_iter()
            .flat_map(|w| w.projects.iter().map(move |p| (w, p)))
            .filter(|(_, p)| self.passes_filter(&[&p.name]))
            .collect()
    }

    /// Services in alphabetical order (ignoring case), narrowed by the filter.
    pub fn service_rows(&self) -> Vec<&ServiceInstance> {
        let mut rows: Vec<&ServiceInstance> = self
            .services
            .value
            .iter()
            .flatten()
            .filter(|s| self.passes_filter(&[&s.service_name]))
            .collect();
        rows.sort_by_cached_key(|s| (s.service_name.to_lowercase(), s.service_id.clone()));
        rows
    }

    /// A loaded deployment by id: a service's latest or active one, or the history.
    pub fn find_deployment(&self, id: &str) -> Option<&DeploymentSummary> {
        let services = self.services.value.iter().flatten();
        let history = self.deployments.value.iter().flatten();
        services
            .flat_map(|s| s.latest.iter().chain(&s.active))
            .chain(history)
            .find(|d| d.id == id)
    }

    /// The Deployments view: failed latest, then active, then history (see `deployment_rows`).
    pub fn deployment_table(&self) -> Vec<DeploymentRow<'_>> {
        let Some(service_id) = self.view_service_id(Kind::Deployments) else {
            return Vec::new();
        };
        let history = self.deployments.value.as_deref().unwrap_or_default();
        let rows = match self.service(service_id) {
            Some(svc) => deployment_rows(svc, history),
            None => history
                .iter()
                .map(|deployment| DeploymentRow {
                    deployment,
                    tag: None,
                })
                .collect(),
        };
        rows.into_iter()
            .filter(|r| {
                let d = r.deployment;
                let msg = d.commit_message.as_deref().or(d.image.as_deref());
                let sha = d.commit_sha.as_deref();
                self.passes_filter(&[&d.id, msg.unwrap_or(""), sha.unwrap_or("")])
            })
            .collect()
    }

    /// Replicas of the Instances view's service: its active deployments, or the latest if none.
    pub fn instance_rows(&self) -> Vec<(&DeploymentSummary, &Instance)> {
        let Some(svc) = self
            .view_service_id(Kind::Instances)
            .and_then(|id| self.service(id))
        else {
            return Vec::new();
        };
        let source: Vec<&DeploymentSummary> = if svc.active.is_empty() {
            svc.latest.iter().collect()
        } else {
            svc.active.iter().collect()
        };
        source
            .into_iter()
            .flat_map(|d| d.instances.iter().map(move |i| (d, i)))
            .filter(|(_, i)| self.passes_filter(&[&i.id]))
            .collect()
    }

    /// Sealed names of the open Variables view.
    fn sealed_names(&self) -> &BTreeSet<String> {
        static NONE: BTreeSet<String> = BTreeSet::new();
        match self.stack.last() {
            Some(View::Variables(v)) => &v.sealed,
            _ => &NONE,
        }
    }

    /// Railway's variables for the open service merged with its pending edits,
    /// by name, narrowed by the name filter (never the values).
    pub fn variable_rows(&self) -> Vec<VarRow<'_>> {
        let Some(service_id) = self.view_service_id(Kind::Variables) else {
            return Vec::new();
        };
        let sealed = self.sealed_names();
        let mut rows: BTreeMap<&str, VarRow> = BTreeMap::new();
        for (name, value) in self.variables.value.iter().flatten() {
            let row = VarRow {
                name,
                current: Some(value),
                pending: None,
                sealed: sealed.contains(name),
            };
            rows.insert(name, row);
        }
        // Railway may leave sealed variables out of the map altogether.
        for name in sealed {
            rows.entry(name).or_insert(VarRow {
                name,
                current: None,
                pending: None,
                sealed: true,
            });
        }
        let pending = self
            .env_id()
            .and_then(|env| self.pending.service(env, service_id));
        for (name, change) in pending.iter().flat_map(|p| &p.vars) {
            rows.entry(name)
                .or_insert(VarRow {
                    name,
                    current: None,
                    pending: None,
                    sealed: false,
                })
                .pending = Some(change);
        }
        rows.into_values()
            .filter(|r| self.passes_filter(&[r.name]))
            .collect()
    }

    /// Indices into `variable_rows()` from the visual anchor to the cursor, either way
    /// round. None outside Variables, with no anchor, or when the anchor's row is gone
    /// (or hidden by the filter).
    pub fn visual_range(&self) -> Option<RangeInclusive<usize>> {
        let Some(View::Variables(v)) = self.stack.last() else {
            return None;
        };
        let anchor = v.anchor.as_deref()?;
        let rows = self.variable_rows();
        let start = rows.iter().position(|r| r.name == anchor)?;
        let cursor = position(&rows, self.selection.variable.as_deref(), |r| r.name);
        Some(start.min(cursor)..=start.max(cursor))
    }

    /// Pending changes of the open project: the current environment first, then the
    /// others by name, and by service name within each.
    pub fn pending_groups(&self) -> Vec<PendingGroup> {
        let Some(project_id) = self.project_id() else {
            return Vec::new();
        };
        let current = self.env_id();
        let loaded = self.environments.value.as_deref();
        let services = self.services.value.as_deref();
        let mut found: Vec<(bool, &str, &str, &str, &str)> = Vec::new();
        for (env_id, services) in &self.pending.envs {
            for (service_id, sp) in services {
                if sp.project_id == project_id {
                    // The env id after its name keeps same-named environments apart.
                    found.push((
                        current != Some(env_id.as_str()),
                        &sp.env_name,
                        env_id,
                        &sp.service_name,
                        service_id,
                    ));
                }
            }
        }
        found.sort();
        found
            .into_iter()
            .map(|(_, _, env_id, _, service_id)| {
                let env_gone = loaded.is_some_and(|l| !l.iter().any(|e| e.id == env_id));
                let service_gone = current == Some(env_id)
                    && services.is_some_and(|l| !l.iter().any(|s| s.service_id == service_id));
                PendingGroup {
                    key: GroupKey::new(env_id, service_id),
                    orphaned: if env_gone {
                        Some(Orphan::Environment)
                    } else if service_gone {
                        Some(Orphan::Service)
                    } else {
                        None
                    },
                }
            })
            .collect()
    }

    fn selected_group(&self) -> Option<PendingGroup> {
        let groups = self.pending_groups();
        let i = position(&groups, self.selection.pending.as_ref(), |g| &g.key);
        groups.into_iter().nth(i)
    }

    /// The pending store to change, copied first if a save still holds it.
    fn pending_mut(&mut self) -> &mut Pending {
        Arc::make_mut(&mut self.pending)
    }

    /// Saves the pending store as it is now.
    fn save_pending(&self) -> Command {
        Command::SavePending(Arc::clone(&self.pending))
    }

    fn open_pending(&mut self, out: &mut Vec<Command>) {
        self.selection.pending = None;
        self.push(View::Pending(PendingView::default()), out);
    }

    /// The active theme (the previewed one while the picker is open).
    pub fn theme(&self) -> &'static Theme {
        &THEMES[self.theme]
    }

    /// Highlighted row of the top table view, or None for an empty table or the log view.
    pub fn selected_index(&self) -> Option<usize> {
        self.row_count_and_selected()
            .and_then(|(len, i)| (len > 0).then_some(i))
    }

    // ---- data events ----

    fn on_data(&mut self, data: DataEvent, now: Instant, out: &mut Vec<Command>) {
        match data {
            DataEvent::Workspaces(result) => {
                // The initial load is requested by `new`, which has no clock.
                self.requested.workspaces.get_or_insert(now);
                self.workspaces.finish(result, now);
                if !self.started && self.workspaces.value.is_some() {
                    self.started = true;
                    self.resolve_start_project(now, out);
                }
            }
            DataEvent::Environments { project_id, result } => {
                if self.project_id() != Some(project_id.as_str()) {
                    return;
                }
                self.environments.finish(result, now);
                self.ensure_environment(now, out);
            }
            DataEvent::Services {
                project_id,
                env_id,
                result,
            } => {
                if self.project_id() != Some(project_id.as_str())
                    || self.env_id() != Some(env_id.as_str())
                {
                    return;
                }
                let ok = result.is_ok();
                self.services.finish(result, now);
                if ok && self.kind() == Kind::Services {
                    out.push(self.watch_command());
                }
            }
            DataEvent::Deployments {
                service_id,
                env_id,
                result,
            } => {
                if self.env_id() != Some(env_id.as_str())
                    || self.view_service_id(Kind::Deployments) != Some(service_id.as_str())
                {
                    return;
                }
                self.deployments.finish(result, now);
            }
            DataEvent::Variables {
                env_id,
                service_id,
                purpose,
                result,
                sealed,
            } => match (purpose, result.map(Secret::into_inner)) {
                (VarsPurpose::View, result) => {
                    if self.env_id() != Some(env_id.as_str())
                        || self.view_service_id(Kind::Variables) != Some(service_id.as_str())
                    {
                        return;
                    }
                    self.variables.finish(result, now);
                    match sealed {
                        Some(Ok(names)) => {
                            if let Some(View::Variables(v)) = self.stack.last_mut() {
                                v.sealed = names;
                            }
                        }
                        Some(Err(e)) => {
                            self.message = Some(format!("sealed status unknown: {e}"));
                        }
                        None => {}
                    }
                }
                (VarsPurpose::Deploy, result) => self.on_deploy_check(env_id, service_id, result),
                (VarsPurpose::Reconcile, result) => {
                    self.on_reconcile(env_id, service_id, result, out)
                }
            },
            DataEvent::Deploy {
                env_id,
                service_id,
                step,
            } => self.on_deploy_step(env_id, service_id, step, now, out),
            DataEvent::DeployedLatest {
                env_id,
                service_id,
                outcome,
            } => self.on_deploy_latest(env_id, service_id, outcome, now, out),
            DataEvent::Branches {
                env_id,
                service_id,
                result,
            } => self.on_branches(&env_id, &service_id, result),
            DataEvent::SaveFailed(message) => self.message = Some(message),
            DataEvent::Edited { name, result } => match result {
                Ok(value) => self.set_variable(&name, value.into_inner(), out),
                Err(message) => self.message = Some(message),
            },
            DataEvent::StatusChanged {
                deployment_id,
                status,
            } => self.on_status_changed(&deployment_id, status, now, out),
            DataEvent::Log { session, event } => self.on_log(session, event),
            DataEvent::Budget {
                interval,
                blocked_until,
            } => {
                self.backstop = interval;
                self.blocked_until = blocked_until;
            }
            DataEvent::LiveUpdates { ok } => self.live_updates = ok,
        }
    }

    fn resolve_start_project(&mut self, now: Instant, out: &mut Vec<Command>) {
        let configured = self.prefs.project.clone();
        let by_pref = configured
            .as_deref()
            .and_then(|want| self.find_project(|p| p.id == want || same_name(&p.name, want)));
        if by_pref.is_none() {
            if let Some(want) = &configured {
                self.message = Some(format!("project {want:?} not found"));
            }
        }
        let remembered = self.prefs.remembered.last_project_id.clone();
        let found = by_pref.or_else(|| {
            remembered
                .as_deref()
                .and_then(|id| self.find_project(|p| p.id == id))
        });
        if let Some((workspace_id, project)) = found {
            self.open_project(workspace_id, project, now, out);
        }
    }

    fn find_project(&self, pred: impl Fn(&ProjectRef) -> bool) -> Option<(String, ProjectRef)> {
        self.ordered_workspaces().into_iter().find_map(|w| {
            w.projects
                .iter()
                .find(|p| pred(p))
                .map(|p| (w.id.clone(), p.clone()))
        })
    }

    /// Picks an environment once they arrive, unless the current one is still listed.
    fn ensure_environment(&mut self, now: Instant, out: &mut Vec<Command>) {
        let Some(envs) = self.environments.value.as_deref() else {
            return;
        };
        if let Some(current) = &self.environment {
            if envs.iter().any(|e| e.id == current.id) {
                return;
            }
        }
        if let Some(env) = self.pick_environment(envs).cloned() {
            self.set_environment(env, now, out);
        }
    }

    /// Configured (id or name), then remembered id, then `production`, then the first.
    fn pick_environment<'a>(&self, envs: &'a [Environment]) -> Option<&'a Environment> {
        let configured = self.prefs.environment.as_deref();
        let remembered = self.prefs.remembered.last_environment_id.as_deref();
        configured
            .and_then(|want| {
                envs.iter()
                    .find(|e| e.id == want || same_name(&e.name, want))
            })
            .or_else(|| remembered.and_then(|id| envs.iter().find(|e| e.id == id)))
            .or_else(|| envs.iter().find(|e| same_name(&e.name, "production")))
            .or_else(|| envs.first())
    }

    fn set_environment(&mut self, env: Environment, now: Instant, out: &mut Vec<Command>) {
        let remembered = Remembered {
            last_project_id: self.project_id().map(String::from),
            last_environment_id: Some(env.id.clone()),
            ..self.prefs.remembered.clone()
        };
        self.environment = Some(env);
        self.services = Loaded::default();
        self.selection.service = None;
        self.load_services(now, out);
        self.prefs.remembered = remembered.clone();
        out.push(Command::SaveRemembered(remembered));
    }

    fn on_status_changed(
        &mut self,
        deployment_id: &str,
        status: DeploymentStatus,
        now: Instant,
        out: &mut Vec<Command>,
    ) {
        let mut matched = false;
        let latest = self
            .services
            .value
            .iter_mut()
            .flatten()
            .filter_map(|s| s.latest.as_mut());
        for d in latest.filter(|d| d.id == deployment_id) {
            d.status = status.clone();
            matched = true;
        }
        let history = self.deployments.value.iter_mut().flatten();
        for d in history.filter(|d| d.id == deployment_id) {
            d.status = status.clone();
        }
        if matched && status.is_terminal() {
            self.load_services(now, out);
        }
    }

    fn on_log(&mut self, session: u64, event: FollowEvent) {
        let Some(View::Logs(logs)) = self.stack.last_mut() else {
            return;
        };
        if logs.session != session {
            return;
        }
        match event {
            FollowEvent::Lines(lines) => {
                if logs.follow {
                    logs.buffer.extend(lines);
                } else {
                    // Keep a paused view anchored: new lines land below it (approximately;
                    // backfilled older lines would shift it slightly).
                    let before = logs.visible_len();
                    logs.buffer.extend(lines);
                    logs.scroll += logs.visible_len().saturating_sub(before);
                }
            }
            FollowEvent::State(state) => logs.state = state,
        }
    }

    // ---- backstop and loads ----

    fn on_tick(&mut self, now: Instant, out: &mut Vec<Command>) {
        if self.blocked_until.is_some_and(|until| until > now) {
            return;
        }
        let (fetched_at, requested, loading) = match self.kind() {
            Kind::Projects => (
                self.workspaces.fetched_at,
                self.requested.workspaces,
                self.workspaces.loading,
            ),
            Kind::Services | Kind::Instances if self.environment.is_none() => (
                self.environments.fetched_at,
                self.requested.environments,
                self.environments.loading,
            ),
            Kind::Services | Kind::Instances => (
                self.services.fetched_at,
                self.requested.services,
                self.services.loading,
            ),
            Kind::Deployments => (
                self.deployments.fetched_at,
                self.requested.deployments,
                self.deployments.loading,
            ),
            Kind::Variables => (
                self.variables.fetched_at,
                self.requested.variables,
                self.variables.loading,
            ),
            Kind::Pending | Kind::Logs => return,
        };
        let last = fetched_at.max(requested);
        if !loading && last.is_some_and(|t| t + self.backstop <= now) {
            self.reload_current(now, out);
        }
    }

    /// The top view's load command (backstop and Ctrl-R). The log view streams
    /// instead; Ctrl-R there reopens its stream (the backstop skips Logs).
    fn reload_current(&mut self, now: Instant, out: &mut Vec<Command>) {
        match self.kind() {
            Kind::Projects => self.load_workspaces(now, out),
            Kind::Services | Kind::Instances if self.environment.is_none() => {
                self.load_environments(now, out)
            }
            Kind::Services | Kind::Instances => self.load_services(now, out),
            Kind::Deployments => {
                if let Some(id) = self.view_service_id(Kind::Deployments).map(String::from) {
                    self.load_deployments(id, now, out);
                }
            }
            Kind::Variables => {
                if let Some(id) = self.view_service_id(Kind::Variables).map(String::from) {
                    self.load_variables(id, now, out);
                }
            }
            Kind::Pending => {}
            Kind::Logs => self.reopen_logs(None, out),
        }
    }

    fn load_workspaces(&mut self, now: Instant, out: &mut Vec<Command>) {
        self.workspaces.loading = true;
        self.requested.workspaces = Some(now);
        out.push(Command::LoadWorkspaces);
    }

    fn load_environments(&mut self, now: Instant, out: &mut Vec<Command>) {
        let Some(project_id) = self.project_id().map(String::from) else {
            return;
        };
        self.environments.loading = true;
        self.requested.environments = Some(now);
        out.push(Command::LoadEnvironments { project_id });
    }

    fn load_services(&mut self, now: Instant, out: &mut Vec<Command>) {
        let (Some(project_id), Some(env_id)) = (self.project_id(), self.env_id()) else {
            return;
        };
        out.push(Command::LoadServices {
            project_id: project_id.to_string(),
            env_id: env_id.to_string(),
        });
        self.services.loading = true;
        self.requested.services = Some(now);
    }

    fn load_deployments(&mut self, service_id: String, now: Instant, out: &mut Vec<Command>) {
        let (Some(project_id), Some(env_id)) = (self.project_id(), self.env_id()) else {
            return;
        };
        out.push(Command::LoadDeployments {
            project_id: project_id.to_string(),
            env_id: env_id.to_string(),
            service_id,
        });
        self.deployments.loading = true;
        self.requested.deployments = Some(now);
    }

    fn load_variables(&mut self, service_id: String, now: Instant, out: &mut Vec<Command>) {
        let (Some(project_id), Some(env_id)) = (self.project_id(), self.env_id()) else {
            return;
        };
        out.push(Command::LoadVariables {
            project_id: project_id.to_string(),
            env_id: env_id.to_string(),
            service_id,
            purpose: VarsPurpose::View,
        });
        self.variables.loading = true;
        self.requested.variables = Some(now);
    }

    // ---- navigation ----

    fn open_project(
        &mut self,
        workspace_id: String,
        project: ProjectRef,
        now: Instant,
        out: &mut Vec<Command>,
    ) {
        self.workspace_id = Some(workspace_id);
        self.selection.project = Some(project.id.clone());
        let reopening = self.project_id() == Some(project.id.as_str());
        if reopening && self.environment.is_some() {
            self.push(View::Services, out);
            self.load_services(now, out);
            return;
        }
        self.project = Some(project);
        self.environment = None;
        self.environments = Loaded::default();
        self.services = Loaded::default();
        self.selection.service = None;
        self.push(View::Services, out);
        self.load_environments(now, out);
    }

    fn open_deployments(&mut self, service_id: String, now: Instant, out: &mut Vec<Command>) {
        self.deployments = Loaded::default();
        self.selection.deployment = None;
        self.push(
            View::Deployments {
                service_id: service_id.clone(),
            },
            out,
        );
        self.load_deployments(service_id, now, out);
    }

    fn open_variables(&mut self, service_id: String, now: Instant, out: &mut Vec<Command>) {
        self.variables = Loaded::default();
        self.selection.variable = None;
        self.push(
            View::Variables(VarsView {
                service_id: service_id.clone(),
                revealed: BTreeSet::new(),
                reveal_all: false,
                sealed: BTreeSet::new(),
                anchor: None,
            }),
            out,
        );
        self.load_variables(service_id, now, out);
    }

    /// Logs of the selected service's latest deployment (the failed one, if it failed).
    fn open_latest_logs(&mut self, out: &mut Vec<Command>) {
        let Some(svc) = self.selected_service() else {
            self.message = Some("no service selected".into());
            return;
        };
        let Some(latest) = &svc.latest else {
            self.message = Some(format!("{} has no deployments", svc.service_name));
            return;
        };
        let (id, name, kind) = (
            latest.id.clone(),
            svc.service_name.clone(),
            default_log_kind(latest),
        );
        self.open_logs(id, name, kind, None, out);
    }

    fn open_logs(
        &mut self,
        deployment_id: String,
        service_name: String,
        kind: LogKind,
        replica: Option<String>,
        out: &mut Vec<Command>,
    ) {
        let Some(env_id) = self.env_id().map(String::from) else {
            return;
        };
        self.next_session += 1;
        let session = self.next_session;
        self.push(
            View::Logs(LogsView {
                session,
                deployment_id: deployment_id.clone(),
                service_name,
                kind,
                replica,
                buffer: LogBuffer::new(LOG_CAP),
                follow: true,
                scroll: 0,
                filter: None,
                state: StreamState::Connecting,
            }),
            out,
        );
        out.push(Command::OpenLogs {
            session,
            env_id,
            deployment_id,
            kind,
        });
    }

    /// `b` in the log view: close the session and reopen with the other log kind.
    fn toggle_log_kind(&mut self, out: &mut Vec<Command>) {
        let other = match self.logs_mut().map(|l| l.kind) {
            Some(LogKind::Build) => LogKind::Runtime,
            Some(LogKind::Runtime) => LogKind::Build,
            None => return,
        };
        self.reopen_logs(Some(other), out);
    }

    /// Closes the log view's session and opens a new one for the same
    /// deployment, as `kind` (or the current kind), starting from empty.
    fn reopen_logs(&mut self, kind: Option<LogKind>, out: &mut Vec<Command>) {
        let Some(env_id) = self.env_id().map(String::from) else {
            return;
        };
        let session = self.next_session + 1;
        let Some(View::Logs(logs)) = self.stack.last_mut() else {
            return;
        };
        self.next_session = session;
        out.push(Command::CloseLogs {
            session: logs.session,
        });
        logs.session = session;
        logs.kind = kind.unwrap_or(logs.kind);
        logs.buffer = LogBuffer::new(LOG_CAP);
        logs.follow = true;
        logs.scroll = 0;
        logs.state = StreamState::Connecting;
        out.push(Command::OpenLogs {
            session,
            env_id,
            deployment_id: logs.deployment_id.clone(),
            kind: logs.kind,
        });
    }

    /// Makes Services the top view (truncating or pushing). False when no project is open.
    fn goto_services(&mut self, out: &mut Vec<Command>) -> bool {
        if self.project.is_none() {
            self.message = Some("no project selected".into());
            return false;
        }
        match self.stack.iter().position(|v| matches!(v, View::Services)) {
            Some(i) => self.truncate(i + 1, out),
            None => {
                self.truncate(1, out);
                self.push(View::Services, out);
            }
        }
        true
    }

    fn push(&mut self, view: View, out: &mut Vec<Command>) {
        self.leave_top(out);
        self.stack.push(view);
        self.enter_top(out);
    }

    fn pop(&mut self, out: &mut Vec<Command>) {
        let len = self.stack.len();
        if len > 1 {
            self.truncate(len - 1, out);
        }
    }

    fn truncate(&mut self, len: usize, out: &mut Vec<Command>) {
        if self.stack.len() <= len.max(1) {
            return;
        }
        self.leave_top(out);
        self.stack.truncate(len.max(1));
        self.enter_top(out);
    }

    /// Only the top view subscribes: leaving Services stops its watch, leaving Logs closes the stream.
    fn leave_top(&mut self, out: &mut Vec<Command>) {
        self.table_filter = None;
        match self.stack.last() {
            Some(View::Services) => out.push(Command::WatchDeployments { ids: Vec::new() }),
            Some(View::Logs(logs)) => out.push(Command::CloseLogs {
                session: logs.session,
            }),
            _ => {}
        }
    }

    fn enter_top(&mut self, out: &mut Vec<Command>) {
        if self.kind() == Kind::Services && self.services.value.is_some() {
            out.push(self.watch_command());
        }
    }

    fn watch_command(&self) -> Command {
        let ids = self
            .services
            .value
            .iter()
            .flatten()
            .filter_map(|s| s.latest.as_ref().map(|d| d.id.clone()))
            .collect();
        Command::WatchDeployments { ids }
    }

    // ---- selection ----

    fn move_selection(&mut self, delta: isize) {
        let Some((len, current)) = self.row_count_and_selected().filter(|(len, _)| *len > 0) else {
            return;
        };
        let next = current.saturating_add_signed(delta).min(len - 1);
        match self.kind() {
            Kind::Projects => self.selection.project = Some(self.project_rows()[next].1.id.clone()),
            Kind::Services => {
                self.selection.service = Some(self.service_rows()[next].service_id.clone());
            }
            Kind::Deployments => {
                self.selection.deployment =
                    Some(self.deployment_table()[next].deployment.id.clone());
            }
            Kind::Instances => {
                self.selection.instance = Some(self.instance_rows()[next].1.id.clone())
            }
            Kind::Variables => {
                self.selection.variable = Some(self.variable_rows()[next].name.to_string());
            }
            Kind::Pending => {
                self.selection.pending = Some(self.pending_groups().swap_remove(next).key);
            }
            Kind::Logs => {}
        }
    }

    /// The top table view's row count and its highlighted row; `None` for the log view.
    fn row_count_and_selected(&self) -> Option<(usize, usize)> {
        fn of<T, K: PartialEq + ?Sized>(
            rows: &[T],
            selected: Option<&K>,
            id: impl Fn(&T) -> &K,
        ) -> Option<(usize, usize)> {
            Some((rows.len(), position(rows, selected, id)))
        }
        let sel = &self.selection;
        match self.kind() {
            Kind::Projects => of(&self.project_rows(), sel.project.as_deref(), |(_, p)| {
                p.id.as_str()
            }),
            Kind::Services => of(&self.service_rows(), sel.service.as_deref(), |s| {
                s.service_id.as_str()
            }),
            Kind::Deployments => of(&self.deployment_table(), sel.deployment.as_deref(), |r| {
                r.deployment.id.as_str()
            }),
            Kind::Instances => of(&self.instance_rows(), sel.instance.as_deref(), |(_, i)| {
                i.id.as_str()
            }),
            Kind::Variables => of(&self.variable_rows(), sel.variable.as_deref(), |r| r.name),
            Kind::Pending => of(&self.pending_groups(), sel.pending.as_ref(), |g| &g.key),
            Kind::Logs => None,
        }
    }

    fn selected_project(&self) -> Option<(String, ProjectRef)> {
        let rows = self.project_rows();
        let selected = self.selection.project.as_deref();
        let (w, p) = rows.get(position(&rows, selected, |(_, p)| p.id.as_str()))?;
        Some((w.id.clone(), (*p).clone()))
    }

    /// The selected service, whatever the top view (used by `l` and `:deploys`/`:logs`).
    fn selected_service(&self) -> Option<&ServiceInstance> {
        let rows = self.service_rows();
        let selected = self.selection.service.as_deref();
        rows.get(position(&rows, selected, |s| s.service_id.as_str()))
            .copied()
    }

    fn selected_deployment(&self) -> Option<&DeploymentSummary> {
        let rows = self.deployment_table();
        let selected = self.selection.deployment.as_deref();
        let index = position(&rows, selected, |r| r.deployment.id.as_str());
        rows.get(index).map(|r| r.deployment)
    }

    fn selected_instance(&self) -> Option<(String, String)> {
        let rows = self.instance_rows();
        let selected = self.selection.instance.as_deref();
        let (d, i) = rows.get(position(&rows, selected, |(_, i)| i.id.as_str()))?;
        Some((d.id.clone(), i.id.clone()))
    }

    // ---- small helpers ----

    fn kind(&self) -> Kind {
        match self.stack.last() {
            Some(View::Projects) | None => Kind::Projects,
            Some(View::Services) => Kind::Services,
            Some(View::Deployments { .. }) => Kind::Deployments,
            Some(View::Instances { .. }) => Kind::Instances,
            Some(View::Logs(_)) => Kind::Logs,
            Some(View::Variables(_)) => Kind::Variables,
            Some(View::Pending(_)) => Kind::Pending,
        }
    }

    fn logs_mut(&mut self) -> Option<&mut LogsView> {
        match self.stack.last_mut() {
            Some(View::Logs(logs)) => Some(logs),
            _ => None,
        }
    }

    /// The service id of the nearest Deployments or Instances view in the stack.
    fn view_service_id(&self, kind: Kind) -> Option<&str> {
        self.stack.iter().rev().find_map(|v| match (v, kind) {
            (View::Deployments { service_id }, Kind::Deployments)
            | (View::Instances { service_id }, Kind::Instances) => Some(service_id.as_str()),
            (View::Variables(v), Kind::Variables) => Some(v.service_id.as_str()),
            _ => None,
        })
    }

    fn service(&self, id: &str) -> Option<&ServiceInstance> {
        self.services
            .value
            .iter()
            .flatten()
            .find(|s| s.service_id == id)
    }

    fn project_id(&self) -> Option<&str> {
        self.project.as_ref().map(|p| p.id.as_str())
    }

    fn env_id(&self) -> Option<&str> {
        self.environment.as_ref().map(|e| e.id.as_str())
    }

    fn ordered_workspaces(&self) -> Vec<&Workspace> {
        let all = self.workspaces.value.iter().flatten();
        let preferred = |w: &Workspace| {
            self.prefs
                .workspace
                .as_deref()
                .is_some_and(|want| w.id == want || same_name(&w.name, want))
        };
        let (mut first, rest): (Vec<&Workspace>, Vec<&Workspace>) = all.partition(|w| preferred(w));
        first.extend(rest);
        first
    }

    /// Table filter: the text still being typed after `/` narrows the rows live,
    /// otherwise the applied filter does.
    fn passes_filter(&self, fields: &[&str]) -> bool {
        let typing = match &self.modal {
            Some(Modal::Filter(text)) if self.kind() != Kind::Logs => Some(text.as_str()),
            _ => None,
        };
        let Some(filter) = typing.or(self.table_filter.as_deref()) else {
            return true;
        };
        let needle = filter.to_lowercase();
        fields.iter().any(|f| f.to_lowercase().contains(&needle))
    }
}

impl<T> Loaded<T> {
    fn finish(&mut self, result: Result<T, String>, now: Instant) {
        self.loading = false;
        match result {
            Ok(value) => {
                self.value = Some(value);
                self.fetched_at = Some(now);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }
}

impl LogsView {
    fn visible_len(&self) -> usize {
        self.buffer
            .visible(self.filter.as_deref(), self.replica.as_deref())
            .len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Projects,
    Services,
    Deployments,
    Instances,
    Variables,
    Pending,
    Logs,
}

/// One line of the Variables view.
pub struct VarRow<'a> {
    pub name: &'a str,
    /// The value on Railway, if the variable exists there.
    pub current: Option<&'a str>,
    pub pending: Option<&'a PendingVar>,
    /// Railway holds the value but never reveals it.
    pub sealed: bool,
}

/// Values must never reach a log or panic message, so `Variables` is left out.
impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("stack", &self.stack)
            .field("project", &self.project)
            .field("environment", &self.environment)
            .field("selection", &self.selection)
            .finish_non_exhaustive()
    }
}

/// Index of the row whose `id` is `selected`, or 0 when it is unset or filtered out.
fn position<T, K: PartialEq + ?Sized>(
    rows: &[T],
    selected: Option<&K>,
    id: impl Fn(&T) -> &K,
) -> usize {
    selected
        .and_then(|s| rows.iter().position(|r| id(r) == s))
        .unwrap_or(0)
}

/// Names equal ignoring case, compared char by char without allocating.
fn same_name(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

/// The remembered pick, then the configured theme, then the default; with a
/// notice when the configured name matches no theme.
fn start_theme(prefs: &StartPrefs) -> (usize, Option<String>) {
    if let Some(i) = prefs.remembered.theme.as_deref().and_then(theme::find) {
        return (i, None);
    }
    match prefs.theme.as_deref() {
        Some(name) => match theme::find(name) {
            Some(i) => (i, None),
            None => (0, Some(format!("unknown theme in config: {name}"))),
        },
        None => (0, None),
    }
}

#[cfg(test)]
mod tests;
