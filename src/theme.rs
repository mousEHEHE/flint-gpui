//! The app palette.
//!
//! Every colour the app paints lives in one `Palette`, instantiated twice:
//! `DARK` (Andromeda) and `LIGHT`. A process-wide flag picks which one the
//! zero-argument accessors below read, so switching the theme is one atomic
//! store plus a repaint.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::LazyLock;

use gpui::{px, App, Hsla, Window, WindowAppearance};
use gpui_component::theme::{Theme, ThemeMode, ThemeTokens};

use crate::preferences::ThemePreference;

/// One complete set of app colours. `DARK` and `LIGHT` are the only instances.
struct Palette {
    // Surfaces and text
    /// The main ground the app sits on.
    background: Hsla,
    /// Raised chrome: title bar, sidebar, tab bar, panels.
    surface: Hsla,
    /// Recessed strip: list/table headers, code blocks, key chips.
    surface_sunken: Hsla,
    /// Floating surfaces: popovers, context menus, dropdowns. In light mode
    /// these stay pure white and lift off the tinted chrome by border + shadow.
    surface_float: Hsla,
    /// The settings modal's content pane, the lighter ground of the two.
    modal_surface: Hsla,
    /// The settings modal's nav rail and inner list panes, the tinted one.
    modal_rail: Hsla,
    border: Hsla,
    /// Border for surfaces that float above the chrome, where `border` is too
    /// faint to read.
    border_focused: Hsla,
    text: Hsla,
    text_muted: Hsla,
    text_dim: Hsla,
    text_placeholder: Hsla,
    /// Code and read-only field text, one notch off `text`.
    text_editor: Hsla,

    // Interactive fills. Light mode inverts the ladder: hover and press get
    // darker, not lighter.
    element: Hsla,
    element_hover: Hsla,
    element_active: Hsla,
    /// Switch track in the off position. It has to read against a white thumb.
    switch_off: Hsla,
    switch_thumb: Hsla,

    // Accent
    accent: Hsla,
    accent_hover: Hsla,
    accent_active: Hsla,
    /// Text painted on top of a filled accent surface.
    accent_ink: Hsla,
    accent_soft: Hsla,
    accent_soft_press: Hsla,
    accent_line: Hsla,

    // Status
    status_off: Hsla,
    conflict: Hsla,

    // Semantic
    danger: Hsla,
    danger_hover: Hsla,
    danger_active: Hsla,
    danger_ink: Hsla,
    success: Hsla,
    success_hover: Hsla,
    success_active: Hsla,
    success_ink: Hsla,
    warning: Hsla,
    warning_hover: Hsla,
    warning_active: Hsla,
    warning_ink: Hsla,

    // Base hues
    /// Failed-run red, one step off `danger`.
    run_red: Hsla,
    /// Window-control "maximize" green, also the "in use" badge.
    green_vc: Hsla,
    blue_light: Hsla,
    yellow: Hsla,
    yellow_light: Hsla,
    magenta: Hsla,
    magenta_light: Hsla,
    cyan: Hsla,
    cyan_light: Hsla,

    // Chrome and detail
    selection: Hsla,
    drop_target: Hsla,
    scrollbar_thumb: Hsla,
    scrollbar_thumb_hover: Hsla,
    /// Modal scrim.
    overlay: Hsla,
    /// Two depths of "pressed into the surface" wash.
    sunken_soft: Hsla,
    sunken: Hsla,
    search_hit: Hsla,
    search_hit_active: Hsla,

    // Terminal
    term_bg: Hsla,
    term_fg: Hsla,
}

