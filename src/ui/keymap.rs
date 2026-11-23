//! The shortcuts a user can rebind: one table of commands with their default
//! chords, the overrides saved in preferences, and the bindings made from both.
//!
//! Every label and hint badge reads the chord from here, so a rebind shows up
//! everywhere at once.

use std::collections::BTreeMap;

use gpui::{
    Action, App, KeyBinding, KeyBindingMetaIndex, KeybindingKeystroke, Keystroke, Modifiers,
    SharedString,
};

use crate::preferences::Preferences;
use crate::ui::app_view::{
    ActivateTab1, ActivateTab2, ActivateTab3, ActivateTab4, ActivateTab5, ActivateTab6,
    ActivateTab7, ActivateTab8, ActivateTab9, CloseTab, DecreaseUiScale, IncreaseUiScale,
    LockVault, NewConnection, NewTerminal, NextTab, OpenLocalTerminal, PrevTab, Quit, ResetUiScale,
    SplitLocalTerminal, SplitTerminal, ToggleCommandPalette, ToggleFilesFocus, ToggleServerPanel,
    ToggleSettings, ToggleSftpPane, ToggleSidebar,
};
use crate::ui::file_pane_keys::{PaneCommand, KEY_CONTEXT as FILE_PANE};
use crate::ui::keyboard_layout::{chord_variants, unshifted_us_key, us_layout_key};
use crate::ui::terminal_search::ToggleSearch;
use crate::ui::terminal_view::{
    ClearScreen, CopySelection, DecreaseFontSize, IncreaseFontSize, Paste, ResetFontSize, SelectAll,
};

/// Command id to chord. `None` means the user removed the shortcut; a command
/// that is missing keeps its default.
pub type Overrides = BTreeMap<String, Option<String>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    General,
    Tabs,
    Terminal,
    Files,
}

impl Group {
    pub const ALL: [Group; 4] = [Group::General, Group::Tabs, Group::Terminal, Group::Files];

    pub fn label(self) -> &'static str {
        match self {
            Group::General => "General",
            Group::Tabs => "Tabs & panes",
            Group::Terminal => "Terminal",
            Group::Files => "Files",
        }
    }
}

/// How a command runs: a gpui action bound in the keymap, or a file pane
/// command the pane reads from its own key handler.
#[derive(Clone, Copy)]
enum Run {
    Action(fn(&str, Option<&str>) -> KeyBinding),
    Pane(PaneCommand),
}

pub struct Command {
    pub id: &'static str,
    pub label: &'static str,
    pub group: Group,
    mac: &'static str,
    other: &'static str,
    context: Option<&'static str>,
    run: Run,
}

impl Command {
    /// The first default chord, the one labels show.
    pub fn default_chord(&self, is_mac: bool) -> Option<&'static str> {
        self.default_chords(is_mac).next()
    }

    /// A default may list a second chord after a space; both run the command
    /// until the user sets their own.
    fn default_chords(&self, is_mac: bool) -> impl Iterator<Item = &'static str> {
        let chords = if is_mac { self.mac } else { self.other };
        chords.split_whitespace()
    }
}

fn action<A: Action + Default>(chord: &str, context: Option<&str>) -> KeyBinding {
    KeyBinding::new(chord, A::default(), context)
}

const TERMINAL: Option<&str> = Some("Terminal");

const fn global(
    id: &'static str,
    label: &'static str,
    group: Group,
    mac: &'static str,
    other: &'static str,
    bind: fn(&str, Option<&str>) -> KeyBinding,
) -> Command {
    Command {
        id,
        label,
        group,
        mac,
        other,
        context: None,
        run: Run::Action(bind),
    }
}

const fn terminal(
    id: &'static str,
    label: &'static str,
    mac: &'static str,
    other: &'static str,
    bind: fn(&str, Option<&str>) -> KeyBinding,
) -> Command {
    Command {
        id,
        label,
        group: Group::Terminal,
        mac,
        other,
        context: TERMINAL,
        run: Run::Action(bind),
    }
}

const fn files(
    id: &'static str,
    label: &'static str,
    mac: &'static str,
    other: &'static str,
    command: PaneCommand,
) -> Command {
    Command {
        id,
        label,
        group: Group::Files,
        mac,
        other,
        context: Some(FILE_PANE),
        run: Run::Pane(command),
    }
}

use Group::{General, Tabs};

