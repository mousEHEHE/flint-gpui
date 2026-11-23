//! Holding the shortcut modifier (Cmd on macOS, Ctrl elsewhere) for a moment
//! shows each key next to what it does: the tab number on an open host, the
//! comma on Settings, and so on.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{div, App, Global, Modifiers, SharedString, Window};

use crate::theme;
use crate::ui::rems_from_px;

/// How long the modifier must be held before the hints show. A quick chord
/// like Cmd+C is over by then, so the hints do not flash on every shortcut.
pub const SHOW_DELAY: Duration = Duration::from_millis(350);

/// The badge of a rebindable command, see `keymap::hint`.
pub fn key(id: &str, cx: &App) -> Option<SharedString> {
    crate::ui::keymap::hint(id, cx)
}

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

const TAB_IDS: [&str; 9] = [
    "tabs.tab_1",
    "tabs.tab_2",
    "tabs.tab_3",
    "tabs.tab_4",
    "tabs.tab_5",
    "tabs.tab_6",
    "tabs.tab_7",
    "tabs.tab_8",
    "tabs.tab_9",
];

/// The badge of the tab at `index`; only the first nine have a shortcut.
pub fn tab_key(index: usize, cx: &App) -> Option<SharedString> {
    key(TAB_IDS.get(index)?, cx)
}

/// The key cap drawn next to a control while the hints are shown.
pub fn badge(keys: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .h(rems_from_px(18.))
        .min_w(rems_from_px(18.))
        .px(rems_from_px(5.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(rems_from_px(5.))
        .bg(theme::accent())
        .text_size(rems_from_px(11.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme::accent_ink())
        .child(keys.into())
}

/// A badge pinned to the top-right corner of a control that is `relative`,
/// so showing it does not move anything.
pub fn corner_badge(keys: impl Into<SharedString>) -> gpui::Div {
    badge(keys)
        .absolute()
        .top(rems_from_px(-7.))
        .right(rems_from_px(-7.))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tab_key_is_a_command() {
        for id in TAB_IDS {
            assert!(crate::ui::keymap::command(id).is_some(), "{id}");
        }
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