/// Andromeda.
#[rustfmt::skip]
static DARK: LazyLock<Palette> = LazyLock::new(|| Palette {
    background:            gpui::rgb(0x23262E).into(),
    surface:               gpui::rgb(0x2B303B).into(),
    surface_sunken:        gpui::rgb(0x1B1D23).into(),
    surface_float:         gpui::rgb(0x2B303B).into(),
    modal_surface:         gpui::rgb(0x2B303B).into(),
    modal_rail:            gpui::rgb(0x23262E).into(),
    border:                gpui::rgb(0x1B1D23).into(),
    border_focused:        gpui::rgb(0x746F77).into(),
    text:                  gpui::rgb(0xE1DDE4).into(),
    text_muted:            gpui::rgb(0xADADAD).into(),
    text_dim:              gpui::rgb(0x9A9AA0).into(),
    text_placeholder:      gpui::rgb(0x69696A).into(),
    text_editor:           gpui::rgb(0xD5CED9).into(),

    element:               gpui::rgb(0x383A3F).into(),
    element_hover:         gpui::rgb(0x434853).into(),
    element_active:        gpui::rgb(0x4F535C).into(),
    switch_off:            gpui::rgb(0x383A3F).into(),
    switch_thumb:          gpui::rgb(0xE1DDE4).into(),

    accent:                gpui::rgb(0x74C7EC).into(),
    accent_hover:          gpui::hsla(0.551, 0.760, 0.770, 1.), // #74C7EC L+8%
    accent_active:         gpui::hsla(0.551, 0.760, 0.590, 1.), // #74C7EC L-10%
    accent_ink:            gpui::rgb(0x051A27).into(),
    accent_soft:           gpui::rgba(0x74C7EC22).into(),
    accent_soft_press:     gpui::rgba(0x74C7EC33).into(),
    accent_line:           gpui::rgba(0x74C7EC44).into(),

    status_off:            gpui::rgb(0x565A64).into(),
    conflict:              gpui::rgb(0xFAB387).into(),

    danger:                gpui::rgb(0xFC644D).into(),
    danger_hover:          gpui::hsla(0.022, 0.967, 0.715, 1.), // L+7%
    danger_active:         gpui::hsla(0.022, 0.967, 0.545, 1.), // L-10%
    danger_ink:            gpui::rgb(0xF7F7F7).into(),
    success:               gpui::rgb(0x96E072).into(),
    success_hover:         gpui::hsla(0.279, 0.640, 0.733, 1.), // L+7%
    success_active:        gpui::hsla(0.279, 0.640, 0.563, 1.), // L-10%
    success_ink:           gpui::rgb(0x1B1D23).into(),
    warning:               gpui::rgb(0xFF9F2E).into(),
    warning_hover:         gpui::hsla(0.090, 1.000, 0.660, 1.), // L+7%
    warning_active:        gpui::hsla(0.090, 1.000, 0.490, 1.), // L-10%
    warning_ink:           gpui::rgb(0x1B1D23).into(),

    run_red:               gpui::rgb(0xEE5D43).into(),
    green_vc:              gpui::rgb(0x9BC53D).into(),
    blue_light:            gpui::rgb(0x7CB7FF).into(),
    yellow:                gpui::rgb(0xFFE66D).into(),
    yellow_light:          gpui::rgb(0xFBD831).into(),
    magenta:               gpui::rgb(0xC74DED).into(),
    magenta_light:         gpui::rgb(0xFF66C4).into(),
    cyan:                  gpui::rgb(0x00E8C6).into(),
    cyan_light:            gpui::rgb(0x5DFFDF).into(),

    selection:             gpui::hsla(0.551, 0.760, 0.690, 0.30),
    drop_target:           gpui::rgb(0x3A404E).into(),
    scrollbar_thumb:       gpui::rgba(0x3A3F4C77).into(),
    scrollbar_thumb_hover: gpui::rgba(0x3A3F4CAA).into(),
    overlay:               gpui::rgba(0x00000099).into(),
    sunken_soft:           gpui::rgba(0x0000002E).into(),
    sunken:                gpui::rgba(0x00000038).into(),
    search_hit:            gpui::rgba(0xFFE66D3C).into(),
    search_hit_active:     gpui::rgba(0xFFE66D8C).into(),

    term_bg:               gpui::rgb(0x23262E).into(),
    term_fg:               gpui::rgb(0xD5CED9).into(),
});

