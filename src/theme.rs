//! Colour themes. Every colour the UI draws comes from the active `Theme`.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// Stable id, used in the config file and the remembered state.
    pub id: &'static str,
    pub name: &'static str,
    /// Screen background; also the text colour on coloured badges.
    pub bg: Color,
    /// Primary text: names and values.
    pub text: Color,
    /// Secondary text: timestamps, commits, labels.
    pub muted: Color,
    /// Headings and the breadcrumb.
    pub accent: Color,
    /// Key hints, tags and the active filter.
    pub hint: Color,
    /// Selected row background; no foreground colour may equal it.
    pub select_bg: Color,
    pub ok: Color,
    pub bad: Color,
    pub busy: Color,
    pub idle: Color,
}

const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

const fn theme(
    id: &'static str,
    name: &'static str,
    [bg, text, muted, accent, hint, select_bg, ok, bad, busy, idle]: [u32; 10],
) -> Theme {
    Theme {
        id,
        name,
        bg: hex(bg),
        text: hex(text),
        muted: hex(muted),
        accent: hex(accent),
        hint: hex(hint),
        select_bg: hex(select_bg),
        ok: hex(ok),
        bad: hex(bad),
        busy: hex(busy),
        idle: hex(idle),
    }
}

/// All themes, in picker order. The first is the default.
#[rustfmt::skip]
pub const THEMES: &[Theme] = &[
    //                                                     bg        text      muted     accent    hint      select    ok        bad       busy      idle
    theme("tokyo-night",          "Tokyo Night",          [0x1a1b26, 0xc0caf5, 0x737aa2, 0x7aa2f7, 0xbb9af7, 0x2a304a, 0x9ece6a, 0xf7768e, 0xe0af68, 0x565f89]),
    theme("tokyo-night-storm",    "Tokyo Night Storm",    [0x24283b, 0xc0caf5, 0x737aa2, 0x7aa2f7, 0xbb9af7, 0x2f3549, 0x9ece6a, 0xf7768e, 0xe0af68, 0x565f89]),
    theme("catppuccin-mocha",     "Catppuccin Mocha",     [0x1e1e2e, 0xcdd6f4, 0x7f849c, 0x89b4fa, 0xcba6f7, 0x313244, 0xa6e3a1, 0xf38ba8, 0xf9e2af, 0x6c7086]),
    theme("catppuccin-macchiato", "Catppuccin Macchiato", [0x24273a, 0xcad3f5, 0x8087a2, 0x8aadf4, 0xc6a0f6, 0x363a4f, 0xa6da95, 0xed8796, 0xeed49f, 0x6e738d]),
    theme("catppuccin-frappe",    "Catppuccin Frappé",    [0x303446, 0xc6d0f5, 0x838ba7, 0x8caaee, 0xca9ee6, 0x414559, 0xa6d189, 0xe78284, 0xe5c890, 0x737994]),
    theme("catppuccin-latte",     "Catppuccin Latte",     [0xeff1f5, 0x4c4f69, 0x8c8fa1, 0x1e66f5, 0x8839ef, 0xccd0da, 0x40a02b, 0xd20f39, 0xdf8e1d, 0x9ca0b0]),
    theme("dracula",              "Dracula",              [0x282a36, 0xf8f8f2, 0x6272a4, 0xbd93f9, 0xff79c6, 0x44475a, 0x50fa7b, 0xff5555, 0xf1fa8c, 0x6272a4]),
    theme("nord",                 "Nord",                 [0x2e3440, 0xd8dee9, 0x7b88a1, 0x88c0d0, 0xb48ead, 0x3b4252, 0xa3be8c, 0xbf616a, 0xebcb8b, 0x616e88]),
    theme("gruvbox-dark",         "Gruvbox Dark",         [0x282828, 0xebdbb2, 0xa89984, 0x83a598, 0xd3869b, 0x3c3836, 0xb8bb26, 0xfb4934, 0xfabd2f, 0x7c6f64]),
    theme("gruvbox-light",        "Gruvbox Light",        [0xfbf1c7, 0x3c3836, 0x7c6f64, 0x076678, 0x8f3f71, 0xebdbb2, 0x79740e, 0x9d0006, 0xb57614, 0xa89984]),
    theme("solarized-dark",       "Solarized Dark",       [0x002b36, 0x93a1a1, 0x657b83, 0x268bd2, 0x6c71c4, 0x073642, 0x859900, 0xdc322f, 0xb58900, 0x586e75]),
    theme("solarized-light",      "Solarized Light",      [0xfdf6e3, 0x586e75, 0x93a1a1, 0x268bd2, 0x6c71c4, 0xeee8d5, 0x859900, 0xdc322f, 0xb58900, 0x93a1a1]),
    theme("one-dark",             "One Dark",             [0x282c34, 0xabb2bf, 0x7f848e, 0x61afef, 0xc678dd, 0x3e4451, 0x98c379, 0xe06c75, 0xe5c07b, 0x5c6370]),
    theme("rose-pine",            "Rosé Pine",            [0x191724, 0xe0def4, 0x908caa, 0xc4a7e7, 0xebbcba, 0x26233a, 0x9ccfd8, 0xeb6f92, 0xf6c177, 0x6e6a86]),
    theme("kanagawa",             "Kanagawa",             [0x1f1f28, 0xdcd7ba, 0x727169, 0x7e9cd8, 0x957fb8, 0x363646, 0x98bb6c, 0xff5d62, 0xe6c384, 0x54546d]),
    theme("everforest",           "Everforest",           [0x2d353b, 0xd3c6aa, 0x859289, 0x7fbbb3, 0xd699b6, 0x3d484d, 0xa7c080, 0xe67e80, 0xdbbc7f, 0x7a8478]),
];

/// Index of the theme with this id or name (case-insensitive).
pub fn find(id_or_name: &str) -> Option<usize> {
    let want = id_or_name.trim().to_lowercase();
    THEMES
        .iter()
        .position(|t| t.id == want || t.name.to_lowercase() == want)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_findable() {
        for (i, t) in THEMES.iter().enumerate() {
            assert_eq!(find(t.id), Some(i));
            assert_eq!(find(&t.name.to_uppercase()), Some(i));
        }
        assert_eq!(find("no-such-theme"), None);
    }

    #[test]
    fn every_foreground_differs_from_the_backgrounds() {
        for t in THEMES {
            for fg in [
                t.text, t.muted, t.accent, t.hint, t.ok, t.bad, t.busy, t.idle,
            ] {
                assert_ne!(fg, t.bg, "{}", t.id);
                assert_ne!(fg, t.select_bg, "{}", t.id);
            }
        }
    }
}
