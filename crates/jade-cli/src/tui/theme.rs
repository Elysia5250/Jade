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
    pub(crate) hover: Color,
    pub(crate) surface: Color,
    /// Full-screen background. Explicit so a transparent terminal or a
    /// wallpaper cannot leak through as random colour bands.
    pub(crate) bg: Color,
    /// Sidebar/editor background, one step off `bg`.
    pub(crate) panel: Color,
    pub(crate) link: Color,
    pub(crate) diff_add_bg: Color,
    pub(crate) diff_add_fg: Color,
    pub(crate) diff_del_bg: Color,
    pub(crate) diff_del_fg: Color,
    pub(crate) diff_word_add: Color,
    pub(crate) diff_word_del: Color,
    pub(crate) syntax_kw: Color,
    pub(crate) syntax_str: Color,
    pub(crate) syntax_num: Color,
    pub(crate) syntax_comment: Color,
    pub(crate) syntax_type: Color,
}

impl Theme {
    /// Downgrade 24-bit RGB tokens to the closest xterm 256-color palette index
    /// for terminals without truecolor support (e.g. macOS Terminal.app).
    pub(crate) fn to_ansi256(self) -> Self {
        Self {
            accent: color_to_ansi256(self.accent),
            secondary: color_to_ansi256(self.secondary),
            dim: color_to_ansi256(self.dim),
            text: color_to_ansi256(self.text),
            ok: color_to_ansi256(self.ok),
            warn: color_to_ansi256(self.warn),
            bad: color_to_ansi256(self.bad),
            border: color_to_ansi256(self.border),
            selection: color_to_ansi256(self.selection),
            hover: color_to_ansi256(self.hover),
            surface: color_to_ansi256(self.surface),
            bg: color_to_ansi256(self.bg),
            panel: color_to_ansi256(self.panel),
            link: color_to_ansi256(self.link),
            diff_add_bg: color_to_ansi256(self.diff_add_bg),
            diff_add_fg: color_to_ansi256(self.diff_add_fg),
            diff_del_bg: color_to_ansi256(self.diff_del_bg),
            diff_del_fg: color_to_ansi256(self.diff_del_fg),
            diff_word_add: color_to_ansi256(self.diff_word_add),
            diff_word_del: color_to_ansi256(self.diff_word_del),
            syntax_kw: color_to_ansi256(self.syntax_kw),
            syntax_str: color_to_ansi256(self.syntax_str),
            syntax_num: color_to_ansi256(self.syntax_num),
            syntax_comment: color_to_ansi256(self.syntax_comment),
            syntax_type: color_to_ansi256(self.syntax_type),
        }
    }
}

fn color_to_ansi256(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    const LEVELS: [u8; 6] = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    let nearest_level = |v: u8| -> usize {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, &lv)| v.abs_diff(lv))
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let ri = nearest_level(r);
    let gi = nearest_level(g);
    let bi = nearest_level(b);
    let cube_idx = (16 + 36 * ri + 6 * gi + bi) as u8;
    let dist_sq = |a: (u8, u8, u8), c: (u8, u8, u8)| -> u32 {
        let dr = a.0 as i32 - c.0 as i32;
        let dg = a.1 as i32 - c.1 as i32;
        let db = a.2 as i32 - c.2 as i32;
        (dr * dr + dg * dg + db * db) as u32
    };
    let cube_dist = dist_sq((r, g, b), (LEVELS[ri], LEVELS[gi], LEVELS[bi]));

    let avg = ((r as u16 + g as u16 + b as u16) / 3) as u8;
    let gray_step = if avg < 8 {
        0
    } else if avg > 238 {
        23
    } else {
        ((avg - 8) as usize + 5) / 10
    }
    .min(23);
    let gray_val = (8 + gray_step * 10) as u8;
    let gray_dist = dist_sq((r, g, b), (gray_val, gray_val, gray_val));

    if gray_dist < cube_dist {
        Color::Indexed((232 + gray_step) as u8)
    } else {
        Color::Indexed(cube_idx)
    }
}

