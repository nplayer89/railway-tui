//! Table views: projects, services, deployments and instances.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::{box_style, clean};
use crate::app::App;
use crate::model::{DeploymentStatus, DeploymentSummary, ServiceInstance};
use crate::status::{box_colour, derive_row, Health, RowTag};
use crate::theme::Theme;

pub(super) fn heading(t: &Theme) -> Style {
    Style::new().fg(t.accent).add_modifier(Modifier::BOLD)
}

pub(super) fn fg(c: Color) -> Style {
    Style::new().fg(c)
}

/// Selection is painted into the row style rather than as a table highlight, because a
/// highlight overrides the explicit background of the `▚▚` marker.
pub(super) fn row_style(t: &Theme, fg: Style, selected: bool) -> Style {
    if selected {
        fg.bg(t.select_bg).add_modifier(Modifier::BOLD)
    } else {
        fg
    }
}

fn health_colour(t: &Theme, h: Health) -> Color {
    match h {
        Health::Healthy => t.ok,
        Health::Degraded | Health::Failing | Health::FailedButServing => t.bad,
        Health::Progressing => t.busy,
        Health::Inactive => t.idle,
    }
}

fn deployment_colour(t: &Theme, s: &DeploymentStatus) -> Color {
    use DeploymentStatus as D;
    match s {
        D::Success => t.ok,
        D::Failed | D::Crashed => t.bad,
        D::Building
        | D::Deploying
        | D::Queued
        | D::Waiting
        | D::Initializing
        | D::NeedsApproval => t.busy,
        D::Removed | D::Removing | D::Skipped | D::Sleeping | D::Unknown(_) => t.idle,
    }
}

