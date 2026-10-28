//! Table views: projects, services, deployments and instances.

use chrono::{DateTime, Utc};
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::theme;
use super::{box_style, clean};
use crate::app::App;
use crate::model::{DeploymentStatus, DeploymentSummary, InstanceStatus};
use crate::status::{box_colour, derive_row, Health, RowTag};

const HEADER: Style = Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD);
const TEXT: Style = Style::new().fg(theme::TEXT);
const MUTED: Style = Style::new().fg(theme::MUTED);

/// Selection is painted into the row style rather than as a table highlight, because a
/// highlight overrides the explicit background of the `▚▚` marker.
fn row_style(fg: Style, selected: bool) -> Style {
    if selected {
        fg.bg(theme::SELECT_BG).add_modifier(Modifier::BOLD)
    } else {
        fg
    }
}

fn health_colour(h: Health) -> Color {
    match h {
        Health::Healthy => theme::OK,
        Health::Degraded | Health::Failing | Health::FailedButServing => theme::BAD,
        Health::Progressing => theme::BUSY,
        Health::Inactive => theme::IDLE,
    }
}

fn deployment_colour(s: &DeploymentStatus) -> Color {
    use DeploymentStatus as D;
    match s {
        D::Success => theme::OK,
        D::Failed | D::Crashed => theme::BAD,
        D::Building
        | D::Deploying
        | D::Queued
        | D::Waiting
        | D::Initializing
        | D::NeedsApproval => theme::BUSY,
        _ => theme::IDLE,
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

fn short_sha(d: &DeploymentSummary) -> String {
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

fn header(cols: &[&'static str]) -> Row<'static> {
    Row::new(cols.iter().map(|c| Cell::from(*c))).style(HEADER)
}

fn render_table(
    frame: &mut Frame,
    area: Rect,
    header: Row<'static>,
    rows: Vec<Row<'static>>,
    widths: Vec<Constraint>,
    selected: Option<usize>,
    empty: &str,
) {
    if rows.is_empty() {
        frame.render_widget(Paragraph::new(Span::styled(empty.to_string(), MUTED)), area);
        return;
    }
    let mut state = TableState::default().with_selected(selected);
    let table = Table::new(rows, widths).header(header);
    frame.render_stateful_widget(table, area, &mut state);
}

pub fn projects(frame: &mut Frame, area: Rect, app: &App) {
    let project_rows = app.project_rows();
    let selected_project = app.selected_index();
    let mut rows: Vec<Row> = Vec::new();
    let mut selected = None;
    let mut current: Option<&str> = None;
    for (i, (workspace, project)) in project_rows.iter().enumerate() {
        if current != Some(workspace.id.as_str()) {
            current = Some(workspace.id.as_str());
            rows.push(Row::new([Cell::from(clean(&workspace.name))]).style(HEADER));
        }
        if selected_project == Some(i) {
            selected = Some(rows.len());
        }
        rows.push(
            Row::new([Cell::from(format!("  {}", clean(&project.name)))])
                .style(row_style(TEXT, selected_project == Some(i))),
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
        header(&["PROJECT"]),
        rows,
        vec![Constraint::Fill(1)],
        selected,
        empty,
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

pub fn services(frame: &mut Frame, area: Rect, app: &App, now: DateTime<Utc>) {
    let mut rows = Vec::new();
    let mut box_width = "INSTANCES".len() as u16;
    let selected = app.selected_index();
    for (i, svc) in app.service_rows().into_iter().enumerate() {
        let row = derive_row(svc);
        let colour = health_colour(row.health);
        let mut boxes: Vec<Span> = Vec::new();
        for (i, b) in row.boxes.iter().enumerate() {
            if i > 0 {
                boxes.push(Span::raw(" "));
            }
            boxes.push(Span::styled("■", box_style(&box_colour(b))));
        }
        box_width = box_width.max((row.boxes.len() * 2).saturating_sub(1) as u16);
        let mut status: Vec<Span> = Vec::new();
        if row.health == Health::FailedButServing {
            status.push(Span::styled(
                "▚▚",
                Style::new().fg(theme::BAD).bg(theme::OK),
            ));
            status.push(Span::raw(" "));
        }
        status.push(Span::styled(clean(&row.label), Style::new().fg(colour)));
        let latest = svc.latest.as_ref();
        rows.push(
            Row::new([
                Cell::from(clean(&svc.service_name)),
                Cell::from(Line::from(boxes)),
                Cell::from(format!("{}/{}", row.ready, row.desired)).style(MUTED),
                Cell::from(Line::from(status)),
                Cell::from(latest.map_or("-".into(), |d| relative(d.created_at, now))).style(MUTED),
                Cell::from(latest.map_or("-".into(), short_sha)).style(MUTED),
                Cell::from(latest.map_or("-".into(), message)),
            ])
            .style(row_style(TEXT, selected == Some(i))),
        );
    }
    let empty = service_empty_text(app, "no services");
    render_table(
        frame,
        area,
        header(&[
            "SERVICE",
            "INSTANCES",
            "READY",
            "STATUS",
            "DEPLOYED",
            "COMMIT",
            "MESSAGE",
        ]),
        rows,
        vec![
            Constraint::Length(20),
            Constraint::Length(box_width),
            Constraint::Length(6),
            Constraint::Length(26),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Fill(1),
        ],
        selected,
        empty,
    );
}

pub fn deployments(frame: &mut Frame, area: Rect, app: &App, now: DateTime<Utc>) {
    let selected = app.selected_index();
    let rows: Vec<Row> = app
        .deployment_table()
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let d = &r.deployment;
            let tag = match r.tag {
                Some(RowTag::Latest) => "LATEST",
                Some(RowTag::Active) => "ACTIVE",
                None => "",
            };
            Row::new([
                Cell::from(tag).style(Style::new().fg(theme::HINT)),
                Cell::from(clean(&d.status.label()))
                    .style(Style::new().fg(deployment_colour(&d.status))),
                Cell::from(relative(d.created_at, now)).style(MUTED),
                Cell::from(short_sha(d)).style(MUTED),
                Cell::from(message(d)),
            ])
            .style(row_style(TEXT, selected == Some(i)))
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
        header(&["TAG", "STATUS", "DEPLOYED", "COMMIT", "MESSAGE"]),
        rows,
        vec![
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Fill(1),
        ],
        selected,
        empty,
    );
}

pub fn instances(frame: &mut Frame, area: Rect, app: &App) {
    let selected = app.selected_index();
    let rows: Vec<Row> = app
        .instance_rows()
        .into_iter()
        .enumerate()
        .map(|(n, (d, i))| {
            let status = match &i.status {
                InstanceStatus::Unknown(raw) => format!("UNKNOWN({})", clean(raw)),
                other => format!("{other:?}").to_uppercase(),
            };
            Row::new([
                Cell::from(clean(&i.id)),
                Cell::from(status).style(box_style(&box_colour(&i.status))),
                Cell::from(clean(&d.id)).style(MUTED),
            ])
            .style(row_style(TEXT, selected == Some(n)))
        })
        .collect();
    let empty = service_empty_text(app, "no instances");
    render_table(
        frame,
        area,
        header(&["ID", "STATUS", "DEPLOYMENT"]),
        rows,
        vec![
            Constraint::Length(40),
            Constraint::Length(14),
            Constraint::Fill(1),
        ],
        selected,
        empty,
    );
}
