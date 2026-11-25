//! App preferences that must be readable before the vault is unlocked.
//!
//! Lives next to `file_associations.json` so the app keeps one config folder.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Terminal font size in pixels, and the bounds zooming clamps to. The
/// default matches the file pane (12px) so both panes read at one size.
pub const DEFAULT_TERMINAL_FONT_SIZE: f32 = 12.0;
pub const MIN_TERMINAL_FONT_SIZE: f32 = 6.0;
pub const MAX_TERMINAL_FONT_SIZE: f32 = 40.0;

/// Side panel widths at 100% interface size, and the bounds a drag clamps to.
pub const DEFAULT_SIDEBAR_WIDTH: f32 = 232.;
// The footer needs it: "New host" and two icon buttons.
pub const MIN_SIDEBAR_WIDTH: f32 = 190.;
const MAX_SIDEBAR_WIDTH: f32 = 480.;
pub const DEFAULT_SERVER_PANEL_WIDTH: f32 = 320.;
pub const MIN_SERVER_PANEL_WIDTH: f32 = 260.;
const MAX_SERVER_PANEL_WIDTH: f32 = 640.;

/// How long a value must stay unchanged before it is written to disk.
/// Holding the zoom shortcut or dragging an edge changes it many times a second.
const SAVE_DELAY: Duration = Duration::from_millis(400);

/// Most hosts have vim. Minimal images often have only vi, which is a preset
/// in the settings.
pub const DEFAULT_EDITOR_COMMAND: &str = "vim";

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    Dark,
    Light,
    /// Follow the OS appearance and keep following it as it changes.
    System,
}

impl ThemePreference {
    pub fn label(self) -> &'static str {
        match self {
            ThemePreference::Dark => "Dark",
            ThemePreference::Light => "Light",
            ThemePreference::System => "System",
        }
    }
}

/// How long an unlock is kept across restarts without asking again.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RememberUnlock {
    #[default]
    Never,
    Day,
    Week,
}

impl RememberUnlock {
    pub const ALL: [RememberUnlock; 3] = [Self::Never, Self::Day, Self::Week];

    pub fn label(self) -> &'static str {
        match self {
            Self::Never => "Never",
            Self::Day => "For 1 day",
            Self::Week => "For 7 days",
        }
    }

    pub fn duration(self) -> Option<Duration> {
        match self {
            Self::Never => None,
            Self::Day => Some(Duration::from_secs(24 * 60 * 60)),
            Self::Week => Some(Duration::from_secs(7 * 24 * 60 * 60)),
        }
    }
}

/// The terminal font. A fixed list keeps proportional fonts, which break the
/// cell grid, out of the terminal.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFont {
    /// Bundled with the app, so it is always there.
    #[default]
    CascadiaCode,
    Menlo,
    Monaco,
    JetBrainsMono,
    FiraCode,
    SourceCodePro,
    Hack,
    Consolas,
    DejaVuSansMono,
    UbuntuMono,
    LiberationMono,
}

impl TerminalFont {
    pub const ALL: [TerminalFont; 11] = [
        TerminalFont::CascadiaCode,
        TerminalFont::Menlo,
        TerminalFont::Monaco,
        TerminalFont::JetBrainsMono,
        TerminalFont::FiraCode,
        TerminalFont::SourceCodePro,
        TerminalFont::Hack,
        TerminalFont::Consolas,
        TerminalFont::DejaVuSansMono,
        TerminalFont::UbuntuMono,
        TerminalFont::LiberationMono,
    ];

    /// The family name the system knows the font by. Also the label.
    pub fn family(self) -> &'static str {
        match self {
            TerminalFont::CascadiaCode => "Cascadia Code",
            TerminalFont::Menlo => "Menlo",
            TerminalFont::Monaco => "Monaco",
            TerminalFont::JetBrainsMono => "JetBrains Mono",
            TerminalFont::FiraCode => "Fira Code",
            TerminalFont::SourceCodePro => "Source Code Pro",
            TerminalFont::Hack => "Hack",
            TerminalFont::Consolas => "Consolas",
            TerminalFont::DejaVuSansMono => "DejaVu Sans Mono",
            TerminalFont::UbuntuMono => "Ubuntu Mono",
            TerminalFont::LiberationMono => "Liberation Mono",
        }
    }

    /// The fonts this machine can draw, out of `ALL`. `installed` is the
    /// system's list of family names.
    pub fn available(installed: &[String]) -> Vec<TerminalFont> {
        Self::ALL
            .into_iter()
            .filter(|font| {
                *font == TerminalFont::CascadiaCode
                    || installed.iter().any(|name| name == font.family())
            })
            .collect()
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalScheme {
    /// Andromeda in the dark app theme, GitHub Light in the light one.
    #[default]
    FollowTheme,
    Andromeda,
    GithubLight,
    Dracula,
    CatppuccinMocha,
    SolarizedDark,
    GruvboxDark,
    TokyoNight,
}

impl TerminalScheme {
    pub const ALL: [TerminalScheme; 8] = [
        TerminalScheme::FollowTheme,
        TerminalScheme::Andromeda,
        TerminalScheme::GithubLight,
        TerminalScheme::Dracula,
        TerminalScheme::CatppuccinMocha,
        TerminalScheme::SolarizedDark,
        TerminalScheme::GruvboxDark,
        TerminalScheme::TokyoNight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TerminalScheme::FollowTheme => "Match app theme",
            TerminalScheme::Andromeda => "Andromeda",
            TerminalScheme::GithubLight => "GitHub Light",
            TerminalScheme::Dracula => "Dracula",
            TerminalScheme::CatppuccinMocha => "Catppuccin Mocha",
            TerminalScheme::SolarizedDark => "Solarized Dark",
            TerminalScheme::GruvboxDark => "Gruvbox Dark",
            TerminalScheme::TokyoNight => "Tokyo Night",
        }
    }
}

