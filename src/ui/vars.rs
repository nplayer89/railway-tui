//! The Variables view: a title line over a marker / NAME / VALUE table.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Cell, Paragraph, Row};
use ratatui::Frame;

use super::tables::{fg, header, range_style, render_table, row_style};
use super::{clean, is_production};
use crate::app::{App, VarRow, VarsView};
use crate::pending::Change;

pub(super) const HIDDEN: &str = "••••••";

pub(super) const OPAQUE: &str = "<Opaque>";
pub(super) const EMPTY: &str = "<Empty>";

/// The value as shown: `<Empty>` for an empty value, dots while hidden,
/// otherwise the first line, cleaned.
pub(super) fn shown(value: &str, revealed: bool) -> String {
    if value.is_empty() {
        return EMPTY.into();
    }
    if !revealed {
        return HIDDEN.into();
    }
    let mut lines = value.lines();
    let first = clean(lines.next().unwrap_or(""));
    if lines.next().is_some() {
        format!("{first}…")
    } else {
        first
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App, view: &VarsView) {
    let t = app.theme();
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);

    let service = app
        .services
        .value
        .iter()
        .flatten()
        .find(|s| s.service_id == view.service_id)
        .map_or("-", |s| s.service_name.as_str());
    let env = app.environment.as_ref().map_or("-", |e| e.name.as_str());
    let env_colour = if is_production(env) { t.bad } else { t.accent };
    let visual = app.visual_range();
    let spans = vec![
        Span::styled(
            format!("{} · variables · ENV ", clean(service)),
            fg(t.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(clean(env), fg(env_colour).add_modifier(Modifier::BOLD)),
    ];
    frame.render_widget(Paragraph::new(ratatui::text::Line::from(spans)), title);

    let selected = app.selected_index();
    let rows: Vec<Row> = app
        .variable_rows()
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let in_range = visual.as_ref().is_some_and(|v| v.contains(&i));
            row(app, view, &r, selected == Some(i), in_range)
        })
        .collect();
    let empty = if app.variables.loading {
        "loading…"
    } else {
        "no variables"
    };
    render_table(
        frame,
        body,
        header(t, &["", "NAME", "VALUE"]),
        rows,
        vec![
            Constraint::Length(2),
            Constraint::Length(32),
            Constraint::Fill(1),
        ],
        selected,
        Span::styled(empty, fg(t.muted)),
    );
}

/// `in_range`: inside the visual range (the cursor row keeps the selected style).
fn row(app: &App, view: &VarsView, r: &VarRow, selected: bool, in_range: bool) -> Row<'static> {
    let t = app.theme();
    let (marker, value, deleted) = match r.pending.map(|p| (&p.change, &p.base)) {
        Some((Change::Set { value }, None)) => ("+", Some(value.as_str()), false),
        Some((Change::Set { value }, Some(_))) => ("~", Some(value.as_str()), false),
        Some((Change::Delete, _)) => ("-", r.current, true),
        None => (" ", r.current, false),
    };
    let revealed = view.reveal_all || view.revealed.contains(r.name);
    let mut style = fg(t.text);
    if deleted {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    Row::new([
        Cell::from(marker).style(if r.pending.is_some() {
            fg(t.hint)
        } else {
            Style::new()
        }),
        Cell::from(clean(r.name)),
        Cell::from(if r.sealed {
            OPAQUE.into()
        } else {
            value.map_or(String::new(), |v| shown(v, revealed))
        }),
    ])
    .style(if in_range && !selected {
        range_style(t, style)
    } else {
        row_style(t, style, selected)
    })
}
