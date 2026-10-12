use gpui::Hsla;

use crate::preferences::{Preferences, TerminalScheme};
use crate::shell::SnapshotColor;

/// Compact color key for batch equality checks. Avoids Hsla float comparisons.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorKey {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub fn color_key(c: &SnapshotColor) -> ColorKey {
    match c {
        SnapshotColor::Default => ColorKey::Default,
        SnapshotColor::Indexed(i) => ColorKey::Indexed(*i),
        SnapshotColor::Rgb(r, g, b) => ColorKey::Rgb(*r, *g, *b),
    }
}

const fn cube_val(v: usize) -> u8 {
    // The xterm cube levels: 0, 95, 135, 175, 215, 255.
    if v == 0 {
        0
    } else {
        (55 + v * 40) as u8
    }
}

/// ANSI 0-15. The 6x6x6 cube and the grey ramp (16-255) are addressed by number
/// and stay at their standard xterm values in every scheme.
const fn make_xterm_palette(ansi: [(u8, u8, u8); 16]) -> [(u8, u8, u8); 256] {
    let mut p = [(0u8, 0u8, 0u8); 256];

    let mut i = 0usize;
    while i < 16 {
        p[i] = ansi[i];
        i += 1;
    }

    // 16-231: the 6x6x6 RGB cube
    let mut i = 16usize;
    while i < 232 {
        let idx = i - 16;
        let r = idx / 36;
        let g = (idx % 36) / 6;
        let b = idx % 6;
        p[i] = (cube_val(r), cube_val(g), cube_val(b));
        i += 1;
    }

    // 232-255: the grayscale ramp (8, 18, 28, ..., 238)
    let mut i = 232usize;
    while i < 256 {
        let gray = (8 + 10 * (i - 232)) as u8;
        p[i] = (gray, gray, gray);
        i += 1;
    }

    p
}

/// Andromeda Dark ANSI colors.
const ANSI_DARK: [(u8, u8, u8); 16] = [
    (0x1B, 0x1D, 0x23), // black
    (0xEE, 0x5D, 0x43), // red
    (0x96, 0xE0, 0x72), // green
    (0xFF, 0xE6, 0x6D), // yellow
    (0x7C, 0xB7, 0xFF), // blue
    (0xFF, 0x66, 0xC4), // magenta
    (0x5D, 0xFF, 0xDF), // cyan
    (0xD5, 0xCE, 0xD9), // white
    (0x74, 0x6F, 0x77), // bright black
    (0xD0, 0x4A, 0x32), // bright red
    (0x5D, 0xCD, 0x26), // bright green
    (0xFB, 0xD8, 0x31), // bright yellow
    (0x5A, 0x8F, 0xD6), // bright blue
    (0xFF, 0x00, 0xAA), // bright magenta
    (0x00, 0xE8, 0xC6), // bright cyan
    (0xD5, 0xCE, 0xD9), // bright white
];

/// GitHub Light ANSI colors. Andromeda's set is tuned for a dark ground and is
/// unreadable on a white one (#FFE66D yellow is ~1.2:1). One deviation: bright
/// white is #4C535C, not GitHub's #8C959F, which at ~2.6:1 would make
/// bold-white output near-invisible.
const ANSI_LIGHT: [(u8, u8, u8); 16] = [
    (0x24, 0x29, 0x2F), // black
    (0xCF, 0x22, 0x2E), // red
    (0x11, 0x63, 0x29), // green
    (0x4D, 0x2D, 0x00), // yellow
    (0x09, 0x69, 0xDA), // blue
    (0x82, 0x50, 0xDF), // magenta
    (0x1B, 0x7C, 0x83), // cyan
    (0x6E, 0x77, 0x81), // white
    (0x57, 0x60, 0x6A), // bright black
    (0xA4, 0x0E, 0x26), // bright red
    (0x1A, 0x7F, 0x37), // bright green
    (0x63, 0x3C, 0x01), // bright yellow
    (0x21, 0x8B, 0xFF), // bright blue
    (0xA4, 0x75, 0xF9), // bright magenta
    (0x31, 0x92, 0xAA), // bright cyan
    (0x4C, 0x53, 0x5C), // bright white
];