/// When flint asks the flint server whether a newer build is out.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChecks {
    /// On every start and every six hours while the app runs.
    OnStart,
    /// Only when the user presses Check now.
    Manual,
}

/// Where a session puts the terminal relative to its file panes.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalPosition {
    /// Files on top, terminal under them.
    #[default]
    Below,
    /// Terminal on the left, files on the right.
    Beside,
}

/// The column the file lists sort by.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSortKey {
    #[default]
    Name,
    Type,
    Perms,
    Size,
    Modified,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSortDir {
    #[default]
    Asc,
    Desc,
}

/// What a sidebar host row shows on its right when it has no status text.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostSubtitle {
    /// When a closed host was last opened; nothing for an open one.
    #[default]
    LastOpened,
    /// host[:port]
    Address,
    /// user@host[:port]
    UserAtHost,
    None,
}

impl HostSubtitle {
    pub const ALL: [HostSubtitle; 4] = [
        HostSubtitle::LastOpened,
        HostSubtitle::Address,
        HostSubtitle::UserAtHost,
        HostSubtitle::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HostSubtitle::LastOpened => "Last opened",
            HostSubtitle::Address => "Address",
            HostSubtitle::UserAtHost => "user@host",
            HostSubtitle::None => "Nothing",
        }
    }
}

/// A block of the server panel the user can hide.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelSection {
    Cpu,
    Memory,
    Network,
    Disks,
    System,
    Addresses,
    Connection,
    Snippets,
}

impl PanelSection {
    pub const ALL: [PanelSection; 8] = [
        PanelSection::Cpu,
        PanelSection::Memory,
        PanelSection::Network,
        PanelSection::Disks,
        PanelSection::System,
        PanelSection::Addresses,
        PanelSection::Connection,
        PanelSection::Snippets,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PanelSection::Cpu => "CPU",
            PanelSection::Memory => "Memory",
            PanelSection::Network => "Network",
            PanelSection::Disks => "Disks",
            PanelSection::System => "System info",
            PanelSection::Addresses => "IP addresses",
            PanelSection::Connection => "Connection",
            PanelSection::Snippets => "Snippets tab",
        }
    }
}

pub const RECENT_HOSTS_LIMITS: [usize; 3] = [3, 6, 10];
const DEFAULT_RECENT_HOSTS_LIMIT: usize = 6;

/// Every field carries a serde default so a file written by an older build
/// still loads.
#[derive(Clone, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default, deserialize_with = "or_default")]
    pub theme: ThemePreference,
    /// Show the history suggestions popup in the terminal, and read the
    /// server's shell history for it.
    #[serde(default = "default_true")]
    pub is_suggestions_enabled: bool,
    /// macOS: Option+key sends ESC + key (Meta) instead of typing the
    /// character the layout composes.
    #[serde(default)]
    pub is_option_meta: bool,
    /// Shared by every terminal, so zoom carries over to new tabs and to
    /// the next start. Read it through `Preferences::terminal_font_size`,
    /// which clamps a hand-edited value.
    #[serde(default = "default_terminal_font_size")]
    pub terminal_font_size: f32,
    #[serde(default, deserialize_with = "or_default")]
    pub terminal_font: TerminalFont,
    #[serde(default, deserialize_with = "or_default")]
    pub terminal_scheme: TerminalScheme,
    /// Whole-interface zoom. `None` follows the display, see `theme::auto_ui_scale`.
    #[serde(default, deserialize_with = "or_default")]
    pub ui_scale: Option<f32>,
    /// `None` until the user answers the question, and until then the app
    /// makes no update request on its own. Set by `load_from` from the raw
    /// file, see `update_checks_in`.
    #[serde(default, deserialize_with = "or_default")]
    pub update_checks: Option<UpdateChecks>,
    /// Shared by every session, so the layout picked once stays.
    #[serde(default, deserialize_with = "or_default")]
    pub terminal_position: TerminalPosition,
    /// Whether a session shows the second file pane (this computer or
    /// another server) next to the session's own files. Off by default: most
    /// uploads come from Finder or Explorer.
    #[serde(default)]
    pub is_second_pane_shown: bool,
    /// Whether the main window shows the hosts sidebar.
    #[serde(default = "default_true")]
    pub is_sidebar_shown: bool,
    /// The vault key is in the OS store behind Touch ID or Windows Hello.
    #[serde(default)]
    pub is_touch_id_enabled: bool,
    #[serde(default, deserialize_with = "or_default")]
    pub remember_unlock: RememberUnlock,
    /// Whether a session shows the server panel on its right.
    #[serde(default)]
    pub is_server_panel_shown: bool,
    /// Read the widths through their getters, which clamp a hand-edited value.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    #[serde(default = "default_server_panel_width")]
    pub server_panel_width: f32,
    #[serde(default, deserialize_with = "or_default")]
    pub hidden_panel_sections: Vec<PanelSection>,
    /// The command that edits a remote file in a terminal tab. Read it
    /// through `Preferences::editor_command`, which fills in an empty one.
    #[serde(default = "default_editor_command")]
    pub editor_command: String,
    /// Double-click and Enter on a remote file run `editor_command` instead
    /// of opening the file on this computer.
    #[serde(default)]
    pub is_editor_default_open: bool,
    /// An onboarding hint only, never for security. Kept after sign-out and
    /// vault deletion. Set by `load_from`, see `is_flag_set_in`.
    #[serde(default)]
    pub has_account: bool,
    /// Shortcuts the user changed, see `ui::keymap`.
    #[serde(default, deserialize_with = "valid_shortcuts")]
    pub shortcuts: crate::ui::keymap::Overrides,
    /// Hosts pinned to the top of the sidebar, in pin order. Device-local.
    #[serde(default, deserialize_with = "or_default")]
    pub pinned_hosts: Vec<uuid::Uuid>,
    #[serde(default = "default_true")]
    pub is_recent_hosts_shown: bool,
    /// Read it through `Preferences::recent_hosts_limit`.
    #[serde(
        default = "default_recent_hosts_limit",
        deserialize_with = "or_default"
    )]
    pub recent_hosts_limit: usize,
    #[serde(default, deserialize_with = "or_default")]
    pub host_subtitle: HostSubtitle,
    #[serde(default)]
    pub is_sidebar_compact: bool,
    #[serde(default = "default_true")]
    pub is_feedback_button_shown: bool,
    /// The sort of the file panes, so it outlives a restart.
    #[serde(default, deserialize_with = "or_default")]
    pub file_sort: (FileSortKey, FileSortDir),
    /// Selecting text in the terminal copies it.
    #[serde(default)]
    pub is_copy_on_select: bool,
}

