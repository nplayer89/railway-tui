//! The NvChad-style status bar and the vim-style command line beneath it.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{clean, is_production};
use crate::app::{App, Confirm, Modal, Mode};
use crate::theme::Theme;

// Nerd Font v3 glyphs; there is no plain-text fallback.
const SEPARATOR: &str = "\u{e0bc}";
const ICON_VIM: &str = "\u{e62b}";
const ICON_GLOBE: &str = "\u{f059f}";
const ICON_BRANCH: &str = "\u{e725}";
const ICON_SITEMAP: &str = "\u{f0631}";
const ICON_LIST: &str = "\u{f03a}";

/// Widest the environment name field grows before the name scrolls.
const ENV_MAX_CELLS: usize = 20;

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

/// `width` display cells of `text`: padded when it fits, otherwise a window onto the endless
/// loop `text` + three spaces, starting `step` chars in. Slices by chars, never bytes, and
/// counts cells, so wide characters never push the field past `width`.
pub(super) fn marquee(text: &str, width: usize, step: u64) -> String {
    let pad = |mut out: String| {
        let used = UnicodeWidthStr::width(out.as_str());
        out.push_str(&" ".repeat(width.saturating_sub(used)));
        out
    };
    if UnicodeWidthStr::width(text) <= width {
        return pad(text.to_string());
    }
    let period = text.chars().count() + 3;
    let start = (step % period as u64) as usize;
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars().chain("   ".chars()).cycle().skip(start) {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(c);
    }
    pad(out)
}

fn width(spans: &[Span]) -> usize {
    Line::from(spans.to_vec()).width()
}

pub(super) fn draw_statusline(frame: &mut Frame, area: Rect, app: &App, step: u64) {
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
                format!(" {ICON_GLOBE} {label} "),
                Style::new().fg(t.text).bg(t.select_bg),
            ),
            Span::styled(SEPARATOR, Style::new().fg(t.select_bg).bg(t.bg)),
        ]
    };
    let branch_block = app.cursor_branch().map(|b| {
        vec![Span::styled(
            format!(" {ICON_BRANCH} {} ", clean(b)),
            Style::new().fg(t.text).bg(t.bg),
        )]
    });
    // Red and bold on production, the accent colour elsewhere. The field fits the name up
    // to the cap; past it, the name scrolls.
    let env_block = app.environment.as_ref().map(|e| {
        let name = clean(&e.name);
        let (tip, text) = if is_production(&e.name) {
            (t.bad, Style::new().add_modifier(Modifier::BOLD))
        } else {
            (t.accent, Style::new())
        };
        let cells = UnicodeWidthStr::width(name.as_str()).min(ENV_MAX_CELLS);
        vec![
            Span::styled(format!(" {ICON_SITEMAP} "), Style::new().fg(t.bg).bg(tip)),
            Span::styled(
                format!(" {} ", marquee(&name, cells, step)),
                text.fg(t.text).bg(t.select_bg),
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
    let count_block = match (mode, app.visual_range()) {
        (Mode::Visual, Some(r)) => Some(vec![Span::styled(
            format!("{} selected ", r.end() - r.start() + 1),
            Style::new().fg(t.muted).bg(t.bg),
        )]),
        _ => None,
    };
    // In drop order: selection count, branch, position. Mode and env stay.
    let mut optional = [count_block, branch_block, position_block];
    let max = area.width as usize;
    let fixed = width(&mode_block) + env_block.as_ref().map_or(0, |b| width(b));
    let total = |optional: &[Option<Vec<Span>>], label: &str| {
        fixed
            + width(&view_block(label))
            + optional.iter().flatten().map(|b| width(b)).sum::<usize>()
    };
    for i in 0..optional.len() {
        if total(&optional, &label) <= max {
            break;
        }
        optional[i] = None;
    }
    let mut label = label;
    let over = total(&optional, &label).saturating_sub(max);
    if over > 0 {
        let keep = label.chars().count().saturating_sub(over);
        label = truncate(&label, keep);
    }
    let [count_block, branch_block, position_block] = optional;

    let mut spans = mode_block.clone();
    spans.extend(view_block(&label));
    spans.extend(branch_block.iter().flatten().cloned());
    let right: Vec<Span> = [&count_block, &env_block, &position_block]
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