/// Dracula. The selection is the scheme's own "current line" grey.
const ANSI_DRACULA: [(u8, u8, u8); 16] = [
    (0x21, 0x22, 0x2C), // black
    (0xFF, 0x55, 0x55), // red
    (0x50, 0xFA, 0x7B), // green
    (0xF1, 0xFA, 0x8C), // yellow
    (0xBD, 0x93, 0xF9), // blue
    (0xFF, 0x79, 0xC6), // magenta
    (0x8B, 0xE9, 0xFD), // cyan
    (0xF8, 0xF8, 0xF2), // white
    (0x62, 0x72, 0xA4), // bright black
    (0xFF, 0x6E, 0x6E), // bright red
    (0x69, 0xFF, 0x94), // bright green
    (0xFF, 0xFF, 0xA5), // bright yellow
    (0xD6, 0xAC, 0xFF), // bright blue
    (0xFF, 0x92, 0xDF), // bright magenta
    (0xA4, 0xFF, 0xFF), // bright cyan
    (0xFF, 0xFF, 0xFF), // bright white
];

const ANSI_CATPPUCCIN_MOCHA: [(u8, u8, u8); 16] = [
    (0x45, 0x47, 0x5A), // black
    (0xF3, 0x8B, 0xA8), // red
    (0xA6, 0xE3, 0xA1), // green
    (0xF9, 0xE2, 0xAF), // yellow
    (0x89, 0xB4, 0xFA), // blue
    (0xF5, 0xC2, 0xE7), // magenta
    (0x94, 0xE2, 0xD5), // cyan
    (0xBA, 0xC2, 0xDE), // white
    (0x58, 0x5B, 0x70), // bright black
    (0xF3, 0x8B, 0xA8), // bright red
    (0xA6, 0xE3, 0xA1), // bright green
    (0xF9, 0xE2, 0xAF), // bright yellow
    (0x89, 0xB4, 0xFA), // bright blue
    (0xF5, 0xC2, 0xE7), // bright magenta
    (0x94, 0xE2, 0xD5), // bright cyan
    (0xA6, 0xAD, 0xC8), // bright white
];

/// Solarized Dark. One deviation: bright black is base01 (#586E75), not
/// base03, which is the background itself and would hide shell autosuggestions
/// and every other program that prints "dim" text in bright black.
const ANSI_SOLARIZED_DARK: [(u8, u8, u8); 16] = [
    (0x07, 0x36, 0x42), // black
    (0xDC, 0x32, 0x2F), // red
    (0x85, 0x99, 0x00), // green
    (0xB5, 0x89, 0x00), // yellow
    (0x26, 0x8B, 0xD2), // blue
    (0xD3, 0x36, 0x82), // magenta
    (0x2A, 0xA1, 0x98), // cyan
    (0xEE, 0xE8, 0xD5), // white
    (0x58, 0x6E, 0x75), // bright black
    (0xCB, 0x4B, 0x16), // bright red
    (0x58, 0x6E, 0x75), // bright green
    (0x65, 0x7B, 0x83), // bright yellow
    (0x83, 0x94, 0x96), // bright blue
    (0x6C, 0x71, 0xC4), // bright magenta
    (0x93, 0xA1, 0xA1), // bright cyan
    (0xFD, 0xF6, 0xE3), // bright white
];

const ANSI_GRUVBOX_DARK: [(u8, u8, u8); 16] = [
    (0x28, 0x28, 0x28), // black
    (0xCC, 0x24, 0x1D), // red
    (0x98, 0x97, 0x1A), // green
    (0xD7, 0x99, 0x21), // yellow
    (0x45, 0x85, 0x88), // blue
    (0xB1, 0x62, 0x86), // magenta
    (0x68, 0x9D, 0x6A), // cyan
    (0xA8, 0x99, 0x84), // white
    (0x92, 0x83, 0x74), // bright black
    (0xFB, 0x49, 0x34), // bright red
    (0xB8, 0xBB, 0x26), // bright green
    (0xFA, 0xBD, 0x2F), // bright yellow
    (0x83, 0xA5, 0x98), // bright blue
    (0xD3, 0x86, 0x9B), // bright magenta
    (0x8E, 0xC0, 0x7C), // bright cyan
    (0xEB, 0xDB, 0xB2), // bright white
];

const ANSI_TOKYO_NIGHT: [(u8, u8, u8); 16] = [
    (0x15, 0x16, 0x1E), // black
    (0xF7, 0x76, 0x8E), // red
    (0x9E, 0xCE, 0x6A), // green
    (0xE0, 0xAF, 0x68), // yellow
    (0x7A, 0xA2, 0xF7), // blue
    (0xBB, 0x9A, 0xF7), // magenta
    (0x7D, 0xCF, 0xFF), // cyan
    (0xA9, 0xB1, 0xD6), // white
    (0x41, 0x48, 0x68), // bright black
    (0xF7, 0x76, 0x8E), // bright red
    (0x9E, 0xCE, 0x6A), // bright green
    (0xE0, 0xAF, 0x68), // bright yellow
    (0x7A, 0xA2, 0xF7), // bright blue
    (0xBB, 0x9A, 0xF7), // bright magenta
    (0x7D, 0xCF, 0xFF), // bright cyan
    (0xC0, 0xCA, 0xF5), // bright white
];