// Off macOS, plain Ctrl+letter belongs to readline at a shell prompt, so the
// app chords take Ctrl+Shift, as in Windows Terminal. Tab cycling uses
// Ctrl+Tab there: bracket keys are shifted on most layouts. Shift+symbol is
// spelled as the symbol it types ("cmd-}"), the way gpui reports the press.
#[rustfmt::skip]
pub const COMMANDS: &[Command] = &[
    global("app.quit", "Quit", General, "cmd-q", "", action::<Quit>),
    global("app.settings", "Settings", General, "cmd-,", "ctrl-,", action::<ToggleSettings>),
    global("app.command_palette", "Command palette", General, "cmd-p", "ctrl-shift-p", action::<ToggleCommandPalette>),
    global("app.lock", "Lock vault", General, "cmd-l", "ctrl-shift-l", action::<LockVault>),
    global("app.new_host", "New host", General, "cmd-t", "ctrl-shift-t", action::<NewConnection>),
    global("app.toggle_sidebar", "Show or hide sidebar", General, "ctrl-cmd-s", "ctrl-shift-s", action::<ToggleSidebar>),
    global("app.zoom_in", "Zoom in", General, "cmd-alt-=", "ctrl-alt-=", action::<IncreaseUiScale>),
    global("app.zoom_out", "Zoom out", General, "cmd-alt--", "ctrl-alt--", action::<DecreaseUiScale>),
    global("app.zoom_reset", "Reset zoom", General, "cmd-alt-0", "ctrl-alt-0", action::<ResetUiScale>),
    global("tabs.new_terminal", "New terminal", Tabs, "cmd-shift-t", "ctrl-shift-n", action::<NewTerminal>),
    global("tabs.local_terminal", "Local terminal", Tabs, "cmd-alt-t", "ctrl-alt-shift-t", action::<OpenLocalTerminal>),
    global("tabs.split", "Split or join terminals", Tabs, "cmd-d", "ctrl-shift-d", action::<SplitTerminal>),
    global("tabs.local_beside", "Local terminal beside", Tabs, "cmd-shift-d", "ctrl-alt-shift-d", action::<SplitLocalTerminal>),
    global("tabs.toggle_files", "Show or hide files", Tabs, "cmd-b", "ctrl-shift-b", action::<ToggleSftpPane>),
    global("tabs.server_panel", "Show or hide server panel", Tabs, "cmd-i", "ctrl-shift-i", action::<ToggleServerPanel>),
    global("tabs.focus_files", "Focus files", Tabs, "cmd-shift-e", "ctrl-shift-e", action::<ToggleFilesFocus>),
    global("tabs.close", "Close tab", Tabs, "cmd-w", "ctrl-shift-w", action::<CloseTab>),
    global("tabs.next", "Next tab", Tabs, "cmd-}", "ctrl-tab", action::<NextTab>),
    global("tabs.prev", "Previous tab", Tabs, "cmd-{", "ctrl-shift-tab", action::<PrevTab>),
    global("tabs.tab_1", "Go to tab 1", Tabs, "cmd-1", "ctrl-1", action::<ActivateTab1>),
    global("tabs.tab_2", "Go to tab 2", Tabs, "cmd-2", "ctrl-2", action::<ActivateTab2>),
    global("tabs.tab_3", "Go to tab 3", Tabs, "cmd-3", "ctrl-3", action::<ActivateTab3>),
    global("tabs.tab_4", "Go to tab 4", Tabs, "cmd-4", "ctrl-4", action::<ActivateTab4>),
    global("tabs.tab_5", "Go to tab 5", Tabs, "cmd-5", "ctrl-5", action::<ActivateTab5>),
    global("tabs.tab_6", "Go to tab 6", Tabs, "cmd-6", "ctrl-6", action::<ActivateTab6>),
    global("tabs.tab_7", "Go to tab 7", Tabs, "cmd-7", "ctrl-7", action::<ActivateTab7>),
    global("tabs.tab_8", "Go to tab 8", Tabs, "cmd-8", "ctrl-8", action::<ActivateTab8>),
    global("tabs.tab_9", "Go to tab 9", Tabs, "cmd-9", "ctrl-9", action::<ActivateTab9>),
    terminal("terminal.copy", "Copy", "cmd-c", "ctrl-shift-c", action::<CopySelection>),
    terminal("terminal.paste", "Paste", "cmd-v", "ctrl-shift-v", action::<Paste>),
    terminal("terminal.select_all", "Select all", "cmd-a", "ctrl-shift-a", action::<SelectAll>),
    terminal("terminal.clear", "Clear screen", "cmd-k", "ctrl-shift-k", action::<ClearScreen>),
    terminal("terminal.find", "Find", "cmd-f", "ctrl-shift-f", action::<ToggleSearch>),
    terminal("terminal.font_bigger", "Bigger text", "cmd-=", "ctrl-=", action::<IncreaseFontSize>),
    terminal("terminal.font_smaller", "Smaller text", "cmd--", "ctrl--", action::<DecreaseFontSize>),
    terminal("terminal.font_reset", "Default text size", "cmd-0", "ctrl-0", action::<ResetFontSize>),
    files("files.rename", "Rename", "f2", "f2", PaneCommand::Rename),
    // F5 needs Fn on laptops and most laptops have no Delete key, so these
    // keep a second chord.
    files("files.delete", "Delete", "cmd-backspace delete", "delete ctrl-backspace", PaneCommand::Delete),
    files("files.transfer", "Upload or download", "cmd-enter f5", "f5 ctrl-enter", PaneCommand::Transfer),
    files("files.refresh", "Refresh", "cmd-r", "ctrl-r", PaneCommand::Refresh),
    files("files.filter", "Filter", "cmd-f", "ctrl-f", PaneCommand::ToggleFilter),
    files("files.select_all", "Select all", "cmd-a", "ctrl-a", PaneCommand::SelectAll),
    files("files.new_folder", "New folder", "cmd-shift-n", "f7", PaneCommand::NewFolder),
    files("files.hidden", "Show or hide hidden files", "cmd->", "ctrl->", PaneCommand::ToggleHidden),
];