pub(crate) const THEMES: [(&str, Theme); 3] = [
    (
        "ink",
        Theme {
            accent: Color::Rgb(0x7e, 0xe0, 0xc6),
            secondary: Color::Rgb(0xb7, 0xc5, 0xff),
            dim: Color::Rgb(0x87, 0x90, 0xa2),
            text: Color::Rgb(0xe7, 0xe9, 0xee),
            ok: Color::Rgb(0x8f, 0xd9, 0x9a),
            warn: Color::Rgb(0xe6, 0xc0, 0x7b),
            bad: Color::Rgb(0xf0, 0x71, 0x78),
            border: Color::Rgb(0x2c, 0x35, 0x47),
            selection: Color::Rgb(0x1f, 0x28, 0x38),
            hover: Color::Rgb(0x19, 0x21, 0x30),
            surface: Color::Rgb(0x17, 0x1d, 0x29),
            bg: Color::Rgb(0x0d, 0x10, 0x15),
            panel: Color::Rgb(0x13, 0x18, 0x22),
            link: Color::Rgb(0x73, 0xd0, 0xff),
            diff_add_bg: Color::Rgb(0x13, 0x2a, 0x1e),
            diff_add_fg: Color::Rgb(0x8f, 0xd9, 0x9a),
            diff_del_bg: Color::Rgb(0x2e, 0x16, 0x1b),
            diff_del_fg: Color::Rgb(0xf0, 0x71, 0x78),
            diff_word_add: Color::Rgb(0x1d, 0x44, 0x2d),
            diff_word_del: Color::Rgb(0x4b, 0x1f, 0x28),
            syntax_kw: Color::Rgb(0xc7, 0x92, 0xea),
            syntax_str: Color::Rgb(0xa5, 0xe0, 0x75),
            syntax_num: Color::Rgb(0xf7, 0x8c, 0x6c),
            syntax_comment: Color::Rgb(0x63, 0x6d, 0x83),
            syntax_type: Color::Rgb(0x82, 0xaa, 0xff),
        },
    ),
    (
        "tokyonight",
        Theme {
            accent: Color::Rgb(0x7a, 0xa2, 0xf7),
            secondary: Color::Rgb(0xbb, 0x9a, 0xf7),
            dim: Color::Rgb(0x7f, 0x87, 0xb0),
            text: Color::Rgb(0xc0, 0xca, 0xf5),
            ok: Color::Rgb(0x9e, 0xce, 0x6a),
            warn: Color::Rgb(0xe0, 0xaf, 0x68),
            bad: Color::Rgb(0xf7, 0x76, 0x8e),
            border: Color::Rgb(0x3b, 0x42, 0x61),
            selection: Color::Rgb(0x28, 0x34, 0x57),
            hover: Color::Rgb(0x23, 0x2a, 0x45),
            surface: Color::Rgb(0x1e, 0x20, 0x30),
            bg: Color::Rgb(0x1a, 0x1b, 0x26),
            panel: Color::Rgb(0x1f, 0x23, 0x35),
            link: Color::Rgb(0x7d, 0xcf, 0xff),
            diff_add_bg: Color::Rgb(0x1a, 0x2b, 0x22),
            diff_add_fg: Color::Rgb(0x9e, 0xce, 0x6a),
            diff_del_bg: Color::Rgb(0x30, 0x1a, 0x24),
            diff_del_fg: Color::Rgb(0xf7, 0x76, 0x8e),
            diff_word_add: Color::Rgb(0x27, 0x46, 0x34),
            diff_word_del: Color::Rgb(0x52, 0x25, 0x36),
            syntax_kw: Color::Rgb(0xbb, 0x9a, 0xf7),
            syntax_str: Color::Rgb(0x9e, 0xce, 0x6a),
            syntax_num: Color::Rgb(0xff, 0x9e, 0x64),
            syntax_comment: Color::Rgb(0x56, 0x5f, 0x89),
            syntax_type: Color::Rgb(0x2a, 0xc3, 0xde),
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
            hover: Color::Rgb(0xec, 0xef, 0xf7),
            surface: Color::Rgb(0xf3, 0xf4, 0xf7),
            bg: Color::Rgb(0xff, 0xff, 0xff),
            panel: Color::Rgb(0xf5, 0xf6, 0xf9),
            link: Color::Rgb(0x09, 0x69, 0xda),
            diff_add_bg: Color::Rgb(0xe6, 0xf6, 0xea),
            diff_add_fg: Color::Rgb(0x1a, 0x63, 0x28),
            diff_del_bg: Color::Rgb(0xfd, 0xec, 0xed),
            diff_del_fg: Color::Rgb(0xa4, 0x1d, 0x24),
            diff_word_add: Color::Rgb(0xc2, 0xeb, 0xcc),
            diff_word_del: Color::Rgb(0xf7, 0xc5, 0xc8),
            syntax_kw: Color::Rgb(0x82, 0x50, 0xdf),
            syntax_str: Color::Rgb(0x11, 0x63, 0x29),
            syntax_num: Color::Rgb(0x95, 0x38, 0x00),
            syntax_comment: Color::Rgb(0x6e, 0x77, 0x81),
            syntax_type: Color::Rgb(0x05, 0x50, 0xae),
        },
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi256_converts_all_themes_to_indexed_colors() {
        for (_, theme) in THEMES {
            let down = theme.to_ansi256();
            for c in [down.bg, down.panel, down.text, down.accent, down.border] {
                assert!(matches!(c, Color::Indexed(_)), "expected indexed: {c:?}");
            }
        }
        assert_eq!(color_to_ansi256(Color::Rgb(0, 0, 0)), Color::Indexed(16));
        assert_eq!(
            color_to_ansi256(Color::Rgb(255, 255, 255)),
            Color::Indexed(231)
        );
    }
}
