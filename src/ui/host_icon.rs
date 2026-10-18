//! The tile in front of a host: the mark of the system it runs, tinted in
//! that system's colour, with a dot on its corner while a session is open.

use gpui::prelude::*;
use gpui::{div, px, svg, AnyElement, Hsla};

use crate::theme;

/// The tile in the host list. Denser lists, like the palette, pass less.
pub const TILE_SIZE: f32 = 28.;
/// How strong the system's colour is under its mark.
const TILE_TINT: f32 = 0.17;
/// On the light theme a mark is never lighter than this, so the pale brand
/// colours (the penguin's yellow, Alma's green) keep 3:1 against their tile.
const LIGHT_GLYPH_MAX_LIGHTNESS: f32 = 0.3;

/// The mark for a detected OS and the colour it is tinted with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistroMark {
    pub icon: &'static str,
    /// 0xRRGGBB. Mostly the brand colour; the few brands drawn near-black
    /// get a lighter one. `glyph_color` adapts it to the theme.
    pub color: u32,
}

/// Checked in order, against the lowercased OS label. The first name found
/// wins, so names that contain others come first and plain "linux" is last.
const MARKS: &[(&str, DistroMark)] = &[
    ("ubuntu", mark("icons/distro/ubuntu.svg", 0xE95420)),
    // Not plain "mint": that is also in "Peppermint".
    ("linux mint", mark("icons/distro/linuxmint.svg", 0x86BE43)),
    ("raspbian", mark("icons/distro/raspberrypi.svg", 0xC7365F)),
    ("debian", mark("icons/distro/debian.svg", 0xD70A53)),
    ("almalinux", mark("icons/distro/almalinux.svg", 0x86DA2F)),
    ("centos", mark("icons/distro/centos.svg", 0x9C8CF2)),
    ("fedora", mark("icons/distro/fedora.svg", 0x51A2DA)),
    ("arch linux", mark("icons/distro/archlinux.svg", 0x1793D1)),
    ("alpine", mark("icons/distro/alpinelinux.svg", 0x2D9CDB)),
    ("rocky", mark("icons/distro/rockylinux.svg", 0x10B981)),
    ("red hat", mark("icons/distro/redhat.svg", 0xEE0000)),
    ("rhel", mark("icons/distro/redhat.svg", 0xEE0000)),
    ("suse", mark("icons/distro/opensuse.svg", 0x73BA25)),
    // SUSE Linux Enterprise reports itself as "SLES".
    ("sles", mark("icons/distro/opensuse.svg", 0x73BA25)),
    ("kali", mark("icons/distro/kalilinux.svg", 0x6C9CC9)),
    ("manjaro", mark("icons/distro/manjaro.svg", 0x35BF5C)),
    ("gentoo", mark("icons/distro/gentoo.svg", 0x8E7CC3)),
    ("nixos", mark("icons/distro/nixos.svg", 0x7EBAE4)),
    ("oracle", mark("icons/distro/oracle.svg", 0xE8423F)),
    ("freebsd", mark("icons/distro/freebsd.svg", 0xE0453F)),
    // `uname` on a Mac says Darwin.
    ("darwin", mark("icons/distro/apple.svg", 0xA2AAAD)),
    ("macos", mark("icons/distro/apple.svg", 0xA2AAAD)),
    ("linux", mark("icons/distro/linux.svg", 0xFCC624)),
];

const fn mark(icon: &'static str, color: u32) -> DistroMark {
    DistroMark { icon, color }
}

/// The mark for an OS label as the connection probe reports it, such as
/// "Ubuntu 24.04", "Debian GNU/Linux 12" or "Linux 6.1.0".
pub fn distro_mark(os: &str) -> Option<DistroMark> {
    let os = os.to_lowercase();
    MARKS
        .iter()
        .find(|(name, _)| os.contains(name))
        .map(|(_, mark)| *mark)
}

/// `ring` is the colour behind the tile: the status dot is cut out of the
/// tile's corner with it. The mark and the corners scale with `tile_size`.
pub fn host_icon(os: Option<&str>, is_connected: bool, ring: Hsla, tile_size: f32) -> AnyElement {
    let (tile, glyph, icon) = match os.and_then(distro_mark) {
        Some(mark) => {
            let color: Hsla = gpui::rgb(mark.color).into();
            (
                color.opacity(TILE_TINT),
                glyph_color(color, theme::is_light()),
                mark.icon,
            )
        }
        None => (theme::field(), theme::text_dim(), "icons/server.svg"),
    };

    div()
        .relative()
        .flex_none()
        .size(px(tile_size))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px((tile_size / 4.).round()))
        .bg(tile)
        .child(
            svg()
                .path(icon)
                .size(px((tile_size * 0.54).round()))
                .text_color(glyph),
        )
        .when(is_connected, |tile| {
            tile.child(
                div()
                    .absolute()
                    .right(px(-2.))
                    .bottom(px(-2.))
                    .size(px(10.))
                    .rounded_full()
                    .border_2()
                    .border_color(ring)
                    .bg(theme::status_online()),
            )
        })
        .into_any_element()
}

