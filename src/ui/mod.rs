//! Rendering: a pure function of `App`.

mod logs;
mod pending;
mod tables;
#[cfg(test)]
mod tests;
mod vars;

use std::time::Instant;

use chrono::Utc;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, BranchPicker, Confirm, LatestConfirm, Loaded, Modal, View};
use crate::status::BoxColour;
use crate::theme::{Theme, THEMES};

fn box_style(t: &Theme, c: BoxColour) -> Style {
    Style::new().fg(match c {
        BoxColour::Green => t.ok,
        BoxColour::Red => t.bad,
        BoxColour::Yellow => t.busy,
        BoxColour::Grey => t.idle,
    })
}

/// Strips control characters (terminal escapes in log or commit text); tabs become spaces.
fn clean(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// Whether an environment name looks like production (case-insensitive `prod` anywhere).
pub fn is_production(env_name: &str) -> bool {
    env_name.to_lowercase().contains("prod")
}

pub fn draw(frame: &mut Frame, app: &App, now: Instant) {
    let t = app.theme();
    frame.render_widget(Block::new().style(base(t)), frame.area());
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, head, app, now);
    let wall = Utc::now();
    match app.stack.last() {
        Some(View::Services) => tables::services(frame, body, app, wall, now),
        Some(View::Deployments { .. }) => tables::deployments(frame, body, app, wall),
        Some(View::Instances { .. }) => tables::instances(frame, body, app),
        Some(View::Logs(l)) => {
            let building = app
                .find_deployment(&l.deployment_id)
                .and_then(|d| tables::build_elapsed(d, wall));
            logs::draw(frame, body, l, building.as_deref(), t)
        }
        Some(View::Pending(v)) => pending::draw(frame, body, app, v),
        Some(View::Variables(v)) => vars::draw(frame, body, app, v),
        Some(View::Projects) | None => tables::projects(frame, body, app),
    }
    draw_footer(frame, foot, app);

    if app.help {
        draw_help(frame, body, t);
    }
    match &app.modal {
        Some(Modal::EnvPicker(index)) => draw_env_picker(frame, body, app, *index),
        Some(Modal::ThemePicker { .. }) => draw_theme_picker(frame, body, app),
        Some(Modal::Confirm(Confirm::Deploy(d))) => {
            pending::draw_deploy_confirm(frame, body, app, d)
        }
        Some(Modal::BranchPicker(picker)) => draw_branch_picker(frame, body, app, picker),
        Some(Modal::Confirm(Confirm::DeployLatest(c))) => draw_latest_confirm(frame, body, app, c),
        // Drawn in the footer.
        Some(Modal::Confirm(Confirm::Discard { .. }))
        | Some(Modal::VarInput(_))
        | Some(Modal::Command(_))
        | Some(Modal::Filter(_))
        | None => {}
    }
}

fn hints(app: &App) -> &'static str {
    match app.stack.last() {
        Some(View::Services) => {
            "<e> env  <l> logs  <v> vars  <p> pending  <D> deploy  <d> branch  </> filter  <?> help"
        }
        Some(View::Deployments { .. }) => {
            "<enter> logs  <D> deploy latest  <d> branch  <r> reload  <esc> back  <?> help"
        }
        Some(View::Instances { .. }) => {
            "<enter> logs  <r> reload  </> filter  <esc> back  <?> help"
        }
        Some(View::Variables(_)) => {
            "<e> edit  <a> add  <d> del  <u> undo  <r> reveal  <p> review  <?> help"
        }
        Some(View::Pending(_)) => "<D> deploy  <x> discard  <r> reveal  <esc> back  <?> help",
        Some(View::Logs(_)) => {
            "<b> build/runtime  <f> follow  <r> reload  </> filter  <esc> back  <?> help"
        }
        _ => "<enter> open  <r> reload  </> filter  <:> command  <t> theme  <?> help",
    }
}

/// What the freshness indicator shows of a load.
struct Freshness<'a> {
    loading: bool,
    fetched_at: Option<Instant>,
    error: Option<&'a str>,
}