/// The update answer the raw file holds. Read on its own so that a file that
/// does not load as a whole keeps it: one value this build does not know must
/// not undo the answer.
///
/// Files from 0.1.x have `is_update_check_enabled` instead. Its `true` was
/// only the default, so just `false` counts as an answer.
fn update_checks_in(data: &[u8]) -> Option<UpdateChecks> {
    let file = serde_json::from_slice::<serde_json::Value>(data).ok()?;
    if let Ok(answer) = UpdateChecks::deserialize(&file["update_checks"]) {
        return Some(answer);
    }
    (file["is_update_check_enabled"] == false).then_some(UpdateChecks::Manual)
}

/// Read from the raw file, like `update_checks_in`.
fn is_flag_set_in(data: &[u8], name: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(data).is_ok_and(|file| file[name] == true)
}

/// A value this build does not know, such as a scheme added later, falls
/// back to the default for that field only.
fn or_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

/// A hand-edited chord gpui cannot parse would panic when bound, so it is
/// dropped here and the command keeps its default. Other entries stay.
fn valid_shortcuts<'de, D>(deserializer: D) -> Result<crate::ui::keymap::Overrides, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let entries: std::collections::BTreeMap<String, serde_json::Value> = or_default(deserializer)?;
    let shortcuts = entries
        .into_iter()
        .filter_map(|(id, value)| match value {
            serde_json::Value::Null => Some((id, None)),
            serde_json::Value::String(chord) if crate::ui::keymap::is_valid_chord(&chord) => {
                Some((id, Some(chord)))
            }
            value => {
                tracing::warn!(id, %value, "invalid shortcut in preferences, using the default");
                None
            }
        })
        .collect();
    Ok(shortcuts)
}

fn default_true() -> bool {
    true
}

fn default_recent_hosts_limit() -> usize {
    DEFAULT_RECENT_HOSTS_LIMIT
}

fn default_editor_command() -> String {
    DEFAULT_EDITOR_COMMAND.to_string()
}

fn default_terminal_font_size() -> f32 {
    DEFAULT_TERMINAL_FONT_SIZE
}

fn default_sidebar_width() -> f32 {
    DEFAULT_SIDEBAR_WIDTH
}

fn default_server_panel_width() -> f32 {
    DEFAULT_SERVER_PANEL_WIDTH
}

/// A size the terminal can lay out. The file is hand-editable, so the stored
/// number can be anything, including one that is not a number.
fn clamp_terminal_font_size(size: f32) -> f32 {
    clamp_or_default(
        size,
        DEFAULT_TERMINAL_FONT_SIZE,
        MIN_TERMINAL_FONT_SIZE,
        MAX_TERMINAL_FONT_SIZE,
    )
}

fn clamp_sidebar_width(width: f32) -> f32 {
    clamp_or_default(
        width,
        DEFAULT_SIDEBAR_WIDTH,
        MIN_SIDEBAR_WIDTH,
        MAX_SIDEBAR_WIDTH,
    )
}

fn clamp_server_panel_width(width: f32) -> f32 {
    clamp_or_default(
        width,
        DEFAULT_SERVER_PANEL_WIDTH,
        MIN_SERVER_PANEL_WIDTH,
        MAX_SERVER_PANEL_WIDTH,
    )
}

fn clamp_or_default(value: f32, default: f32, min: f32, max: f32) -> f32 {
    if !value.is_finite() {
        return default;
    }
    value.clamp(min, max)
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: ThemePreference::default(),
            is_suggestions_enabled: true,
            is_option_meta: false,
            terminal_font_size: DEFAULT_TERMINAL_FONT_SIZE,
            terminal_font: TerminalFont::default(),
            terminal_scheme: TerminalScheme::default(),
            ui_scale: None,
            update_checks: None,
            terminal_position: TerminalPosition::default(),
            is_second_pane_shown: false,
            is_sidebar_shown: true,
            is_touch_id_enabled: false,
            remember_unlock: RememberUnlock::Never,
            is_server_panel_shown: false,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            server_panel_width: DEFAULT_SERVER_PANEL_WIDTH,
            hidden_panel_sections: Vec::new(),
            editor_command: default_editor_command(),
            is_editor_default_open: false,
            has_account: false,
            shortcuts: Default::default(),
            pinned_hosts: Vec::new(),
            is_recent_hosts_shown: true,
            recent_hosts_limit: DEFAULT_RECENT_HOSTS_LIMIT,
            host_subtitle: HostSubtitle::default(),
            is_sidebar_compact: false,
            is_feedback_button_shown: true,
            file_sort: Default::default(),
            is_copy_on_select: false,
        }
    }
}