/// Marks the bindings made from this table, so `apply` can take them out
/// again. 0 keeps gpui's precedence rules the same as for unmarked bindings.
const REBINDABLE: KeyBindingMetaIndex = KeyBindingMetaIndex(0);

const IS_MAC: bool = cfg!(target_os = "macos");

static NO_OVERRIDES: Overrides = BTreeMap::new();

pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id == id)
}

fn overrides(cx: &App) -> &Overrides {
    cx.try_global::<Preferences>()
        .map_or(&NO_OVERRIDES, |prefs| &prefs.shortcuts)
}

/// A chord gpui can bind: one keystroke that parses.
pub fn is_valid_chord(chord: &str) -> bool {
    !chord.is_empty() && !chord.contains(char::is_whitespace) && Keystroke::parse(chord).is_ok()
}

pub fn effective_chord<'a>(
    command: &'a Command,
    overrides: &'a Overrides,
    is_mac: bool,
) -> Option<&'a str> {
    effective_chords(command, overrides, is_mac)
        .into_iter()
        .next()
}

/// Every chord that runs the command: the user's one, or all the defaults.
fn effective_chords<'a>(
    command: &'a Command,
    overrides: &'a Overrides,
    is_mac: bool,
) -> Vec<&'a str> {
    let chords: Vec<&str> = match overrides.get(command.id) {
        Some(custom) => custom.as_deref().into_iter().collect(),
        None => command.default_chords(is_mac).collect(),
    };
    chords
        .into_iter()
        .filter(|chord| is_valid_chord(chord))
        .collect()
}

fn effective_keystroke(id: &str, cx: &App) -> Option<Keystroke> {
    let chord = effective_chord(command(id)?, overrides(cx), IS_MAC)?;
    Keystroke::parse(chord).ok()
}

/// Bind every command to its chord. Our old bindings are taken out first and
/// all others, gpui-component's included, are put back in the same order.
pub fn apply(cx: &mut App) {
    let kept: Vec<KeyBinding> = cx
        .key_bindings()
        .borrow()
        .bindings()
        .filter(|binding| binding.meta() != Some(REBINDABLE))
        .cloned()
        .collect();
    let ours = bindings(overrides(cx), IS_MAC);
    cx.clear_key_bindings();
    cx.bind_keys(kept);
    cx.bind_keys(ours);
}

fn bindings(overrides: &Overrides, is_mac: bool) -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    for command in COMMANDS {
        let Run::Action(bind) = command.run else {
            continue;
        };
        // Also the spellings of the same keys on the Russian layout.
        for chord in effective_chords(command, overrides, is_mac) {
            for variant in chord_variants(chord) {
                bindings.push(bind(&variant, command.context).with_meta(REBINDABLE));
            }
        }
    }
    bindings
}

/// Change one command's chord and bind it at once. A chord equal to the
/// default is stored as no override.
pub fn set_chord(id: &str, chord: Option<String>, cx: &mut App) {
    let Some(command) = command(id) else {
        return;
    };
    Preferences::update(cx, |prefs| {
        set_override(&mut prefs.shortcuts, command, chord, IS_MAC)
    });
    apply(cx);
}

/// `set_chord`, after taking the chord away from `others`.
pub fn replace_chord(id: &str, others: &[&Command], chord: String, cx: &mut App) {
    let Some(command) = command(id) else {
        return;
    };
    Preferences::update(cx, |prefs| {
        for other in others {
            take_chord(&mut prefs.shortcuts, other, &chord, IS_MAC);
        }
        set_override(&mut prefs.shortcuts, command, Some(chord), IS_MAC);
    });
    apply(cx);
}

pub fn reset(id: Option<&str>, cx: &mut App) {
    Preferences::update(cx, |prefs| match id {
        Some(id) => {
            prefs.shortcuts.remove(id);
        }
        None => prefs.shortcuts.clear(),
    });
    apply(cx);
}

pub fn is_customized(id: &str, cx: &App) -> bool {
    overrides(cx).contains_key(id)
}