/// Relative age such as `just now`, `4m ago`, `2h ago`, `3d ago`.
fn relative(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds();
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// Compact elapsed time such as `42s`, `3m07s`, `1h04m`.
pub(super) fn elapsed(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// How long a deployment has been building, measured from its creation (so it
/// includes any time spent queued); `None` unless it is building now.
pub(super) fn build_elapsed(d: &DeploymentSummary, now: DateTime<Utc>) -> Option<String> {
    (d.status == DeploymentStatus::Building).then(|| elapsed(d.created_at, now))
}

/// How long a scrolling name rests at each end, in steps.
const MARQUEE_PAUSE: u64 = 6;
/// How long each one-character scroll step lasts.
const MARQUEE_STEP: Duration = Duration::from_millis(250);

/// The `width`-character window of `text` to show `elapsed` into scrolling it:
/// rest at the start, scroll one character per step to the end, rest, repeat.
pub(super) fn marquee(text: &str, width: usize, elapsed: Duration) -> String {
    let chars: Vec<char> = text.chars().collect();
    let overflow = chars.len().saturating_sub(width) as u64;
    if overflow == 0 {
        return text.into();
    }
    let cycle = overflow + 2 * MARQUEE_PAUSE;
    let step = (elapsed.as_millis() / MARQUEE_STEP.as_millis()) as u64 % cycle;
    let offset = step.saturating_sub(MARQUEE_PAUSE).min(overflow) as usize;
    chars[offset..offset + width].iter().collect()
}

/// The first 7 characters of the commit, or `-` without one.
pub(super) fn short_sha(d: &DeploymentSummary) -> String {
    d.commit_sha
        .as_deref()
        .filter(|s| !s.is_empty())
        .map_or("-".into(), |s| s.chars().take(7).collect())
}

fn message(d: &DeploymentSummary) -> String {
    clean(
        d.commit_message
            .as_deref()
            .or(d.image.as_deref())
            .unwrap_or("-"),
    )
}

pub(super) fn header(t: &Theme, cols: &[&'static str]) -> Row<'static> {
    Row::new(cols.iter().map(|c| Cell::from(*c))).style(heading(t))
}

pub(super) fn render_table(
    frame: &mut Frame,
    area: Rect,
    header: Row<'static>,
    rows: Vec<Row<'static>>,
    widths: Vec<Constraint>,
    selected: Option<usize>,
    empty: Span<'static>,
) {
    if rows.is_empty() {
        frame.render_widget(Paragraph::new(empty), area);
        return;
    }
    let mut state = TableState::default().with_selected(selected);
    let table = Table::new(rows, widths).header(header);
    frame.render_stateful_widget(table, area, &mut state);
}

pub fn projects(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme();
    let project_rows = app.project_rows();
    let selected_project = app.selected_index();
    let mut rows: Vec<Row> = Vec::new();
    let mut selected = None;
    let mut current: Option<&str> = None;
    for (i, (workspace, project)) in project_rows.iter().enumerate() {
        if current != Some(workspace.id.as_str()) {
            current = Some(workspace.id.as_str());
            rows.push(Row::new([Cell::from(clean(&workspace.name))]).style(heading(t)));
        }
        if selected_project == Some(i) {
            selected = Some(rows.len());
        }
        rows.push(
            Row::new([Cell::from(format!("  {}", clean(&project.name)))]).style(row_style(
                t,
                fg(t.text),
                selected_project == Some(i),
            )),
        );
    }
    let empty = if app.workspaces.loading {
        "loading…"
    } else {
        "no projects"
    };
    render_table(
        frame,
        area,
        header(t, &["PROJECT"]),
        rows,
        vec![Constraint::Fill(1)],
        selected,
        Span::styled(empty, fg(t.muted)),
    );
}

/// What an empty Services or Instances table says; before an environment is
/// chosen it reports on the environments load instead.
fn service_empty_text(app: &App, none: &'static str) -> &'static str {
    if app.environment.is_some() {
        return if app.services.loading {
            "loading…"
        } else {
            none
        };
    }
    match &app.environments.value {
        _ if app.environments.loading => "loading…",
        Some(envs) if envs.is_empty() => "no environments",
        _ => "no environment selected",
    }
}

/// Width of the SERVICE column; a longer selected name scrolls within it.
const SERVICE_WIDTH: u16 = 30;

pub fn services(frame: &mut Frame, area: Rect, app: &App, now: DateTime<Utc>, tick: Instant) {
    let t = app.theme();
    let mut rows = Vec::new();
    let mut box_width = "INSTANCES".len() as u16;
    let selected = app.selected_index();
    for (i, svc) in app.service_rows().into_iter().enumerate() {
        let row = derive_row(svc);
        let colour = health_colour(t, row.health);
        let mut boxes: Vec<Span> = Vec::new();
        for (i, b) in row.boxes.iter().enumerate() {
            if i > 0 {
                boxes.push(Span::raw(" "));
            }
            boxes.push(Span::styled("■", box_style(t, box_colour(b))));
        }
        box_width = box_width.max((row.boxes.len() * 2).saturating_sub(1) as u16);
        let mut status: Vec<Span> = Vec::new();
        if row.health == Health::FailedButServing {
            status.push(Span::styled("▚▚", Style::new().fg(t.bad).bg(t.ok)));
            status.push(Span::raw(" "));
        }
        status.push(Span::styled(clean(&row.label), Style::new().fg(colour)));
        let latest = svc.latest.as_ref();
        if let Some(e) = latest.and_then(|d| build_elapsed(d, now)) {
            status.push(Span::styled(format!(" {e}"), fg(t.muted)));
        }
        rows.push(
            Row::new([
                Cell::from(service_name(app, svc, tick)),
                Cell::from(Line::from(boxes)),
                Cell::from(format!("{}/{}", row.ready, row.desired)).style(fg(t.muted)),
                Cell::from(Line::from(status)),
                Cell::from(latest.map_or("-".into(), |d| relative(d.created_at, now)))
                    .style(fg(t.muted)),
                Cell::from(latest.map_or("-".into(), short_sha)).style(fg(t.muted)),
                Cell::from(latest.map_or("-".into(), message)),
            ])
            .style(row_style(t, fg(t.text), selected == Some(i))),
        );
    }
    let empty = service_empty_text(app, "no services");
    render_table(
        frame,
        area,
        header(
            t,
            &[
                "SERVICE",
                "INSTANCES",
                "READY",
                "STATUS",
                "DEPLOYED",
                "COMMIT",
                "MESSAGE",
            ],
        ),
        rows,
        vec![
            Constraint::Length(SERVICE_WIDTH),
            Constraint::Length(box_width),
            Constraint::Length(6),
            Constraint::Length(26),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Fill(1),
        ],
        selected,
        Span::styled(empty, fg(t.muted)),
    );
}

/// The service's name, scrolled if it is the focused row and too long to fit.
fn service_name(app: &App, svc: &ServiceInstance, tick: Instant) -> String {
    let name = clean(&svc.service_name);
    match &app.service_focus {
        Some((id, since)) if *id == svc.service_id => marquee(
            &name,
            SERVICE_WIDTH as usize,
            tick.saturating_duration_since(*since),
        ),
        _ => name,
    }
}

pub fn deployments(frame: &mut Frame, area: Rect, app: &App, now: DateTime<Utc>) {
    let t = app.theme();
    let selected = app.selected_index();
    let rows: Vec<Row> = app
        .deployment_table()
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let d = r.deployment;
            let tag = match r.tag {
                Some(RowTag::Latest) => "LATEST",
                Some(RowTag::Active) => "ACTIVE",
                None => "",
            };
            let mut status = vec![Span::styled(
                clean(&d.status.to_string()),
                Style::new().fg(deployment_colour(t, &d.status)),
            )];
            if let Some(e) = build_elapsed(d, now) {
                status.push(Span::styled(format!(" {e}"), fg(t.muted)));
            }
            Row::new([
                Cell::from(tag).style(Style::new().fg(t.hint)),
                Cell::from(Line::from(status)),
                Cell::from(relative(d.created_at, now)).style(fg(t.muted)),
                Cell::from(short_sha(d)).style(fg(t.muted)),
                Cell::from(message(d)),
            ])
            .style(row_style(t, fg(t.text), selected == Some(i)))
        })
        .collect();
    let empty = if app.deployments.loading {
        "loading…"
    } else {
        "no deployments"
    };
    render_table(
        frame,
        area,
        header(t, &["TAG", "STATUS", "DEPLOYED", "COMMIT", "MESSAGE"]),
        rows,
        vec![
            Constraint::Length(8),
            Constraint::Length(18),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Fill(1),
        ],
        selected,
        Span::styled(empty, fg(t.muted)),
    );
}

pub fn instances(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme();
    let selected = app.selected_index();
    let rows: Vec<Row> = app
        .instance_rows()
        .into_iter()
        .enumerate()
        .map(|(n, (d, i))| {
            // `clean` strips control characters from an unknown raw value.
            let status = clean(&i.status.to_string());
            Row::new([
                Cell::from(clean(&i.id)),
                Cell::from(status).style(box_style(t, box_colour(&i.status))),
                Cell::from(clean(&d.id)).style(fg(t.muted)),
            ])
            .style(row_style(t, fg(t.text), selected == Some(n)))
        })
        .collect();
    let empty = service_empty_text(app, "no instances");
    render_table(
        frame,
        area,
        header(t, &["ID", "STATUS", "DEPLOYMENT"]),
        rows,
        vec![
            Constraint::Length(40),
            Constraint::Length(14),
            Constraint::Fill(1),
        ],
        selected,
        Span::styled(empty, fg(t.muted)),
    );
}
