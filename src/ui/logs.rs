//! The log view: title, stream/follow badges and the visible window of lines.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::clean;
use crate::app::LogsView;
use crate::logs::follow::StreamState;
use crate::model::LogKind;
use crate::theme::Theme;

/// `building` is the elapsed build time while the deployment is building.
pub fn draw(frame: &mut Frame, area: Rect, logs: &LogsView, building: Option<&str>, t: &Theme) {
    let [title, badges, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);

    let kind = match logs.kind {
        LogKind::Build => "BUILD",
        LogKind::Runtime => "RUNTIME",
    };
    let mut text = format!(
        "{} · {} · {}",
        clean(&logs.service_name),
        clean(&logs.deployment_id),
        kind
    );
    if let Some(replica) = &logs.replica {
        text.push_str(&format!(" · replica {}", clean(replica)));
    }
    if let Some(filter) = logs.filter.as_deref().filter(|f| !f.is_empty()) {
        text.push_str(&format!(" · filter: {}", clean(filter)));
    }
    frame.render_widget(
        Paragraph::new(Span::styled(
            text,
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD),
        )),
        title,
    );

    let (stream, line_style) = match &logs.state {
        StreamState::Live => ("● LIVE".to_string(), Style::new().fg(t.ok)),
        StreamState::Connecting => ("CONNECTING…".to_string(), Style::new().fg(t.busy)),
        StreamState::Reconnecting { attempt, retry_in } => (
            format!(
                "RECONNECTING (attempt {attempt}, retry in {}s)",
                retry_in.as_secs()
            ),
            Style::new().fg(t.bg).bg(t.busy),
        ),
        StreamState::Failed(msg) => (
            format!("DISCONNECTED: {}", clean(msg)),
            Style::new().fg(t.bg).bg(t.bad),
        ),
    };
    let follow = if logs.follow { "FOLLOW" } else { "PAUSED" };
    let mut badge = vec![Span::raw(format!("{stream}  {follow}"))];
    if let Some(e) = building {
        badge.push(Span::raw("  "));
        // On a banner background the line's own colours keep it readable.
        let style = if line_style.bg.is_none() {
            Style::new().fg(t.busy)
        } else {
            Style::new()
        };
        badge.push(Span::styled(format!("BUILDING {e}"), style));
    }
    frame.render_widget(Paragraph::new(Line::from(badge)).style(line_style), badges);

    let lines = logs
        .buffer
        .visible(logs.filter.as_deref(), logs.replica.as_deref());
    let height = body.height as usize;
    // `scroll` is lines up from the bottom; the filter may have shrunk the list since.
    let scroll = logs.scroll.min(lines.len().saturating_sub(1));
    let end = lines.len() - scroll;
    let start = end.saturating_sub(height);
    let shown: Vec<Line> = lines[start..end]
        .iter()
        .map(|l| {
            Line::from(vec![
                Span::styled(
                    l.timestamp.format("%H:%M:%S ").to_string(),
                    Style::new().fg(t.muted),
                ),
                Span::styled(clean(&l.message), Style::new().fg(t.text)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(shown), body);
}
