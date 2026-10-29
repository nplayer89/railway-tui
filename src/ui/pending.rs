//! The review screen: every pending change in the open project, by environment then service.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use super::tables::fg;
use super::vars::{shown, HIDDEN};
use super::{centered, clean, is_production, popup};
use crate::app::{App, DeployConfirm, Orphan, PendingView, View};
use crate::pending::{Change, PendingVar};

/// Lines of the deploy confirm around its list of changes: title, service,
/// a blank line, then a blank line and the prompt.
const CONFIRM_FRAME_LINES: usize = 5;

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
        let Some(sp) = app.pending.service(&g.key.env_id, &g.key.service_id) else {
            continue;
        };
        if last_env != Some(g.key.env_id.as_str()) {
            last_env = Some(g.key.env_id.as_str());
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
        match g.orphaned {
            Some(Orphan::Environment) => head.push_str("  (environment no longer exists)"),
            Some(Orphan::Service) => head.push_str("  (service no longer exists)"),
            None => {}
        }
        // Written, but some entries are not on Railway yet (a failed write, or
        // an edit of a written entry): never shown as fully written.
        let unapplied = applied < sp.vars.len();
        if unapplied && (applied > 0 || sp.written_not_deployed) {
            head.push_str("  partly written");
        } else if !unapplied && sp.written_not_deployed {
            head.push_str("  written, not deployed");
        }
        let mut style = fg(t.text).add_modifier(Modifier::BOLD);
        if i == selected {
            style = style.bg(t.select_bg);
        }
        lines.push(Line::styled(head, style));
        for (name, var) in &sp.vars {
            lines.push(change_line(app, view.reveal_all, name, var));
        }
        if i == selected {
            span = (start, lines.len());
        }
    }
    let height = area.height as usize;
    let scroll = span.1.saturating_sub(height).min(span.0);
    frame.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), area);
}

fn change_line(app: &App, reveal: bool, name: &str, var: &PendingVar) -> Line<'static> {
    let t = app.theme();
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

/// The deploy confirm: target, the unapplied changes as on the review screen,
/// a warning per variable changed on Railway since its edit, and the prompt.
/// A list longer than the screen is cut short, warnings last to go.
pub fn draw_deploy_confirm(frame: &mut Frame, area: Rect, app: &App, d: &DeployConfirm) {
    let t = app.theme();
    let Some(sp) = app.pending.service(&d.env_id, &d.service_id) else {
        return;
    };
    let reveal = matches!(app.stack.last(), Some(View::Pending(v)) if v.reveal_all);
    let mut changes: Vec<Line> = sp
        .vars
        .iter()
        .filter(|(_, v)| !v.applied)
        .map(|(name, var)| change_line(app, reveal, name, var))
        .collect();
    if changes.is_empty() {
        changes.push(Line::styled("     redeploy only", fg(t.muted)));
    }
    let mut drift: Vec<Line> = d
        .drift
        .iter()
        .map(|name| {
            Line::styled(
                format!("   ! {}  changed on Railway since your edit", clean(name)),
                fg(t.busy),
            )
        })
        .collect();

    let wanted = CONFIRM_FRAME_LINES + changes.len() + drift.len() + 2;
    let height = wanted.min(area.height as usize) as u16;
    let room = (height as usize).saturating_sub(2 + CONFIRM_FRAME_LINES);
    drift.truncate(room);
    let change_room = room - drift.len();
    if changes.len() > change_room {
        let keep = change_room.saturating_sub(1);
        let more = changes.len() - keep;
        changes.truncate(keep);
        if change_room > 0 {
            changes.push(Line::styled(
                format!("     … {more} more changes"),
                fg(t.muted),
            ));
        }
    }

    let env_colour = if is_production(&sp.env_name) {
        t.bad
    } else {
        t.accent
    };
    let bold = Modifier::BOLD;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(" Deploy to ", fg(t.text).add_modifier(bold)),
            Span::styled(clean(&sp.env_name), fg(env_colour).add_modifier(bold)),
        ]),
        Line::styled(format!(" service: {}", clean(&sp.service_name)), fg(t.text)),
        Line::raw(""),
    ];
    lines.extend(changes);
    lines.extend(drift);
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        " Deploy? y to confirm, any other key cancels",
        fg(t.busy).add_modifier(bold),
    ));
    let rect = centered(area, area.width.saturating_sub(4).min(80), height);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).block(popup(t, " Deploy ")), rect);
}
