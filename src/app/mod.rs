//! App state machine: `update(event, now) -> commands`. No IO, no rendering.

mod deploy;
mod keys;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::KeyEvent;

use crate::budget::{backstop_interval, DEFAULT_HOURLY_LIMIT};
use crate::logs::buffer::{LogBuffer, LOG_CAP};
use crate::logs::follow::{FollowEvent, StreamState};
use crate::model::{
    DeploymentStatus, DeploymentSummary, Environment, Instance, LogKind, ProjectRef,
    ServiceInstance, Variables, Workspace,
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
}

/// The review of every pending change in the open project.
#[derive(Debug, Default)]
pub struct PendingView {
    pub reveal_all: bool,
}

/// One service's pending changes in one environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGroup {
    pub env_id: String,
    pub service_id: String,
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

impl PendingGroup {
    /// The selection key of this group.
    pub fn id(&self) -> String {
        format!("{}/{}", self.env_id, self.service_id)
    }
}

/// A question waiting for `y`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    Discard { env_id: String, service_id: String },
    Deploy(DeployConfirm),
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
        /// Sent, but Railway may or may not have applied it.
        unknown: bool,
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
        result: Result<Variables, String>,
    },
    Deploy {
        env_id: String,
        service_id: String,
        step: DeployStep,
    },
    /// Saving the pending file failed; the text never holds a value.
    SaveFailed(String),
    /// The external editor finished: the new value, or why it could not run.
    Edited {
        name: String,
        result: Result<String, String>,
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

#[derive(Clone, PartialEq, Eq)]
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
    SavePending(Pending),
    /// Writes one group's unapplied changes, then redeploys: upsert (skipped
    /// when `upserts` is empty), each delete, then the redeploy, stopping at
    /// the first failure. `redeploy_only` skips the writes.
    DeployService {
        project_id: String,
        env_id: String,
        service_id: String,
        upserts: Variables,
        deletes: Vec<String>,
        redeploy_only: bool,
    },
    /// Handled by the main loop, not the worker: edit `initial` in `$VISUAL`/`$EDITOR`.
    EditExternal {
        name: String,
        initial: String,
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
#[derive(Clone, Default, PartialEq, Eq)]
pub struct VarInput {
    pub name: Option<String>,
    pub text: String,
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
    pub pending: Option<String>,
}

pub struct App {
    pub prefs: StartPrefs,
    /// Never empty; the root is always `View::Projects`.
    pub stack: Vec<View>,
    pub workspace_id: Option<String>,
    pub project: Option<ProjectRef>,
    pub environment: Option<Environment>,
    pub workspaces: Loaded<Vec<Workspace>>,
    pub environments: Loaded<Vec<Environment>>,
    pub services: Loaded<Vec<ServiceInstance>>,
    /// History for the service of the `Deployments` view in the stack.
    pub deployments: Loaded<Vec<DeploymentSummary>>,
    /// Railway's variables for the service of the open `Variables` view.
    pub variables: Loaded<Variables>,
    /// Edits not deployed yet, for every service.
    pub pending: Pending,
    pub selection: Selection,
    /// The service under the cursor in the Services view and when it got there,
    /// so a long name scrolls from its start.
    pub service_focus: Option<(String, Instant)>,
    /// The variable name or value being typed in the Variables view.
    pub var_input: Option<VarInput>,
    /// The question being asked, answered with `y` or any other key.
    pub confirm: Option<Confirm>,
    /// Deploys sent to the worker and not finished, by (env id, service id).
    deploys: BTreeMap<(String, String), deploy::Sent>,
    /// Ctrl-C was pressed once while deploys are in flight; a second one quits.
    /// Cleared once no deploy is in flight.
    force_quit_armed: bool,
    /// Applied name filter for the current table view (cleared on view change).
    pub table_filter: Option<String>,
    /// Filter text being typed after `/` (table or logs, by the top view).
    pub filter_input: Option<String>,
    /// Command text being typed after `:`.
    pub command_line: Option<String>,
    /// Highlighted index into `environments` while the picker is open.
    pub env_picker: Option<usize>,
    /// Index into `THEMES` of the active (or previewed) theme.
    pub theme: usize,
    /// While the theme picker is open: the theme to restore on Esc.
    pub theme_picker: Option<usize>,
    pub help: bool,
    /// One-shot notice (unknown command, nothing to open); cleared on the next key.
    pub message: Option<String>,
    pub backstop: Duration,
    pub blocked_until: Option<Instant>,
    pub live_updates: bool,
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
        let pending = prefs.pending.clone();
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
            var_input: None,
            confirm: None,
            deploys: BTreeMap::new(),
            force_quit_armed: false,
            table_filter: None,
            filter_input: None,
            command_line: None,
            env_picker: None,
            theme,
            theme_picker: None,
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
        if self.deploys.is_empty() {
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
    pub fn deployment_table(&self) -> Vec<DeploymentRow> {
        let Some(service_id) = self.view_service_id(Kind::Deployments) else {
            return Vec::new();
        };
        let history = self.deployments.value.as_deref().unwrap_or_default();
        let rows = match self.service(service_id) {
            Some(svc) => deployment_rows(svc, history),
            None => history
                .iter()
                .map(|d| DeploymentRow {
                    deployment: d.clone(),
                    tag: None,
                })
                .collect(),
        };
        rows.into_iter()
            .filter(|r| {
                let d = &r.deployment;
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

    /// Railway's variables for the open service merged with its pending edits,
    /// by name, narrowed by the name filter (never the values).
    pub fn variable_rows(&self) -> Vec<VarRow<'_>> {
        let Some(service_id) = self.view_service_id(Kind::Variables) else {
            return Vec::new();
        };
        let mut rows: BTreeMap<&str, VarRow> = BTreeMap::new();
        for (name, value) in self.variables.value.iter().flatten() {
            let row = VarRow {
                name,
                current: Some(value),
                pending: None,
            };
            rows.insert(name, row);
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
                })
                .pending = Some(change);
        }
        rows.into_values()
            .filter(|r| self.passes_filter(&[r.name]))
            .collect()
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
                    env_id: env_id.to_string(),
                    service_id: service_id.to_string(),
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
        let ids: Vec<String> = groups.iter().map(|g| g.id()).collect();
        let i = position(&ids, self.selection.pending.as_deref());
        groups.into_iter().nth(i)
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
        let ids = self.row_ids();
        if ids.is_empty() {
            return None;
        }
        let selected = self.selection_slot(self.kind())?;
        Some(position(&ids, selected.as_deref()))
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
            } => match purpose {
                VarsPurpose::View => {
                    if self.env_id() != Some(env_id.as_str())
                        || self.view_service_id(Kind::Variables) != Some(service_id.as_str())
                    {
                        return;
                    }
                    self.variables.finish(result, now);
                }
                VarsPurpose::Deploy => self.on_deploy_check(env_id, service_id, result),
                VarsPurpose::Reconcile => self.on_reconcile(env_id, service_id, result, out),
            },
            DataEvent::Deploy {
                env_id,
                service_id,
                step,
            } => self.on_deploy_step(env_id, service_id, step, now, out),
            DataEvent::SaveFailed(message) => self.message = Some(message),
            DataEvent::Edited { name, result } => match result {
                Ok(value) => self.set_variable(&name, value, out),
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
        let terminal = matches!(
            status,
            DeploymentStatus::Success
                | DeploymentStatus::Failed
                | DeploymentStatus::Crashed
                | DeploymentStatus::Removed
        );
        if matched && terminal {
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
        let ids = self.row_ids();
        let Some(current) = self.selected_index() else {
            return;
        };
        let next = current.saturating_add_signed(delta).min(ids.len() - 1);
        let kind = self.kind();
        if let Some(slot) = self.selection_slot_mut(kind) {
            *slot = Some(ids[next].clone());
        }
    }

    fn row_ids(&self) -> Vec<String> {
        match self.kind() {
            Kind::Projects => self
                .project_rows()
                .iter()
                .map(|(_, p)| p.id.clone())
                .collect(),
            Kind::Services => self
                .service_rows()
                .iter()
                .map(|s| s.service_id.clone())
                .collect(),
            Kind::Deployments => self
                .deployment_table()
                .into_iter()
                .map(|r| r.deployment.id)
                .collect(),
            Kind::Instances => self
                .instance_rows()
                .iter()
                .map(|(_, i)| i.id.clone())
                .collect(),
            Kind::Variables => self
                .variable_rows()
                .iter()
                .map(|r| r.name.to_string())
                .collect(),
            Kind::Pending => self.pending_groups().iter().map(|g| g.id()).collect(),
            Kind::Logs => Vec::new(),
        }
    }

    fn selection_slot(&self, kind: Kind) -> Option<&Option<String>> {
        match kind {
            Kind::Projects => Some(&self.selection.project),
            Kind::Services => Some(&self.selection.service),
            Kind::Deployments => Some(&self.selection.deployment),
            Kind::Instances => Some(&self.selection.instance),
            Kind::Variables => Some(&self.selection.variable),
            Kind::Pending => Some(&self.selection.pending),
            Kind::Logs => None,
        }
    }

    fn selection_slot_mut(&mut self, kind: Kind) -> Option<&mut Option<String>> {
        match kind {
            Kind::Projects => Some(&mut self.selection.project),
            Kind::Services => Some(&mut self.selection.service),
            Kind::Deployments => Some(&mut self.selection.deployment),
            Kind::Instances => Some(&mut self.selection.instance),
            Kind::Variables => Some(&mut self.selection.variable),
            Kind::Pending => Some(&mut self.selection.pending),
            Kind::Logs => None,
        }
    }

    fn selected_project(&self) -> Option<(String, ProjectRef)> {
        let rows = self.project_rows();
        let ids: Vec<String> = rows.iter().map(|(_, p)| p.id.clone()).collect();
        let (w, p) = rows.get(position(&ids, self.selection.project.as_deref()))?;
        Some((w.id.clone(), (*p).clone()))
    }

    /// The selected service, whatever the top view (used by `l` and `:deploys`/`:logs`).
    fn selected_service(&self) -> Option<&ServiceInstance> {
        let rows = self.service_rows();
        let ids: Vec<String> = rows.iter().map(|s| s.service_id.clone()).collect();
        rows.get(position(&ids, self.selection.service.as_deref()))
            .copied()
    }

    fn selected_deployment(&self) -> Option<DeploymentSummary> {
        let rows = self.deployment_table();
        let ids: Vec<String> = rows.iter().map(|r| r.deployment.id.clone()).collect();
        let index = position(&ids, self.selection.deployment.as_deref());
        rows.into_iter().nth(index).map(|r| r.deployment)
    }

    fn selected_instance(&self) -> Option<(String, String)> {
        let rows = self.instance_rows();
        let ids: Vec<String> = rows.iter().map(|(_, i)| i.id.clone()).collect();
        let (d, i) = rows.get(position(&ids, self.selection.instance.as_deref()))?;
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
        let typing = self
            .filter_input
            .as_deref()
            .filter(|_| self.kind() != Kind::Logs);
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

/// Written by hand so a loaded `Variables` prints as a count, not its values.
impl fmt::Debug for DataEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataEvent::Workspaces(r) => f.debug_tuple("Workspaces").field(r).finish(),
            DataEvent::Environments { project_id, result } => f
                .debug_struct("Environments")
                .field("project_id", project_id)
                .field("result", result)
                .finish(),
            DataEvent::Services {
                project_id,
                env_id,
                result,
            } => f
                .debug_struct("Services")
                .field("project_id", project_id)
                .field("env_id", env_id)
                .field("result", result)
                .finish(),
            DataEvent::Deployments {
                service_id,
                env_id,
                result,
            } => f
                .debug_struct("Deployments")
                .field("service_id", service_id)
                .field("env_id", env_id)
                .field("result", result)
                .finish(),
            DataEvent::Variables {
                env_id,
                service_id,
                purpose,
                result,
            } => {
                let shown = match result {
                    Ok(vars) => format!("Ok({} variables)", vars.len()),
                    Err(e) => format!("Err({e:?})"),
                };
                f.debug_struct("Variables")
                    .field("env_id", env_id)
                    .field("service_id", service_id)
                    .field("purpose", purpose)
                    .field("result", &format_args!("{shown}"))
                    .finish()
            }
            DataEvent::Deploy {
                env_id,
                service_id,
                step,
            } => f
                .debug_struct("Deploy")
                .field("env_id", env_id)
                .field("service_id", service_id)
                .field("step", step)
                .finish(),
            DataEvent::SaveFailed(m) => f.debug_tuple("SaveFailed").field(m).finish(),
            DataEvent::Edited { name, result } => f
                .debug_struct("Edited")
                .field("name", name)
                .field("result", &result.as_ref().map(|_| "<redacted>"))
                .finish(),
            DataEvent::StatusChanged {
                deployment_id,
                status,
            } => f
                .debug_struct("StatusChanged")
                .field("deployment_id", deployment_id)
                .field("status", status)
                .finish(),
            DataEvent::Log { session, event } => f
                .debug_struct("Log")
                .field("session", session)
                .field("event", event)
                .finish(),
            DataEvent::Budget {
                interval,
                blocked_until,
            } => f
                .debug_struct("Budget")
                .field("interval", interval)
                .field("blocked_until", blocked_until)
                .finish(),
            DataEvent::LiveUpdates { ok } => f.debug_struct("LiveUpdates").field("ok", ok).finish(),
        }
    }
}

impl fmt::Debug for VarInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VarInput")
            .field("name", &self.name)
            .field("text", &"<redacted>")
            .finish()
    }
}

/// Written by hand so `EditExternal` never prints the value being edited.
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::LoadWorkspaces => f.write_str("LoadWorkspaces"),
            Command::LoadEnvironments { project_id } => f
                .debug_struct("LoadEnvironments")
                .field("project_id", project_id)
                .finish(),
            Command::LoadServices { project_id, env_id } => f
                .debug_struct("LoadServices")
                .field("project_id", project_id)
                .field("env_id", env_id)
                .finish(),
            Command::LoadDeployments {
                project_id,
                env_id,
                service_id,
            } => f
                .debug_struct("LoadDeployments")
                .field("project_id", project_id)
                .field("env_id", env_id)
                .field("service_id", service_id)
                .finish(),
            Command::LoadVariables {
                project_id,
                env_id,
                service_id,
                purpose,
            } => f
                .debug_struct("LoadVariables")
                .field("project_id", project_id)
                .field("env_id", env_id)
                .field("service_id", service_id)
                .field("purpose", purpose)
                .finish(),
            Command::SavePending(p) => f.debug_tuple("SavePending").field(p).finish(),
            Command::DeployService {
                project_id,
                env_id,
                service_id,
                upserts,
                deletes,
                redeploy_only,
            } => f
                .debug_struct("DeployService")
                .field("project_id", project_id)
                .field("env_id", env_id)
                .field("service_id", service_id)
                .field("upserts", &upserts.keys().collect::<Vec<_>>())
                .field("deletes", deletes)
                .field("redeploy_only", redeploy_only)
                .finish(),
            Command::EditExternal { name, .. } => f
                .debug_struct("EditExternal")
                .field("name", name)
                .field("initial", &"<redacted>")
                .finish(),
            Command::WatchDeployments { ids } => f
                .debug_struct("WatchDeployments")
                .field("ids", ids)
                .finish(),
            Command::OpenLogs {
                session,
                env_id,
                deployment_id,
                kind,
            } => f
                .debug_struct("OpenLogs")
                .field("session", session)
                .field("env_id", env_id)
                .field("deployment_id", deployment_id)
                .field("kind", kind)
                .finish(),
            Command::CloseLogs { session } => f
                .debug_struct("CloseLogs")
                .field("session", session)
                .finish(),
            Command::SaveRemembered(r) => f.debug_tuple("SaveRemembered").field(r).finish(),
            Command::Quit => f.write_str("Quit"),
        }
    }
}

