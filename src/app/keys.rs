//! Key handling: Ctrl-C, then any modal input (command line, filter, env picker),
//! then global keys, then the top view's keys, then table movement and Esc.

use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{
    position, App, Command, Confirm, Kind, LogsView, Modal, Orphan, PendingGroup, Secret, VarInput,
    View,
};
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
            if !self.deploy_in_flight() || self.force_quit_armed {
                out.push(Command::Quit);
            } else {
                self.force_quit_armed = true;
                self.message = Some(DEPLOY_IN_PROGRESS.into());
            }
            return;
        }
        match self.modal {
            Some(Modal::Confirm(_)) => self.confirm_key(key.code, out),
            Some(Modal::VarInput(_)) => self.var_input_key(key, out),
            Some(Modal::Command(_)) => self.command_key(key, now, out),
            Some(Modal::Filter(_)) => self.filter_key(key),
            Some(Modal::BranchPicker(_)) => self.branch_picker_key(key),
            Some(Modal::EnvPicker(_)) => self.picker_key(key.code, now, out),
            Some(Modal::ThemePicker { .. }) => self.theme_key(key.code, out),
            None if ctrl => {
                if key.code == KeyCode::Char('r') {
                    self.reload_current(now, out);
                }
            }
            None => match key.code {
                KeyCode::Char('q') => self.quit(out),
                KeyCode::Char(':') => self.modal = Some(Modal::Command(String::new())),
                KeyCode::Char('/') => {
                    self.set_visual_anchor(None);
                    let current = match self.logs_mut() {
                        Some(logs) => logs.filter.clone(),
                        None => self.table_filter.clone(),
                    };
                    self.modal = Some(Modal::Filter(current.unwrap_or_default()));
                }
                KeyCode::Char('?') => self.help = !self.help,
                KeyCode::Char('t') => self.open_theme_picker(),
                KeyCode::Esc if self.help => self.help = false,
                code => self.view_key(code, now, out),
            },
        }
    }

    fn open_theme_picker(&mut self) {
        self.modal = Some(Modal::ThemePicker {
            restore: self.theme,
        });
    }

    /// `q` and `:q`: refused while a deploy is in flight (Ctrl-C twice forces it).
    fn quit(&mut self, out: &mut Vec<Command>) {
        if !self.deploy_in_flight() {
            out.push(Command::Quit);
        } else {
            self.message = Some(DEPLOY_IN_PROGRESS.into());
        }
    }

    fn command_key(&mut self, key: KeyEvent, now: Instant, out: &mut Vec<Command>) {
        let Some(Modal::Command(text)) = &mut self.modal else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let text = std::mem::take(text);
                self.modal = None;
                self.run_command(&text, now, out);
            }
            KeyCode::Esc => self.modal = None,
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
            "theme" => self.open_theme_picker(),
            "" => {}
            other => self.message = Some(format!("unknown command: {other}")),
        }
    }

    fn filter_key(&mut self, key: KeyEvent) {
        let Some(Modal::Filter(text)) = &mut self.modal else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let text = std::mem::take(text);
                self.modal = None;
                let filter = (!text.is_empty()).then_some(text);
                match self.logs_mut() {
                    Some(logs) => logs.filter = filter,
                    None => self.table_filter = filter,
                }
            }
            KeyCode::Esc => self.modal = None,
            _ => edit_text(text, key),
        }
    }

    fn picker_key(&mut self, code: KeyCode, now: Instant, out: &mut Vec<Command>) {
        let Some(Modal::EnvPicker(index)) = self.modal else {
            return;
        };
        let Some(envs) = self.environments.value.as_ref() else {
            self.modal = None;
            return;
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                let next = (index + 1).min(envs.len().saturating_sub(1));
                self.modal = Some(Modal::EnvPicker(next));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.modal = Some(Modal::EnvPicker(index.saturating_sub(1)));
            }
            KeyCode::Esc => self.modal = None,
            KeyCode::Enter => {
                let chosen = envs.get(index).cloned();
                self.modal = None;
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
        let Some(Modal::ThemePicker { restore }) = self.modal else {
            return;
        };
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.theme = (self.theme + 1).min(THEMES.len() - 1);
            }
            KeyCode::Char('k') | KeyCode::Up => self.theme = self.theme.saturating_sub(1),
            KeyCode::Esc => {
                self.theme = restore;
                self.modal = None;
            }
            KeyCode::Enter => {
                self.modal = None;
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
            // Views that bind `r` themselves (reveal) keep it; Ctrl-R still reloads there.
            KeyCode::Char('r') => self.reload_current(now, out),
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
            KeyCode::Char('D') => match selected {
                Some(id) => self.request_deploy_latest(id),
                None => self.message = Some("no service selected".into()),
            },
            KeyCode::Char('d') => match selected {
                Some(id) => self.open_branch_picker(id, out),
                None => self.message = Some("no service selected".into()),
            },
            _ => return false,
        }
        true
    }

    /// Typing filters the branches; arrows move, Enter picks, Esc closes.
    fn branch_picker_key(&mut self, key: KeyEvent) {
        let Some(Modal::BranchPicker(picker)) = &mut self.modal else {
            return;
        };
        let last = picker.filtered().len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => self.modal = None,
            KeyCode::Enter => self.pick_branch(),
            KeyCode::Down => picker.index = (picker.index + 1).min(last),
            KeyCode::Up => picker.index = picker.index.saturating_sub(1),
            _ => {
                edit_text(&mut picker.query, key);
                picker.index = 0;
            }
        }
    }

    fn open_env_picker(&mut self, now: Instant, out: &mut Vec<Command>) {
        match self.environments.value.as_deref() {
            Some(envs) if !envs.is_empty() => {
                let current = envs
                    .iter()
                    .position(|e| Some(e.id.as_str()) == self.env_id());
                self.modal = Some(Modal::EnvPicker(current.unwrap_or(0)));
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
        if let KeyCode::Char(c @ ('D' | 'd')) = code {
            if let Some(id) = self.view_service_id(Kind::Deployments).map(String::from) {
                if c == 'D' {
                    self.request_deploy_latest(id);
                } else {
                    self.open_branch_picker(id, out);
                }
            }
            return true;
        }
        if code != KeyCode::Enter {
            return false;
        }
        let selected = self.selected_deployment();
        if let Some((id, kind)) = selected.map(|d| (d.id.clone(), default_log_kind(d))) {
            let name = self.view_service_name(Kind::Deployments);
            self.open_logs(id, name, kind, None, out);
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
        // An anchor whose row is gone (or filtered out) counts as none.
        if self.visual_range().is_none() {
            self.set_visual_anchor(None);
        } else if let Some(handled) = self.visual_key(code, out) {
            return handled;
        }
        let edits = matches!(
            code,
            KeyCode::Char('a' | 'e' | 'E' | 'd' | 'u') | KeyCode::Enter
        );
        if edits && self.refuse_while_deploying() {
            return true;
        }
        match code {
            KeyCode::Char('a') => self.modal = Some(Modal::VarInput(VarInput::default())),
            KeyCode::Char('e') | KeyCode::Enter => {
                if let Some(name) = self.selected_var_name() {
                    self.start_inline_edit(name);
                }
            }
            KeyCode::Char('E') => {
                if let Some(name) = self.selected_var_name() {
                    let initial = self.edit_start_value(&name);
                    out.push(Command::EditExternal {
                        name,
                        initial: Secret::new(initial),
                    });
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
            KeyCode::Char(c @ ('y' | 'Y')) => {
                if let Some(name) = self.selected_var_name() {
                    self.copy_rows(&[name], c == 'Y', out);
                }
            }
            KeyCode::Char('v') => {
                let name = self.selected_var_name();
                self.set_visual_anchor(name);
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

    /// Keys while a visual range is active. None leaves the key to the normal handling
    /// (movement, `p`, `R`); the range follows the cursor.
    fn visual_key(&mut self, code: KeyCode, out: &mut Vec<Command>) -> Option<bool> {
        let names: Vec<String> = {
            let range = self.visual_range()?;
            let rows = self.variable_rows();
            rows[range].iter().map(|r| r.name.to_string()).collect()
        };
        match code {
            KeyCode::Char('v') | KeyCode::Esc => {}
            KeyCode::Char(c @ ('y' | 'Y')) => self.copy_rows(&names, c == 'Y', out),
            KeyCode::Char('d') => {
                if self.refuse_while_deploying() {
                    return Some(true);
                }
                let mut staged = false;
                for name in &names {
                    if self.var_state(name).1 != Some(Change::Delete) {
                        staged |= self.stage_delete(name);
                    }
                }
                if staged {
                    out.push(self.save_pending());
                }
            }
            KeyCode::Char('r') => {
                if let Some(View::Variables(v)) = self.stack.last_mut() {
                    if names.iter().all(|n| v.revealed.contains(n)) {
                        for name in &names {
                            v.revealed.remove(name);
                        }
                    } else {
                        v.revealed.extend(names);
                    }
                }
            }
            // Single-row edits make no sense over a range.
            KeyCode::Char('a' | 'e' | 'E' | 'u') | KeyCode::Enter => return Some(true),
            _ => return None,
        }
        self.set_visual_anchor(None);
        Some(true)
    }

    fn set_visual_anchor(&mut self, name: Option<String>) {
        if let Some(View::Variables(v)) = self.stack.last_mut() {
            v.anchor = name;
        }
    }

    /// Copies the shown value (a pending `Set`, else the current one) of each name, raw
    /// with `as_env` false, else as `NAME=VALUE` lines. Sealed rows are skipped. Pushes no
    /// command when nothing is copyable. The message names what was sent, never a value.
    pub(super) fn copy_rows(&mut self, names: &[String], as_env: bool, out: &mut Vec<Command>) {
        let rows = self.variable_rows();
        let mut lines = Vec::new();
        let mut sealed = 0;
        for name in names {
            let Some(row) = rows.iter().find(|r| r.name == name) else {
                continue;
            };
            if row.sealed {
                sealed += 1;
                continue;
            }
            let value = match row.pending.map(|p| &p.change) {
                Some(Change::Set { value }) => Some(value.as_str()),
                _ => row.current,
            };
            if let Some(value) = value {
                lines.push(if as_env {
                    crate::clipboard::env_line(name, value)
                } else {
                    value.to_string()
                });
            }
        }
        let copied = lines.len();
        self.message = Some(match names {
            [name] if sealed == 1 => format!("{name} is sealed \u{2014} nothing to copy"),
            [name] if copied == 0 => format!("{name} has no value to copy"),
            [name] if as_env => format!("copied {name}"),
            [name] => format!("copied value of {name}"),
            _ if sealed > 0 => format!("copied {copied} variables ({sealed} sealed skipped)"),
            _ => format!("copied {copied} variables"),
        });
        if copied > 0 {
            out.push(Command::Copy {
                text: Secret::new(lines.join("\n")),
            });
        }
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
                    if self.is_deploying(&g.key.env_id, &g.key.service_id) {
                        self.message = Some("cannot discard while it is deploying".into());
                        return true;
                    }
                    self.modal = Some(Modal::Confirm(Confirm::Discard {
                        env_id: g.key.env_id,
                        service_id: g.key.service_id,
                    }));
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
                Some(g) => out.extend(self.request_deploy(g.key.env_id, g.key.service_id)),
                None => {}
            },
            _ => return false,
        }
        true
    }

    /// `y` answers the question; any other key cancels it.
    fn confirm_key(&mut self, code: KeyCode, out: &mut Vec<Command>) {
        let Some(Modal::Confirm(confirm)) = self.modal.take() else {
            return;
        };
        if code != KeyCode::Char('y') {
            return;
        }
        match confirm {
            Confirm::Discard { env_id, service_id } => {
                self.pending_mut().discard(&env_id, &service_id);
                out.push(self.save_pending());
            }
            Confirm::Deploy(d) => self.start_deploy(d, out),
            Confirm::DeployLatest(c) => self.start_deploy_latest(*c, out),
        }
    }

    /// Keys while a variable name or value is being typed.
    fn var_input_key(&mut self, key: KeyEvent, out: &mut Vec<Command>) {
        let Some(Modal::VarInput(input)) = &mut self.modal else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.modal = None,
            KeyCode::Enter => match input.name.clone() {
                None => {
                    let name = input.text.expose().clone();
                    if !valid_var_name(&name) {
                        self.message = Some(
                            "invalid name: use letters, digits and _, not starting with a digit"
                                .into(),
                        );
                        return;
                    }
                    self.modal = None;
                    self.selection.variable = Some(name.clone());
                    self.start_inline_edit(name);
                }
                Some(name) => {
                    let value = std::mem::take(&mut input.text).into_inner();
                    self.modal = None;
                    self.set_variable(&name, value, out);
                }
            },
            _ => edit_text(input.text.expose_mut(), key),
        }
    }

    fn selected_var_name(&self) -> Option<String> {
        let rows = self.variable_rows();
        let selected = self.selection.variable.as_deref();
        rows.get(position(&rows, selected, |r| r.name))
            .map(|r| r.name.to_string())
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
        self.modal = Some(Modal::VarInput(VarInput {
            name: Some(name),
            text: Secret::new(text),
        }));
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
        f(self.pending_mut(), &target);
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
            out.push(self.save_pending());
        }
    }

    fn toggle_delete(&mut self, name: &str, out: &mut Vec<Command>) {
        let pending = self.var_state(name).1;
        if pending == Some(Change::Delete) {
            self.undo_variable(name, out);
        } else if self.stage_delete(name) {
            out.push(self.save_pending());
        }
    }

    /// Marks `name` for deletion; the caller saves. False without a target.
    fn stage_delete(&mut self, name: &str) -> bool {
        let current = self.var_state(name).0;
        // A sealed variable the value map omits still exists on Railway.
        let current = current.or_else(|| self.sealed_names().contains(name).then(String::new));
        self.with_target(|p, t| p.delete(t, name, current.as_deref()))
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
            self.pending_mut().undo(&env, &svc, name);
            out.push(self.save_pending());
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