/// The load that backs the current view, for the freshness indicator.
fn freshness_source(app: &App) -> Option<Freshness<'_>> {
    fn of<T>(l: &Loaded<T>) -> Option<Freshness<'_>> {
        Some(Freshness {
            loading: l.loading,
            fetched_at: l.fetched_at,
            error: l.error.as_deref(),
        })
    }
    match app.stack.last() {
        Some(View::Projects) | None => of(&app.workspaces),
        Some(View::Deployments { .. }) => of(&app.deployments),
        // Until an environment is chosen, its list is what these views wait on.
        Some(View::Services) | Some(View::Instances { .. }) if app.environment.is_none() => {
            of(&app.environments)
        }
        Some(View::Services) | Some(View::Instances { .. }) => of(&app.services),
        Some(View::Variables(_)) => of(&app.variables),
        Some(View::Pending(_)) | Some(View::Logs(_)) => None,
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App, now: Instant) {
    let t = app.theme();
    let workspace = app
        .workspace_id
        .as_deref()
        .and_then(|id| {
            app.workspaces
                .value
                .iter()
                .flatten()
                .find(|w| w.id == id)
                .map(|w| w.name.as_str())
        })
        .unwrap_or("-");
    let project = app.project.as_ref().map_or("-", |p| p.name.as_str());
    let env = app.environment.as_ref().map_or("-", |e| e.name.as_str());
    let dim = Style::new().fg(t.muted);
    let text = Style::new().fg(t.text);
    let line1 = Line::from(vec![
        Span::styled("Workspace: ", dim),
        Span::styled(clean(workspace), text),
        Span::styled("  Project: ", dim),
        Span::styled(clean(project), text),
        Span::styled("  Env: ", dim),
        Span::styled(clean(env), text),
        Span::raw("    "),
        Span::styled(hints(app), Style::new().fg(t.hint)),
    ]);

    let mut spans: Vec<Span> = Vec::new();
    if let Some(Freshness {
        loading,
        fetched_at,
        error,
    }) = freshness_source(app)
    {
        let age = fetched_at.map(|at| now.saturating_duration_since(at).as_secs());
        let red = Style::new().fg(t.bad);
        match (error, age, loading) {
            (Some(err), Some(a), _) => spans.push(Span::styled(
                format!("STALE: {} (last ok {a}s ago)", clean(err)),
                red,
            )),
            (Some(err), None, _) => spans.push(Span::styled(format!("STALE: {}", clean(err)), red)),
            (None, Some(a), true) => {
                spans.push(Span::styled(format!("updated {a}s ago · refreshing…"), dim))
            }
            (None, Some(a), false) => spans.push(Span::styled(format!("updated {a}s ago"), dim)),
            (None, None, true) => spans.push(Span::styled("loading…", dim)),
            (None, None, false) => spans.push(Span::styled("not loaded", dim)),
        }
    }
    let pending = app
        .project
        .as_ref()
        .map_or(0, |p| app.pending.count_for_project(&p.id));
    if pending > 0 {
        spans.push(Span::styled(
            format!("  {pending} pending"),
            Style::new().fg(t.busy),
        ));
    }
    if !app.live_updates {
        spans.push(Span::styled("  live updates off", Style::new().fg(t.busy)));
    }
    if let Some(until) = app.blocked_until.filter(|u| *u > now) {
        let secs = until.saturating_duration_since(now).as_secs();
        spans.push(Span::styled(
            format!("  rate limited, retrying in {secs}s"),
            Style::new().fg(t.busy),
        ));
    }
    frame.render_widget(Paragraph::new(vec![line1, Line::from(spans)]), area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme();
    match &app.modal {
        Some(Modal::Confirm(Confirm::Discard { env_id, service_id })) => {
            let sp = app.pending.service(env_id, service_id);
            let service = sp.map_or(service_id.as_str(), |s| s.service_name.as_str());
            let env = sp.map_or(env_id.as_str(), |s| s.env_name.as_str());
            let line = format!(
                "Discard pending changes for {} in {}? (y/n)",
                clean(service),
                clean(env)
            );
            frame.render_widget(Paragraph::new(line).style(Style::new().fg(t.busy)), area);
            return;
        }
        Some(Modal::VarInput(input)) => {
            let line = match &input.name {
                None => format!("name: {}▏", clean(input.text.expose())),
                Some(name) => format!("{} = {}▏", clean(name), clean(input.text.expose())),
            };
            frame.render_widget(Paragraph::new(line), area);
            return;
        }
        Some(Modal::Command(text)) => {
            frame.render_widget(Paragraph::new(format!(":{}▏", clean(text))), area);
            return;
        }
        Some(Modal::Filter(text)) => {
            frame.render_widget(Paragraph::new(format!("/{}▏", clean(text))), area);
            return;
        }
        _ => {}
    }
    let mut crumbs = vec!["projects".to_string()];
    for view in app.stack.iter().skip(1) {
        crumbs.push(match view {
            View::Projects => "projects".into(),
            View::Services => app
                .project
                .as_ref()
                .map_or("services".into(), |p| clean(&p.name)),
            View::Deployments { .. } => "deployments".into(),
            View::Instances { .. } => "instances".into(),
            View::Variables(_) => "variables".into(),
            View::Pending(_) => "pending".into(),
            View::Logs(_) => "logs".into(),
        });
    }
    let mut spans = vec![Span::styled(
        format!(" {} ", crumbs.join(" > ")),
        Style::new().fg(t.bg).bg(t.accent),
    )];
    if let Some(filter) = &app.table_filter {
        spans.push(Span::styled(
            format!(" filter: {}", clean(filter)),
            Style::new().fg(t.hint),
        ));
    }
    if let Some(msg) = &app.message {
        spans.push(Span::styled(
            format!("  {}", clean(msg)),
            Style::new().fg(t.busy),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Default text on the theme background.
fn base(t: &Theme) -> Style {
    Style::new().fg(t.text).bg(t.bg)
}

/// A bordered overlay in the theme's colours.
fn popup<'a>(t: &Theme, title: &'a str) -> Block<'a> {
    Block::bordered()
        .title(Span::styled(title, Style::new().fg(t.accent)))
        .border_style(Style::new().fg(t.muted))
        .style(base(t))
}

/// Lists every theme with a swatch of its status colours; the highlighted one is live.
fn draw_theme_picker(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme();
    let rect = centered(area, 40, (THEMES.len() as u16).saturating_add(2));
    frame.render_widget(Clear, rect);
    let lines: Vec<Line> = THEMES
        .iter()
        .enumerate()
        .map(|(i, theme)| {
            let row = if i == app.theme {
                Style::new()
                    .fg(t.text)
                    .bg(t.select_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(t.text)
            };
            let mut spans = vec![Span::styled(format!(" {:<24}", theme.name), row)];
            for c in [theme.ok, theme.busy, theme.bad, theme.accent, theme.hint] {
                spans.push(Span::styled("■ ", row.fg(c)));
            }
            Line::from(spans).style(row)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(popup(t, " Theme ")), rect);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

const HELP_LINES: [&str; 21] = [
    "j/k, arrows  move / scroll",
    "enter        open",
    "esc          back / clear filter",
    "/            filter",
    ":            command (projects services deploys logs)",
    "             also :vars :pending :theme :q",
    "e            environment picker (services)",
    "l            latest logs   i  instances",
    "D            deploy latest commit (services, deploys)",
    "d            pick a branch to deploy: type to filter, enter",
    "b            build/runtime logs",
    "f, G         follow logs",
    "r, ctrl-r    reload (in logs: reconnect)",
    "t            theme picker (:theme)",
    "v            service variables (:vars)",
    "  e/enter edit  E $EDITOR  a add  d delete  u undo",
    "  r reveal  R reveal all  p review  / name filter",
    "p            review pending changes (:pending)",
    "  D deploy  x discard  r/R reveal  (y confirms)",
    "?            toggle help",
    "q            quit",
];

fn draw_help(frame: &mut Frame, area: Rect, t: &Theme) {
    let text = HELP_LINES;
    let rect = centered(area, 64, text.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    let lines: Vec<Line> = text.iter().map(|t| Line::from(*t)).collect();
    frame.render_widget(Paragraph::new(lines).block(popup(t, " Help ")), rect);
}

/// The latest-commit confirm: target, source, what is deployed now, and the prompt.
fn draw_latest_confirm(frame: &mut Frame, area: Rect, app: &App, c: &LatestConfirm) {
    let t = app.theme();
    let bold = Modifier::BOLD;
    let text = Style::new().fg(t.text);
    let muted = Style::new().fg(t.muted);
    let env_colour = if is_production(&c.env_name) {
        t.bad
    } else {
        t.accent
    };
    let unknown = || "unknown".to_string();
    let now = match &c.current {
        Some(d) => {
            let sha = tables::short_sha(d);
            let msg = d.commit_message.as_deref().unwrap_or_default();
            let msg = msg.lines().next().unwrap_or_default();
            format!("{sha}  {}", clean(msg))
        }
        None => "nothing deployed yet".into(),
    };
    let next = match &c.branch {
        Some(b) => format!("latest commit on {}", clean(b)),
        None => "latest commit of the connected branch".into(),
    };
    let title = match &c.switch {
        Some(sw) => format!(" Deploy branch {} to ", clean(&sw.to)),
        None => " Deploy latest commit to ".into(),
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(title, text.add_modifier(bold)),
            Span::styled(
                clean(&c.env_name),
                Style::new().fg(env_colour).add_modifier(bold),
            ),
        ]),
        Line::styled(format!(" service: {}", clean(&c.service_name)), text),
        Line::styled(
            format!(
                " repo:    {}",
                c.repo.as_deref().map_or_else(unknown, clean)
            ),
            text,
        ),
    ];
    match &c.switch {
        Some(sw) => {
            let warn = Style::new().fg(t.busy);
            lines.push(Line::styled(
                format!(" branch:  {} → {}", clean(&sw.from), clean(&sw.to)),
                warn.add_modifier(bold),
            ));
            lines.push(Line::styled(
                format!(
                    "          the service stays on {} afterwards",
                    clean(&sw.to)
                ),
                warn,
            ));
        }
        None => lines.push(Line::styled(
            format!(
                " branch:  {}",
                c.branch.as_deref().map_or_else(unknown, clean)
            ),
            text,
        )),
    }
    lines.extend([
        Line::raw(""),
        Line::from(vec![
            Span::styled(" now:     ", muted),
            Span::styled(now, muted),
        ]),
        Line::from(vec![
            Span::styled(" deploy:  ", text),
            Span::styled(next, Style::new().fg(t.accent).add_modifier(bold)),
        ]),
    ]);
    if c.written > 0 {
        lines.push(Line::styled(
            format!(
                "   ! {} written variable change{} will ship with this deploy",
                c.written,
                if c.written == 1 { "" } else { "s" }
            ),
            Style::new().fg(t.busy),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        " Deploy? y to confirm, any other key cancels",
        Style::new().fg(t.busy).add_modifier(bold),
    ));
    let height = lines.len() as u16 + 2;
    let rect = centered(area, area.width.saturating_sub(4).min(80), height);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(popup(t, " Deploy ")), rect);
}

const BRANCH_PICKER_WIDTH: u16 = 60;
/// Room for text inside the picker: its width less the borders and a margin each side.
const BRANCH_PICKER_TEXT: usize = BRANCH_PICKER_WIDTH as usize - 4;

/// Splits `text` into lines of at most `width` characters, at spaces where it
/// can and mid-word where a word alone is too long.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        while word.len() > width {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            lines.push(word.drain(..width).collect());
        }
        let word: String = word.into_iter().collect();
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(&word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The branch picker: the query, then the matching branches (connected one
/// first and marked), scrolled to keep the highlighted one in view.
fn draw_branch_picker(frame: &mut Frame, area: Rect, app: &App, p: &BranchPicker) {
    let t = app.theme();
    let muted = Style::new().fg(t.muted);
    let mut lines = vec![
        Line::styled(format!(" > {}▏", clean(&p.query)), Style::new().fg(t.text)),
        Line::raw(""),
    ];
    // Border, query line and blank line.
    let room = (area.height as usize).saturating_sub(4).clamp(1, 15);
    match &p.choices {
        None => lines.push(Line::styled(" loading branches…", muted)),
        Some(Err(e)) => {
            let text = format!("cannot list branches: {}", clean(e));
            for l in wrap(&text, BRANCH_PICKER_TEXT) {
                lines.push(Line::styled(format!(" {l}"), Style::new().fg(t.bad)));
            }
        }
        Some(Ok(c)) if c.unlisted.is_some() => {
            let why = c.unlisted.as_deref().unwrap_or_default();
            let text = format!("could not list branches: {}", clean(why));
            for l in wrap(&text, BRANCH_PICKER_TEXT) {
                lines.push(Line::styled(format!(" {l}"), Style::new().fg(t.busy)));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                " type the exact branch name, enter to pick",
                Style::new().fg(t.text),
            ));
            lines.push(Line::styled(
                format!(" connected: {}", clean(&c.connected)),
                muted,
            ));
        }
        Some(Ok(c)) => {
            let names = p.filtered();
            if names.is_empty() {
                lines.push(Line::styled(" no branch matches", muted));
            }
            let skip = (p.index + 1).saturating_sub(room);
            for (i, name) in names.iter().enumerate().skip(skip).take(room) {
                let mut spans = vec![Span::raw(format!(" {}", clean(name)))];
                if *name == c.connected {
                    spans.push(Span::styled("  (connected)", muted));
                }
                let mut line = Line::from(spans);
                if i == p.index {
                    line = line.style(Style::new().bg(t.select_bg).add_modifier(Modifier::BOLD));
                }
                lines.push(line);
            }
        }
    }
    let title = format!(" Deploy branch: {} ", clean(&p.target.service_name));
    let rect = centered(area, BRANCH_PICKER_WIDTH, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(popup(t, &title)), rect);
}

fn draw_env_picker(frame: &mut Frame, area: Rect, app: &App, index: usize) {
    let t = app.theme();
    let envs = app.environments.value.as_deref().unwrap_or_default();
    let rect = centered(area, 40, (envs.len() as u16).saturating_add(2));
    frame.render_widget(Clear, rect);
    let lines: Vec<Line> = envs
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let style = if i == index {
                Style::new().bg(t.select_bg).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Line::styled(format!(" {}", clean(&e.name)), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(popup(t, " Environment ")), rect);
}
