//! Keys of a file pane: which key runs which command, type-to-select, and the
//! shortcut labels the context menu shows.

use std::time::{Duration, Instant};

use gpui::{actions, App, KeyBinding, Keystroke, Modifiers};

use crate::ui::keyboard_layout::us_layout_key;

actions!(file_pane, [SwitchPane]);

pub const KEY_CONTEXT: &str = "FilePane";

pub fn init(cx: &mut App) {
    // Tab is an action and not a key the pane reads: gpui-component binds Tab
    // at the window root to walk the tab stops, and only a binding in a deeper
    // context runs before it.
    cx.bind_keys([
        KeyBinding::new("tab", SwitchPane, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-tab", SwitchPane, Some(KEY_CONTEXT)),
    ]);
}

/// Where a key moves the lead row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowMove {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneCommand {
    /// `is_extending` (Shift held) keeps the rows passed over selected.
    Move {
        to: RowMove,
        is_extending: bool,
    },
    Open,
    GoUp,
    Rename,
    Delete,
    /// Upload from the local pane, download from the remote one.
    Transfer,
    Refresh,
    ToggleFilter,
    ToggleHidden,
    NewFolder,
    SelectAll,
    /// Escape: close whatever is open, then drop the selection.
    Cancel,
}

/// The command a key press runs, if any. "Secondary" is Cmd on macOS and Ctrl
/// elsewhere.
pub fn pane_command(key: &str, modifiers: &Modifiers) -> Option<PaneCommand> {
    if modifiers.alt {
        return None;
    }
    if modifiers.secondary() {
        // Cmd and Ctrl together is nobody's shortcut here.
        if modifiers.platform && modifiers.control {
            return None;
        }
        return secondary_command(key, modifiers.shift);
    }
    if modifiers.platform || modifiers.control {
        return None;
    }
    plain_command(key, modifiers.shift)
}

fn plain_command(key: &str, is_shift: bool) -> Option<PaneCommand> {
    let row_move = match key {
        "up" => Some(RowMove::Up),
        "down" => Some(RowMove::Down),
        "pageup" => Some(RowMove::PageUp),
        "pagedown" => Some(RowMove::PageDown),
        "home" => Some(RowMove::First),
        "end" => Some(RowMove::Last),
        _ => None,
    };
    if let Some(to) = row_move {
        return Some(PaneCommand::Move {
            to,
            is_extending: is_shift,
        });
    }
    if is_shift {
        return None;
    }
    match key {
        "enter" => Some(PaneCommand::Open),
        "backspace" => Some(PaneCommand::GoUp),
        "f2" => Some(PaneCommand::Rename),
        "delete" => Some(PaneCommand::Delete),
        "f5" => Some(PaneCommand::Transfer),
        "escape" => Some(PaneCommand::Cancel),
        _ => None,
    }
}

fn secondary_command(key: &str, is_shift: bool) -> Option<PaneCommand> {
    match (us_key(key).as_str(), is_shift) {
        // F5 needs Fn on a laptop keyboard and Delete is missing from most of
        // them, so both have a chord as well.
        ("enter", false) => Some(PaneCommand::Transfer),
        ("backspace", false) => Some(PaneCommand::Delete),
        ("r", false) => Some(PaneCommand::Refresh),
        ("f", false) => Some(PaneCommand::ToggleFilter),
        ("a", false) => Some(PaneCommand::SelectAll),
        ("n", true) => Some(PaneCommand::NewFolder),
        // Shift turns the key into ">" on some platforms and leaves it on others.
        ("." | ">", true) => Some(PaneCommand::ToggleHidden),
        _ => None,
    }
}

/// The key as a US layout names it, lower case. gpui names a key by what the
/// active layout prints on it, so R arrives as "к" on the Russian layout.
fn us_key(key: &str) -> String {
    match us_layout_key(key) {
        Some(us) => us.to_string(),
        None => key.to_ascii_lowercase(),
    }
}

/// Shortcut labels for the context menu. They must track `pane_command`.
#[cfg(target_os = "macos")]
pub mod hint {
    pub const OPEN: &str = "\u{21B5}";
    pub const RENAME: &str = "F2";
    pub const DELETE: &str = "\u{2318}\u{232B}";
    pub const TRANSFER: &str = "\u{2318}\u{21B5}";
    pub const REFRESH: &str = "\u{2318}R";
    pub const NEW_FOLDER: &str = "\u{21E7}\u{2318}N";
}

#[cfg(not(target_os = "macos"))]
pub mod hint {
    pub const OPEN: &str = "Enter";
    pub const RENAME: &str = "F2";
    pub const DELETE: &str = "Del";
    pub const TRANSFER: &str = "F5";
    pub const REFRESH: &str = "Ctrl+R";
    pub const NEW_FOLDER: &str = "Ctrl+Shift+N";
}

