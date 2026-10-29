//! Key handling: Ctrl-C, then any modal input (command line, filter, env picker),
//! then global keys, then the top view's keys, then table movement and Esc.

use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, Command, Confirm, Kind, LogsView, Orphan, PendingGroup, VarInput, View};
use crate::model::LogKind;
use crate::pending::{Change, Pending, Target};
use crate::status::default_log_kind;
use crate::theme::THEMES;

const DEPLOY_IN_PROGRESS: &str =
    "a deploy is in progress — wait for it to finish, or press ctrl-c twice to force quit";

/// Lines moved by PgUp/PgDn in the log view.
const PAGE: usize = 20;

impl App {
    pub(super) fn on_key(&mut self, key: KeyEvent, now: Instant, out: &mut Vec<Command>) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.message = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.deploys.is_empty() || self.force_quit_armed {
                out.push(Command::Quit);
            } else {
                self.force_quit_armed = true;
                self.message = Some(DEPLOY_IN_PROGRESS.into());
            }
        } else if self.confirm.is_some() {
            self.confirm_key(key.code, out);
        } else if self.var_input.is_some() {
            self.var_input_key(key, out);
        } else if self.command_line.is_some() {
            self.command_key(key, now, out);
        } else if self.filter_input.is_some() {
            self.filter_key(key);
        } else if self.env_picker.is_some() {
            self.picker_key(key.code, now, out);
        } else if self.theme_picker.is_some() {
            self.theme_key(key.code, out);
        } else if ctrl {
            if key.code == KeyCode::Char('r') {
                self.reload_current(now, out);
            }
        } else {
            match key.code {
                KeyCode::Char('q') => self.quit(out),
                KeyCode::Char(':') => self.command_line = Some(String::new()),
                KeyCode::Char('/') => {
                    let current = match self.logs_mut() {
                        Some(logs) => logs.filter.clone(),
                        None => self.table_filter.clone(),
                    };
                    self.filter_input = Some(current.unwrap_or_default());
                }
                KeyCode::Char('?') => self.help = !self.help,
                KeyCode::Char('t') => self.theme_picker = Some(self.theme),
                KeyCode::Esc if self.help => self.help = false,
                code => self.view_key(code, now, out),
            }
        }
    }

    /// `q` and `:q`: refused while a deploy is in flight (Ctrl-C twice forces it).
    fn quit(&mut self, out: &mut Vec<Command>) {
        if self.deploys.is_empty() {
            out.push(Command::Quit);
        } else {
            self.message = Some(DEPLOY_IN_PROGRESS.into());
        }
    }

    fn command_key(&mut self, key: KeyEvent, now: Instant, out: &mut Vec<Command>) {
        let Some(text) = self.command_line.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let text = std::mem::take(text);
                self.command_line = None;
                self.run_command(&text, now, out);
            }
            KeyCode::Esc => self.command_line = None,
            _ => edit_text(text, key),
        }
    }

    fn run_command(&mut self, text: &str, now: Instant, out: &mut Vec<Command>) {
        match text.trim().trim_start_matches(':') {
            "q" | "quit" => self.quit(out),
            "projects" => self.truncate(1, out),
            "services" => {
                self.goto_services(out);
            }
            "deploys" => {
                if self.goto_services(out) {
                    match self.selected_service().map(|s| s.service_id.clone()) {
                        Some(id) => self.open_deployments(id, now, out),
                        None => self.message = Some("no service selected".into()),
                    }
                }
            }
            "logs" => {
                if self.kind() != Kind::Logs && self.goto_services(out) {
                    self.open_latest_logs(out);
                }
            }
            "vars" => {
                if self.goto_services(out) {
                    match self.selected_service().map(|s| s.service_id.clone()) {
                        Some(id) => self.open_variables(id, now, out),
                        None => self.message = Some("no service selected".into()),
                    }
                }
            }
            "pending" => {
                if self.goto_services(out) {
                    self.open_pending(out);
                }
            }
            "theme" => self.theme_picker = Some(self.theme),
            "" => {}
            other => self.message = Some(format!("unknown command: {other}")),
        }
    }

    fn filter_key(&mut self, key: KeyEvent) {
        let Some(text) = self.filter_input.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let text = std::mem::take(text);
                self.filter_input = None;
                let filter = (!text.is_empty()).then_some(text);
                match self.logs_mut() {
                    Some(logs) => logs.filter = filter,
                    None => self.table_filter = filter,
                }
            }
            KeyCode::Esc => self.filter_input = None,
            _ => edit_text(text, key),
        }
    }

    fn picker_key(&mut self, code: KeyCode, now: Instant, out: &mut Vec<Command>) {
        let (Some(index), Some(envs)) = (self.env_picker, self.environments.value.as_ref()) else {
            self.env_picker = None;
            return;
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.env_picker = Some((index + 1).min(envs.len().saturating_sub(1)));
            }
            KeyCode::Char('k') | KeyCode::Up => self.env_picker = Some(index.saturating_sub(1)),
            KeyCode::Esc => self.env_picker = None,
            KeyCode::Enter => {
                let chosen = envs.get(index).cloned();
                self.env_picker = None;
                if let Some(env) = chosen {
                    if self.env_id() != Some(env.id.as_str()) {
                        if self.kind() == Kind::Services {
                            out.push(Command::WatchDeployments { ids: Vec::new() });
                        }
                        self.set_environment(env, now, out);
                    }
                }
            }
            _ => {}
        }
    }

    /// Moving previews a theme; Enter keeps and saves it, Esc restores the previous one.
    fn theme_key(&mut self, code: KeyCode, out: &mut Vec<Command>) {
        let Some(original) = self.theme_picker else {
            return;
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.theme = (self.theme + 1).min(THEMES.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => self.theme = self.theme.saturating_sub(1),
            KeyCode::Esc => {
                self.theme = original;
                self.theme_picker = None;
            }
            KeyCode::Enter => {
                self.theme_picker = None;
                let id = self.theme().id.to_string();
                if self.prefs.remembered.theme.as_deref() != Some(id.as_str()) {
                    self.prefs.remembered.theme = Some(id);
                    out.push(Command::SaveRemembered(self.prefs.remembered.clone()));
                }
            }
            _ => {}
        }
    }

    fn view_key(&mut self, code: KeyCode, now: Instant, out: &mut Vec<Command>) {
        let handled = match self.kind() {
            Kind::Projects => self.projects_key(code, now, out),
            Kind::Services => self.services_key(code, now, out),
            Kind::Deployments => self.deployments_key(code, out),
            Kind::Instances => self.instances_key(code, out),
            Kind::Variables => self.variables_key(code, out),
            Kind::Pending => self.pending_key(code, out),
            Kind::Logs => self.logs_key(code, out),
        };
        if handled {
            return;
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Esc if self.table_filter.is_some() => self.table_filter = None,
            KeyCode::Esc => self.pop(out),
            _ => {}
        }
    }

    fn projects_key(&mut self, code: KeyCode, now: Instant, out: &mut Vec<Command>) -> bool {
        if code != KeyCode::Enter {
            return false;
        }
        if let Some((workspace_id, project)) = self.selected_project() {
            self.open_project(workspace_id, project, now, out);
        }
        true
    }

    fn services_key(&mut self, code: KeyCode, now: Instant, out: &mut Vec<Command>) -> bool {
        let selected = self.selected_service().map(|s| s.service_id.clone());
        match code {
            KeyCode::Enter => {
                if let Some(id) = selected {
                    self.open_deployments(id, now, out);
                }
            }
            KeyCode::Char('l') => self.open_latest_logs(out),
            KeyCode::Char('i') => {
                if let Some(service_id) = selected {
                    self.selection.instance = None;
                    self.push(View::Instances { service_id }, out);
                }
            }
            KeyCode::Char('v') => match selected {
                Some(id) => self.open_variables(id, now, out),
                None => self.message = Some("no service selected".into()),
            },
            KeyCode::Char('p') => self.open_pending(out),
            KeyCode::Char('e') => self.open_env_picker(now, out),
            _ => return false,
        }
        true
    }

    fn open_env_picker(&mut self, now: Instant, out: &mut Vec<Command>) {
        match self.environments.value.as_deref() {
            Some(envs) if !envs.is_empty() => {
                let current = envs
                    .iter()
                    .position(|e| Some(e.id.as_str()) == self.env_id());
                self.env_picker = Some(current.unwrap_or(0));
            }
            _ => {
                self.message = Some("environments are not loaded yet".into());
                if !self.environments.loading {
                    self.load_environments(now, out);
                }
            }
        }
    }

    fn deployments_key(&mut self, code: KeyCode, out: &mut Vec<Command>) -> bool {
        if code != KeyCode::Enter {
            return false;
        }
        if let Some(deployment) = self.selected_deployment() {
            let name = self.view_service_name(Kind::Deployments);
            let kind = default_log_kind(&deployment);
            self.open_logs(deployment.id, name, kind, None, out);
        }
        true
    }

    fn instances_key(&mut self, code: KeyCode, out: &mut Vec<Command>) -> bool {
        if code != KeyCode::Enter {
            return false;
        }
        if let Some((deployment_id, instance_id)) = self.selected_instance() {
            let name = self.view_service_name(Kind::Instances);
            self.open_logs(
                deployment_id,
                name,
                LogKind::Runtime,
                Some(instance_id),
                out,
            );
        }
        true
    }

    fn logs_key(&mut self, code: KeyCode, out: &mut Vec<Command>) -> bool {
        match code {
            KeyCode::Char('b') => self.toggle_log_kind(out),
            KeyCode::Esc => {
                let cleared = self
                    .logs_mut()
                    .is_some_and(|logs| logs.filter.take().is_some());
                if !cleared {
                    self.pop(out);
                }
            }
            _ => {
                let Some(logs) = self.logs_mut() else {
                    return false;
                };
                match code {
                    KeyCode::Char('k') | KeyCode::Up => logs.scroll_up(1),
                    KeyCode::PageUp => logs.scroll_up(PAGE),
                    KeyCode::Char('j') | KeyCode::Down => logs.scroll_down(1),
                    KeyCode::PageDown => logs.scroll_down(PAGE),
                    KeyCode::Char('G') | KeyCode::Char('f') => {
                        logs.follow = true;
                        logs.scroll = 0;
                    }
                    _ => return false,
                }
            }
        }
        true
    }

    fn variables_key(&mut self, code: KeyCode, out: &mut Vec<Command>) -> bool {
        let edits = matches!(
            code,
            KeyCode::Char('a' | 'e' | 'E' | 'd' | 'u') | KeyCode::Enter
        );
        if edits && self.refuse_while_deploying() {
            return true;
        }
        match code {
            KeyCode::Char('a') => self.var_input = Some(VarInput::default()),
            KeyCode::Char('e') | KeyCode::Enter => {
                if let Some(name) = self.selected_var_name() {
                    self.start_inline_edit(name);
                }
            }
            KeyCode::Char('E') => {
                if let Some(name) = self.selected_var_name() {
                    let initial = self.edit_start_value(&name);
                    out.push(Command::EditExternal { name, initial });
                }
            }
            KeyCode::Char('d') => {
                if let Some(name) = self.selected_var_name() {
                    self.toggle_delete(&name, out);
                }
            }
            KeyCode::Char('u') => {
                if let Some(name) = self.selected_var_name() {
                    self.undo_variable(&name, out);
                }
            }
            KeyCode::Char('r') => {
                if let Some(name) = self.selected_var_name() {
                    if let Some(View::Variables(v)) = self.stack.last_mut() {
                        if !v.revealed.remove(&name) {
                            v.revealed.insert(name);
                        }
                    }
                }
            }
            KeyCode::Char('p') => self.open_pending(out),
            KeyCode::Char('R') => {
                if let Some(View::Variables(v)) = self.stack.last_mut() {
                    v.reveal_all = !v.reveal_all;
                }
            }
            _ => return false,
        }
        true
    }

    fn pending_key(&mut self, code: KeyCode, out: &mut Vec<Command>) -> bool {
        match code {
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if let Some(View::Pending(v)) = self.stack.last_mut() {
                    v.reveal_all = !v.reveal_all;
                }
            }
            KeyCode::Char('x') => {
                if let Some(g) = self.selected_group() {
                    if self.is_deploying(&g.env_id, &g.service_id) {
                        self.message = Some("cannot discard while it is deploying".into());
                        return true;
                    }
                    self.confirm = Some(Confirm::Discard {
                        env_id: g.env_id,
                        service_id: g.service_id,
                    });
                }
            }
            KeyCode::Char('D') => match self.selected_group() {
                Some(PendingGroup {
                    orphaned: Some(orphan),
                    ..
                }) => {
                    let gone = match orphan {
                        Orphan::Environment => "environment",
                        Orphan::Service => "service",
                    };
                    self.message = Some(format!("cannot deploy: the {gone} no longer exists"));
                }
                Some(g) => out.extend(self.request_deploy(g.env_id, g.service_id)),
                None => {}
            },
            _ => return false,
        }
        true
    }

    /// `y` answers the question; any other key cancels it.
    fn confirm_key(&mut self, code: KeyCode, out: &mut Vec<Command>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        if code != KeyCode::Char('y') {
            return;
        }
        match confirm {
            Confirm::Discard { env_id, service_id } => {
                self.pending.discard(&env_id, &service_id);
                out.push(Command::SavePending(self.pending.clone()));
            }
            Confirm::Deploy(d) => self.start_deploy(d, out),
        }
    }

    /// Keys while a variable name or value is being typed.
    fn var_input_key(&mut self, key: KeyEvent, out: &mut Vec<Command>) {
        let Some(input) = self.var_input.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.var_input = None,
            KeyCode::Enter => match input.name.clone() {
                None => {
                    let name = input.text.clone();
                    if !valid_var_name(&name) {
                        self.message = Some(
                            "invalid name: use letters, digits and _, not starting with a digit"
                                .into(),
                        );
                        return;
                    }
                    self.var_input = None;
                    self.selection.variable = Some(name.clone());
                    self.start_inline_edit(name);
                }
                Some(name) => {
                    let value = std::mem::take(&mut input.text);
                    self.var_input = None;
                    self.set_variable(&name, value, out);
                }
            },
            _ => edit_text(&mut input.text, key),
        }
    }

    fn selected_var_name(&self) -> Option<String> {
        let rows = self.variable_rows();
        let selected = self.selection.variable.as_deref();
        let index = rows
            .iter()
            .position(|r| Some(r.name) == selected)
            .unwrap_or(0);
        rows.get(index).map(|r| r.name.to_string())
    }

    /// The value on Railway and the pending change of `name` (unfiltered).
    fn var_state(&self, name: &str) -> (Option<String>, Option<Change>) {
        let current = self
            .variables
            .value
            .as_ref()
            .and_then(|v| v.get(name))
            .cloned();
        let pending = self
            .env_id()
            .zip(self.view_service_id(Kind::Variables))
            .and_then(|(env, svc)| self.pending.service(env, svc))
            .and_then(|s| s.vars.get(name))
            .map(|p| p.change.clone());
        (current, pending)
    }

    /// Where an edit starts: the pending value, else Railway's (also for a variable
    /// marked for deletion), else empty.
    fn edit_start_value(&self, name: &str) -> String {
        match self.var_state(name) {
            (_, Some(Change::Set { value })) => value,
            (current, _) => current.unwrap_or_default(),
        }
    }

    fn start_inline_edit(&mut self, name: String) {
        let text = self.edit_start_value(&name);
        if text.contains('\n') {
            self.message = Some("multi-line value: press E to edit in $EDITOR".into());
            return;
        }
        self.var_input = Some(VarInput {
            name: Some(name),
            text,
        });
    }

    /// Runs `f` with the pending store and the open service as the target; false when
    /// no service of an open environment is showing.
    fn with_target(&mut self, f: impl FnOnce(&mut Pending, &Target)) -> bool {
        let (Some(project), Some(env), Some(service_id)) = (
            self.project.clone(),
            self.environment.clone(),
            self.view_service_id(Kind::Variables).map(String::from),
        ) else {
            return false;
        };
        let service_name = self
            .service(&service_id)
            .map_or_else(|| service_id.clone(), |s| s.service_name.clone());
        let target = Target {
            project_id: &project.id,
            env_id: &env.id,
            env_name: &env.name,
            service_id: &service_id,
            service_name: &service_name,
        };
        f(&mut self.pending, &target);
        true
    }

    /// True (with a message) when the open service has a deploy in flight: its
    /// pending entries are being written and must not change underneath it.
    fn refuse_while_deploying(&mut self) -> bool {
        let deploying = self
            .env_id()
            .zip(self.view_service_id(Kind::Variables))
            .is_some_and(|(env, svc)| self.is_deploying(env, svc));
        if deploying {
            let name = self.view_service_name(Kind::Variables);
            self.message = Some(format!(
                "{name} is deploying — wait for it to finish before changing its variables"
            ));
        }
        deploying
    }

    pub(super) fn set_variable(&mut self, name: &str, value: String, out: &mut Vec<Command>) {
        if self.refuse_while_deploying() {
            return;
        }
        let (current, _) = self.var_state(name);
        if self.with_target(|p, t| p.set(t, name, value, current.as_deref())) {
            out.push(Command::SavePending(self.pending.clone()));
        }
    }

    fn toggle_delete(&mut self, name: &str, out: &mut Vec<Command>) {
        let (current, pending) = self.var_state(name);
        if pending == Some(Change::Delete) {
            self.undo_variable(name, out);
        } else if self.with_target(|p, t| p.delete(t, name, current.as_deref())) {
            out.push(Command::SavePending(self.pending.clone()));
        }
    }

    /// Forgets the pending change of `name`, unless Railway already holds it:
    /// forgetting that would leave the written value to ship with the next deploy.
    fn undo_variable(&mut self, name: &str, out: &mut Vec<Command>) {
        if let (Some(env), Some(svc)) = (
            self.env_id().map(String::from),
            self.view_service_id(Kind::Variables).map(String::from),
        ) {
            let Some(var) = self
                .pending
                .service(&env, &svc)
                .and_then(|s| s.vars.get(name))
            else {
                return;
            };
            if var.applied {
                self.message = Some("already written to Railway — edit it to revert".into());
                return;
            }
            self.pending.undo(&env, &svc, name);
            out.push(Command::SavePending(self.pending.clone()));
        }
    }

    fn view_service_name(&self, kind: Kind) -> String {
        let id = self.view_service_id(kind).unwrap_or_default();
        self.service(id)
            .map_or_else(|| id.to_string(), |s| s.service_name.clone())
    }
}

impl LogsView {
    /// Scrolling up pauses follow; it stops at the first line.
    fn scroll_up(&mut self, lines: usize) {
        self.follow = false;
        let max = self.visible_len().saturating_sub(1);
        self.scroll = (self.scroll + lines).min(max);
    }

    /// Scrolling down never resumes follow on its own; `G` or `f` does.
    fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }
}

/// A variable name Railway accepts: `^[A-Za-z_][A-Za-z0-9_]*$`.
fn valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Typing into a one-line input: printable characters and Backspace.
fn edit_text(text: &mut String, key: KeyEvent) {
    let plain = !key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if plain => text.push(c),
        KeyCode::Backspace => {
            text.pop();
        }
        _ => {}
    }
}