pub fn has_customized(cx: &App) -> bool {
    !overrides(cx).is_empty()
}

/// Take `chord` away from `command`, keeping its other default chord if it has one.
fn take_chord(overrides: &mut Overrides, command: &Command, chord: &str, is_mac: bool) {
    let Ok(taken) = Keystroke::parse(chord) else {
        return;
    };
    let kept = effective_chords(command, overrides, is_mac)
        .into_iter()
        .find(|theirs| Keystroke::parse(theirs).is_ok_and(|k| !same_keys(&k, &taken)))
        .map(String::from);
    overrides.insert(command.id.to_string(), kept);
}

fn set_override(overrides: &mut Overrides, command: &Command, chord: Option<String>, is_mac: bool) {
    if chord.as_deref() == command.default_chord(is_mac) {
        overrides.remove(command.id);
    } else {
        overrides.insert(command.id.to_string(), chord);
    }
}

/// The chord as the platform writes it: "⇧⌘D" on macOS, "Ctrl+Shift+D" elsewhere.
pub fn label(id: &str, cx: &App) -> Option<SharedString> {
    effective_keystroke(id, cx).map(|keystroke| format_chord(&keystroke, IS_MAC).into())
}

/// "Close tab (⌘W)", or just the text when the command has no shortcut.
pub fn with_label(text: &str, id: &str, cx: &App) -> String {
    match label(id, cx) {
        Some(label) => format!("{text} ({label})"),
        None => text.to_string(),
    }
}

/// The badge shown while the hint modifier is held. That modifier is implied
/// and left out; a chord without it, or with Alt, gets no badge.
pub fn hint(id: &str, cx: &App) -> Option<SharedString> {
    hint_label(&effective_keystroke(id, cx)?, IS_MAC).map(SharedString::from)
}

pub fn format_chord(keystroke: &Keystroke, is_mac: bool) -> String {
    let keystroke = &as_pressed(keystroke);
    let modifiers = &keystroke.modifiers;
    let key = key_label(&keystroke.key, is_mac);
    if is_mac {
        let mut label: String = [
            (modifiers.control, '\u{2303}'),
            (modifiers.alt, '\u{2325}'),
            (modifiers.shift, '\u{21E7}'),
            (modifiers.platform, '\u{2318}'),
        ]
        .into_iter()
        .filter_map(|(is_held, symbol)| is_held.then_some(symbol))
        .collect();
        label.push_str(&key);
        return label;
    }
    let mut parts: Vec<&str> = [
        (modifiers.control, "Ctrl"),
        (modifiers.alt, "Alt"),
        (modifiers.shift, "Shift"),
        (modifiers.platform, "Win"),
    ]
    .into_iter()
    .filter_map(|(is_held, name)| is_held.then_some(name))
    .collect();
    parts.push(&key);
    parts.join("+")
}

fn hint_label(keystroke: &Keystroke, is_mac: bool) -> Option<String> {
    let keystroke = &as_pressed(keystroke);
    let modifiers = &keystroke.modifiers;
    let (is_held, other) = if is_mac {
        (modifiers.platform, false)
    } else {
        (modifiers.control, modifiers.platform)
    };
    if !is_held || other || modifiers.alt {
        return None;
    }
    let mut label = String::new();
    if is_mac && modifiers.control {
        label.push('\u{2303}');
    }
    if modifiers.shift {
        label.push('\u{21E7}');
    }
    // The badges use the macOS key symbols everywhere.
    label.push_str(&key_label(&keystroke.key, true));
    Some(label)
}

/// "cmd-}" as the keys pressed: Shift and "]".
// shortcut: assumes a US layout, so "cmd-/" typed with Shift+7 on a German one
// still reads ⇧⌘/; good enough until labels come from the platform's keymap.
fn as_pressed(keystroke: &Keystroke) -> Keystroke {
    let mut pressed = keystroke.clone();
    let mut chars = keystroke.key.chars();
    if let (Some(symbol), None) = (chars.next(), chars.next()) {
        if let Some(key) = unshifted_us_key(symbol) {
            pressed.key = key.to_string();
            pressed.modifiers.shift = true;
        }
    }
    pressed
}