/// How long the typed prefix is kept between two key presses.
const TYPE_AHEAD_IDLE: Duration = Duration::from_millis(800);

/// The letters typed over the list to jump to a file by name.
#[derive(Default)]
pub struct TypeAhead {
    typed: String,
    last_key_at: Option<Instant>,
}

impl TypeAhead {
    /// Add `text` and return everything typed so far. A pause starts a new
    /// prefix. A space cannot start one, so it stays free for other uses.
    pub fn push(&mut self, text: &str, now: Instant) -> Option<&str> {
        let is_stale = self
            .last_key_at
            .is_none_or(|at| now.duration_since(at) > TYPE_AHEAD_IDLE);
        if is_stale {
            self.typed.clear();
        }
        if self.typed.is_empty() && text.trim().is_empty() {
            return None;
        }
        self.typed.push_str(text);
        self.last_key_at = Some(now);
        Some(&self.typed)
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The text a key press types, when it is a plain character and not a command.
/// `prefers_character_input` is set for AltGr, which arrives as Ctrl+Alt on
/// Windows and still types a character.
pub fn typed_text(keystroke: &Keystroke, prefers_character_input: bool) -> Option<&str> {
    let modifiers = &keystroke.modifiers;
    let has_command_modifier = modifiers.platform || modifiers.control || modifiers.alt;
    if has_command_modifier && !prefers_character_input {
        return None;
    }
    let text = keystroke.key_char.as_deref()?;
    if text.is_empty() || text.chars().any(char::is_control) {
        return None;
    }
    Some(text)
}

/// Position of the first name that starts with `typed`, ignoring case.
pub fn type_ahead_match(names: &[&str], typed: &str) -> Option<usize> {
    let typed = typed.to_lowercase();
    names
        .iter()
        .position(|name| name.to_lowercase().starts_with(&typed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Modifiers {
        Modifiers::default()
    }

    fn shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..Modifiers::default()
        }
    }

    /// Cmd on macOS, Ctrl elsewhere.
    fn secondary() -> Modifiers {
        Modifiers::secondary_key()
    }

    fn secondary_shift() -> Modifiers {
        Modifiers {
            shift: true,
            ..secondary()
        }
    }

    #[test]
    fn plain_keys_run_list_commands() {
        assert_eq!(pane_command("enter", &plain()), Some(PaneCommand::Open));
        assert_eq!(pane_command("backspace", &plain()), Some(PaneCommand::GoUp));
        assert_eq!(pane_command("f2", &plain()), Some(PaneCommand::Rename));
        assert_eq!(pane_command("delete", &plain()), Some(PaneCommand::Delete));
        assert_eq!(pane_command("f5", &plain()), Some(PaneCommand::Transfer));
        assert_eq!(pane_command("escape", &plain()), Some(PaneCommand::Cancel));
        // Letters are left for type-to-select.
        assert_eq!(pane_command("r", &plain()), None);
        assert_eq!(pane_command("n", &shift()), None);
    }

    #[test]
    fn movement_keys_extend_with_shift() {
        assert_eq!(
            pane_command("down", &plain()),
            Some(PaneCommand::Move {
                to: RowMove::Down,
                is_extending: false
            })
        );
        assert_eq!(
            pane_command("end", &shift()),
            Some(PaneCommand::Move {
                to: RowMove::Last,
                is_extending: true
            })
        );
        assert_eq!(
            pane_command("pageup", &shift()),
            Some(PaneCommand::Move {
                to: RowMove::PageUp,
                is_extending: true
            })
        );
        assert_eq!(
            pane_command("home", &plain()),
            Some(PaneCommand::Move {
                to: RowMove::First,
                is_extending: false
            })
        );
    }

    #[test]
    fn secondary_chords_run_pane_commands() {
        assert_eq!(
            pane_command("enter", &secondary()),
            Some(PaneCommand::Transfer)
        );
        assert_eq!(
            pane_command("backspace", &secondary()),
            Some(PaneCommand::Delete)
        );
        assert_eq!(pane_command("r", &secondary()), Some(PaneCommand::Refresh));
        assert_eq!(
            pane_command("f", &secondary()),
            Some(PaneCommand::ToggleFilter)
        );
        assert_eq!(
            pane_command("a", &secondary()),
            Some(PaneCommand::SelectAll)
        );
        assert_eq!(
            pane_command("n", &secondary_shift()),
            Some(PaneCommand::NewFolder)
        );
        assert_eq!(
            pane_command(".", &secondary_shift()),
            Some(PaneCommand::ToggleHidden)
        );
        assert_eq!(
            pane_command(">", &secondary_shift()),
            Some(PaneCommand::ToggleHidden)
        );
        // Shift makes it a different chord.
        assert_eq!(pane_command("r", &secondary_shift()), None);
        assert_eq!(pane_command("n", &secondary()), None);
        // Arrows with Cmd/Ctrl are not list keys.
        assert_eq!(pane_command("down", &secondary()), None);
    }

    #[test]
    fn chords_work_on_the_russian_layout_and_in_upper_case() {
        assert_eq!(pane_command("к", &secondary()), Some(PaneCommand::Refresh));
        assert_eq!(
            pane_command("а", &secondary()),
            Some(PaneCommand::ToggleFilter)
        );
        assert_eq!(
            pane_command("ф", &secondary()),
            Some(PaneCommand::SelectAll)
        );
        assert_eq!(
            pane_command("Т", &secondary_shift()),
            Some(PaneCommand::NewFolder)
        );
        assert_eq!(
            pane_command("Ю", &secondary_shift()),
            Some(PaneCommand::ToggleHidden)
        );
        // gpui reports the key upper case on Windows.
        assert_eq!(pane_command("R", &secondary()), Some(PaneCommand::Refresh));
    }

    #[test]
    fn other_modifiers_run_nothing() {
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        let both = Modifiers {
            platform: true,
            control: true,
            ..Modifiers::default()
        };
        // The one of Cmd and Ctrl that is not "secondary" on this platform.
        let other = Modifiers {
            platform: !secondary().platform,
            control: !secondary().control,
            ..Modifiers::default()
        };
        for modifiers in [alt, both, other] {
            assert_eq!(pane_command("enter", &modifiers), None);
            assert_eq!(pane_command("r", &modifiers), None);
            assert_eq!(pane_command("down", &modifiers), None);
        }
    }

    fn keystroke(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.to_string(),
            key_char: key_char.map(str::to_string),
        }
    }

    #[test]
    fn only_plain_characters_count_as_typed_text() {
        assert_eq!(
            typed_text(&keystroke("n", Some("n"), plain()), false),
            Some("n")
        );
        assert_eq!(
            typed_text(&keystroke("n", Some("N"), shift()), false),
            Some("N")
        );
        assert_eq!(
            typed_text(&keystroke("т", Some("т"), plain()), false),
            Some("т")
        );
        assert_eq!(
            typed_text(&keystroke("r", Some("r"), secondary()), false),
            None
        );
        assert_eq!(typed_text(&keystroke("f2", None, plain()), false), None);
        assert_eq!(
            typed_text(&keystroke("enter", Some("\n"), plain()), false),
            None
        );
        // AltGr on Windows: Ctrl+Alt is held, and the key still types.
        let alt_gr = Modifiers {
            control: true,
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            typed_text(&keystroke("q", Some("@"), alt_gr), true),
            Some("@")
        );
        assert_eq!(typed_text(&keystroke("q", Some("@"), alt_gr), false), None);
    }

    #[test]
    fn typed_prefix_grows_until_a_pause() {
        let start = Instant::now();
        let mut type_ahead = TypeAhead::default();
        assert_eq!(type_ahead.push("n", start), Some("n"));
        assert_eq!(
            type_ahead.push("g", start + Duration::from_millis(300)),
            Some("ng")
        );
        // Measured from the last key, not from the first one.
        assert_eq!(
            type_ahead.push("i", start + Duration::from_millis(1000)),
            Some("ngi")
        );
        assert_eq!(
            type_ahead.push("a", start + Duration::from_millis(2000)),
            Some("a")
        );
        type_ahead.clear();
        assert_eq!(
            type_ahead.push("b", start + Duration::from_millis(2100)),
            Some("b")
        );
    }

    #[test]
    fn space_continues_a_prefix_but_does_not_start_one() {
        let start = Instant::now();
        let mut type_ahead = TypeAhead::default();
        assert_eq!(type_ahead.push(" ", start), None);
        assert_eq!(type_ahead.push("my", start), Some("my"));
        assert_eq!(type_ahead.push(" ", start), Some("my "));
        assert_eq!(type_ahead.push(" ", start + Duration::from_secs(5)), None);
    }

    #[test]
    fn type_ahead_finds_the_first_name_with_the_prefix() {
        let names = ["Nginx", "etc", "nginx.conf", "notes.txt"];
        assert_eq!(type_ahead_match(&names, "n"), Some(0));
        assert_eq!(type_ahead_match(&names, "ng"), Some(0));
        assert_eq!(type_ahead_match(&names, "nginx."), Some(2));
        assert_eq!(type_ahead_match(&names, "NO"), Some(3));
        assert_eq!(type_ahead_match(&names, "conf"), None);
        assert_eq!(type_ahead_match(&[], "n"), None);
    }
}
