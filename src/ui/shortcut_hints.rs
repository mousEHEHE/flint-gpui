//! Holding the shortcut modifier (Cmd on macOS, Ctrl elsewhere) for a moment
//! shows each key next to what it does: the tab number on an open host, the
//! comma on Settings, and so on.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{div, px, App, Global, Modifiers, SharedString, Window};

use crate::theme;

/// How long the modifier must be held before the hints show. A quick chord
/// like Cmd+C is over by then, so the hints do not flash on every shortcut.
pub const SHOW_DELAY: Duration = Duration::from_millis(350);

/// The keys shown next to each control. They must track the bindings in
/// `app_view::init` and `terminal_tabs`: off macOS the letter chords take
/// Shift as well, so that plain Ctrl+letter stays with the shell.
pub const SETTINGS: &str = ",";
pub const NEW_HOST: &str = if cfg!(target_os = "macos") {
    "T"
} else {
    "\u{21E7}T"
};
pub const FILES: &str = if cfg!(target_os = "macos") {
    "B"
} else {
    "\u{21E7}B"
};
pub const NEW_TERMINAL: &str = if cfg!(target_os = "macos") {
    "\u{21E7}T"
} else {
    "\u{21E7}N"
};

/// Whether the hints are on screen right now.
#[derive(Default)]
struct ShortcutHints {
    is_shown: bool,
}

impl Global for ShortcutHints {}

pub fn is_shown(cx: &App) -> bool {
    cx.try_global::<ShortcutHints>()
        .is_some_and(|hints| hints.is_shown)
}

/// Show or hide the hints in every view of the window.
pub fn set_shown(is_shown: bool, window: &mut Window, cx: &mut App) {
    if self::is_shown(cx) == is_shown {
        return;
    }
    cx.set_global(ShortcutHints { is_shown });
    window.refresh();
}

/// The shortcut modifier is down, without Alt or the other platform key that
/// would make a different chord. Shift may be down: some shortcuts use it.
pub fn is_hint_modifier(modifiers: &Modifiers) -> bool {
    let other = if cfg!(target_os = "macos") {
        modifiers.control
    } else {
        modifiers.platform
    };
    modifiers.secondary() && !modifiers.alt && !other
}

/// The tab number shown for the tab at `index`; only the first nine have a
/// shortcut.
pub fn tab_key(index: usize) -> Option<SharedString> {
    (index < 9).then(|| SharedString::from((index + 1).to_string()))
}

/// The key cap drawn next to a control while the hints are shown.
pub fn badge(keys: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .h(px(18.))
        .min_w(px(18.))
        .px(px(5.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .bg(theme::accent())
        .text_size(px(11.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme::accent_ink())
        .child(keys.into())
}

/// A badge pinned to the top-right corner of a control that is `relative`,
/// so showing it does not move anything.
pub fn corner_badge(keys: impl Into<SharedString>) -> gpui::Div {
    badge(keys).absolute().top(px(-7.)).right(px(-7.))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_nine_tabs_have_a_key() {
        assert_eq!(tab_key(0).as_deref(), Some("1"));
        assert_eq!(tab_key(8).as_deref(), Some("9"));
        assert_eq!(tab_key(9), None);
    }

    #[test]
    fn the_hints_follow_the_shortcut_modifier_alone_or_with_shift() {
        let held = Modifiers::secondary_key();
        assert!(is_hint_modifier(&held));
        assert!(is_hint_modifier(&Modifiers {
            shift: true,
            ..held
        }));
        assert!(!is_hint_modifier(&Modifiers { alt: true, ..held }));
        assert!(!is_hint_modifier(&Modifiers::default()));
    }
}