/// Index of `selected` in `ids`, or 0 when it is unset or filtered out.
fn position(ids: &[String], selected: Option<&str>) -> usize {
    selected
        .and_then(|s| ids.iter().position(|id| id == s))
        .unwrap_or(0)
}

fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
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
mod tests {
    use super::*;
    use crate::model::{InstanceStatus, LogLine};
    use chrono::{TimeZone, Utc};
    use ratatui::crossterm::event::{KeyCode, KeyModifiers};

    fn key(c: char) -> AppEvent {
        AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn code(c: KeyCode) -> AppEvent {
        AppEvent::Key(KeyEvent::new(c, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> AppEvent {
        AppEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn typed(app: &mut App, text: &str, now: Instant) {
        for c in text.chars() {
            app.update(key(c), now);
        }
    }

    fn deploy(id: &str, status: DeploymentStatus) -> DeploymentSummary {
        DeploymentSummary {
            id: id.into(),
            status,
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            commit_sha: None,
            commit_message: None,
            image: None,
            instances: vec![Instance {
                id: format!("{id}-i0"),
                status: InstanceStatus::Running,
            }],
        }
    }

    fn service(
        id: &str,
        latest: DeploymentSummary,
        active: Vec<DeploymentSummary>,
    ) -> ServiceInstance {
        ServiceInstance {
            service_id: id.into(),
            service_name: format!("{id}-name"),
            num_replicas: 1,
            latest: Some(latest),
            active,
        }
    }

    /// svc-a: latest d2 FAILED while d1 still runs. svc-b: latest d3 SUCCESS.
    fn services() -> Vec<ServiceInstance> {
        let failed = DeploymentSummary {
            instances: vec![],
            ..deploy("d2", DeploymentStatus::Failed)
        };
        vec![
            service(
                "svc-a",
                failed,
                vec![deploy("d1", DeploymentStatus::Success)],
            ),
            service("svc-b", deploy("d3", DeploymentStatus::Success), vec![]),
        ]
    }

    #[test]
    fn service_focus_restarts_when_the_cursor_moves_and_clears_off_services() {
        let t0 = Instant::now();
        let mut app = app_in_services(t0);
        let t1 = t0 + Duration::from_secs(5);
        app.update(key('j'), t1);
        assert_eq!(app.service_focus, Some(("svc-b".into(), t1)));
        app.update(AppEvent::Tick(t1), t1 + Duration::from_secs(3));
        assert_eq!(app.service_focus, Some(("svc-b".into(), t1)));
        app.update(code(KeyCode::Esc), t1);
        assert_eq!(app.service_focus, None);
    }

    fn workspaces() -> Vec<Workspace> {
        let ws = |id: &str, name: &str, pid: &str, pname: &str| Workspace {
            id: id.into(),
            name: name.into(),
            projects: vec![ProjectRef {
                id: pid.into(),
                name: pname.into(),
            }],
        };
        vec![
            ws("w1", "Personal", "p1", "Alpha"),
            ws("w2", "Team", "p2", "Beta"),
        ]
    }

    fn envs() -> Vec<Environment> {
        let env = |id: &str, name: &str| Environment {
            id: id.into(),
            name: name.into(),
        };
        vec![env("e-stg", "staging"), env("e-prod", "production")]
    }

    fn line(sec: u32, msg: &str) -> LogLine {
        LogLine {
            timestamp: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, sec).unwrap(),
            message: msg.into(),
            attributes: vec![],
        }
    }

    fn data(app: &mut App, d: DataEvent, now: Instant) -> Vec<Command> {
        app.update(AppEvent::Data(d), now)
    }

    fn envs_event(project_id: &str) -> DataEvent {
        DataEvent::Environments {
            project_id: project_id.into(),
            result: Ok(envs()),
        }
    }

    fn services_event(env_id: &str, result: Result<Vec<ServiceInstance>, String>) -> DataEvent {
        DataEvent::Services {
            project_id: "p2".into(),
            env_id: env_id.into(),
            result,
        }
    }

    fn log_event(session: u64, event: FollowEvent) -> DataEvent {
        DataEvent::Log { session, event }
    }

    fn load_services(env_id: &str) -> Command {
        Command::LoadServices {
            project_id: "p2".into(),
            env_id: env_id.into(),
        }
    }

    fn prefs_for(project: &str) -> StartPrefs {
        StartPrefs {
            project: Some(project.into()),
            ..StartPrefs::default()
        }
    }

    /// Project p2 / production, services loaded, Services view on top.
    fn app_in_services(now: Instant) -> App {
        let (mut app, _) = App::new(prefs_for("beta"));
        data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
        data(&mut app, envs_event("p2"), now);
        data(&mut app, services_event("e-prod", Ok(services())), now);
        app
    }

    fn logs_view(app: &App) -> &LogsView {
        match app.stack.last() {
            Some(View::Logs(l)) => l,
            other => panic!("expected logs view, got {other:?}"),
        }
    }

    #[test]
    fn new_loads_workspaces_with_default_backstop() {
        let (app, cmds) = App::new(StartPrefs::default());
        assert_eq!(cmds, vec![Command::LoadWorkspaces]);
        assert_eq!(app.backstop, Duration::from_secs(360));
        assert!(matches!(app.stack.as_slice(), [View::Projects]));
    }

    #[test]
    fn failed_first_workspace_load_is_retried_on_the_backstop() {
        let t0 = Instant::now();
        let (mut app, _) = App::new(StartPrefs::default());
        assert!(app.workspaces.loading);
        data(&mut app, DataEvent::Workspaces(Err("offline".into())), t0);
        assert_eq!(app.workspaces.error.as_deref(), Some("offline"));
        let early = t0 + Duration::from_secs(359);
        assert!(app.update(AppEvent::Tick(early), early).is_empty());
        let due = t0 + Duration::from_secs(360);
        assert_eq!(
            app.update(AppEvent::Tick(due), due),
            vec![Command::LoadWorkspaces]
        );
    }

    #[test]
    fn start_resolves_configured_project_by_name_and_loads_envs() {
        let now = Instant::now();
        let mut prefs = prefs_for("BETA");
        prefs.remembered.last_project_id = Some("p1".into());
        let (mut app, _) = App::new(prefs);
        let cmds = data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
        assert_eq!(
            cmds,
            vec![Command::LoadEnvironments {
                project_id: "p2".into()
            }]
        );
        assert_eq!(app.project.as_ref().map(|p| p.id.as_str()), Some("p2"));
        assert_eq!(app.workspace_id.as_deref(), Some("w2"));
        assert!(matches!(
            app.stack.as_slice(),
            [View::Projects, View::Services]
        ));
    }

    #[test]
    fn start_falls_back_to_remembered_project() {
        let now = Instant::now();
        let mut prefs = prefs_for("no-such-project");
        prefs.remembered.last_project_id = Some("p1".into());
        let (mut app, _) = App::new(prefs);
        let cmds = data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
        assert_eq!(
            cmds,
            vec![Command::LoadEnvironments {
                project_id: "p1".into()
            }]
        );
    }

    #[test]
    fn start_without_prefs_shows_projects() {
        let now = Instant::now();
        let prefs = StartPrefs {
            workspace: Some("team".into()),
            ..StartPrefs::default()
        };
        let (mut app, _) = App::new(prefs);
        let cmds = data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
        assert!(cmds.is_empty());
        assert!(matches!(app.stack.as_slice(), [View::Projects]));
        let ids: Vec<_> = app
            .project_rows()
            .iter()
            .map(|(_, p)| p.id.clone())
            .collect();
        assert_eq!(ids, ["p2", "p1"], "preferred workspace is listed first");
        assert_eq!(app.selected_index(), Some(0));

        let cmds = app.update(code(KeyCode::Enter), now);
        assert_eq!(
            cmds,
            vec![Command::LoadEnvironments {
                project_id: "p2".into()
            }]
        );
        assert!(matches!(
            app.stack.as_slice(),
            [View::Projects, View::Services]
        ));
    }

    #[test]
    fn env_resolution_prefers_production() {
        let now = Instant::now();
        let (mut app, _) = App::new(prefs_for("p2"));
        data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
        let cmds = data(&mut app, envs_event("p2"), now);
        assert_eq!(
            cmds,
            vec![
                load_services("e-prod"),
                Command::SaveRemembered(Remembered {
                    last_project_id: Some("p2".into()),
                    last_environment_id: Some("e-prod".into()),
                    theme: None,
                }),
            ]
        );
        assert_eq!(
            app.environment.as_ref().map(|e| e.id.as_str()),
            Some("e-prod")
        );
    }

    #[test]
    fn env_resolution_prefers_configured_then_remembered() {
        let now = Instant::now();
        let resolve = |prefs: StartPrefs| {
            let (mut app, _) = App::new(prefs);
            data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
            data(&mut app, envs_event("p2"), now);
            app.environment.unwrap().id
        };
        let mut prefs = prefs_for("p2");
        prefs.environment = Some("STAGING".into());
        prefs.remembered.last_environment_id = Some("e-prod".into());
        assert_eq!(resolve(prefs.clone()), "e-stg");
        prefs.environment = None;
        prefs.remembered.last_environment_id = Some("e-stg".into());
        assert_eq!(resolve(prefs), "e-stg");
    }

    #[test]
    fn services_arrival_watches_latest_and_leaving_stops() {
        let now = Instant::now();
        let (mut app, _) = App::new(prefs_for("p2"));
        data(&mut app, DataEvent::Workspaces(Ok(workspaces())), now);
        data(&mut app, envs_event("p2"), now);
        let watch = Command::WatchDeployments {
            ids: vec!["d2".into(), "d3".into()],
        };
        let cmds = data(&mut app, services_event("e-prod", Ok(services())), now);
        assert_eq!(cmds, vec![watch.clone()]);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert_eq!(
            cmds,
            vec![
                Command::WatchDeployments { ids: vec![] },
                Command::LoadDeployments {
                    project_id: "p2".into(),
                    env_id: "e-prod".into(),
                    service_id: "svc-a".into()
                },
            ]
        );
        assert_eq!(app.update(code(KeyCode::Esc), now), vec![watch]);
    }

    #[test]
    fn l_on_failed_service_opens_build_logs_of_latest() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        let cmds = app.update(key('l'), now);
        assert!(cmds.contains(&Command::OpenLogs {
            session: 1,
            env_id: "e-prod".into(),
            deployment_id: "d2".into(),
            kind: LogKind::Build,
        }));
        let l = logs_view(&app);
        assert_eq!(l.deployment_id, "d2");
        assert_eq!(l.service_name, "svc-a-name");
        assert!(l.follow);
        assert_eq!(l.state, StreamState::Connecting);
    }

    #[test]
    fn deployments_view_shows_failed_latest_then_active_and_enter_opens_row() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(code(KeyCode::Enter), now);
        let history = vec![deploy("d0", DeploymentStatus::Removed)];
        data(
            &mut app,
            DataEvent::Deployments {
                service_id: "svc-a".into(),
                env_id: "e-prod".into(),
                result: Ok(history),
            },
            now,
        );
        let ids: Vec<_> = app
            .deployment_table()
            .iter()
            .map(|r| r.deployment.id.clone())
            .collect();
        assert_eq!(ids, ["d2", "d1", "d0"]);
        app.update(key('j'), now);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert_eq!(
            cmds,
            vec![Command::OpenLogs {
                session: 1,
                env_id: "e-prod".into(),
                deployment_id: "d1".into(),
                kind: LogKind::Runtime,
            }]
        );
    }

    #[test]
    fn instances_enter_opens_runtime_logs_for_replica() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('i'), now);
        assert!(matches!(app.stack.last(), Some(View::Instances { .. })));
        let cmds = app.update(code(KeyCode::Enter), now);
        assert!(cmds.contains(&Command::OpenLogs {
            session: 1,
            env_id: "e-prod".into(),
            deployment_id: "d1".into(),
            kind: LogKind::Runtime,
        }));
        assert_eq!(logs_view(&app).replica.as_deref(), Some("d1-i0"));
    }