/// A built-in colour scheme as plain RGB.
struct Scheme {
    xterm: [(u8, u8, u8); 256],
    fg: (u8, u8, u8),
    bg: (u8, u8, u8),
    /// `None` uses the app theme's selection colour.
    selection: Option<(u8, u8, u8)>,
}

// Foreground and background repeat `term_fg` and `term_bg` of the matching
// app theme, so picking the scheme by name looks the same as following it.
static ANDROMEDA: Scheme = Scheme {
    xterm: make_xterm_palette(ANSI_DARK),
    fg: (0xD5, 0xCE, 0xD9),
    bg: (0x23, 0x26, 0x2E),
    selection: None,
};

static GITHUB_LIGHT: Scheme = Scheme {
    xterm: make_xterm_palette(ANSI_LIGHT),
    fg: (0x2A, 0x2F, 0x3A),
    bg: (0xFF, 0xFF, 0xFF),
    selection: None,
};

static DRACULA: Scheme = Scheme {
    xterm: make_xterm_palette(ANSI_DRACULA),
    fg: (0xF8, 0xF8, 0xF2),
    bg: (0x28, 0x2A, 0x36),
    selection: Some((0x44, 0x47, 0x5A)),
};

static CATPPUCCIN_MOCHA: Scheme = Scheme {
    xterm: make_xterm_palette(ANSI_CATPPUCCIN_MOCHA),
    fg: (0xCD, 0xD6, 0xF4),
    bg: (0x1E, 0x1E, 0x2E),
    selection: Some((0x45, 0x47, 0x5A)),
};

// Solarized's own selection (base02) is almost the background colour, so the
// app theme's selection is used instead.
static SOLARIZED_DARK: Scheme = Scheme {
    xterm: make_xterm_palette(ANSI_SOLARIZED_DARK),
    fg: (0x83, 0x94, 0x96),
    bg: (0x00, 0x2B, 0x36),
    selection: None,
};

static GRUVBOX_DARK: Scheme = Scheme {
    xterm: make_xterm_palette(ANSI_GRUVBOX_DARK),
    fg: (0xEB, 0xDB, 0xB2),
    bg: (0x28, 0x28, 0x28),
    selection: Some((0x50, 0x49, 0x45)),
};

static TOKYO_NIGHT: Scheme = Scheme {
    xterm: make_xterm_palette(ANSI_TOKYO_NIGHT),
    fg: (0xC0, 0xCA, 0xF5),
    bg: (0x1A, 0x1B, 0x26),
    selection: Some((0x28, 0x34, 0x57)),
};

/// The built-in scheme behind a preference. `None` for "match app theme".
fn named_scheme(scheme: TerminalScheme) -> Option<&'static Scheme> {
    match scheme {
        TerminalScheme::FollowTheme => None,
        TerminalScheme::Andromeda => Some(&ANDROMEDA),
        TerminalScheme::GithubLight => Some(&GITHUB_LIGHT),
        TerminalScheme::Dracula => Some(&DRACULA),
        TerminalScheme::CatppuccinMocha => Some(&CATPPUCCIN_MOCHA),
        TerminalScheme::SolarizedDark => Some(&SOLARIZED_DARK),
        TerminalScheme::GruvboxDark => Some(&GRUVBOX_DARK),
        TerminalScheme::TokyoNight => Some(&TOKYO_NIGHT),
    }
}

/// The colours the terminal draws with right now: the chosen scheme, or the
/// app theme's own terminal colours when the scheme follows the theme.
#[derive(Clone, Copy)]
pub struct TerminalColors {
    xterm: &'static [(u8, u8, u8); 256],
    pub fg: Hsla,
    pub bg: Hsla,
    /// `None` uses the app theme's selection colour.
    pub selection: Option<Hsla>,
}

impl TerminalColors {
    pub fn current(cx: &gpui::App) -> Self {
        Self::for_scheme(Preferences::terminal_scheme(cx))
    }

    pub(crate) fn for_scheme(scheme: TerminalScheme) -> Self {
        let Some(scheme) = named_scheme(scheme) else {
            let base = if crate::theme::is_light() {
                &GITHUB_LIGHT
            } else {
                &ANDROMEDA
            };
            return Self {
                xterm: &base.xterm,
                fg: crate::theme::term_fg(),
                bg: crate::theme::term_bg(),
                selection: None,
            };
        };
        Self {
            xterm: &scheme.xterm,
            fg: tuple_to_hsla(scheme.fg),
            bg: tuple_to_hsla(scheme.bg),
            selection: scheme.selection.map(tuple_to_hsla),
        }
    }

