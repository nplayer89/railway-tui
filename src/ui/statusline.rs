//! The NvChad-style status bar and the vim-style command line beneath it.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{clean, is_production};
use crate::app::{App, Confirm, Modal, Mode};
use crate::theme::Theme;

// Nerd Font v3 glyphs; there is no plain-text fallback.
const SEPARATOR: &str = "\u{e0bc}";
const ICON_VIM: &str = "\u{e62b}";
const ICON_FILE: &str = "\u{f15b}";
const ICON_BRANCH: &str = "\u{e725}";
const ICON_FOLDER: &str = "\u{f07b}";
const ICON_LIST: &str = "\u{f03a}";

fn mode_colour(t: &Theme, mode: Mode) -> ratatui::style::Color {
    match mode {
        Mode::Command | Mode::Filter => t.busy,
        Mode::Insert => t.ok,
        Mode::Visual => t.hint,
        Mode::Normal => t.accent,
    }
}

/// Cuts `s` to at most `max` characters, ending in `…` when anything was cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    if max > 0 {
        out.push('…');
    }
    out
}

fn width(spans: &[Span]) -> usize {
    Line::from(spans.to_vec()).width()
}

pub(super) fn draw_statusline(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme();
    let mode = app.mode();
    let colour = mode_colour(t, mode);
    let label = clean(&app.status_label());

    let mode_block = vec![
        Span::styled(
            format!(" {ICON_VIM} {} ", mode.name()),
            Style::new()
                .fg(t.bg)
                .bg(colour)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(SEPARATOR, Style::new().fg(colour).bg(t.select_bg)),
    ];
    let view_block = |label: &str| {
        vec![
            Span::styled(
                format!(" {ICON_FILE} {label} "),
                Style::new().fg(t.text).bg(t.select_bg),
            ),
            Span::styled(SEPARATOR, Style::new().fg(t.select_bg).bg(t.bg)),
        ]
    };
    let env_block = app.environment.as_ref().map(|e| {
        let fg = if is_production(&e.name) {
            t.bad
        } else {
            t.muted
        };
        vec![Span::styled(
            format!(" {ICON_BRANCH} {} ", clean(&e.name)),
            Style::new().fg(fg).bg(t.bg),
        )]
    });
    let project_block = app.project.as_ref().map(|p| {
        vec![
            Span::styled(format!(" {ICON_FOLDER} "), Style::new().fg(t.bg).bg(t.bad)),
            Span::styled(
                format!(" {} ", clean(&p.name)),
                Style::new().fg(t.text).bg(t.select_bg),
            ),
        ]
    });
    let position_block = app.position().map(|(row, total)| {
        vec![
            Span::styled(format!(" {ICON_LIST} "), Style::new().fg(t.bg).bg(t.ok)),
            Span::styled(
                format!(" {row}/{total} "),
                Style::new().fg(t.text).bg(t.select_bg),
            ),
        ]
    });
    let mut count_block = match (mode, app.visual_range()) {
        (Mode::Visual, Some(r)) => Some(vec![Span::styled(
            format!("{} selected ", r.end() - r.start() + 1),
            Style::new().fg(t.muted).bg(t.bg),
        )]),
        _ => None,
    };
    let (mut project_block, mut env_block, mut position_block) =
        (project_block, env_block, position_block);

    let total = |count: &Option<Vec<Span>>,
                 env: &Option<Vec<Span>>,
                 project: &Option<Vec<Span>>,
                 position: &Option<Vec<Span>>,
                 label: &str| {
        let mut n = width(&mode_block) + width(&view_block(label));
        for block in [count, env, project, position].into_iter().flatten() {
            n += width(block);
        }
        n
    };
    let max = area.width as usize;
    // Drop order: selection count, project, env, position.
    if total(
        &count_block,
        &env_block,
        &project_block,
        &position_block,
        &label,
    ) > max
    {
        count_block = None;
    }
    if total(
        &count_block,
        &env_block,
        &project_block,
        &position_block,
        &label,
    ) > max
    {
        project_block = None;
    }
    if total(
        &count_block,
        &env_block,
        &project_block,
        &position_block,
        &label,
    ) > max
    {
        env_block = None;
    }
    if total(
        &count_block,
        &env_block,
        &project_block,
        &position_block,
        &label,
    ) > max
    {
        position_block = None;
    }
    let mut label = label;
    let over = total(
        &count_block,
        &env_block,
        &project_block,
        &position_block,
        &label,
    )
    .saturating_sub(max);
    if over > 0 {
        let keep = label.chars().count().saturating_sub(over);
        label = truncate(&label, keep);
    }

    let mut spans = mode_block.clone();
    spans.extend(view_block(&label));
    spans.extend(env_block.iter().flatten().cloned());
    let right: Vec<Span> = [&count_block, &project_block, &position_block]
        .into_iter()
        .flatten()
        .flatten()
        .cloned()
        .collect();
    let fill = max.saturating_sub(width(&spans) + width(&right));
    spans.push(Span::styled(" ".repeat(fill), Style::new().bg(t.bg)));
    spans.extend(right);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::new().bg(t.bg)),
        area,
    );
}

pub(super) fn draw_cmdline(frame: &mut Frame, area: Rect, app: &App) {
    let t = app.theme();
    let (text, style) = match &app.modal {
        Some(Modal::Confirm(Confirm::Discard { env_id, service_id })) => {
            let sp = app.pending.service(env_id, service_id);
            let service = sp.map_or(service_id.as_str(), |s| s.service_name.as_str());
            let env = sp.map_or(env_id.as_str(), |s| s.env_name.as_str());
            (
                format!(
                    "Discard pending changes for {} in {}? (y/n)",
                    clean(service),
                    clean(env)
                ),
                Style::new().fg(t.busy),
            )
        }
        Some(Modal::VarInput(input)) => (
            match &input.name {
                None => format!("name: {}▏", clean(input.text.expose())),
                Some(name) => format!("{} = {}▏", clean(name), clean(input.text.expose())),
            },
            Style::new(),
        ),
        Some(Modal::Command(text)) => (format!(":{}▏", clean(text)), Style::new()),
        Some(Modal::Filter(text)) => (format!("/{}▏", clean(text)), Style::new()),
        _ => match (&app.message, &app.table_filter) {
            (Some(msg), _) => (clean(msg), Style::new().fg(t.busy)),
            (None, Some(filter)) => (
                format!("filter: {}", clean(filter)),
                Style::new().fg(t.hint),
            ),
            (None, None) => (String::new(), Style::new()),
        },
    };
    frame.render_widget(Paragraph::new(text).style(style), area);
}