fn key_label(key: &str, is_mac: bool) -> String {
    let named = match key {
        "enter" => Some(if is_mac { "\u{21B5}" } else { "Enter" }),
        "backspace" => Some(if is_mac { "\u{232B}" } else { "Backspace" }),
        "delete" => Some(if is_mac { "\u{2326}" } else { "Del" }),
        "tab" => Some(if is_mac { "\u{21E5}" } else { "Tab" }),
        "escape" => Some(if is_mac { "\u{238B}" } else { "Esc" }),
        "space" => Some("Space"),
        "up" => Some("\u{2191}"),
        "down" => Some("\u{2193}"),
        "left" => Some("\u{2190}"),
        "right" => Some("\u{2192}"),
        "pageup" => Some("PgUp"),
        "pagedown" => Some("PgDn"),
        _ => None,
    };
    if let Some(named) = named {
        return named.to_string();
    }
    let mut chars = key.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The file pane command bound to this key press, if any.
pub fn pane_command(keystroke: &Keystroke, cx: &App) -> Option<PaneCommand> {
    pane_command_in(keystroke, overrides(cx), IS_MAC)
}

fn pane_command_in(
    keystroke: &Keystroke,
    overrides: &Overrides,
    is_mac: bool,
) -> Option<PaneCommand> {
    let mut typed = keystroke.clone();
    typed.key = typed.key.to_ascii_lowercase();
    COMMANDS.iter().find_map(|command| {
        let Run::Pane(pane_command) = command.run else {
            return None;
        };
        effective_chords(command, overrides, is_mac)
            .into_iter()
            .flat_map(chord_variants)
            .filter_map(|variant| Keystroke::parse(&variant).ok())
            .any(|target| typed.should_match(&KeybindingKeystroke::from_keystroke(target)))
            .then_some(pane_command)
    })
}

/// Why a recorded chord cannot simply be set.
pub enum Clash {
    /// A key that cannot be rebound wins where the command runs.
    BuiltIn,
    /// These commands also run on the chord where the command runs.
    Commands(Vec<&'static Command>),
}

pub fn conflict(id: &str, chord: &str, cx: &App) -> Option<Clash> {
    let command = command(id)?;
    let wanted = Keystroke::parse(chord).ok()?;
    let keymap = cx.key_bindings();
    if is_built_in(command, &wanted, keymap.borrow().bindings()) {
        return Some(Clash::BuiltIn);
    }
    let others = conflicts_in(command, chord, overrides(cx), IS_MAC);
    (!others.is_empty()).then_some(Clash::Commands(others))
}

/// Whether a fixed binding in the terminal, the file pane or no context has
/// the chord. Bindings of other contexts (text fields, lists) are left out.
fn is_built_in<'a>(
    command: &Command,
    wanted: &Keystroke,
    bindings: impl IntoIterator<Item = &'a KeyBinding>,
) -> bool {
    bindings.into_iter().any(|binding| {
        let context = binding.predicate().map(|predicate| predicate.to_string());
        let context = context.as_deref();
        let is_app_context = context.is_none_or(|c| c == "Terminal" || c == FILE_PANE);
        binding.meta() != Some(REBINDABLE)
            && is_app_context
            && contexts_overlap(command.context, context)
            && binding.match_keystrokes(std::slice::from_ref(wanted)) == Some(false)
    })
}

fn conflicts_in(
    command: &Command,
    chord: &str,
    overrides: &Overrides,
    is_mac: bool,
) -> Vec<&'static Command> {
    let Ok(wanted) = Keystroke::parse(chord) else {
        return Vec::new();
    };
    COMMANDS
        .iter()
        .filter(|other| {
            other.id != command.id
                && contexts_overlap(command.context, other.context)
                && effective_chords(other, overrides, is_mac)
                    .into_iter()
                    .filter_map(|chord| Keystroke::parse(chord).ok())
                    .any(|theirs| same_keys(&theirs, &wanted))
        })
        .collect()
}

fn contexts_overlap(a: Option<&str>, b: Option<&str>) -> bool {
    a.is_none() || b.is_none() || a == b
}

fn same_keys(a: &Keystroke, b: &Keystroke) -> bool {
    a.key == b.key && a.modifiers == b.modifiers
}

/// Whether the chord takes a key the shell would otherwise get.
pub fn steals_from_terminal(id: &str, cx: &App) -> bool {
    let Some(command) = command(id) else {
        return false;
    };
    effective_chord(command, overrides(cx), IS_MAC)
        .and_then(|chord| Keystroke::parse(chord).ok())
        .is_some_and(|keystroke| takes_terminal_key(command, &keystroke))
}

fn takes_terminal_key(command: &Command, keystroke: &Keystroke) -> bool {
    if command.context == Some(FILE_PANE) {
        return false;
    }
    let modifiers = &keystroke.modifiers;
    let is_plain = !modifiers.control && !modifiers.alt && !modifiers.platform;
    if is_plain {
        return true;
    }
    let is_single_char = keystroke.key.chars().count() == 1;
    // The shell reads Ctrl+letter (readline) and Alt+key (Meta) on every platform.
    let is_ctrl_letter = modifiers.control
        && !modifiers.shift
        && !modifiers.alt
        && !modifiers.platform
        && is_single_char
        && keystroke.key.chars().all(|c| c.is_ascii_alphabetic());
    let is_alt_key = modifiers.alt && !modifiers.control && !modifiers.platform && is_single_char;
    is_ctrl_letter || is_alt_key
}

