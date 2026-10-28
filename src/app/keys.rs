//! Key handling: Ctrl-C, then any modal input (command line, filter, env picker),
//! then global keys, then the top view's keys, then table movement and Esc.

use std::time::Instant;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, Command, Kind, LogsView, View};
use crate::model::LogKind;
use crate::status::default_log_kind;

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
            out.push(Command::Quit);
        } else if self.command_line.is_some() {
            self.command_key(key, now, out);
        } else if self.filter_input.is_some() {
            self.filter_key(key);
        } else if self.env_picker.is_some() {
            self.picker_key(key.code, now, out);
        } else if ctrl {
            if key.code == KeyCode::Char('r') {
                self.reload_current(now, out);
            }
        } else {
            match key.code {
                KeyCode::Char('q') => out.push(Command::Quit),
                KeyCode::Char(':') => self.command_line = Some(String::new()),
                KeyCode::Char('/') => {
                    let current = match self.logs_mut() {
                        Some(logs) => logs.filter.clone(),
                        None => self.table_filter.clone(),
                    };
                    self.filter_input = Some(current.unwrap_or_default());
                }
                KeyCode::Char('?') => self.help = !self.help,
                KeyCode::Esc if self.help => self.help = false,
                code => self.view_key(code, now, out),
            }
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
            "q" | "quit" => out.push(Command::Quit),
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

    fn view_key(&mut self, code: KeyCode, now: Instant, out: &mut Vec<Command>) {
        let handled = match self.kind() {
            Kind::Projects => self.projects_key(code, now, out),
            Kind::Services => self.services_key(code, now, out),
            Kind::Deployments => self.deployments_key(code, out),
            Kind::Instances => self.instances_key(code, out),
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