impl gpui::Global for Preferences {}

impl Preferences {
    /// Whether terminal suggestions are on. True when the preferences were
    /// never loaded, which matches the default.
    pub fn is_suggestions_enabled(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_none_or(|prefs| prefs.is_suggestions_enabled)
    }

    /// Whether Option acts as Meta in the terminal. False when the
    /// preferences were never loaded, which matches the default.
    pub fn is_option_meta(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|prefs| prefs.is_option_meta)
    }

    /// `None` when the user has not answered, or the preferences were never
    /// loaded.
    pub fn update_checks(cx: &gpui::App) -> Option<UpdateChecks> {
        cx.try_global::<Self>()
            .and_then(|prefs| prefs.update_checks)
    }

    pub fn terminal_font_size(cx: &gpui::App) -> f32 {
        cx.try_global::<Self>()
            .map_or(DEFAULT_TERMINAL_FONT_SIZE, |prefs| {
                clamp_terminal_font_size(prefs.terminal_font_size)
            })
    }

    pub fn ui_scale(cx: &gpui::App) -> Option<f32> {
        cx.try_global::<Self>()
            .and_then(|prefs| prefs.ui_scale)
            .filter(|scale| scale.is_finite())
            .map(|scale| scale.clamp(0.75, 2.0))
    }

    pub fn terminal_font(cx: &gpui::App) -> TerminalFont {
        cx.try_global::<Self>()
            .map(|prefs| prefs.terminal_font)
            .unwrap_or_default()
    }

    pub fn terminal_scheme(cx: &gpui::App) -> TerminalScheme {
        cx.try_global::<Self>()
            .map(|prefs| prefs.terminal_scheme)
            .unwrap_or_default()
    }

    pub fn terminal_position(cx: &gpui::App) -> TerminalPosition {
        cx.try_global::<Self>()
            .map(|prefs| prefs.terminal_position)
            .unwrap_or_default()
    }

    pub fn is_second_pane_shown(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|prefs| prefs.is_second_pane_shown)
    }

    /// True when the preferences were never loaded, which matches the default.
    pub fn is_sidebar_shown(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_none_or(|prefs| prefs.is_sidebar_shown)
    }

    pub fn is_touch_id_enabled(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|prefs| prefs.is_touch_id_enabled)
    }

    pub fn remember_unlock(cx: &gpui::App) -> RememberUnlock {
        cx.try_global::<Self>()
            .map_or(RememberUnlock::Never, |prefs| prefs.remember_unlock)
    }

    pub fn is_server_panel_shown(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|prefs| prefs.is_server_panel_shown)
    }

    pub fn sidebar_width(cx: &gpui::App) -> f32 {
        cx.try_global::<Self>()
            .map_or(DEFAULT_SIDEBAR_WIDTH, |prefs| {
                clamp_sidebar_width(prefs.sidebar_width)
            })
    }

    pub fn server_panel_width(cx: &gpui::App) -> f32 {
        cx.try_global::<Self>()
            .map_or(DEFAULT_SERVER_PANEL_WIDTH, |prefs| {
                clamp_server_panel_width(prefs.server_panel_width)
            })
    }

    pub fn editor_command(cx: &gpui::App) -> String {
        let command = cx
            .try_global::<Self>()
            .map_or("", |prefs| prefs.editor_command.trim());
        if command.is_empty() {
            return DEFAULT_EDITOR_COMMAND.to_string();
        }
        command.to_string()
    }

    pub fn is_editor_default_open(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|prefs| prefs.is_editor_default_open)
    }

    /// False when the preferences were never loaded.
    pub fn has_account(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|prefs| prefs.has_account)
    }

    pub fn pinned_hosts(cx: &gpui::App) -> Vec<uuid::Uuid> {
        cx.try_global::<Self>()
            .map(|prefs| prefs.pinned_hosts.clone())
            .unwrap_or_default()
    }

    pub fn is_pinned(id: uuid::Uuid, cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|prefs| prefs.pinned_hosts.contains(&id))
    }

    /// Pin a host at the end of the list, or unpin it.
    pub fn set_pinned(id: uuid::Uuid, is_pinned: bool, cx: &mut gpui::App) {
        Self::update(cx, |prefs| {
            prefs.pinned_hosts.retain(|pinned| *pinned != id);
            if is_pinned {
                prefs.pinned_hosts.push(id);
            }
        });
    }

    /// Put a pinned host where `onto` is, from a drag in the sidebar.
    pub fn move_pin(id: uuid::Uuid, onto: uuid::Uuid, cx: &mut gpui::App) {
        Self::update(cx, |prefs| move_pin(&mut prefs.pinned_hosts, id, onto));
    }

    pub fn is_panel_section_shown(section: PanelSection, cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_none_or(|prefs| !prefs.hidden_panel_sections.contains(&section))
    }

    pub fn set_panel_section_shown(section: PanelSection, is_shown: bool, cx: &mut gpui::App) {
        Self::update(cx, |prefs| {
            prefs
                .hidden_panel_sections
                .retain(|hidden| *hidden != section);
            if !is_shown {
                prefs.hidden_panel_sections.push(section);
            }
        });
    }

    pub fn is_recent_hosts_shown(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_none_or(|prefs| prefs.is_recent_hosts_shown)
    }

    /// The sidebar's cap on recent hosts. A hand-edited value outside the
    /// choices falls back to the default.
    pub fn recent_hosts_limit(cx: &gpui::App) -> usize {
        cx.try_global::<Self>()
            .map(|prefs| prefs.recent_hosts_limit)
            .filter(|limit| RECENT_HOSTS_LIMITS.contains(limit))
            .unwrap_or(DEFAULT_RECENT_HOSTS_LIMIT)
    }

    pub fn host_subtitle(cx: &gpui::App) -> HostSubtitle {
        cx.try_global::<Self>()
            .map(|prefs| prefs.host_subtitle)
            .unwrap_or_default()
    }

    pub fn is_sidebar_compact(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|prefs| prefs.is_sidebar_compact)
    }

    pub fn is_feedback_button_shown(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_none_or(|prefs| prefs.is_feedback_button_shown)
    }

    /// Change the preferences and write them to disk. The change has already
    /// taken effect in memory, so a failed write is logged and not surfaced.
    pub fn file_sort(cx: &gpui::App) -> (FileSortKey, FileSortDir) {
        cx.try_global::<Self>()
            .map(|prefs| prefs.file_sort)
            .unwrap_or_default()
    }

    pub fn is_copy_on_select(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|prefs| prefs.is_copy_on_select)
    }

    pub fn update(cx: &mut gpui::App, change: impl FnOnce(&mut Self)) {
        let prefs = cx.default_global::<Self>();
        change(prefs);
        if let Err(e) = prefs.save() {
            tracing::warn!(?e, "failed to save preferences");
        }
    }

    /// Set the font size of every terminal. The write to disk waits until the
    /// size settles, so holding the zoom shortcut gives one write.
    pub fn set_terminal_font_size(size: f32, cx: &mut gpui::App) {
        let size = clamp_terminal_font_size(size);
        if Self::terminal_font_size(cx) == size {
            return;
        }
        cx.default_global::<Self>().terminal_font_size = size;
        Self::save_when_settled(cx, move |prefs| prefs.terminal_font_size == size);
    }

    pub fn set_sidebar_width(width: f32, cx: &mut gpui::App) {
        let width = clamp_sidebar_width(width);
        if Self::sidebar_width(cx) == width {
            return;
        }
        cx.default_global::<Self>().sidebar_width = width;
        Self::save_when_settled(cx, move |prefs| prefs.sidebar_width == width);
    }

    pub fn set_server_panel_width(width: f32, cx: &mut gpui::App) {
        let width = clamp_server_panel_width(width);
        if Self::server_panel_width(cx) == width {
            return;
        }
        cx.default_global::<Self>().server_panel_width = width;
        Self::save_when_settled(cx, move |prefs| prefs.server_panel_width == width);
    }

    /// Writes only if `is_unchanged` still holds after the delay, so a drag or
    /// a held shortcut gives one write.
    fn save_when_settled(cx: &mut gpui::App, is_unchanged: impl Fn(&Self) -> bool + 'static) {
        cx.spawn(async move |cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            cx.update(|cx| {
                let Some(prefs) = cx.try_global::<Self>() else {
                    return;
                };
                // A later change started its own timer and will do the write.
                if !is_unchanged(prefs) {
                    return;
                }
                // Written on the main thread like every other preference. A
                // copy written from another thread could land after a newer
                // save and put an old value back.
                if let Err(e) = prefs.save() {
                    tracing::warn!(?e, "failed to save preferences");
                }
            });
        })
        .detach();
    }

    pub fn load_or_default() -> Self {
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => {
                tracing::warn!("no config directory, using default preferences");
                Self::default()
            }
        }
    }

    /// A corrupt preferences file must never stop the app from starting, so
    /// every failure here falls back to the defaults instead of propagating.
    fn load_from(path: &Path) -> Self {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no preferences file, using defaults");
                return Self::default();
            }
            Err(e) => {
                tracing::warn!(?e, path = %path.display(), "failed to read preferences.json");
                return Self::default();
            }
        };
        let prefs = serde_json::from_slice::<Self>(&data).unwrap_or_else(|e| {
            tracing::warn!(?e, "invalid preferences.json, using defaults");
            Self::default()
        });
        Self {
            update_checks: update_checks_in(&data),
            has_account: is_flag_set_in(&data, "has_account"),
            is_touch_id_enabled: is_flag_set_in(&data, "is_touch_id_enabled"),
            ..prefs
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "could not find the config directory",
            )
        })?;
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        // Written beside the file and moved over it, so a crash in the
        // middle of the write cannot leave half a file.
        let unfinished = path.with_extension("json.new");
        std::fs::write(&unfinished, json)?;
        std::fs::rename(&unfinished, path)
    }

    /// `None` when the OS gives no config directory, as in some sandboxes.
    fn path() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("Flint").join("preferences.json"))
    }
}