/// The light counterpart. Every hue that carries text is darkened until it
/// clears WCAG AA (4.5:1) on every light ground it can land on. The contrast
/// test owns the matrix.
#[rustfmt::skip]
static LIGHT: LazyLock<Palette> = LazyLock::new(|| Palette {
    background:            gpui::rgb(0xFFFFFF).into(),
    surface:               gpui::rgb(0xF1F3F7).into(),
    surface_sunken:        gpui::rgb(0xE9ECF2).into(),
    surface_float:         gpui::rgb(0xFFFFFF).into(),
    modal_surface:         gpui::rgb(0xFFFFFF).into(),
    modal_rail:            gpui::rgb(0xF1F3F7).into(),
    border:                gpui::rgb(0xD8DCE4).into(),
    border_focused:        gpui::rgb(0xB4BAC6).into(),
    text:                  gpui::rgb(0x1C1F26).into(),
    text_muted:            gpui::rgb(0x474D58).into(),
    text_dim:              gpui::rgb(0x5F6572).into(),
    text_placeholder:      gpui::rgb(0x8A909C).into(),
    text_editor:           gpui::rgb(0x2A2F3A).into(),

    element:               gpui::rgb(0xE6E9EF).into(),
    // Hover and press stay close to the pane: they carry body text, and a
    // darker wash would drag the dim and accent roles under AA.
    element_hover:         gpui::rgb(0xE1E5EC).into(),
    element_active:        gpui::rgb(0xDEE2EA).into(),
    switch_off:            gpui::rgb(0xCBD1DC).into(),
    switch_thumb:          gpui::rgb(0xFFFFFF).into(),

    accent:                gpui::rgb(0x0B6C95).into(),
    accent_hover:          gpui::hsla(0.5495, 0.8625, 0.264, 1.), // #0B6C95 L-5%
    accent_active:         gpui::hsla(0.5495, 0.8625, 0.214, 1.), // #0B6C95 L-10%
    accent_ink:            gpui::rgb(0xFFFFFF).into(),
    accent_soft:           gpui::rgba(0x0B6C951C).into(),
    accent_soft_press:     gpui::rgba(0x0B6C952E).into(),
    // A tinted border needs more alpha to read on a light ground than on a dark one.
    accent_line:           gpui::rgba(0x0B6C9566).into(),

    status_off:            gpui::rgb(0xA3A9B5).into(),
    conflict:              gpui::rgb(0x9E4F0F).into(),

    danger:                gpui::rgb(0xC02A1B).into(),
    danger_hover:          gpui::hsla(0.0152, 0.7534, 0.369, 1.), // L-6%
    danger_active:         gpui::hsla(0.0152, 0.7534, 0.319, 1.), // L-11%
    danger_ink:            gpui::rgb(0xFFFFFF).into(),
    success:               gpui::rgb(0x1B7335).into(),
    success_hover:         gpui::hsla(0.3826, 0.6197, 0.228, 1.), // L-5%
    success_active:        gpui::hsla(0.3826, 0.6197, 0.178, 1.), // L-10%
    success_ink:           gpui::rgb(0xFFFFFF).into(),
    warning:               gpui::rgb(0x935600).into(),
    warning_hover:         gpui::hsla(0.0975, 1.000, 0.238, 1.), // L-5%
    warning_active:        gpui::hsla(0.0975, 1.000, 0.188, 1.), // L-10%
    warning_ink:           gpui::rgb(0xFFFFFF).into(),

    run_red:               gpui::rgb(0xBA3229).into(),
    green_vc:              gpui::rgb(0x457010).into(),
    blue_light:            gpui::rgb(0x1C68C4).into(),
    yellow:                gpui::rgb(0x7B6200).into(),
    yellow_light:          gpui::rgb(0x8F7100).into(),
    magenta:               gpui::rgb(0x8A2BC0).into(),
    magenta_light:         gpui::rgb(0xAB399B).into(),
    cyan:                  gpui::rgb(0x0C7162).into(),
    cyan_light:            gpui::rgb(0x0A7266).into(),

    selection:             gpui::hsla(0.550, 0.865, 0.620, 0.32),
    drop_target:           gpui::rgb(0xDCE7F2).into(),
    scrollbar_thumb:       gpui::rgba(0x8A909C66).into(),
    scrollbar_thumb_hover: gpui::rgba(0x8A909C99).into(),
    overlay:               gpui::rgba(0x1C1F2659).into(),
    sunken_soft:           gpui::rgba(0x1C1F260A).into(),
    sunken:                gpui::rgba(0x1C1F260F).into(),
    // Andromeda's yellow at 24% is invisible over white, so light uses amber.
    search_hit:            gpui::rgba(0xFFC53D66).into(),
    search_hit_active:     gpui::rgba(0xFFC53DB8).into(),

    term_bg:               gpui::rgb(0xFFFFFF).into(),
    term_fg:               gpui::rgb(0x2A2F3A).into(),
});