    /// Background plus red, green, yellow, blue, magenta and cyan: enough to
    /// tell the schemes apart in the settings list.
    pub fn swatch(scheme: TerminalScheme) -> (Hsla, [Hsla; 6]) {
        let colors = Self::for_scheme(scheme);
        let dots = std::array::from_fn(|ix| tuple_to_hsla(colors.xterm[ix + 1]));
        (colors.bg, dots)
    }

    pub fn resolve(&self, color: &SnapshotColor, is_bg: bool) -> Hsla {
        match color {
            SnapshotColor::Default => {
                if is_bg {
                    self.bg
                } else {
                    self.fg
                }
            }
            SnapshotColor::Indexed(idx) => tuple_to_hsla(self.xterm[*idx as usize]),
            SnapshotColor::Rgb(r, g, b) => rgb_to_hsla(*r, *g, *b),
        }
    }
}

fn tuple_to_hsla((r, g, b): (u8, u8, u8)) -> Hsla {
    rgb_to_hsla(r, g, b)
}

fn rgb_to_hsla(r: u8, g: u8, b: u8) -> Hsla {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let l = (max + min) / 2.0;

    let s = if delta == 0.0 {
        0.0
    } else {
        delta / (1.0 - (2.0 * l - 1.0).abs())
    };

    let h = if delta == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / delta).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / delta + 2.0) / 6.0
    } else {
        ((r - g) / delta + 4.0) / 6.0
    };

    Hsla { h, s, l, a: 1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_of(color: Hsla) -> (u8, u8, u8) {
        let rgba = gpui::Rgba::from(color);
        let channel = |v: f32| (v * 255.0).round() as u8;
        (channel(rgba.r), channel(rgba.g), channel(rgba.b))
    }

    #[test]
    fn a_named_scheme_resolves_its_own_default_and_ansi_colours() {
        let colors = TerminalColors::for_scheme(TerminalScheme::Dracula);
        assert_eq!(rgb_of(colors.bg), (0x28, 0x2A, 0x36));
        assert_eq!(rgb_of(colors.fg), (0xF8, 0xF8, 0xF2));
        assert_eq!(colors.resolve(&SnapshotColor::Default, true), colors.bg);
        assert_eq!(colors.resolve(&SnapshotColor::Default, false), colors.fg);
        assert_eq!(
            rgb_of(colors.resolve(&SnapshotColor::Indexed(1), false)),
            (0xFF, 0x55, 0x55)
        );
        assert_eq!(
            rgb_of(colors.resolve(&SnapshotColor::Rgb(1, 2, 3), false)),
            (1, 2, 3)
        );
    }

    #[test]
    fn schemes_share_the_standard_cube_and_grey_ramp() {
        for scheme in TerminalScheme::ALL {
            let colors = TerminalColors::for_scheme(scheme);
            assert_eq!(colors.xterm[16], (0, 0, 0), "{scheme:?}");
            assert_eq!(colors.xterm[231], (255, 255, 255), "{scheme:?}");
            assert_eq!(colors.xterm[232], (8, 8, 8), "{scheme:?}");
            assert_eq!(colors.xterm[255], (238, 238, 238), "{scheme:?}");
        }
    }

    /// WCAG contrast ratio between two colours.
    fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
        fn luminance((r, g, b): (u8, u8, u8)) -> f32 {
            let linear = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        }
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn named_schemes_keep_text_readable() {
        for scheme in TerminalScheme::ALL {
            let Some(named) = named_scheme(scheme) else {
                continue;
            };
            assert!(
                contrast(named.fg, named.bg) >= 4.5,
                "{scheme:?} default text"
            );
            // Bright black carries "dim" text such as shell autosuggestions.
            assert!(
                contrast(named.xterm[8], named.bg) >= 1.8,
                "{scheme:?} bright black"
            );
            // Red through cyan, normal and bright.
            for index in (1..7).chain(9..15) {
                assert!(
                    contrast(named.xterm[index], named.bg) >= 2.5,
                    "{scheme:?} ansi {index}"
                );
            }
            if let Some(selection) = named.selection {
                assert!(
                    contrast(named.fg, selection) >= 3.0,
                    "{scheme:?} selected text"
                );
                assert_ne!(selection, named.bg, "{scheme:?} selection must show");
            }
        }
    }
}
