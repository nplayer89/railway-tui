//! The review screen: every pending change in the open project, by environment then service.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::tables::fg;
use super::vars::{shown, HIDDEN};
use super::{clean, is_production};
use crate::app::{App, PendingView};
use crate::pending::{Change, PendingVar};

pub fn draw(frame: &mut Frame, area: Rect, app: &App, view: &PendingView) {
    let t = app.theme();
    let groups = app.pending_groups();
    if groups.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("no pending changes", fg(t.muted))),
            area,
        );
        return;
    }
    let selected = app.selected_index().unwrap_or(0);
    let mut lines: Vec<Line> = Vec::new();
    // The line range of the selected group, to keep it on screen.
    let mut span = (0, 0);
    let mut last_env: Option<&str> = None;
    for (i, g) in groups.iter().enumerate() {
        let Some(sp) = app.pending.service(&g.env_id, &g.service_id) else {
            continue;
        };
        if last_env != Some(g.env_id.as_str()) {
            last_env = Some(g.env_id.as_str());
            let colour = if is_production(&sp.env_name) {
                t.bad
            } else {
                t.accent
            };
            lines.push(Line::from(vec![
                Span::styled("ENV ", fg(t.accent).add_modifier(Modifier::BOLD)),
                Span::styled(clean(&sp.env_name), fg(colour).add_modifier(Modifier::BOLD)),
            ]));
        }
        let start = lines.len();
        let applied = sp.vars.values().filter(|v| v.applied).count();
        let mut head = format!("  {}", clean(&sp.service_name));
        if g.orphaned {
            head.push_str("  (environment no longer exists)");
        }
        if sp.written_not_deployed {
            head.push_str("  written, not deployed");
        } else if applied > 0 && applied < sp.vars.len() {
            head.push_str("  partly written");
        }
        let mut style = fg(t.text).add_modifier(Modifier::BOLD);
        if i == selected {
            style = style.bg(t.select_bg);
        }
        lines.push(Line::styled(head, style));
        for (name, var) in &sp.vars {
            lines.push(change_line(app, view, name, var));
        }
        if i == selected {
            span = (start, lines.len());
        }
    }
    let height = area.height as usize;
    let scroll = span.1.saturating_sub(height).min(span.0);
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), area);
}

fn change_line(app: &App, view: &PendingView, name: &str, var: &PendingVar) -> Line<'static> {
    let t = app.theme();
    let reveal = view.reveal_all;
    let (marker, text) = match (&var.change, &var.base) {
        (Change::Set { value }, None) => ("+", shown(value, reveal)),
        (Change::Set { value }, Some(old)) => (
            "~",
            format!("{} → {}", shown(old, reveal), shown(value, reveal)),
        ),
        (Change::Delete, old) => (
            "-",
            old.as_deref().map_or(HIDDEN.into(), |o| shown(o, reveal)),
        ),
    };
    let tick = if var.applied { "✓" } else { " " };
    Line::from(vec![
        Span::styled(format!("   {tick} "), fg(t.muted)),
        Span::styled(marker, fg(t.hint)),
        Span::styled(format!(" {}  {text}", clean(name)), fg(t.text)),
    ])
}