static IS_LIGHT: AtomicBool = AtomicBool::new(false);
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// A relaxed atomic load costs nothing on the render path, and unlike a
/// `thread_local!` it hands a background thread a complete palette rather than
/// a silently stale dark one.
fn active() -> &'static Palette {
    if IS_LIGHT.load(Ordering::Relaxed) {
        &LIGHT
    } else {
        &DARK
    }
}

pub fn is_light() -> bool {
    IS_LIGHT.load(Ordering::Relaxed)
}

/// Bumped on every real mode change. The terminal bakes colours into its grid
/// cache, so it keys on this to know when to throw the cache away.
pub fn generation() -> u32 {
    GENERATION.load(Ordering::Relaxed)
}

/// Selects the palette the accessors read. A no-op call must not bump the
/// generation, or every open terminal would relayout for nothing.
fn set_mode(mode: ThemeMode) {
    let want_light = !mode.is_dark();
    if IS_LIGHT.swap(want_light, Ordering::Relaxed) == want_light {
        return;
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

pub fn text() -> Hsla {
    active().text
}

/// Placeholder text in inputs. No ThemeColor field exists for this.
pub fn text_placeholder() -> Hsla {
    active().text_placeholder
}

/// Tertiary text: addresses, captions, column heads, icons.
pub fn text_dim() -> Hsla {
    active().text_dim
}

/// Secondary muted text: pill labels, section heads.
pub fn muted_gray() -> Hsla {
    active().text_muted
}

/// Code and read-only field text.
pub fn text_editor() -> Hsla {
    active().text_editor
}

/// Recessed strip: list/table headers, code blocks, key chips.
pub fn surface_sunken() -> Hsla {
    active().surface_sunken
}

/// Popover surface: tasks popover, terminal suggestions, search bar.
pub fn pop_surface() -> Hsla {
    active().surface_float
}

/// The settings modal's content pane.
pub fn modal_surface() -> Hsla {
    active().modal_surface
}

/// The settings modal's nav rail and inner list panes.
pub fn modal_rail() -> Hsla {
    active().modal_rail
}

/// Focused/variant border for popovers and context menus where the default
/// `border` is too faint to read.
pub fn border_focused() -> Hsla {
    active().border_focused
}

/// Status indicator "online" green. Also the colour of the check mark glyph.
pub fn status_online() -> Hsla {
    active().success
}

/// Status indicator "off"/idle gray.
pub fn status_off() -> Hsla {
    active().status_off
}

/// Accent base: the primary blue of progress bars and active runs.
pub fn accent() -> Hsla {
    active().accent
}

/// Accent soft tint: selected backgrounds.
pub fn accent_soft() -> Hsla {
    active().accent_soft
}

/// Accent pressed tint: pressed accent buttons.
pub fn accent_soft_press() -> Hsla {
    active().accent_soft_press
}

/// Accent line: accent borders.
pub fn accent_line() -> Hsla {
    active().accent_line
}

/// Tasks-popover error red: failed run glyph/bar.
pub fn run_red() -> Hsla {
    active().run_red
}

/// Interactive element fill: pills, idle chips.
pub fn element() -> Hsla {
    active().element
}

pub fn element_hover() -> Hsla {
    active().element_hover
}

/// Pressed fill, one step past hover.
pub fn element_active() -> Hsla {
    active().element_active
}

/// Sync-conflict badge color. No ThemeColor field for this.
pub fn conflict() -> Hsla {
    active().conflict
}

/// Warning orange, for free helpers that have no `cx` to reach the theme.
pub fn warning() -> Hsla {
    active().warning
}

/// Window-control "minimize" accent, and code-block variable chips.
pub fn yellow() -> Hsla {
    active().yellow
}

/// Window-control "close" accent, and the danger red.
pub fn red_error() -> Hsla {
    active().danger
}

/// Snippet glyph / "SCRIPT" pill teal.
pub fn cyan() -> Hsla {
    active().cyan
}

/// Terminal search hit. Yellow/amber, not the blue of `selection`, so a hit
/// inside a selection still stands out.
pub fn search_hit() -> Hsla {
    active().search_hit
}

/// The search hit the user stepped to.
pub fn search_hit_active() -> Hsla {
    active().search_hit_active
}

/// Faintest recessed wash: popover header strip.
pub fn sunken_soft() -> Hsla {
    active().sunken_soft
}

/// Recessed wash: inset detail panels, key chips.
pub fn sunken() -> Hsla {
    active().sunken
}

/// Terminal default background. Kept equal to `background`.
pub fn term_bg() -> Hsla {
    active().term_bg
}

pub fn term_fg() -> Hsla {
    active().term_fg
}

/// Applies a theme preference: forces (or releases) the native window
/// appearance, resolves what "System" means right now, and repaints.
pub fn apply_preference(pref: ThemePreference, window: Option<&mut Window>, cx: &mut App) {
    // Order matters: `window_appearance()` reports the forced value while an
    // override is active, so System has to be resolved after the override is
    // cleared or it resolves to the mode we are leaving.
    cx.set_window_appearance(forced_appearance(pref));
    let mode = resolve_mode(pref, cx.window_appearance());
    apply_mode(mode, window, cx);
}

/// `None` means "follow the OS". Forcing an appearance is what makes the native
/// window edge and traffic lights match a light app on a dark OS, but it also
/// stops the window tracking system changes, so System must not force one.
fn forced_appearance(pref: ThemePreference) -> Option<WindowAppearance> {
    match pref {
        ThemePreference::Dark => Some(WindowAppearance::Dark),
        ThemePreference::Light => Some(WindowAppearance::Light),
        ThemePreference::System => None,
    }
}

fn resolve_mode(pref: ThemePreference, system: WindowAppearance) -> ThemeMode {
    match pref {
        ThemePreference::Dark => ThemeMode::Dark,
        ThemePreference::Light => ThemeMode::Light,
        ThemePreference::System => ThemeMode::from(system),
    }
}

/// Switches to `mode` and repaints. `Theme::change` must run even though we
/// overwrite every colour right after: gpui-component branches on the mode
/// internally (`Theme::input_background`, for one).
pub fn apply_mode(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    set_mode(mode);
    Theme::change(mode, window, cx);
    apply_theme(cx);
}

/// Paints our palette over gpui-component's stock theme.
///
/// Must run after every `Theme::change`: `apply_config` rewrites every colour
/// and every token from the registry config.
#[rustfmt::skip]
fn apply_theme(cx: &mut App) {
    let p = active();
    let t = Theme::global_mut(cx);

    t.font_family = "Cascadia Code".into();
    // gpui-component sizes every stock control off the theme's font size: it
    // is the window's rem, so `h_6`, `px_2`, `text_sm` and friends all scale
    // with it. 16px puts a plain Input at 32px and `text_sm` at 14px, both a
    // size up from the rest of the chrome; 14px lands them on the 22/28px
    // ladder the hand-built rows use.
    t.font_size = px(14.);

    t.background       = p.background;
    t.foreground       = p.text;
    t.muted            = p.surface;
    t.muted_foreground = p.text_muted;

    t.border = p.border;
    t.input  = p.element;
    t.ring   = p.accent;

    t.primary            = p.accent;
    t.primary_hover      = p.accent_hover;
    t.primary_active     = p.accent_active;
    t.primary_foreground = p.accent_ink;

    // Set explicitly: do not rely on the fallback to `primary`.
    t.button_primary            = p.accent;
    t.button_primary_hover      = p.accent_hover;
    t.button_primary_active     = p.accent_active;
    t.button_primary_foreground = p.accent_ink;

    t.secondary            = p.surface;
    t.secondary_hover      = p.element_hover;
    t.secondary_active     = p.element_active;
    t.secondary_foreground = p.text;

    // gpui-component's "accent" is a surface, not our accent blue.
    t.accent            = p.surface;
    t.accent_foreground = p.text;

    t.popover            = p.surface_float;
    t.popover_foreground = p.text;

    t.tab_bar               = p.surface;
    t.tab_bar_segmented     = p.element;
    t.tab                   = p.surface;
    t.tab_active            = p.background; // active tab "down" onto the ground
    t.tab_active_foreground = p.text;
    t.tab_foreground        = p.text_muted;

    t.title_bar        = p.surface;
    t.title_bar_border = p.border;

    // `Theme.list` is `ListSettings`, so the colour only reaches the theme
    // through `t.colors.list`.
    t.colors.list        = gpui::rgba(0x00000000).into(); // transparent, inherits parent bg
    t.list_head          = p.surface_sunken;
    t.list_hover         = p.element_hover;
    t.list_active        = p.element_active;
    t.list_active_border = p.accent;
    t.list_even          = gpui::rgba(0x00000000).into(); // transparent, no stripes

    t.danger            = p.danger;
    t.danger_hover      = p.danger_hover;
    t.danger_active     = p.danger_active;
    t.danger_foreground = p.danger_ink;

    t.success            = p.success;
    t.success_hover      = p.success_hover;
    t.success_active     = p.success_active;
    t.success_foreground = p.success_ink;

    t.warning            = p.warning;
    t.warning_hover      = p.warning_hover;
    t.warning_active     = p.warning_active;
    t.warning_foreground = p.warning_ink;

    // Andromeda has no info colour, so info is the accent blue.
    t.info            = p.accent;
    t.info_hover      = p.accent_hover;
    t.info_active     = p.accent_active;
    t.info_foreground = p.accent_ink;

    t.selection   = p.selection;
    t.drop_target = p.drop_target;

    t.scrollbar             = gpui::rgba(0x00000000).into(); // transparent track
    t.scrollbar_thumb       = p.scrollbar_thumb;
    t.scrollbar_thumb_hover = p.scrollbar_thumb_hover;

    t.overlay = p.overlay;

    t.sidebar                    = p.surface;
    t.sidebar_accent             = p.element_hover;
    t.sidebar_accent_foreground  = p.text;
    t.sidebar_border             = p.border;
    t.sidebar_foreground         = p.text;
    t.sidebar_primary            = p.accent;
    t.sidebar_primary_foreground = p.accent_ink;

    t.accordion = p.surface;

    t.group_box            = p.surface;
    t.group_box_foreground = p.text;

    t.caret = p.accent;

    t.link        = p.accent;
    t.link_hover  = p.accent_hover;
    t.link_active = p.accent_active;

    t.progress_bar = p.accent;
    t.slider_bar   = p.element;
    t.slider_thumb = p.accent;
    t.switch       = p.switch_off;
    t.switch_thumb = p.switch_thumb;

    t.skeleton = p.element;

    t.drag_border = p.accent;

    t.description_list_label            = p.surface;
    t.description_list_label_foreground = p.text_muted;

    t.table                 = gpui::rgba(0x00000000).into(); // transparent
    t.table_active          = p.element_active;
    t.table_active_border   = p.accent;
    t.table_even            = p.surface; // subtle stripe
    t.table_head            = p.surface_sunken;
    t.table_head_foreground = p.text_muted;
    t.table_foot            = p.surface_sunken;
    t.table_foot_foreground = p.text_muted;
    t.table_hover           = p.element_hover;
    t.table_row_border      = p.border;

    // Linux only.
    t.window_border = p.border;

    t.chart_1       = p.accent;
    t.chart_2       = p.success;
    t.chart_3       = p.warning;
    t.chart_4       = p.magenta;
    t.chart_5       = p.cyan;
    t.chart_bullish = p.success;
    t.chart_bearish = p.danger;

    t.red           = p.danger;
    t.red_light     = p.run_red;
    t.green         = p.success;
    t.green_light   = p.green_vc;
    t.blue          = p.accent;
    t.blue_light    = p.blue_light;
    t.yellow        = p.yellow;
    t.yellow_light  = p.yellow_light;
    t.magenta       = p.magenta;
    t.magenta_light = p.magenta_light;
    t.cyan          = p.cyan;
    t.cyan_light    = p.cyan_light;

    // Tooltip, Checkbox, Switch, Kbd, Notification, GroupBox and Root all read
    // `tokens`, never `colors`. Without this they keep gpui-component's stock
    // values and stay dark in light mode.
    t.tokens = ThemeTokens::from(&t.colors);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.1 relative luminance.
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

    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// The palette selector is process-wide and `cargo test` runs test functions
    /// in parallel, so every assertion that flips it lives in this one test and
    /// it restores Dark before returning.
    #[test]
    fn palette_switches_and_restores() {
        assert_eq!(text_dim(), DARK.text_dim);
        assert_eq!(accent(), DARK.accent);

        let before = generation();
        set_mode(ThemeMode::Light);
        assert_eq!(text_dim(), LIGHT.text_dim);
        assert_eq!(accent(), LIGHT.accent);
        assert!(is_light());
        assert!(generation() > before);

        // Re-selecting the active mode must not invalidate the terminal caches.
        let steady = generation();
        set_mode(ThemeMode::Light);
        assert_eq!(generation(), steady);

        set_mode(ThemeMode::Dark);
        assert!(!is_light());
        assert_eq!(text_dim(), DARK.text_dim);
        assert_eq!(accent(), DARK.accent);
    }

    /// Every light role the app paints as text or as a legible glyph, against
    /// every ground it can land on. `element_hover` and `element_active` are in
    /// here because a hovered or selected row is a background like any other:
    /// `list_hover` and `list_active` are exactly those two.
    #[test]
    fn light_text_roles_meet_wcag_aa() {
        let p = &*LIGHT;
        let grounds = [
            ("background", p.background),
            ("surface", p.surface),
            ("surface_sunken", p.surface_sunken),
            ("element", p.element),
            ("element_hover", p.element_hover),
            ("element_active", p.element_active),
            ("drop_target", p.drop_target),
        ];
        let roles = [
            ("text", p.text),
            ("text_muted", p.text_muted),
            ("text_dim", p.text_dim),
            ("text_editor", p.text_editor),
            ("accent", p.accent),
            ("success", p.success),
            ("warning", p.warning),
            ("danger", p.danger),
            ("run_red", p.run_red),
            ("green_vc", p.green_vc),
            ("conflict", p.conflict),
            ("yellow", p.yellow),
            ("cyan", p.cyan),
        ];

        for (role_name, role) in roles {
            for (ground_name, ground) in grounds {
                let ratio = contrast(role, ground);
                assert!(
                    ratio >= 4.5,
                    "{role_name} on {ground_name} is {ratio:.2}:1, below AA"
                );
            }
        }
    }

    #[test]
    fn dark_text_roles_meet_wcag_aa() {
        let p = &*DARK;
        let grounds = [("background", p.background), ("surface", p.surface)];
        let roles = [
            ("text", p.text),
            ("text_muted", p.text_muted),
            ("text_dim", p.text_dim),
        ];

        for (role_name, role) in roles {
            for (ground_name, ground) in grounds {
                let ratio = contrast(role, ground);
                assert!(
                    ratio >= 4.5,
                    "{role_name} on {ground_name} is {ratio:.2}:1, below AA"
                );
            }
        }
    }

    /// Ink is painted on its own filled surface, so it is measured against that.
    #[test]
    fn light_ink_meets_wcag_aa() {
        let p = &*LIGHT;
        for (name, ink, fill) in [
            ("accent_ink", p.accent_ink, p.accent),
            ("danger_ink", p.danger_ink, p.danger),
            ("success_ink", p.success_ink, p.success),
            ("warning_ink", p.warning_ink, p.warning),
        ] {
            let ratio = contrast(ink, fill);
            assert!(ratio >= 4.5, "{name} is {ratio:.2}:1 on its fill, below AA");
        }
    }

    #[test]
    fn resolve_mode_covers_every_combination() {
        use WindowAppearance::*;
        for system in [Light, VibrantLight, Dark, VibrantDark] {
            assert_eq!(resolve_mode(ThemePreference::Dark, system), ThemeMode::Dark);
            assert_eq!(
                resolve_mode(ThemePreference::Light, system),
                ThemeMode::Light
            );
        }
        assert_eq!(
            resolve_mode(ThemePreference::System, WindowAppearance::Light),
            ThemeMode::Light
        );
        assert_eq!(
            resolve_mode(ThemePreference::System, WindowAppearance::VibrantLight),
            ThemeMode::Light
        );
        assert_eq!(
            resolve_mode(ThemePreference::System, WindowAppearance::Dark),
            ThemeMode::Dark
        );
        assert_eq!(
            resolve_mode(ThemePreference::System, WindowAppearance::VibrantDark),
            ThemeMode::Dark
        );
    }

    #[test]
    fn forced_appearance_only_releases_for_system() {
        assert_eq!(
            forced_appearance(ThemePreference::Dark),
            Some(WindowAppearance::Dark)
        );
        assert_eq!(
            forced_appearance(ThemePreference::Light),
            Some(WindowAppearance::Light)
        );
        assert_eq!(forced_appearance(ThemePreference::System), None);
    }
}