    #[test]
    fn esc_from_logs_closes_session() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('l'), now);
        let cmds = app.update(code(KeyCode::Esc), now);
        assert_eq!(cmds[0], Command::CloseLogs { session: 1 });
        assert!(matches!(app.stack.last(), Some(View::Services)));
    }

    #[test]
    fn b_toggles_kind_with_a_new_session() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('l'), now);
        let cmds = app.update(key('b'), now);
        assert_eq!(
            cmds,
            vec![
                Command::CloseLogs { session: 1 },
                Command::OpenLogs {
                    session: 2,
                    env_id: "e-prod".into(),
                    deployment_id: "d2".into(),
                    kind: LogKind::Runtime,
                },
            ]
        );
        assert_eq!(logs_view(&app).kind, LogKind::Runtime);
    }

    #[test]
    fn ctrl_r_in_logs_reopens_the_same_stream_as_a_new_session() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('l'), now);
        data(
            &mut app,
            log_event(
                1,
                FollowEvent::State(StreamState::Failed("Railway rejected".into())),
            ),
            now,
        );
        let cmds = app.update(ctrl('r'), now);
        assert_eq!(
            cmds,
            vec![
                Command::CloseLogs { session: 1 },
                Command::OpenLogs {
                    session: 2,
                    env_id: "e-prod".into(),
                    deployment_id: "d2".into(),
                    kind: LogKind::Build,
                },
            ]
        );
        let l = logs_view(&app);
        assert_eq!((l.session, l.kind), (2, LogKind::Build));
        assert_eq!(l.state, StreamState::Connecting);
        // Events for the old session no longer land.
        data(
            &mut app,
            log_event(1, FollowEvent::Lines(vec![line(1, "old")])),
            now,
        );
        assert!(logs_view(&app).buffer.is_empty());
    }

    #[test]
    fn late_services_for_old_env_ignored() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('e'), now);
        assert_eq!(app.env_picker, Some(1), "picker starts on the current env");
        // Modal: k moves the picker, it is not a table move.
        app.update(key('k'), now);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert!(cmds.contains(&load_services("e-stg")));
        assert!(cmds.contains(&Command::SaveRemembered(Remembered {
            last_project_id: Some("p2".into()),
            last_environment_id: Some("e-stg".into()),
            theme: None,
        })));
        assert_eq!(app.env_picker, None);
        assert!(app.services.value.is_none());
        assert!(app.services.loading);

        let cmds = data(&mut app, services_event("e-prod", Ok(services())), now);
        assert!(cmds.is_empty());
        assert!(app.services.value.is_none());
        assert!(app.services.loading);
        assert!(app.services.fetched_at.is_none());
    }

    #[test]
    fn late_deployments_and_environments_for_old_context_ignored() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(code(KeyCode::Enter), now); // Deployments for svc-a
        let other = vec![deploy("x", DeploymentStatus::Success)];
        let cmds = data(
            &mut app,
            DataEvent::Deployments {
                service_id: "svc-b".into(),
                env_id: "e-prod".into(),
                result: Ok(other),
            },
            now,
        );
        assert!(cmds.is_empty());
        assert!(app.deployments.value.is_none());
        let cmds = data(
            &mut app,
            DataEvent::Environments {
                project_id: "p1".into(),
                result: Ok(vec![]),
            },
            now,
        );
        assert!(cmds.is_empty());
        assert_eq!(app.environments.value.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn log_events_for_closed_session_ignored() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('l'), now);
        app.update(key('b'), now); // closes session 1, opens session 2
        assert_eq!(logs_view(&app).session, 2);
        data(
            &mut app,
            log_event(1, FollowEvent::Lines(vec![line(1, "old")])),
            now,
        );
        data(
            &mut app,
            log_event(1, FollowEvent::State(StreamState::Live)),
            now,
        );
        assert!(logs_view(&app).buffer.is_empty());
        assert_eq!(logs_view(&app).state, StreamState::Connecting);

        data(
            &mut app,
            log_event(2, FollowEvent::Lines(vec![line(1, "new")])),
            now,
        );
        data(
            &mut app,
            log_event(2, FollowEvent::State(StreamState::Live)),
            now,
        );
        assert_eq!(logs_view(&app).buffer.len(), 1);
        assert_eq!(logs_view(&app).state, StreamState::Live);

        // Once the view is gone, its session's events are dropped too.
        app.update(code(KeyCode::Esc), now);
        data(
            &mut app,
            log_event(2, FollowEvent::Lines(vec![line(2, "x")])),
            now,
        );
        assert!(matches!(app.stack.last(), Some(View::Services)));
    }

    #[test]
    fn scroll_up_pauses_follow_and_shift_g_resumes() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('l'), now);
        let lines = (0..10).map(|i| line(i, &format!("l{i}"))).collect();
        data(&mut app, log_event(1, FollowEvent::Lines(lines)), now);
        app.update(key('k'), now);
        assert!(!logs_view(&app).follow);
        assert_eq!(logs_view(&app).scroll, 1);
        app.update(code(KeyCode::Up), now);
        assert_eq!(logs_view(&app).scroll, 2);
        // Lines arriving while paused keep the view anchored.
        let late = vec![line(30, "late")];
        data(&mut app, log_event(1, FollowEvent::Lines(late)), now);
        assert_eq!(logs_view(&app).scroll, 3);

        let shift_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        app.update(AppEvent::Key(shift_g), now);
        assert!(logs_view(&app).follow);
        assert_eq!(logs_view(&app).scroll, 0);

        app.update(code(KeyCode::PageUp), now);
        assert!(!logs_view(&app).follow);
        assert_eq!(logs_view(&app).scroll, 10, "page up clamps to the top line");
        app.update(key('f'), now);
        assert!(logs_view(&app).follow);
    }

    #[test]
    fn logs_filter_is_modal_and_esc_clears_then_goes_back() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('l'), now);
        app.update(key('/'), now);
        assert!(
            app.update(key('q'), now).is_empty(),
            "q is filter text, not quit"
        );
        app.update(code(KeyCode::Enter), now);
        assert_eq!(app.filter_input, None);
        assert_eq!(logs_view(&app).filter.as_deref(), Some("q"));
        assert!(app.update(code(KeyCode::Esc), now).is_empty());
        assert_eq!(logs_view(&app).filter, None);
        let cmds = app.update(code(KeyCode::Esc), now);
        assert_eq!(cmds[0], Command::CloseLogs { session: 1 });
    }

    #[test]
    fn table_filter_narrows_rows_and_esc_clears_it_first() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        typed(&mut app, "/SVC-B", now);
        app.update(code(KeyCode::Enter), now);
        let ids: Vec<_> = app
            .service_rows()
            .iter()
            .map(|s| s.service_id.clone())
            .collect();
        assert_eq!(ids, ["svc-b"]);
        assert_eq!(app.selected_index(), Some(0));
        app.update(code(KeyCode::Esc), now);
        assert_eq!(app.table_filter, None);
        assert!(matches!(app.stack.last(), Some(View::Services)));
    }

    #[test]
    fn theme_picker_previews_and_esc_restores() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('t'), now);
        assert_eq!(app.theme_picker, Some(0));
        app.update(key('j'), now);
        app.update(key('j'), now);
        assert_eq!(app.theme, 2, "moving previews the theme");
        let cmds = app.update(code(KeyCode::Esc), now);
        assert!(cmds.is_empty());
        assert_eq!((app.theme, app.theme_picker), (0, None));
        assert!(matches!(app.stack.last(), Some(View::Services)));
    }

    #[test]
    fn theme_picker_enter_saves_choice_with_remembered_ids() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        typed(&mut app, ":theme", now);
        app.update(code(KeyCode::Enter), now);
        assert!(app.theme_picker.is_some());
        app.update(code(KeyCode::Down), now);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert_eq!(app.theme_picker, None);
        let Some(Command::SaveRemembered(saved)) = cmds.first() else {
            panic!("expected a save, got {cmds:?}");
        };
        assert_eq!(saved.theme.as_deref(), Some(THEMES[1].id));
        assert_eq!(saved.last_environment_id.as_deref(), Some("e-prod"));
        // Keeping the same theme again writes nothing.
        app.update(key('t'), now);
        assert!(app.update(code(KeyCode::Enter), now).is_empty());
    }

    #[test]
    fn start_theme_prefers_remembered_then_config() {
        let mut prefs = StartPrefs {
            theme: Some("Nord".into()),
            ..StartPrefs::default()
        };
        let nord = theme::find("nord").unwrap();
        assert_eq!(App::new(prefs.clone()).0.theme, nord);
        prefs.remembered.theme = Some("dracula".into());
        assert_eq!(
            App::new(prefs.clone()).0.theme,
            theme::find("dracula").unwrap()
        );
        // A stale remembered id falls back to the config.
        prefs.remembered.theme = Some("gone".into());
        assert_eq!(App::new(prefs).0.theme, nord);
        let (app, _) = App::new(StartPrefs {
            theme: Some("nope".into()),
            ..StartPrefs::default()
        });
        assert_eq!(app.theme, 0);
        assert_eq!(
            app.message.as_deref(),
            Some("unknown theme in config: nope")
        );
    }

    #[test]
    fn services_are_listed_alphabetically_ignoring_case() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        let named = |id: &str, name: &str| ServiceInstance {
            service_name: name.into(),
            ..service(id, deploy("d", DeploymentStatus::Success), vec![])
        };
        let list = vec![named("1", "worker"), named("2", "Api"), named("3", "db")];
        data(&mut app, services_event("e-prod", Ok(list)), now);
        let names: Vec<_> = app
            .service_rows()
            .iter()
            .map(|s| s.service_name.clone())
            .collect();
        assert_eq!(names, ["Api", "db", "worker"]);
    }

    #[test]
    fn table_filter_narrows_rows_while_typing() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        typed(&mut app, "/b", now);
        assert_eq!(app.filter_input.as_deref(), Some("b"));
        let ids: Vec<_> = app
            .service_rows()
            .iter()
            .map(|s| s.service_id.clone())
            .collect();
        assert_eq!(ids, ["svc-b"]);
        assert_eq!(app.selected_index(), Some(0));
        // Esc while typing abandons the search and the full list returns.
        app.update(code(KeyCode::Esc), now);
        assert_eq!(app.service_rows().len(), 2);
        assert!(matches!(app.stack.last(), Some(View::Services)));
    }

    #[test]
    fn backstop_tick_emits_reload_after_interval_only() {
        let t0 = Instant::now();
        let secs = Duration::from_secs;
        let mut app = app_in_services(t0);
        let budget = |blocked_until| DataEvent::Budget {
            interval: secs(60),
            blocked_until,
        };
        data(&mut app, budget(None), t0);
        assert_eq!(app.backstop, secs(60));

        let early = t0 + secs(59);
        assert!(app.update(AppEvent::Tick(early), early).is_empty());
        let t1 = t0 + secs(60);
        assert_eq!(
            app.update(AppEvent::Tick(t1), t1),
            vec![load_services("e-prod")]
        );
        assert!(
            app.update(AppEvent::Tick(t1), t1).is_empty(),
            "no duplicate while loading"
        );

        // While rate limited nothing goes out; afterwards the reload does.
        data(&mut app, services_event("e-prod", Ok(services())), t1);
        let t2 = t1 + secs(60);
        data(&mut app, budget(Some(t2 + secs(10))), t1);
        assert_eq!(app.blocked_until, Some(t2 + secs(10)));
        assert!(app.update(AppEvent::Tick(t2), t2).is_empty());
        let t3 = t2 + secs(10);
        assert_eq!(
            app.update(AppEvent::Tick(t3), t3),
            vec![load_services("e-prod")]
        );
    }

    #[test]
    fn r_reloads_before_the_backstop_except_where_it_reveals() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        let early = now + Duration::from_secs(1);
        assert_eq!(app.update(key('r'), early), vec![load_services("e-prod")]);
        assert!(app.services.loading);

        let mut app = app_in_variables(now);
        assert!(
            app.update(key('r'), now).is_empty(),
            "r reveals in Variables"
        );
    }

    #[test]
    fn load_error_keeps_old_value_and_sets_error() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        let later = now + Duration::from_secs(5);
        assert_eq!(app.update(ctrl('r'), later), vec![load_services("e-prod")]);
        assert!(app.services.loading);
        data(
            &mut app,
            services_event("e-prod", Err("boom".into())),
            later,
        );
        assert_eq!(app.services.value.as_ref().map(Vec::len), Some(2));
        assert_eq!(app.services.error.as_deref(), Some("boom"));
        assert_eq!(app.services.fetched_at, Some(now));
        assert!(!app.services.loading);
    }

    #[test]
    fn terminal_status_event_triggers_service_reload() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        let status_of = |app: &App| {
            let svc = &app.services.value.as_ref().unwrap()[1];
            svc.latest.as_ref().unwrap().status.clone()
        };
        let changed = |status| DataEvent::StatusChanged {
            deployment_id: "d3".into(),
            status,
        };
        assert!(data(&mut app, changed(DeploymentStatus::Deploying), now).is_empty());
        assert_eq!(status_of(&app), DeploymentStatus::Deploying);
        let cmds = data(&mut app, changed(DeploymentStatus::Crashed), now);
        assert_eq!(cmds, vec![load_services("e-prod")]);
        assert_eq!(status_of(&app), DeploymentStatus::Crashed);
    }

    #[test]
    fn colon_q_quits() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key(':'), now);
        assert!(
            app.update(key('q'), now).is_empty(),
            "q is command text, not quit"
        );
        assert_eq!(app.update(code(KeyCode::Enter), now), vec![Command::Quit]);
        assert_eq!(app.update(key('q'), now), vec![Command::Quit]);
    }

    #[test]
    fn command_line_jumps_between_views() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        typed(&mut app, ":logs", now);
        app.update(code(KeyCode::Enter), now);
        assert_eq!(logs_view(&app).session, 1);

        typed(&mut app, ":projects", now);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert_eq!(cmds, vec![Command::CloseLogs { session: 1 }]);
        assert!(matches!(app.stack.as_slice(), [View::Projects]));

        typed(&mut app, ":deploys", now);
        app.update(code(KeyCode::Enter), now);
        assert!(matches!(
            app.stack.as_slice(),
            [View::Projects, View::Services, View::Deployments { .. }]
        ));

        typed(&mut app, ":nope", now);
        assert!(app.update(code(KeyCode::Enter), now).is_empty());
        assert!(app.message.is_some());
    }

    #[test]
    fn help_toggles_and_ctrl_c_always_quits() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        app.update(key('?'), now);
        assert!(app.help);
        app.update(key('?'), now);
        assert!(!app.help);
        app.update(key('/'), now);
        assert_eq!(app.update(ctrl('c'), now), vec![Command::Quit]);
    }

    // ---- variables ----

    fn vars(pairs: &[(&str, &str)]) -> Variables {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn vars_event(service_id: &str, result: Result<Variables, String>) -> DataEvent {
        DataEvent::Variables {
            env_id: "e-prod".into(),
            service_id: service_id.into(),
            purpose: VarsPurpose::View,
            result,
        }
    }

    fn names(app: &App) -> Vec<String> {
        app.variable_rows()
            .iter()
            .map(|r| r.name.to_string())
            .collect()
    }

    /// Variables view of svc-a, loaded with a few variables.
    fn app_in_variables(now: Instant) -> App {
        let mut app = app_in_services(now);
        app.update(key('v'), now);
        let loaded = vars(&[
            ("DB_URL", "postgres://x"),
            ("PORT", "has db inside"),
            ("REDIS_DB", "1"),
        ]);
        data(&mut app, vars_event("svc-a", Ok(loaded)), now);
        app
    }

    #[test]
    fn v_opens_variables_and_loads() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        let cmds = app.update(key('v'), now);
        // Leaving Services stops its deployment watch first.
        assert_eq!(
            cmds,
            vec![
                Command::WatchDeployments { ids: Vec::new() },
                Command::LoadVariables {
                    project_id: "p2".into(),
                    env_id: "e-prod".into(),
                    service_id: "svc-a".into(),
                    purpose: VarsPurpose::View,
                },
            ]
        );
        assert!(matches!(app.stack.last(), Some(View::Variables(_))));
        assert!(app.variables.loading);
        let cmds = app.update(code(KeyCode::Esc), now);
        assert!(matches!(app.stack.as_slice(), [.., View::Services]));
        assert_eq!(
            cmds,
            vec![Command::WatchDeployments {
                ids: vec!["d2".into(), "d3".into()]
            }]
        );
        typed(&mut app, ":vars", now);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert!(matches!(
            cmds.last(),
            Some(Command::LoadVariables { service_id, .. }) if service_id == "svc-a"
        ));
    }

    #[test]
    fn ctrl_r_reloads_variables_and_stale_replies_are_ignored() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        let cmds = app.update(ctrl('r'), now);
        assert!(matches!(cmds.as_slice(), [Command::LoadVariables { .. }]));
        data(
            &mut app,
            vars_event("svc-b", Ok(vars(&[("OTHER", "1")]))),
            now,
        );
        assert_eq!(names(&app), ["DB_URL", "PORT", "REDIS_DB"]);
        data(&mut app, vars_event("svc-a", Err("boom".into())), now);
        assert_eq!(app.variables.error.as_deref(), Some("boom"));
        assert_eq!(names(&app).len(), 3, "a failed reload keeps the old rows");
    }

    #[test]
    fn variable_rows_merge_railway_and_pending_sorted() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        let target = crate::pending::Target {
            project_id: "p2",
            env_id: "e-prod",
            env_name: "production",
            service_id: "svc-a",
            service_name: "svc-a-name",
        };
        app.pending.set(&target, "ALPHA_NEW", "v".into(), None);
        app.pending.delete(&target, "PORT", Some("has db inside"));
        app.pending.set(&target, "REDIS_DB", "2".into(), Some("1"));
        let rows = app.variable_rows();
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r.name, r.current, r.pending.is_some()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("ALPHA_NEW", None, true),
                ("DB_URL", Some("postgres://x"), false),
                ("PORT", Some("has db inside"), true),
                ("REDIS_DB", Some("1"), true),
            ]
        );
    }

    #[test]
    fn slash_filters_variables_live_by_name_only() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        app.update(key('/'), now);
        typed(&mut app, "db", now);
        assert_eq!(names(&app), ["DB_URL", "REDIS_DB"]);
        assert!(!names(&app).contains(&"PORT".to_string()));
        app.update(code(KeyCode::Esc), now);
        assert_eq!(names(&app), ["DB_URL", "PORT", "REDIS_DB"]);
    }

    #[test]
    fn search_survives_reload() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        app.update(key('/'), now);
        typed(&mut app, "db", now);
        app.update(code(KeyCode::Enter), now);
        let reloaded = vars(&[("DB_URL", "y"), ("PORT", "db"), ("REDIS_DB", "z")]);
        data(&mut app, vars_event("svc-a", Ok(reloaded)), now);
        assert_eq!(names(&app), ["DB_URL", "REDIS_DB"]);
    }

    #[test]
    fn leaving_variables_rehides_values() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        if let Some(View::Variables(v)) = app.stack.last_mut() {
            v.reveal_all = true;
            v.revealed.insert("PORT".into());
        }
        app.update(code(KeyCode::Esc), now);
        assert!(!app.stack.iter().any(|v| matches!(v, View::Variables(_))));
        app.update(key('v'), now);
        match app.stack.last() {
            Some(View::Variables(v)) => assert!(!v.reveal_all && v.revealed.is_empty()),
            other => panic!("expected variables, got {other:?}"),
        }
    }

    #[test]
    fn pending_error_is_shown_at_start() {
        let (app, _) = App::new(StartPrefs {
            pending_error: Some("pending.toml was unreadable".into()),
            ..StartPrefs::default()
        });
        assert_eq!(app.message.as_deref(), Some("pending.toml was unreadable"));
    }

    #[test]
    fn save_failed_is_shown_and_debug_hides_values() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        data(
            &mut app,
            DataEvent::SaveFailed("could not save".into()),
            now,
        );
        assert_eq!(app.message.as_deref(), Some("could not save"));
        let event = vars_event("svc-a", Ok(vars(&[("K", "s3cret-value")])));
        assert!(!format!("{event:?}").contains("s3cret-value"));
    }

    // ---- editing ----

    fn pending_of(app: &App, name: &str) -> Option<PendingVar> {
        app.pending
            .service("e-prod", "svc-a")
            .and_then(|s| s.vars.get(name))
            .cloned()
    }

    fn select_var(app: &mut App, name: &str, now: Instant) {
        for _ in 0..10 {
            if app.selection.variable.as_deref() == Some(name) {
                return;
            }
            app.update(key('j'), now);
            if app.selection.variable.is_none() {
                app.selection.variable = app.variable_rows().first().map(|r| r.name.into());
            }
        }
        panic!("variable not found");
    }

    #[test]
    fn edit_commits_set_and_saves() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        select_var(&mut app, "REDIS_DB", now);
        assert!(app.update(key('e'), now).is_empty());
        let input = app.var_input.as_ref().unwrap();
        assert_eq!(input.name.as_deref(), Some("REDIS_DB"));
        assert_eq!(input.text, "1");
        app.update(key('0'), now);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert!(app.var_input.is_none());
        assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
        let p = pending_of(&app, "REDIS_DB").unwrap();
        assert_eq!(p.change, crate::pending::Change::Set { value: "10".into() });
        assert_eq!(p.base.as_deref(), Some("1"));
    }

    #[test]
    fn esc_cancels_inline_edit() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        select_var(&mut app, "REDIS_DB", now);
        app.update(key('e'), now);
        app.update(key('x'), now);
        assert!(app.update(code(KeyCode::Esc), now).is_empty());
        assert!(app.var_input.is_none());
        assert!(app.pending.envs.is_empty());
    }

    #[test]
    fn add_validates_name() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        app.update(key('a'), now);
        assert_eq!(app.var_input.as_ref().unwrap().name, None);
        typed(&mut app, "1BAD", now);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert!(cmds.is_empty());
        assert!(app.message.is_some());
        assert_eq!(app.var_input.as_ref().unwrap().name, None);
        for _ in 0..4 {
            app.update(code(KeyCode::Backspace), now);
        }
        typed(&mut app, "GOOD_1", now);
        app.update(code(KeyCode::Enter), now);
        let input = app.var_input.as_ref().unwrap();
        assert_eq!(input.name.as_deref(), Some("GOOD_1"));
        assert_eq!(input.text, "");
        typed(&mut app, "v", now);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
        assert_eq!(pending_of(&app, "GOOD_1").unwrap().base, None);
    }

    #[test]
    fn add_existing_name_edits_it() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        app.update(key('a'), now);
        typed(&mut app, "REDIS_DB", now);
        app.update(code(KeyCode::Enter), now);
        let input = app.var_input.as_ref().unwrap();
        assert_eq!(input.name.as_deref(), Some("REDIS_DB"));
        assert_eq!(input.text, "1");
    }

    #[test]
    fn d_toggles_delete_and_u_undoes() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        select_var(&mut app, "REDIS_DB", now);
        let cmds = app.update(key('d'), now);
        assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
        assert_eq!(
            pending_of(&app, "REDIS_DB").unwrap().change,
            crate::pending::Change::Delete
        );
        let cmds = app.update(key('d'), now);
        assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
        assert!(pending_of(&app, "REDIS_DB").is_none());
        app.update(key('d'), now);
        let cmds = app.update(key('u'), now);
        assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
        assert!(app.pending.envs.is_empty());
        // Nothing to undo: no save.
        assert!(app.update(key('u'), now).is_empty());
    }

    #[test]
    fn editing_a_deleted_variable_starts_from_railway_value() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        select_var(&mut app, "REDIS_DB", now);
        app.update(key('d'), now);
        app.update(key('e'), now);
        assert_eq!(app.var_input.as_ref().unwrap().text, "1");
    }

    #[test]
    fn multiline_value_refuses_inline_edit() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        data(
            &mut app,
            vars_event("svc-a", Ok(vars(&[("KEY", "a\nb")]))),
            now,
        );
        app.selection.variable = Some("KEY".into());
        assert!(app.update(key('e'), now).is_empty());
        assert!(app.var_input.is_none());
        assert_eq!(
            app.message.as_deref(),
            Some("multi-line value: press E to edit in $EDITOR")
        );
    }

    #[test]
    #[allow(non_snake_case)]
    fn E_emits_edit_external_with_current_value() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        select_var(&mut app, "REDIS_DB", now);
        let cmds = app.update(key('E'), now);
        assert_eq!(
            cmds,
            vec![Command::EditExternal {
                name: "REDIS_DB".into(),
                initial: "1".into()
            }]
        );
        assert!(!format!("{cmds:?}").contains('1'));
    }

    #[test]
    fn edited_event_sets_pending() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        let cmds = data(
            &mut app,
            DataEvent::Edited {
                name: "REDIS_DB".into(),
                result: Ok("7".into()),
            },
            now,
        );
        assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
        let p = pending_of(&app, "REDIS_DB").unwrap();
        assert_eq!(p.change, crate::pending::Change::Set { value: "7".into() });
        assert_eq!(p.base.as_deref(), Some("1"));
        let cmds = data(
            &mut app,
            DataEvent::Edited {
                name: "REDIS_DB".into(),
                result: Err("editor exited with status 1".into()),
            },
            now,
        );
        assert!(cmds.is_empty());
        assert_eq!(app.message.as_deref(), Some("editor exited with status 1"));
        let event = DataEvent::Edited {
            name: "N".into(),
            result: Ok("s3cret-value".into()),
        };
        assert!(!format!("{event:?}").contains("s3cret-value"));
    }

    #[test]
    #[allow(non_snake_case)]
    fn r_and_R_toggle_reveal() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        select_var(&mut app, "PORT", now);
        let revealed = |app: &App| match app.stack.last() {
            Some(View::Variables(v)) => (v.revealed.clone(), v.reveal_all),
            _ => panic!(),
        };
        app.update(key('r'), now);
        assert_eq!(revealed(&app).0.iter().collect::<Vec<_>>(), ["PORT"]);
        app.update(key('r'), now);
        assert!(revealed(&app).0.is_empty());
        app.update(key('R'), now);
        assert!(revealed(&app).1);
        app.update(key('R'), now);
        assert!(!revealed(&app).1);
    }

    /// REDIS_DB set 1 → 10 and written to Railway (applied), the view reloaded with it.
    fn app_with_applied_edit(now: Instant) -> App {
        let mut app = app_in_variables(now);
        select_var(&mut app, "REDIS_DB", now);
        app.update(key('e'), now);
        app.update(key('0'), now);
        app.update(code(KeyCode::Enter), now);
        app.pending
            .envs
            .get_mut("e-prod")
            .unwrap()
            .get_mut("svc-a")
            .unwrap()
            .vars
            .get_mut("REDIS_DB")
            .unwrap()
            .applied = true;
        let loaded = vars(&[
            ("DB_URL", "postgres://x"),
            ("PORT", "has db inside"),
            ("REDIS_DB", "10"),
        ]);
        data(&mut app, vars_event("svc-a", Ok(loaded)), now);
        app
    }

    #[test]
    fn u_refused_on_an_applied_entry() {
        let now = Instant::now();
        let mut app = app_with_applied_edit(now);
        let before = app.pending.clone();
        assert!(app.update(key('u'), now).is_empty());
        assert_eq!(
            app.message.as_deref(),
            Some("already written to Railway — edit it to revert")
        );
        assert_eq!(app.pending, before);
    }

    #[test]
    fn setting_an_applied_entry_back_sends_the_revert() {
        let now = Instant::now();
        let mut app = app_with_applied_edit(now);
        app.update(key('e'), now);
        assert_eq!(app.var_input.as_ref().unwrap().text, "10");
        app.update(code(KeyCode::Backspace), now);
        let cmds = app.update(code(KeyCode::Enter), now);
        assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
        let p = pending_of(&app, "REDIS_DB").expect("the revert stays pending");
        assert_eq!(p.change, crate::pending::Change::Set { value: "1".into() });
        assert_eq!(p.base.as_deref(), Some("10"));
        assert!(!p.applied);
    }

    #[test]
    fn d_on_an_applied_addition_deletes_it_and_d_on_an_applied_delete_is_refused() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        app.update(key('a'), now);
        typed(&mut app, "NEW", now);
        app.update(code(KeyCode::Enter), now);
        typed(&mut app, "n", now);
        app.update(code(KeyCode::Enter), now);
        let sp = app
            .pending
            .envs
            .get_mut("e-prod")
            .unwrap()
            .get_mut("svc-a")
            .unwrap();
        sp.vars.get_mut("NEW").unwrap().applied = true;
        sp.written_not_deployed = true;
        select_var(&mut app, "NEW", now);
        let cmds = app.update(key('d'), now);
        assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
        let p = pending_of(&app, "NEW").expect("the delete stays pending");
        assert_eq!(p.change, crate::pending::Change::Delete);
        assert_eq!(p.base.as_deref(), Some("n"));
        assert!(!p.applied);

        // Now the delete is written too: `d` again would forget it.
        app.pending
            .envs
            .get_mut("e-prod")
            .unwrap()
            .get_mut("svc-a")
            .unwrap()
            .vars
            .get_mut("NEW")
            .unwrap()
            .applied = true;
        let before = app.pending.clone();
        assert!(app.update(key('d'), now).is_empty());
        assert_eq!(
            app.message.as_deref(),
            Some("already written to Railway — edit it to revert")
        );
        assert_eq!(app.pending, before);
    }

    #[test]
    fn variable_edits_refused_while_the_service_deploys() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        select_var(&mut app, "REDIS_DB", now);
        app.update(key('d'), now);
        app.update(key('p'), now);
        app.update(key('D'), now);
        data(
            &mut app,
            deploy_vars_event("e-prod", "svc-a", vars(&[("REDIS_DB", "1")])),
            now,
        );
        assert!(app
            .update(key('y'), now)
            .iter()
            .any(|c| matches!(c, Command::DeployService { .. })));
        app.update(code(KeyCode::Esc), now);
        assert!(matches!(app.stack.last(), Some(View::Variables(_))));
        let before = app.pending.clone();
        for k in [
            KeyCode::Char('u'),
            KeyCode::Char('d'),
            KeyCode::Char('e'),
            KeyCode::Enter,
            KeyCode::Char('E'),
            KeyCode::Char('a'),
        ] {
            assert!(app.update(code(k), now).is_empty(), "{k:?}");
            assert!(app.var_input.is_none(), "{k:?}");
            assert!(
                app.message.as_deref().unwrap().contains("is deploying"),
                "{k:?}: {:?}",
                app.message
            );
            assert_eq!(app.pending, before, "{k:?}");
        }
        let edited = DataEvent::Edited {
            name: "REDIS_DB".into(),
            result: Ok("7".into()),
        };
        assert!(data(&mut app, edited, now).is_empty());
        assert_eq!(app.pending, before);
        assert!(app.message.as_deref().unwrap().contains("is deploying"));
        // Revealing is still fine.
        app.update(key('r'), now);
        assert!(!app.message.as_deref().unwrap_or("").contains("deploying"));
    }

    #[test]
    fn ctrl_c_quits_while_editing() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        app.update(key('a'), now);
        assert_eq!(app.update(ctrl('c'), now), vec![Command::Quit]);
    }

    // ---- review screen ----

    fn add_pending(app: &mut App, env: (&str, &str), svc: (&str, &str), name: &str, v: &str) {
        let sp = app
            .pending
            .envs
            .entry(env.0.into())
            .or_default()
            .entry(svc.0.into())
            .or_default();
        sp.project_id = "p2".into();
        sp.env_name = env.1.into();
        sp.service_name = svc.1.into();
        sp.vars.insert(
            name.into(),
            crate::pending::PendingVar {
                change: crate::pending::Change::Set { value: v.into() },
                base: None,
                applied: false,
            },
        );
    }

    /// Three groups in two environments plus another project's, then the review open.
    /// Production's services include s1 and s2.
    fn app_in_review(now: Instant) -> App {
        let mut app = app_in_services(now);
        let mut loaded = services();
        for id in ["s1", "s2"] {
            loaded.push(service(id, deploy("d9", DeploymentStatus::Success), vec![]));
        }
        data(&mut app, services_event("e-prod", Ok(loaded)), now);
        add_pending(&mut app, ("e-stg", "staging"), ("s1", "web"), "A", "1");
        add_pending(
            &mut app,
            ("e-prod", "production"),
            ("s2", "worker"),
            "B",
            "2",
        );
        add_pending(&mut app, ("e-prod", "production"), ("s1", "api"), "C", "3");
        add_pending(&mut app, ("e-x", "other"), ("s1", "api"), "D", "4");
        app.pending
            .envs
            .get_mut("e-x")
            .unwrap()
            .get_mut("s1")
            .unwrap()
            .project_id = "p9".into();
        app.update(key('p'), now);
        assert!(matches!(app.stack.last(), Some(View::Pending(_))));
        app
    }

    #[test]
    fn review_lists_all_envs_current_first() {
        let now = Instant::now();
        let app = app_in_review(now);
        assert_eq!(app.environment.as_ref().unwrap().id, "e-prod");
        let ids: Vec<(String, String)> = app
            .pending_groups()
            .into_iter()
            .map(|g| (g.env_id, g.service_id))
            .collect();
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            ids,
            [
                pair("e-prod", "s1"),
                pair("e-prod", "s2"),
                pair("e-stg", "s1")
            ]
        );
    }

    #[test]
    fn same_named_environments_do_not_interleave() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        app.pending.envs.remove("e-stg");
        add_pending(&mut app, ("e-b", "dup"), ("s1", "alpha"), "A", "1");
        add_pending(&mut app, ("e-a", "dup"), ("s1", "beta"), "A", "1");
        add_pending(&mut app, ("e-a", "dup"), ("s2", "zeta"), "A", "1");
        let envs: Vec<String> = app
            .pending_groups()
            .into_iter()
            .skip(2)
            .map(|g| g.env_id)
            .collect();
        assert_eq!(envs, ["e-a", "e-a", "e-b"]);
    }

    #[test]
    fn review_selection_moves_between_groups() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        assert_eq!(app.selected_index(), Some(0));
        app.update(key('j'), now);
        app.update(key('j'), now);
        app.update(key('j'), now);
        assert_eq!(app.selected_index(), Some(2));
    }

    #[test]
    fn orphaned_group_is_labelled_and_cannot_deploy() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        add_pending(&mut app, ("e-gone", "preview"), ("s1", "web"), "Z", "9");
        let groups = app.pending_groups();
        assert_eq!(groups.iter().filter(|g| g.orphaned.is_some()).count(), 1);
        // Current env first, then "preview" before "staging".
        assert_eq!(groups[2].orphaned, Some(Orphan::Environment));
        assert_eq!(groups[2].env_id, "e-gone");
        for _ in 0..2 {
            app.update(key('j'), now);
        }
        app.update(key('D'), now);
        assert!(app.message.is_some());
        assert!(app.confirm.is_none());
    }

    #[test]
    fn group_of_a_service_gone_from_the_current_env_is_orphaned() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        add_pending(
            &mut app,
            ("e-prod", "production"),
            ("s-gone", "old"),
            "Z",
            "9",
        );
        // Another environment's services are not loaded: not judged.
        add_pending(&mut app, ("e-stg", "staging"), ("s-other", "x"), "Z", "9");
        let groups = app.pending_groups();
        let orphans: Vec<(&str, Option<Orphan>)> = groups
            .iter()
            .filter(|g| g.orphaned.is_some())
            .map(|g| (g.service_id.as_str(), g.orphaned))
            .collect();
        assert_eq!(orphans, [("s-gone", Some(Orphan::Service))]);
        // Groups sort by service name: "old" is after "api" and before "worker".
        assert_eq!(groups[1].service_id, "s-gone");
        app.update(key('j'), now);
        assert!(app.update(key('D'), now).is_empty());
        assert_eq!(
            app.message.as_deref(),
            Some("cannot deploy: the service no longer exists")
        );
        // Not judged before the services list has loaded.
        app.services = Loaded::default();
        assert!(app.pending_groups().iter().all(|g| g.orphaned.is_none()));
    }

    #[test]
    fn discard_requires_y() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        app.update(key('x'), now);
        assert_eq!(
            app.confirm,
            Some(Confirm::Discard {
                env_id: "e-prod".into(),
                service_id: "s1".into()
            })
        );
        assert!(app.update(key('n'), now).is_empty());
        assert!(app.confirm.is_none());
        assert_eq!(app.pending_groups().len(), 3);

        app.update(key('x'), now);
        let cmds = app.update(key('y'), now);
        assert!(app.confirm.is_none());
        assert_eq!(app.pending_groups().len(), 2);
        assert!(
            matches!(cmds.as_slice(), [Command::SavePending(p)] if p.service("e-prod", "s1").is_none())
        );
    }

    #[test]
    fn pending_command_and_p_from_variables_open_review() {
        let now = Instant::now();
        let mut app = app_in_variables(now);
        app.update(key('p'), now);
        assert!(matches!(app.stack.last(), Some(View::Pending(_))));
        app.update(code(KeyCode::Esc), now);
        assert!(matches!(app.stack.last(), Some(View::Variables(_))));
        let mut app = app_in_services(now);
        app.update(key(':'), now);
        typed(&mut app, "pending", now);
        app.update(code(KeyCode::Enter), now);
        assert!(matches!(app.stack.last(), Some(View::Pending(_))));
    }

    #[test]
    #[allow(non_snake_case)]
    fn review_r_and_R_toggle_reveal_all() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        let all = |app: &App| match app.stack.last() {
            Some(View::Pending(v)) => v.reveal_all,
            _ => panic!(),
        };
        app.update(key('r'), now);
        assert!(all(&app));
        app.update(key('R'), now);
        assert!(!all(&app));
    }

    // ---- deploy ----

    fn deploy_vars_event(env_id: &str, service_id: &str, current: Variables) -> DataEvent {
        DataEvent::Variables {
            env_id: env_id.into(),
            service_id: service_id.into(),
            purpose: VarsPurpose::Deploy,
            result: Ok(current),
        }
    }

    fn step(env_id: &str, service_id: &str, step: DeployStep) -> DataEvent {
        DataEvent::Deploy {
            env_id: env_id.into(),
            service_id: service_id.into(),
            step,
        }
    }

    fn sp_mut<'a>(
        app: &'a mut App,
        env: &str,
        svc: &str,
    ) -> &'a mut crate::pending::ServicePending {
        app.pending.envs.get_mut(env).unwrap().get_mut(svc).unwrap()
    }

    /// The review open on (e-prod, s1 "api"): `D`, the check's reply, then `y`.
    fn deploy_first_group(app: &mut App, current: Variables, now: Instant) -> Vec<Command> {
        let cmds = app.update(key('D'), now);
        assert_eq!(
            cmds,
            vec![Command::LoadVariables {
                project_id: "p2".into(),
                env_id: "e-prod".into(),
                service_id: "s1".into(),
                purpose: VarsPurpose::Deploy,
            }]
        );
        assert!(app.confirm.is_none(), "no confirm before Railway is read");
        assert!(data(app, deploy_vars_event("e-prod", "s1", current), now).is_empty());
        assert!(matches!(app.confirm, Some(Confirm::Deploy(_))));
        app.update(key('y'), now)
    }

    fn deploy_cmd(cmds: &[Command]) -> (Variables, Vec<String>, bool) {
        match cmds {
            [Command::DeployService {
                upserts,
                deletes,
                redeploy_only,
                ..
            }] => (upserts.clone(), deletes.clone(), *redeploy_only),
            other => panic!("expected one DeployService, got {other:?}"),
        }
    }

    fn delete_var(base: &str) -> PendingVar {
        PendingVar {
            change: crate::pending::Change::Delete,
            base: Some(base.into()),
            applied: false,
        }
    }

    #[test]
    fn deploy_targets_group_not_current_env() {
        let now = Instant::now();
        let mut app = app_in_services(now);
        // Switch to staging and queue an edit there.
        app.update(key('e'), now);
        app.update(key('k'), now);
        app.update(code(KeyCode::Enter), now);
        assert_eq!(app.env_id(), Some("e-stg"));
        data(&mut app, services_event("e-stg", Ok(services())), now);
        app.update(key('v'), now);
        app.update(key('a'), now);
        typed(&mut app, "NEW", now);
        app.update(code(KeyCode::Enter), now);
        typed(&mut app, "v", now);
        app.update(code(KeyCode::Enter), now);
        assert!(app.pending.service("e-stg", "svc-a").is_some());
        // Back to production, then deploy the staging group from the review.
        app.update(code(KeyCode::Esc), now);
        app.update(key('e'), now);
        app.update(key('j'), now);
        app.update(code(KeyCode::Enter), now);
        assert_eq!(app.env_id(), Some("e-prod"));
        app.update(key('p'), now);
        let cmds = app.update(key('D'), now);
        assert_eq!(
            cmds,
            vec![Command::LoadVariables {
                project_id: "p2".into(),
                env_id: "e-stg".into(),
                service_id: "svc-a".into(),
                purpose: VarsPurpose::Deploy,
            }]
        );
        data(
            &mut app,
            deploy_vars_event("e-stg", "svc-a", vars(&[])),
            now,
        );
        let cmds = app.update(key('y'), now);
        assert_eq!(
            cmds,
            vec![Command::DeployService {
                project_id: "p2".into(),
                env_id: "e-stg".into(),
                service_id: "svc-a".into(),
                upserts: vars(&[("NEW", "v")]),
                deletes: vec![],
                redeploy_only: false,
            }]
        );
    }

    #[test]
    fn drift_listed_in_confirm() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        let set = |v: &str, base: &str, applied| PendingVar {
            change: crate::pending::Change::Set { value: v.into() },
            base: Some(base.into()),
            applied,
        };
        let sp = sp_mut(&mut app, "e-prod", "s1");
        sp.vars.insert("X".into(), set("new", "old", false));
        sp.vars.insert("Y".into(), delete_var("keep"));
        sp.vars.insert("Z".into(), set("new", "same", false));
        sp.vars.insert("W".into(), set("new", "b", true));
        app.update(key('D'), now);
        // C was added and now exists; X changed; Y is gone; Z is as edited; W is already applied.
        let current = vars(&[("C", "theirs"), ("X", "other"), ("Z", "same"), ("W", "new")]);
        data(&mut app, deploy_vars_event("e-prod", "s1", current), now);
        assert_eq!(
            app.confirm,
            Some(Confirm::Deploy(DeployConfirm {
                env_id: "e-prod".into(),
                service_id: "s1".into(),
                drift: vec!["C".into(), "X".into(), "Y".into()],
            }))
        );
    }

    #[test]
    fn only_y_deploys() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        for cancel in [key('n'), key('Y'), code(KeyCode::Enter), code(KeyCode::Esc)] {
            app.update(key('D'), now);
            data(&mut app, deploy_vars_event("e-prod", "s1", vars(&[])), now);
            assert!(app.confirm.is_some());
            assert!(app.update(cancel, now).is_empty());
            assert!(app.confirm.is_none());
        }
        assert!(matches!(app.stack.last(), Some(View::Pending(_))));
        let cmds = deploy_first_group(&mut app, vars(&[]), now);
        assert_eq!(deploy_cmd(&cmds), (vars(&[("C", "3")]), vec![], false));
    }

    #[test]
    fn d_refused_without_environments_or_while_deploying() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        let envs = app.environments.value.take();
        assert!(app.update(key('D'), now).is_empty());
        assert!(app.message.as_deref().unwrap().contains("not loaded"));
        app.environments.value = envs;
        deploy_first_group(&mut app, vars(&[]), now);
        assert!(app.update(key('D'), now).is_empty());
        assert!(app
            .message
            .as_deref()
            .unwrap()
            .contains("already deploying"));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn steps_mark_applied_then_redeployed_discards_and_reloads() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        add_pending(&mut app, ("e-prod", "production"), ("s1", "api"), "N", "n");
        sp_mut(&mut app, "e-prod", "s1")
            .vars
            .insert("OLD".into(), delete_var("x"));
        let cmds = deploy_first_group(&mut app, vars(&[("OLD", "x")]), now);
        assert_eq!(
            deploy_cmd(&cmds),
            (vars(&[("C", "3"), ("N", "n")]), vec!["OLD".into()], false)
        );
        let cmds = data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
        let sp = app.pending.service("e-prod", "s1").unwrap();
        assert!(sp.vars["C"].applied && sp.vars["N"].applied && !sp.vars["OLD"].applied);
        assert!(!sp.written_not_deployed);
        assert!(matches!(cmds.as_slice(), [Command::SavePending(p)] if p == &app.pending));
        let deleted = DeployStep::Deleted("OLD".into());
        let cmds = data(&mut app, step("e-prod", "s1", deleted), now);
        let sp = app.pending.service("e-prod", "s1").unwrap();
        assert!(sp.vars.values().all(|v| v.applied) && sp.written_not_deployed);
        assert!(matches!(cmds.as_slice(), [Command::SavePending(p)] if p == &app.pending));
        let cmds = data(&mut app, step("e-prod", "s1", DeployStep::Redeployed), now);
        assert!(app.pending.service("e-prod", "s1").is_none());
        assert_eq!(app.message.as_deref(), Some("deployed api to production"));
        assert!(matches!(cmds.first(), Some(Command::SavePending(p)) if p == &app.pending));
        assert!(cmds.contains(&load_services("e-prod")), "{cmds:?}");
    }

    #[test]
    fn edit_during_deploy_is_not_marked_applied() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        deploy_first_group(&mut app, vars(&[]), now);
        // C is edited again while its upsert is on the way.
        sp_mut(&mut app, "e-prod", "s1")
            .vars
            .get_mut("C")
            .unwrap()
            .change = crate::pending::Change::Set { value: "4".into() };
        data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
        assert!(!app.pending.service("e-prod", "s1").unwrap().vars["C"].applied);
        data(&mut app, step("e-prod", "s1", DeployStep::Redeployed), now);
        let sp = app.pending.service("e-prod", "s1").unwrap();
        assert_eq!(sp.vars.len(), 1, "the unsent edit is kept");
        assert!(!sp.written_not_deployed);
    }

    #[test]
    fn redeploy_failure_retry_is_redeploy_only() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        deploy_first_group(&mut app, vars(&[]), now);
        data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
        let failed = DeployStep::Failed {
            during: Phase::Redeploy,
            error: "Railway API returned HTTP 400".into(),
            unknown: false,
        };
        assert!(data(&mut app, step("e-prod", "s1", failed), now).is_empty());
        assert_eq!(
            app.message.as_deref(),
            Some("deploy failed during redeploy: Railway API returned HTTP 400")
        );
        let sp = app.pending.service("e-prod", "s1").unwrap();
        assert!(sp.vars["C"].applied && sp.written_not_deployed);
        // C now matches Railway, but it is applied: no drift.
        let cmds = deploy_first_group(&mut app, vars(&[("C", "3")]), now);
        assert_eq!(deploy_cmd(&cmds), (vars(&[]), vec![], true));
    }

    #[test]
    fn failed_write_keeps_entries_and_retry_sends_only_unapplied() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        sp_mut(&mut app, "e-prod", "s1")
            .vars
            .insert("OLD".into(), delete_var("x"));
        deploy_first_group(&mut app, vars(&[("OLD", "x")]), now);
        data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
        let failed = DeployStep::Failed {
            during: Phase::Delete("OLD".into()),
            error: "Railway API returned HTTP 400".into(),
            unknown: false,
        };
        data(&mut app, step("e-prod", "s1", failed), now);
        assert_eq!(
            app.message.as_deref(),
            Some("deploy failed during delete OLD: Railway API returned HTTP 400")
        );
        let sp = app.pending.service("e-prod", "s1").unwrap();
        assert!(sp.vars["C"].applied && !sp.vars["OLD"].applied && !sp.written_not_deployed);
        let cmds = deploy_first_group(&mut app, vars(&[("OLD", "x"), ("C", "3")]), now);
        assert_eq!(deploy_cmd(&cmds), (vars(&[]), vec!["OLD".into()], false));
    }

    #[test]
    fn unknown_write_reconciles_and_unknown_redeploy_is_reported() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        add_pending(&mut app, ("e-prod", "production"), ("s1", "api"), "N", "n");
        deploy_first_group(&mut app, vars(&[]), now);
        let unknown = DeployStep::Failed {
            during: Phase::Upsert,
            error: "timed out".into(),
            unknown: true,
        };
        let cmds = data(&mut app, step("e-prod", "s1", unknown), now);
        assert_eq!(
            cmds,
            vec![Command::LoadVariables {
                project_id: "p2".into(),
                env_id: "e-prod".into(),
                service_id: "s1".into(),
                purpose: VarsPurpose::Reconcile,
            }]
        );
        // Still in flight until Railway has been read.
        assert!(app.update(key('D'), now).is_empty());
        let reply = DataEvent::Variables {
            env_id: "e-prod".into(),
            service_id: "s1".into(),
            purpose: VarsPurpose::Reconcile,
            result: Ok(vars(&[("C", "3"), ("N", "other")])),
        };
        let cmds = data(&mut app, reply, now);
        let sp = app.pending.service("e-prod", "s1").unwrap();
        assert!(sp.vars["C"].applied && !sp.vars["N"].applied);
        assert!(matches!(cmds.as_slice(), [Command::SavePending(_)]));
        // Retry: only N; then the redeploy's outcome is unknown.
        let cmds = deploy_first_group(&mut app, vars(&[("C", "3")]), now);
        assert_eq!(deploy_cmd(&cmds), (vars(&[("N", "n")]), vec![], false));
        data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
        let unknown = DeployStep::Failed {
            during: Phase::Redeploy,
            error: "timed out".into(),
            unknown: true,
        };
        assert!(data(&mut app, step("e-prod", "s1", unknown), now).is_empty());
        assert_eq!(
            app.message.as_deref(),
            Some("redeploy outcome unknown — check the deployments view")
        );
        let sp = app.pending.service("e-prod", "s1").unwrap();
        assert!(sp.written_not_deployed);
    }

    #[test]
    fn deploy_debug_hides_values() {
        let cmd = Command::DeployService {
            project_id: "p".into(),
            env_id: "e".into(),
            service_id: "s".into(),
            upserts: vars(&[("KEY", "s3cret-value")]),
            deletes: vec!["OLD".into()],
            redeploy_only: false,
        };
        let out = format!("{cmd:?}");
        assert!(out.contains("KEY") && out.contains("OLD"), "{out}");
        assert!(!out.contains("s3cret-value"), "{out}");
        let ev = deploy_vars_event("e", "s", vars(&[("KEY", "s3cret-value")]));
        assert!(!format!("{ev:?}").contains("s3cret-value"));
    }

    fn no_deploy(cmds: &[Command]) -> bool {
        !cmds
            .iter()
            .any(|c| matches!(c, Command::DeployService { .. }))
    }

    #[test]
    fn deploy_check_never_pops_over_typing() {
        let now = Instant::now();
        // Typing a variable value in another view.
        let mut app = app_in_review(now);
        app.update(key('D'), now);
        app.update(code(KeyCode::Esc), now);
        app.update(key('v'), now);
        app.update(key('a'), now);
        typed(&mut app, "KE", now);
        data(&mut app, deploy_vars_event("e-prod", "s1", vars(&[])), now);
        assert!(app.confirm.is_none());
        assert!(app.message.as_deref().unwrap().contains("press D again"));
        assert!(no_deploy(&app.update(key('y'), now)));
        assert_eq!(app.var_input.as_ref().unwrap().text, "KEy");

        // A filter being typed on the review itself, and the command line.
        for start in [key('/'), key(':')] {
            let mut app = app_in_review(now);
            app.update(key('D'), now);
            app.update(start, now);
            data(&mut app, deploy_vars_event("e-prod", "s1", vars(&[])), now);
            assert!(app.confirm.is_none());
            assert!(no_deploy(&app.update(key('y'), now)));
        }

        // Help open on the review.
        let mut app = app_in_review(now);
        app.update(key('D'), now);
        app.update(key('?'), now);
        data(&mut app, deploy_vars_event("e-prod", "s1", vars(&[])), now);
        assert!(app.confirm.is_none());
    }

    #[test]
    fn discard_refused_while_deploying() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        deploy_first_group(&mut app, vars(&[]), now);
        assert!(app.update(key('x'), now).is_empty());
        assert!(app.confirm.is_none());
        assert!(app.message.as_deref().unwrap().contains("deploying"));
        assert!(app.pending.service("e-prod", "s1").is_some());
    }

    const DEPLOY_IN_PROGRESS: &str =
        "a deploy is in progress — wait for it to finish, or press ctrl-c twice to force quit";

    #[test]
    fn quit_refused_while_deploying_and_ctrl_c_twice_forces_it() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        deploy_first_group(&mut app, vars(&[]), now);
        assert!(app.update(key('q'), now).is_empty());
        assert_eq!(app.message.as_deref(), Some(DEPLOY_IN_PROGRESS));
        typed(&mut app, ":q", now);
        assert!(app.update(code(KeyCode::Enter), now).is_empty());
        assert_eq!(app.message.as_deref(), Some(DEPLOY_IN_PROGRESS));
        assert!(app.update(ctrl('c'), now).is_empty());
        assert_eq!(app.message.as_deref(), Some(DEPLOY_IN_PROGRESS));
        assert_eq!(app.update(ctrl('c'), now), vec![Command::Quit]);
    }

    #[test]
    fn ctrl_c_arming_ends_with_the_deploy() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        deploy_first_group(&mut app, vars(&[]), now);
        assert!(app.update(ctrl('c'), now).is_empty());
        let failed = DeployStep::Failed {
            during: Phase::Upsert,
            error: "Railway API returned HTTP 400".into(),
            unknown: false,
        };
        data(&mut app, step("e-prod", "s1", failed), now);
        // A new deploy is a new in-flight period: one ctrl-c only warns.
        deploy_first_group(&mut app, vars(&[]), now);
        assert!(app.update(ctrl('c'), now).is_empty());
        assert_eq!(app.message.as_deref(), Some(DEPLOY_IN_PROGRESS));
        data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
        data(&mut app, step("e-prod", "s1", DeployStep::Redeployed), now);
        assert_eq!(app.update(key('q'), now), vec![Command::Quit]);
    }

    #[test]
    fn progress_is_shown_step_by_step() {
        let now = Instant::now();
        let mut app = app_in_review(now);
        sp_mut(&mut app, "e-prod", "s1")
            .vars
            .insert("OLD".into(), delete_var("x"));
        deploy_first_group(&mut app, vars(&[("OLD", "x")]), now);
        assert_eq!(app.message.as_deref(), Some("deploying api to production…"));
        data(&mut app, step("e-prod", "s1", DeployStep::Upserted), now);
        assert_eq!(
            app.message.as_deref(),
            Some("deploying api to production: variables written…")
        );
        let deleted = DeployStep::Deleted("OLD".into());
        data(&mut app, step("e-prod", "s1", deleted), now);
        assert_eq!(
            app.message.as_deref(),
            Some("deploying api to production: OLD deleted…")
        );
    }
}