/// The mark's colour on each theme: lifted toward white on the dark one so
/// the darker brands still read, and darkened on the light one so the pale
/// ones do.
fn glyph_color(color: Hsla, is_light: bool) -> Hsla {
    let l = if is_light {
        color.l.min(LIGHT_GLYPH_MAX_LIGHTNESS)
    } else {
        color.l + (1. - color.l) * 0.2
    };
    Hsla { l, ..color }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon_for(os: &str) -> Option<&'static str> {
        distro_mark(os).map(|mark| mark.icon)
    }

    #[test]
    fn os_labels_from_the_probe_find_their_mark() {
        assert_eq!(icon_for("Ubuntu 24.04"), Some("icons/distro/ubuntu.svg"));
        assert_eq!(icon_for("ubuntu"), Some("icons/distro/ubuntu.svg"));
        assert_eq!(
            icon_for("AlmaLinux 8.10"),
            Some("icons/distro/almalinux.svg")
        );
        assert_eq!(
            icon_for("Rocky Linux 9.3"),
            Some("icons/distro/rockylinux.svg")
        );
        assert_eq!(icon_for("Darwin 23.1.0"), Some("icons/distro/apple.svg"));
    }

    #[test]
    fn a_distro_wins_over_the_linux_in_its_name() {
        assert_eq!(
            icon_for("Debian GNU/Linux 12"),
            Some("icons/distro/debian.svg")
        );
        assert_eq!(
            icon_for("Linux Mint 21.3"),
            Some("icons/distro/linuxmint.svg")
        );
        assert_eq!(
            icon_for("Red Hat Enterprise Linux 9.2"),
            Some("icons/distro/redhat.svg")
        );
        assert_eq!(
            icon_for("Raspbian GNU/Linux 11"),
            Some("icons/distro/raspberrypi.svg")
        );
    }

    #[test]
    fn plain_linux_gets_the_penguin_and_the_rest_gets_nothing() {
        assert_eq!(
            icon_for("Linux 6.1.0-18-amd64"),
            Some("icons/distro/linux.svg")
        );
        assert_eq!(
            icon_for("Amazon Linux 2023"),
            Some("icons/distro/linux.svg")
        );
        assert_eq!(icon_for("SunOS 5.11"), None);
        assert_eq!(icon_for(""), None);
    }

    #[test]
    fn mint_and_sles_are_matched_by_their_own_names() {
        assert_eq!(
            icon_for("Linux Mint 22"),
            Some("icons/distro/linuxmint.svg")
        );
        assert_eq!(icon_for("Peppermint 11"), None);
        assert_eq!(icon_for("SLES 15.5"), Some("icons/distro/opensuse.svg"));
    }

    fn luminance(color: Hsla) -> f32 {
        let rgba = gpui::Rgba::from(color);
        let channel = |c: f32| {
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
    }

    /// `tint` at `alpha` over the opaque `ground`.
    fn blend(tint: Hsla, alpha: f32, ground: Hsla) -> Hsla {
        let (t, g) = (gpui::Rgba::from(tint), gpui::Rgba::from(ground));
        let mix = |a: f32, b: f32| a * alpha + b * (1. - alpha);
        gpui::Rgba {
            r: mix(t.r, g.r),
            g: mix(t.g, g.g),
            b: mix(t.b, g.b),
            a: 1.,
        }
        .into()
    }

    /// Marks are graphics, so they need 3:1 against their tile, which sits
    /// on the host card (`card` of each theme).
    #[test]
    fn every_mark_reads_against_its_tile_on_both_themes() {
        for (is_light, card) in [(false, 0x272A32), (true, 0xF7F8FA)] {
            let card: Hsla = gpui::rgb(card).into();
            for (name, mark) in MARKS {
                let color: Hsla = gpui::rgb(mark.color).into();
                let tile = blend(color, TILE_TINT, card);
                let glyph = glyph_color(color, is_light);
                let (a, b) = (luminance(glyph), luminance(tile));
                let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                assert!(
                    ratio >= 3.,
                    "{name} on the {} theme is {ratio:.2}:1",
                    if is_light { "light" } else { "dark" }
                );
            }
        }
    }

    /// The app embeds `assets/`; a mark that is not there would be blank.
    #[test]
    fn every_mark_is_in_the_assets() {
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        for (name, mark) in MARKS {
            assert!(
                assets.join(mark.icon).exists(),
                "{name}: {} is missing",
                mark.icon
            );
        }
    }
}
