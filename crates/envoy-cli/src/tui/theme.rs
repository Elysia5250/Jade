//! Colour palettes. The first entry is the default.
use ratatui::style::Color;

#[derive(Clone, Copy)]
pub(crate) struct Theme {
    pub(crate) accent: Color,
    pub(crate) secondary: Color,
    pub(crate) dim: Color,
    pub(crate) text: Color,
    pub(crate) ok: Color,
    pub(crate) warn: Color,
    pub(crate) bad: Color,
    pub(crate) border: Color,
    pub(crate) selection: Color,
    pub(crate) surface: Color,
    /// Full-screen background. Explicit so a transparent terminal or a
    /// wallpaper cannot leak through as random colour bands.
    pub(crate) bg: Color,
    /// Sidebar/editor background, one step off `bg`.
    pub(crate) panel: Color,
}

pub(crate) const THEMES: [(&str, Theme); 3] = [
    (
        "ink",
        Theme {
            accent: Color::Rgb(0x7e, 0xe0, 0xc6),
            secondary: Color::Rgb(0xb7, 0xc5, 0xff),
            dim: Color::Rgb(0x6d, 0x75, 0x85),
            text: Color::Rgb(0xe7, 0xe9, 0xee),
            ok: Color::Rgb(0x8f, 0xd9, 0x9a),
            warn: Color::Rgb(0xe6, 0xc0, 0x7b),
            bad: Color::Rgb(0xf0, 0x71, 0x78),
            border: Color::Rgb(0x2a, 0x31, 0x40),
            selection: Color::Rgb(0x1c, 0x24, 0x33),
            surface: Color::Rgb(0x18, 0x1e, 0x2a),
            bg: Color::Rgb(0x0e, 0x11, 0x16),
            panel: Color::Rgb(0x15, 0x19, 0x22),
        },
    ),
    (
        "tokyonight",
        Theme {
            accent: Color::Rgb(0x7a, 0xa2, 0xf7),
            secondary: Color::Rgb(0xbb, 0x9a, 0xf7),
            dim: Color::Rgb(0x73, 0x7a, 0xa2),
            text: Color::Rgb(0xc0, 0xca, 0xf5),
            ok: Color::Rgb(0x9e, 0xce, 0x6a),
            warn: Color::Rgb(0xe0, 0xaf, 0x68),
            bad: Color::Rgb(0xf7, 0x76, 0x8e),
            border: Color::Rgb(0x3b, 0x42, 0x61),
            selection: Color::Rgb(0x28, 0x34, 0x57),
            surface: Color::Rgb(0x1e, 0x20, 0x30),
            bg: Color::Rgb(0x1a, 0x1b, 0x26),
            panel: Color::Rgb(0x1f, 0x23, 0x35),
        },
    ),
    (
        "light",
        Theme {
            accent: Color::Rgb(0x3b, 0x5f, 0xc4),
            secondary: Color::Rgb(0x6b, 0x46, 0xc1),
            dim: Color::Rgb(0x66, 0x6a, 0x75),
            text: Color::Rgb(0x24, 0x26, 0x2d),
            ok: Color::Rgb(0x2f, 0x7d, 0x32),
            warn: Color::Rgb(0x9a, 0x6b, 0x00),
            bad: Color::Rgb(0xb3, 0x26, 0x1e),
            border: Color::Rgb(0xb5, 0xb8, 0xc2),
            selection: Color::Rgb(0xe4, 0xe7, 0xf2),
            surface: Color::Rgb(0xf3, 0xf4, 0xf7),
            bg: Color::Rgb(0xff, 0xff, 0xff),
            panel: Color::Rgb(0xf5, 0xf6, 0xf9),
        },
    ),
];