/// `id` takes the place of `onto`: dragged down it lands after `onto`, up before it.
fn move_pin(pins: &mut Vec<uuid::Uuid>, id: uuid::Uuid, onto: uuid::Uuid) {
    let (Some(from), Some(to)) = (
        pins.iter().position(|pin| *pin == id),
        pins.iter().position(|pin| *pin == onto),
    ) else {
        return;
    };
    let pin = pins.remove(from);
    pins.insert(to, pin);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_preference_uses_snake_case_on_the_wire() {
        for (pref, wire) in [
            (ThemePreference::Dark, "\"dark\""),
            (ThemePreference::Light, "\"light\""),
            (ThemePreference::System, "\"system\""),
        ] {
            assert_eq!(serde_json::to_string(&pref).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ThemePreference>(wire).unwrap(), pref);
        }
    }

    #[test]
    fn missing_and_unknown_fields_fall_back_to_dark() {
        let empty: Preferences = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.theme, ThemePreference::Dark);

        let future: Preferences =
            serde_json::from_str(r#"{"theme":"light","future_field":42}"#).unwrap();
        assert_eq!(future.theme, ThemePreference::Light);
    }

    #[test]
    fn suggestions_default_to_on_and_round_trip() {
        assert!(Preferences::default().is_suggestions_enabled);
        // A file from before the field existed must not turn suggestions off.
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert!(old.is_suggestions_enabled);

        let off = Preferences {
            is_suggestions_enabled: false,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&off).unwrap();
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert!(!back.is_suggestions_enabled);
    }

    #[test]
    fn the_update_answer_starts_unset_and_round_trips() {
        assert_eq!(Preferences::default().update_checks, None);
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(old.update_checks, None);

        for answer in [UpdateChecks::OnStart, UpdateChecks::Manual] {
            let prefs = Preferences {
                update_checks: Some(answer),
                ..Preferences::default()
            };
            let json = serde_json::to_string(&prefs).unwrap();
            assert_eq!(update_checks_in(json.as_bytes()), Some(answer), "{json}");
        }
        assert_eq!(
            serde_json::to_string(&UpdateChecks::OnStart).unwrap(),
            r#""on_start""#
        );
        assert_eq!(
            serde_json::to_string(&UpdateChecks::Manual).unwrap(),
            r#""manual""#
        );
    }

    #[test]
    fn a_file_from_0_1_counts_only_a_turned_off_check_as_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            Preferences::load_from(&path).update_checks
        };

        assert_eq!(
            load(r#"{"theme":"light","is_update_check_enabled":false}"#),
            Some(UpdateChecks::Manual)
        );
        assert_eq!(
            load(r#"{"theme":"light","is_update_check_enabled":true}"#),
            None
        );
        assert_eq!(load(r#"{"theme":"light"}"#), None);
        // An answer given in this build wins over the old key.
        assert_eq!(
            load(r#"{"update_checks":"on_start","is_update_check_enabled":false}"#),
            Some(UpdateChecks::OnStart)
        );
    }

    #[test]
    fn the_update_answer_stays_when_the_rest_of_the_file_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            Preferences::load_from(&path)
        };

        // A value written by a newer build.
        let manual = load(r#"{"terminal_scheme":"from-the-future","update_checks":"manual"}"#);
        assert_eq!(manual.update_checks, Some(UpdateChecks::Manual));
        assert_eq!(manual.terminal_font_size, DEFAULT_TERMINAL_FONT_SIZE);

        assert_eq!(
            load(r#"{"terminal_scheme":7,"is_update_check_enabled":false}"#).update_checks,
            Some(UpdateChecks::Manual)
        );
        assert_eq!(
            load(r#"{"terminal_scheme":7,"update_checks":"on_start"}"#).update_checks,
            Some(UpdateChecks::OnStart)
        );
        assert_eq!(
            load(r#"{"terminal_scheme":"from-the-future"}"#).update_checks,
            None
        );
        assert_eq!(load("not json").update_checks, None);
    }

    #[test]
    fn the_account_flag_stays_when_the_rest_of_the_file_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            Preferences::load_from(&path)
        };

        let future = load(r#"{"terminal_scheme":"from-the-future","has_account":true}"#);
        assert!(future.has_account);
        assert_eq!(future.terminal_font_size, DEFAULT_TERMINAL_FONT_SIZE);

        assert!(load(r#"{"has_account":true}"#).has_account);
        assert!(!load(r#"{"has_account":false}"#).has_account);
        assert!(!load(r#"{"theme":"light"}"#).has_account);
        assert!(!load(r#"{"terminal_scheme":7}"#).has_account);
        assert!(!load("not json").has_account);
    }

    #[test]
    fn an_unknown_scheme_keeps_the_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(
            &path,
            r#"{"theme":"light","terminal_scheme":"from-the-future","terminal_font":"menlo","update_checks":"weekly","is_touch_id_enabled":true}"#,
        )
        .unwrap();
        let prefs = Preferences::load_from(&path);
        assert_eq!(prefs.theme, ThemePreference::Light);
        assert_eq!(prefs.terminal_scheme, TerminalScheme::FollowTheme);
        assert_eq!(prefs.terminal_font, TerminalFont::Menlo);
        assert!(prefs.is_touch_id_enabled);
    }

    #[test]
    fn touch_id_stays_when_the_rest_of_the_file_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(
            &path,
            r#"{"terminal_font_size":"big","is_touch_id_enabled":true}"#,
        )
        .unwrap();
        assert!(Preferences::load_from(&path).is_touch_id_enabled);
    }

    #[test]
    fn saving_replaces_the_file_and_leaves_nothing_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Flint").join("preferences.json");
        let manual = Preferences {
            update_checks: Some(UpdateChecks::Manual),
            ..Preferences::default()
        };

        Preferences::default().save_to(&path).unwrap();
        manual.save_to(&path).unwrap();

        assert_eq!(
            Preferences::load_from(&path).update_checks,
            Some(UpdateChecks::Manual)
        );
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(left.len(), 1, "{left:?}");
    }

    #[test]
    fn option_meta_defaults_to_off_and_round_trips() {
        assert!(!Preferences::default().is_option_meta);
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert!(!old.is_option_meta);

        let on = Preferences {
            is_option_meta: true,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&on).unwrap();
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert!(back.is_option_meta);
    }

    #[test]
    fn session_layout_defaults_when_missing_and_round_trips() {
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(old.terminal_position, TerminalPosition::Below);
        assert!(!old.is_second_pane_shown);

        let changed = Preferences {
            terminal_position: TerminalPosition::Beside,
            is_second_pane_shown: true,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&changed).unwrap();
        assert!(json.contains(r#""terminal_position":"beside""#), "{json}");
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back.terminal_position, TerminalPosition::Beside);
        assert!(back.is_second_pane_shown);
    }

    #[test]
    fn sidebar_is_shown_when_missing_and_round_trips() {
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert!(old.is_sidebar_shown);

        let hidden = Preferences {
            is_sidebar_shown: false,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&hidden).unwrap();
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert!(!back.is_sidebar_shown);
    }

    #[test]
    fn sidebar_options_default_when_missing_and_round_trip() {
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert!(old.pinned_hosts.is_empty());
        assert!(old.is_recent_hosts_shown);
        assert_eq!(old.recent_hosts_limit, 6);
        assert_eq!(old.host_subtitle, HostSubtitle::LastOpened);
        assert!(!old.is_sidebar_compact);
        assert!(old.is_feedback_button_shown);

        let pinned = uuid::Uuid::new_v4();
        let changed = Preferences {
            pinned_hosts: vec![pinned],
            is_recent_hosts_shown: false,
            recent_hosts_limit: 10,
            host_subtitle: HostSubtitle::UserAtHost,
            is_sidebar_compact: true,
            is_feedback_button_shown: false,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&changed).unwrap();
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pinned_hosts, vec![pinned]);
        assert!(!back.is_recent_hosts_shown);
        assert_eq!(back.recent_hosts_limit, 10);
        assert_eq!(back.host_subtitle, HostSubtitle::UserAtHost);
        assert!(back.is_sidebar_compact);
        assert!(!back.is_feedback_button_shown);
    }

    #[test]
    fn panel_sections_are_shown_when_missing_and_round_trip() {
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert!(old.hidden_panel_sections.is_empty());

        let changed = Preferences {
            hidden_panel_sections: vec![PanelSection::Disks, PanelSection::Snippets],
            ..Preferences::default()
        };
        let json = serde_json::to_string(&changed).unwrap();
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hidden_panel_sections, changed.hidden_panel_sections);

        let unknown: Preferences =
            serde_json::from_str(r#"{"theme":"light","hidden_panel_sections":["later"]}"#).unwrap();
        assert_eq!(unknown.theme, ThemePreference::Light);
        assert!(unknown.hidden_panel_sections.is_empty());
    }

    #[test]
    fn the_file_sort_survives_a_save_and_a_bad_value_falls_back() {
        let prefs = Preferences {
            file_sort: (FileSortKey::Size, FileSortDir::Desc),
            ..Preferences::default()
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back.file_sort, (FileSortKey::Size, FileSortDir::Desc));
        let unknown: Preferences =
            serde_json::from_str(r#"{"file_sort": ["colour", "up"]}"#).unwrap();
        assert_eq!(unknown.file_sort, Default::default());
    }

    #[test]
    fn a_dragged_pin_takes_the_place_of_the_one_it_is_dropped_on() {
        let [a, b, c] = [(); 3].map(|_| uuid::Uuid::new_v4());
        let mut pins = vec![a, b, c];
        move_pin(&mut pins, a, c);
        assert_eq!(pins, vec![b, c, a]);
        move_pin(&mut pins, a, b);
        assert_eq!(pins, vec![a, b, c]);
        move_pin(&mut pins, b, uuid::Uuid::new_v4());
        assert_eq!(pins, vec![a, b, c]);
    }

    #[test]
    fn an_unknown_subtitle_or_bad_pin_keeps_the_other_settings() {
        let prefs: Preferences = serde_json::from_str(
            r#"{"theme":"light","host_subtitle":"later","pinned_hosts":["nope"],"recent_hosts_limit":-1}"#,
        )
        .unwrap();
        assert_eq!(prefs.theme, ThemePreference::Light);
        assert_eq!(prefs.recent_hosts_limit, 0);
        assert_eq!(prefs.host_subtitle, HostSubtitle::LastOpened);
        assert!(prefs.pinned_hosts.is_empty());
    }

    #[test]
    fn shortcuts_round_trip_and_drop_invalid_chords() {
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert!(old.shortcuts.is_empty());

        let mut changed = Preferences::default();
        changed
            .shortcuts
            .insert("app.lock".into(), Some("cmd-shift-l".into()));
        changed.shortcuts.insert("app.quit".into(), None);
        let json = serde_json::to_string(&changed).unwrap();
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back.shortcuts, changed.shortcuts);

        let edited: Preferences = serde_json::from_str(
            r#"{"shortcuts":{"app.lock":"hyper-x","app.quit":"ctrl-q x","app.settings":null,"future.id":"f3"}}"#,
        )
        .unwrap();
        assert_eq!(edited.shortcuts.get("app.lock"), None);
        assert_eq!(edited.shortcuts.get("app.quit"), None);
        assert_eq!(edited.shortcuts.get("app.settings"), Some(&None));
        // Unknown ids are kept, so a newer build's shortcuts survive.
        assert_eq!(edited.shortcuts.get("future.id"), Some(&Some("f3".into())));

        // A map that is not a map falls back to no overrides, not to defaults for the file.
        let broken: Preferences =
            serde_json::from_str(r#"{"theme":"light","shortcuts":[1]}"#).unwrap();
        assert!(broken.shortcuts.is_empty());
        assert_eq!(broken.theme, ThemePreference::Light);
    }

    #[test]
    fn one_bad_shortcut_keeps_the_others() {
        let prefs: Preferences = serde_json::from_str(
            r#"{"shortcuts":{"app.lock":"cmd-shift-l","app.quit":5,"app.settings":null}}"#,
        )
        .unwrap();
        assert_eq!(prefs.shortcuts.len(), 2);
        assert_eq!(
            prefs.shortcuts.get("app.lock"),
            Some(&Some("cmd-shift-l".into()))
        );
        assert_eq!(prefs.shortcuts.get("app.settings"), Some(&None));
    }

    #[test]
    fn editor_settings_default_when_missing_and_round_trip() {
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(old.editor_command, DEFAULT_EDITOR_COMMAND);
        assert!(!old.is_editor_default_open);

        let changed = Preferences {
            editor_command: "sudoedit {path}".to_string(),
            is_editor_default_open: true,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&changed).unwrap();
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back.editor_command, "sudoedit {path}");
        assert!(back.is_editor_default_open);
    }

    #[test]
    fn terminal_settings_default_when_missing_and_round_trip() {
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(old.terminal_font_size, DEFAULT_TERMINAL_FONT_SIZE);
        assert_eq!(old.terminal_font, TerminalFont::CascadiaCode);
        assert_eq!(old.terminal_scheme, TerminalScheme::FollowTheme);
        assert_eq!(old.ui_scale, None);

        let changed = Preferences {
            terminal_font_size: 17.0,
            ui_scale: Some(1.25),
            terminal_font: TerminalFont::JetBrainsMono,
            terminal_scheme: TerminalScheme::TokyoNight,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&changed).unwrap();
        assert!(
            json.contains(r#""terminal_font":"jet_brains_mono""#),
            "{json}"
        );
        assert!(
            json.contains(r#""terminal_scheme":"tokyo_night""#),
            "{json}"
        );
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back.terminal_font_size, 17.0);
        assert_eq!(back.terminal_font, TerminalFont::JetBrainsMono);
        assert_eq!(back.terminal_scheme, TerminalScheme::TokyoNight);
        assert_eq!(back.ui_scale, Some(1.25));
    }

    #[test]
    fn terminal_font_size_is_clamped_to_a_usable_range() {
        assert_eq!(clamp_terminal_font_size(14.0), 14.0);
        assert_eq!(clamp_terminal_font_size(0.0), MIN_TERMINAL_FONT_SIZE);
        assert_eq!(clamp_terminal_font_size(-3.0), MIN_TERMINAL_FONT_SIZE);
        assert_eq!(clamp_terminal_font_size(900.0), MAX_TERMINAL_FONT_SIZE);
        assert_eq!(
            clamp_terminal_font_size(f32::NAN),
            DEFAULT_TERMINAL_FONT_SIZE
        );
        assert_eq!(
            clamp_terminal_font_size(f32::INFINITY),
            DEFAULT_TERMINAL_FONT_SIZE
        );
    }

    #[test]
    fn panel_width_is_clamped() {
        assert_eq!(clamp_sidebar_width(300.), 300.);
        assert_eq!(clamp_sidebar_width(10.), MIN_SIDEBAR_WIDTH);
        assert_eq!(clamp_sidebar_width(9e9), MAX_SIDEBAR_WIDTH);
        assert_eq!(clamp_sidebar_width(f32::NAN), DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(clamp_server_panel_width(0.), MIN_SERVER_PANEL_WIDTH);
        assert_eq!(
            clamp_server_panel_width(f32::INFINITY),
            DEFAULT_SERVER_PANEL_WIDTH
        );
        let empty: Preferences = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        assert_eq!(empty.server_panel_width, DEFAULT_SERVER_PANEL_WIDTH);
    }

    #[test]
    fn every_terminal_font_and_scheme_is_listed_once() {
        for (ix, font) in TerminalFont::ALL.iter().enumerate() {
            assert!(
                !TerminalFont::ALL[..ix].contains(font),
                "{font:?} listed twice"
            );
        }
        for (ix, scheme) in TerminalScheme::ALL.iter().enumerate() {
            assert!(
                !TerminalScheme::ALL[..ix].contains(scheme),
                "{scheme:?} listed twice"
            );
        }
    }

    #[test]
    fn available_fonts_keep_the_bundled_one_and_drop_missing_ones() {
        let installed = vec!["Menlo".to_string(), "Helvetica".to_string()];
        assert_eq!(
            TerminalFont::available(&installed),
            vec![TerminalFont::CascadiaCode, TerminalFont::Menlo]
        );
        assert_eq!(
            TerminalFont::available(&[]),
            vec![TerminalFont::CascadiaCode]
        );
    }

    #[test]
    fn load_from_survives_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "not json at all").unwrap();
        assert_eq!(Preferences::load_from(&path).theme, ThemePreference::Dark);
    }

    #[test]
    fn load_from_reads_a_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"theme":"system"}"#).unwrap();
        assert_eq!(Preferences::load_from(&path).theme, ThemePreference::System);
    }
}
