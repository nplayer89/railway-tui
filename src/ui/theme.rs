//! The colour palette (Tokyo Night). Every colour the UI draws comes from here.

use ratatui::style::Color;

/// Primary text: names and values.
pub const TEXT: Color = Color::Rgb(0xc0, 0xca, 0xf5);
/// Secondary text: timestamps, commits, labels, hints.
pub const MUTED: Color = Color::Rgb(0x73, 0x7a, 0xa2);
/// Headings and the breadcrumb.
pub const ACCENT: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
/// Key hints and the active filter.
pub const HINT: Color = Color::Rgb(0xbb, 0x9a, 0xf7);
/// Selected row background; no foreground colour here may equal it.
pub const SELECT_BG: Color = Color::Rgb(0x2a, 0x30, 0x4a);
/// Text drawn on a coloured badge background.
pub const ON_BADGE: Color = Color::Rgb(0x1a, 0x1b, 0x26);

pub const OK: Color = Color::Rgb(0x9e, 0xce, 0x6a);
pub const BAD: Color = Color::Rgb(0xf7, 0x76, 0x8e);
pub const BUSY: Color = Color::Rgb(0xe0, 0xaf, 0x68);
pub const IDLE: Color = Color::Rgb(0x56, 0x5f, 0x89);