/// The chord a key press records in Settings, or `None` when the press cannot
/// be a shortcut: a modifier alone, or a key that is not F1-F24 with no
/// modifier but Shift, which only types a capital.
pub fn recorded_chord(keystroke: &Keystroke) -> Option<String> {
    let key = keystroke.key.as_str();
    if matches!(
        key,
        "" | "shift" | "control" | "alt" | "platform" | "function"
    ) {
        return None;
    }
    let modifiers = Modifiers {
        function: false,
        ..keystroke.modifiers
    };
    let is_command_chord = modifiers.control || modifiers.alt || modifiers.platform;
    if !is_command_chord && !is_function_key(key) {
        return None;
    }
    // shortcut: on Russian Windows, Ctrl+Б and Ctrl+Shift+Б both arrive as "Б" with
    // Shift off, so Shift on a symbol key is lost; it needs the physical key from gpui.
    let key = match us_layout_key(key) {
        Some(us) => us.to_string(),
        None => key.to_ascii_lowercase(),
    };
    let chord = Keystroke {
        key,
        key_char: None,
        modifiers,
    }
    .unparse();
    is_valid_chord(&chord).then_some(chord)
}

fn is_function_key(key: &str) -> bool {
    key.strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=24).contains(&number))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(chord: &str) -> Keystroke {
        Keystroke::parse(chord).unwrap()
    }

    fn overrides(pairs: &[(&str, Option<&str>)]) -> Overrides {
        pairs
            .iter()
            .map(|(id, chord)| (id.to_string(), chord.map(String::from)))
            .collect()
    }

    #[test]
    fn chords_read_the_platform_way() {
        assert_eq!(
            format_chord(&parse("cmd-shift-d"), true),
            "\u{21E7}\u{2318}D"
        );
        assert_eq!(
            format_chord(&parse("ctrl-cmd-s"), true),
            "\u{2303}\u{2318}S"
        );
        assert_eq!(format_chord(&parse("cmd-alt-t"), true), "\u{2325}\u{2318}T");
        assert_eq!(
            format_chord(&parse("cmd-backspace"), true),
            "\u{2318}\u{232B}"
        );
        assert_eq!(format_chord(&parse("f2"), true), "F2");
        assert_eq!(format_chord(&parse("ctrl-shift-d"), false), "Ctrl+Shift+D");
        assert_eq!(
            format_chord(&parse("ctrl-alt-shift-t"), false),
            "Ctrl+Alt+Shift+T"
        );
        assert_eq!(format_chord(&parse("ctrl-,"), false), "Ctrl+,");
        assert_eq!(format_chord(&parse("delete"), false), "Del");
        assert_eq!(format_chord(&parse("ctrl-tab"), false), "Ctrl+Tab");
        // A shifted symbol reads as the keys pressed.
        assert_eq!(format_chord(&parse("cmd-}"), true), "\u{21E7}\u{2318}]");
        assert_eq!(format_chord(&parse("ctrl->"), false), "Ctrl+Shift+.");
    }

    #[test]
    fn hints_leave_out_the_held_modifier() {
        assert_eq!(hint_label(&parse("cmd-t"), true).as_deref(), Some("T"));
        assert_eq!(
            hint_label(&parse("ctrl-cmd-s"), true).as_deref(),
            Some("\u{2303}S")
        );
        assert_eq!(
            hint_label(&parse("ctrl-shift-t"), false).as_deref(),
            Some("\u{21E7}T")
        );
        assert_eq!(hint_label(&parse("cmd-,"), true).as_deref(), Some(","));
        assert_eq!(
            hint_label(&parse("cmd-{"), true).as_deref(),
            Some("\u{21E7}[")
        );
        // Alt hides the hints, and a chord without the modifier has none.
        assert_eq!(hint_label(&parse("cmd-alt-t"), true), None);
        assert_eq!(hint_label(&parse("ctrl-shift-t"), true), None);
        assert_eq!(hint_label(&parse("f2"), false), None);
    }

    #[test]
    fn overrides_win_over_defaults_and_can_unbind() {
        let lock = command("app.lock").unwrap();
        let none = Overrides::new();
        assert_eq!(effective_chord(lock, &none, true), Some("cmd-l"));
        assert_eq!(effective_chord(lock, &none, false), Some("ctrl-shift-l"));

        let custom = overrides(&[("app.lock", Some("cmd-shift-l"))]);
        assert_eq!(effective_chord(lock, &custom, true), Some("cmd-shift-l"));

        let unbound = overrides(&[("app.lock", None)]);
        assert_eq!(effective_chord(lock, &unbound, true), None);
        // Quit has no default off macOS.
        assert_eq!(
            effective_chord(command("app.quit").unwrap(), &none, false),
            None
        );
    }

    #[test]
    fn setting_the_default_drops_the_override() {
        let lock = command("app.lock").unwrap();
        let mut stored = Overrides::new();
        set_override(&mut stored, lock, Some("cmd-shift-l".into()), true);
        assert_eq!(stored.get("app.lock"), Some(&Some("cmd-shift-l".into())));
        set_override(&mut stored, lock, Some("cmd-l".into()), true);
        assert!(stored.is_empty());
        set_override(&mut stored, lock, None, true);
        assert_eq!(stored.get("app.lock"), Some(&None));
    }

    fn conflict_ids(command: &Command, chord: &str, overrides: &Overrides) -> Vec<&'static str> {
        conflicts_in(command, chord, overrides, true)
            .iter()
            .map(|c| c.id)
            .collect()
    }

    #[test]
    fn conflicts_need_the_same_keys_where_both_run() {
        let lock = command("app.lock").unwrap();
        let none = Overrides::new();
        assert_eq!(conflict_ids(lock, "cmd-t", &none), ["app.new_host"]);
        // Modifier order does not matter.
        assert_eq!(
            conflict_ids(lock, "shift-cmd-t", &none),
            ["tabs.new_terminal"]
        );
        // A global chord also runs in the terminal and the file pane.
        assert_eq!(conflict_ids(lock, "cmd-k", &none), ["terminal.clear"]);
        assert_eq!(
            conflict_ids(lock, "cmd-f", &none),
            ["terminal.find", "files.filter"]
        );
        // A second default chord is held too.
        assert_eq!(conflict_ids(lock, "f5", &none), ["files.transfer"]);
        assert!(conflict_ids(lock, "cmd-l", &none).is_empty());

        // The terminal and the file pane never have focus together.
        let find = command("terminal.find").unwrap();
        assert!(conflict_ids(find, "cmd-r", &none).is_empty());

        // An unbound command no longer holds its chord.
        let unbound = overrides(&[("app.new_host", None)]);
        assert!(conflict_ids(lock, "cmd-t", &unbound).is_empty());
    }

    #[test]
    fn fixed_bindings_are_built_in_where_they_run() {
        let fixed = [
            KeyBinding::new("cmd-shift-=", IncreaseFontSize, TERMINAL),
            KeyBinding::new("cmd-shift-=", Quit, Some("Input")),
        ];
        let wanted = parse("cmd-shift-=");
        let lock = command("app.lock").unwrap();
        let rename = command("files.rename").unwrap();
        assert!(is_built_in(lock, &wanted, &fixed));
        assert!(!is_built_in(rename, &wanted, &fixed));
        // Text field bindings do not count.
        assert!(!is_built_in(lock, &wanted, &fixed[1..]));
        // Our own bindings are conflicts, not built-in keys.
        let ours = [KeyBinding::new("cmd-shift-=", Quit, None).with_meta(REBINDABLE)];
        assert!(!is_built_in(lock, &wanted, &ours));
    }

    #[test]
    fn defaults_do_not_conflict() {
        for is_mac in [true, false] {
            for command in COMMANDS {
                for chord in command.default_chords(is_mac) {
                    let others = conflicts_in(command, chord, &Overrides::new(), is_mac);
                    let ids: Vec<_> = others.iter().map(|c| c.id).collect();
                    assert!(ids.is_empty(), "{} and {ids:?} share {chord}", command.id);
                }
            }
        }
    }

    #[test]
    fn every_default_binds() {
        let ids: std::collections::HashSet<_> = COMMANDS.iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), COMMANDS.len(), "ids must be unique");
        for is_mac in [true, false] {
            for command in COMMANDS {
                for chord in command.default_chords(is_mac) {
                    assert!(is_valid_chord(chord), "{chord}");
                    // gpui never reports Shift with a symbol key, so this would not fire.
                    let keystroke = parse(chord);
                    let mut chars = keystroke.key.chars();
                    let is_symbol = chars
                        .next()
                        .and_then(crate::ui::keyboard_layout::shifted_us_key)
                        .is_some()
                        && chars.next().is_none();
                    assert!(!(keystroke.modifiers.shift && is_symbol), "{chord}");
                }
            }
            // `KeyBinding::new` panics on a chord it cannot parse.
            assert!(!bindings(&Overrides::new(), is_mac).is_empty());
        }
    }

    #[test]
    fn terminal_keys_are_flagged() {
        let lock = command("app.lock").unwrap();
        let rename = command("files.rename").unwrap();
        assert!(takes_terminal_key(lock, &parse("ctrl-r")));
        assert!(takes_terminal_key(lock, &parse("alt-b")));
        assert!(!takes_terminal_key(lock, &parse("ctrl-shift-r")));
        assert!(!takes_terminal_key(lock, &parse("ctrl-cmd-s")));
        assert!(!takes_terminal_key(lock, &parse("ctrl-alt-=")));
        assert!(takes_terminal_key(lock, &parse("shift-x")));
        assert!(takes_terminal_key(lock, &parse("f2")));
        assert!(!takes_terminal_key(lock, &parse("cmd-r")));
        // The file pane's keys never reach a terminal.
        assert!(!takes_terminal_key(rename, &parse("f2")));
    }

    #[test]
    fn recording_takes_chords_and_function_keys() {
        assert_eq!(
            recorded_chord(&parse("ctrl-shift-x")).as_deref(),
            Some("ctrl-shift-x")
        );
        assert_eq!(recorded_chord(&parse("f5")).as_deref(), Some("f5"));
        assert_eq!(recorded_chord(&parse("ctrl-К")).as_deref(), Some("ctrl-r"));
        assert_eq!(
            recorded_chord(&parse("shift-f5")).as_deref(),
            Some("shift-f5")
        );
        assert_eq!(recorded_chord(&parse("x")), None);
        // Shift alone types a capital.
        assert_eq!(recorded_chord(&parse("shift-l")), None);
        assert_eq!(recorded_chord(&parse("shift-up")), None);
        assert_eq!(recorded_chord(&parse("enter")), None);
        assert_eq!(recorded_chord(&parse("shift")), None);
        // gpui reports Shift+] as "}" with Shift off.
        assert_eq!(recorded_chord(&parse("cmd-}")).as_deref(), Some("cmd-}"));
    }

    #[test]
    fn russian_symbol_key_records_the_us_key() {
        // Windows reports the unshifted Russian key as a capital: this is Ctrl+,.
        assert_eq!(recorded_chord(&parse("ctrl-Б")).as_deref(), Some("ctrl-,"));
        assert_eq!(recorded_chord(&parse("ctrl-б")).as_deref(), Some("ctrl-,"));
    }

    #[test]
    fn file_pane_keys_follow_the_bindings() {
        let none = Overrides::new();
        let is_mac = IS_MAC;
        let secondary = if is_mac { "cmd" } else { "ctrl" };
        let refresh = parse(&format!("{secondary}-r"));
        assert_eq!(
            pane_command_in(&refresh, &none, is_mac),
            Some(PaneCommand::Refresh)
        );
        let russian = parse(&format!("{secondary}-к"));
        assert_eq!(
            pane_command_in(&russian, &none, is_mac),
            Some(PaneCommand::Refresh)
        );
        // Shift+. arrives as ">" with Shift off.
        let hidden = parse(&format!("{secondary}->"));
        assert_eq!(
            pane_command_in(&hidden, &none, is_mac),
            Some(PaneCommand::ToggleHidden)
        );
        assert_eq!(
            pane_command_in(&parse("f2"), &none, is_mac),
            Some(PaneCommand::Rename)
        );

        let moved = overrides(&[("files.refresh", Some("f9")), ("files.rename", None)]);
        assert_eq!(pane_command_in(&refresh, &moved, is_mac), None);
        assert_eq!(
            pane_command_in(&parse("f9"), &moved, is_mac),
            Some(PaneCommand::Refresh)
        );
        assert_eq!(pane_command_in(&parse("f2"), &moved, is_mac), None);
    }

    #[test]
    fn delete_and_transfer_keep_both_default_chords() {
        let none = Overrides::new();
        for is_mac in [true, false] {
            let secondary = if is_mac { "cmd" } else { "ctrl" };
            for (chord, wanted) in [
                ("delete".to_string(), PaneCommand::Delete),
                (format!("{secondary}-backspace"), PaneCommand::Delete),
                ("f5".to_string(), PaneCommand::Transfer),
                (format!("{secondary}-enter"), PaneCommand::Transfer),
            ] {
                assert_eq!(pane_command_in(&parse(&chord), &none, is_mac), Some(wanted));
            }
        }
        // A chord of the user's own replaces both.
        let moved = overrides(&[("files.delete", Some("f8"))]);
        assert_eq!(pane_command_in(&parse("delete"), &moved, true), None);
        // Taking one chord away keeps the other.
        let mut stored = Overrides::new();
        let transfer = command("files.transfer").unwrap();
        take_chord(&mut stored, transfer, "f5", true);
        assert_eq!(
            stored.get("files.transfer"),
            Some(&Some("cmd-enter".into()))
        );
        assert_eq!(pane_command_in(&parse("f5"), &stored, true), None);
        assert_eq!(
            pane_command_in(&parse("cmd-enter"), &stored, true),
            Some(PaneCommand::Transfer)
        );
        // A command with one chord loses its shortcut.
        let refresh = command("files.refresh").unwrap();
        take_chord(&mut stored, refresh, "cmd-r", true);
        assert_eq!(stored.get("files.refresh"), Some(&None));
        // Labels show the first one.
        let delete = command("files.delete").unwrap();
        assert_eq!(effective_chord(delete, &none, true), Some("cmd-backspace"));
    }
}
