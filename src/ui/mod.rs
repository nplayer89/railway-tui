//! Rendering: a pure function of `App`.

mod logs;
mod tables;
#[cfg(test)]
mod tests;

use std::time::Instant;

use chrono::Utc;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, Loaded, View};
use crate::status::BoxColour;

/// Selection background; no foreground colour used anywhere may equal it.
const SELECT_BG: Color = Color::Indexed(236);

fn box_style(c: &BoxColour) -> Style {
    Style::new().fg(match c {
        BoxColour::Green => Color::Green,
        BoxColour::Red => Color::Red,
        BoxColour::Yellow => Color::Yellow,
        BoxColour::Grey => Color::Gray,
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

pub fn draw(frame: &mut Frame, app: &App, now: Instant) {
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, head, app, now);
    let wall = Utc::now();
    match app.stack.last() {
        Some(View::Services) => tables::services(frame, body, app, wall),
        Some(View::Deployments { .. }) => tables::deployments(frame, body, app, wall),
        Some(View::Instances { .. }) => tables::instances(frame, body, app),
        Some(View::Logs(l)) => logs::draw(frame, body, l),
        Some(View::Projects) | None => tables::projects(frame, body, app),
    }
    draw_footer(frame, foot, app);

    if app.help {
        draw_help(frame, body);
    }
    if let Some(index) = app.env_picker {
        draw_env_picker(frame, body, app, index);
    }
}

fn hints(app: &App) -> &'static str {
    match app.stack.last() {
        Some(View::Services) => {
            "<e> env  <l> logs  <i> instances  <enter> deploys  </> filter  <?> help"
        }
        Some(View::Deployments { .. }) | Some(View::Instances { .. }) => {
            "<enter> logs  </> filter  <esc> back  <?> help"
        }
        Some(View::Logs(_)) => "<b> build/runtime  <f> follow  </> filter  <esc> back  <?> help",
        _ => "<enter> open  </> filter  <:> command  <?> help",
    }
}

type Freshness<'a> = (bool, Option<Instant>, Option<&'a str>);

/// The load that backs the current view, for the freshness indicator.
fn freshness_source(app: &App) -> Option<Freshness<'_>> {
    fn of<T>(l: &Loaded<T>) -> Option<Freshness<'_>> {
        Some((l.loading, l.fetched_at, l.error.as_deref()))
    }
    match app.stack.last() {
        Some(View::Projects) | None => of(&app.workspaces),
        Some(View::Deployments { .. }) => of(&app.deployments),
        Some(View::Services) | Some(View::Instances { .. }) => of(&app.services),
        Some(View::Logs(_)) => None,
    }
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App, now: Instant) {
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
    let dim = Style::new().fg(Color::DarkGray);
    let line1 = Line::from(vec![
        Span::styled("Workspace: ", dim),
        Span::raw(clean(workspace)),
        Span::styled("  Project: ", dim),
        Span::raw(clean(project)),
        Span::styled("  Env: ", dim),
        Span::raw(clean(env)),
        Span::raw("    "),
        Span::styled(hints(app), Style::new().fg(Color::Magenta)),
    ]);

    let mut spans: Vec<Span> = Vec::new();
    if let Some((loading, fetched_at, error)) = freshness_source(app) {
        let age = fetched_at.map(|at| now.saturating_duration_since(at).as_secs());
        let red = Style::new().fg(Color::Red);
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
    if !app.live_updates {
        spans.push(Span::styled(
            "  live updates off",
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(until) = app.blocked_until.filter(|u| *u > now) {
        let secs = until.saturating_duration_since(now).as_secs();
        spans.push(Span::styled(
            format!("  rate limited, retrying in {secs}s"),
            Style::new().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(vec![line1, Line::from(spans)]), area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(text) = &app.command_line {
        frame.render_widget(Paragraph::new(format!(":{}▏", clean(text))), area);
        return;
    }
    if let Some(text) = &app.filter_input {
        frame.render_widget(Paragraph::new(format!("/{}▏", clean(text))), area);
        return;
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
            View::Logs(_) => "logs".into(),
        });
    }
    let mut spans = vec![Span::styled(
        format!(" {} ", crumbs.join(" > ")),
        Style::new().fg(Color::Black).bg(Color::Blue),
    )];
    if let Some(filter) = &app.table_filter {
        spans.push(Span::styled(
            format!(" filter: {}", clean(filter)),
            Style::new().fg(Color::Yellow),
        ));
    }
    if let Some(msg) = &app.message {
        spans.push(Span::styled(
            format!("  {}", clean(msg)),
            Style::new().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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

fn draw_help(frame: &mut Frame, area: Rect) {
    let text = [
        "j/k, arrows  move / scroll",
        "enter        open",
        "esc          back / clear filter",
        "/            filter",
        ":            command (projects services deploys logs q)",
        "e            environment picker (services)",
        "l            latest logs   i  instances",
        "b            build/runtime logs",
        "f, G         follow logs",
        "ctrl-r       reload",
        "?            toggle help",
        "q            quit",
    ];
    let rect = centered(area, 64, text.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    let lines: Vec<Line> = text.iter().map(|t| Line::from(*t)).collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Help ")),
        rect,
    );
}

fn draw_env_picker(frame: &mut Frame, area: Rect, app: &App, index: usize) {
    let envs = app.environments.value.as_deref().unwrap_or_default();
    let rect = centered(area, 40, (envs.len() as u16).saturating_add(2));
    frame.render_widget(Clear, rect);
    let lines: Vec<Line> = envs
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let style = if i == index {
                Style::new().bg(SELECT_BG).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            Line::styled(format!(" {}", clean(&e.name)), style)
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Environment ")),
        rect,
    );
}
