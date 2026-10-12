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

/// How long the font size must stay unchanged before it is written to disk.
/// Holding the zoom shortcut repeats many times a second.
const FONT_SIZE_SAVE_DELAY: Duration = Duration::from_millis(400);

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

    pub fn caption(self) -> &'static str {
        match self {
            ThemePreference::Dark => "The Andromeda palette.",
            ThemePreference::Light => "A bright palette for daylight.",
            ThemePreference::System => "Follow the system appearance.",
        }
    }
}

/// The terminal font. A fixed list keeps `Preferences` a `Copy` value and
/// keeps proportional fonts, which break the cell grid, out of the terminal.
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

/// Every field carries a serde default so a file written by an older build
/// still loads.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default)]
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
    #[serde(default)]
    pub terminal_font: TerminalFont,
    #[serde(default)]
    pub terminal_scheme: TerminalScheme,
    /// Ask the server now and then whether a newer build is out. Off means
    /// the app makes no such request at all.
    #[serde(default = "default_true")]
    pub is_update_check_enabled: bool,
}

/// Whether a preferences file that does not load as a whole still says that
/// the update check is off. One value this build does not know must not turn
/// back on a request the user turned off.
fn is_update_check_off(data: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(data)
        .is_ok_and(|file| file["is_update_check_enabled"] == false)
}

fn default_true() -> bool {
    true
}

fn default_terminal_font_size() -> f32 {
    DEFAULT_TERMINAL_FONT_SIZE
}

/// A size the terminal can lay out. The file is hand-editable, so the stored
/// number can be anything, including one that is not a number.
fn clamp_terminal_font_size(size: f32) -> f32 {
    if !size.is_finite() {
        return DEFAULT_TERMINAL_FONT_SIZE;
    }
    size.clamp(MIN_TERMINAL_FONT_SIZE, MAX_TERMINAL_FONT_SIZE)
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
            is_update_check_enabled: true,
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

    /// Whether the app may ask for updates. True when the preferences were
    /// never loaded, which matches the default.
    pub fn is_update_check_enabled(cx: &gpui::App) -> bool {
        cx.try_global::<Self>()
            .is_none_or(|prefs| prefs.is_update_check_enabled)
    }

    pub fn terminal_font_size(cx: &gpui::App) -> f32 {
        cx.try_global::<Self>()
            .map_or(DEFAULT_TERMINAL_FONT_SIZE, |prefs| {
                clamp_terminal_font_size(prefs.terminal_font_size)
            })
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

    /// Change the preferences and write them to disk. The change has already
    /// taken effect in memory, so a failed write is logged and not surfaced.
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

        cx.spawn(async move |cx| {
            cx.background_executor().timer(FONT_SIZE_SAVE_DELAY).await;
            cx.update(|cx| {
                let Some(prefs) = cx.try_global::<Self>() else {
                    return;
                };
                // A later change started its own timer and will do the write.
                if prefs.terminal_font_size != size {
                    return;
                }
                // Written on the main thread like every other preference. A
                // copy written from another thread could land after a newer
                // save and put an old value back.
                if let Err(e) = prefs.save() {
                    tracing::warn!(?e, "failed to save terminal font size");
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
        match serde_json::from_slice::<Self>(&data) {
            Ok(prefs) => prefs,
            Err(e) => {
                tracing::warn!(?e, "invalid preferences.json, using defaults");
                Self {
                    is_update_check_enabled: !is_update_check_off(&data),
                    ..Self::default()
                }
            }
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
    fn the_update_check_defaults_to_on_and_round_trips() {
        assert!(Preferences::default().is_update_check_enabled);
        // A file from before the field existed must not turn the check off.
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert!(old.is_update_check_enabled);

        let off = Preferences {
            is_update_check_enabled: false,
            ..Preferences::default()
        };
        let json = serde_json::to_string(&off).unwrap();
        let back: Preferences = serde_json::from_str(&json).unwrap();
        assert!(!back.is_update_check_enabled);
    }

    #[test]
    fn the_update_check_stays_off_when_the_rest_of_the_file_does_not_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            Preferences::load_from(&path)
        };

        // A value written by a newer build.
        let off = load(r#"{"terminal_scheme":"from-the-future","is_update_check_enabled":false}"#);
        assert!(!off.is_update_check_enabled);
        assert_eq!(off.terminal_font_size, DEFAULT_TERMINAL_FONT_SIZE);

        assert!(load(r#"{"terminal_scheme":"from-the-future"}"#).is_update_check_enabled);
        assert!(
            load(r#"{"terminal_scheme":7,"is_update_check_enabled":true}"#).is_update_check_enabled
        );
        assert!(load("not json").is_update_check_enabled);
    }

    #[test]
    fn saving_replaces_the_file_and_leaves_nothing_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Flint").join("preferences.json");
        let off = Preferences {
            is_update_check_enabled: false,
            ..Preferences::default()
        };

        Preferences::default().save_to(&path).unwrap();
        off.save_to(&path).unwrap();

        assert!(!Preferences::load_from(&path).is_update_check_enabled);
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
    fn terminal_settings_default_when_missing_and_round_trip() {
        let old: Preferences = serde_json::from_str(r#"{"theme":"light"}"#).unwrap();
        assert_eq!(old.terminal_font_size, DEFAULT_TERMINAL_FONT_SIZE);
        assert_eq!(old.terminal_font, TerminalFont::CascadiaCode);
        assert_eq!(old.terminal_scheme, TerminalScheme::FollowTheme);

        let changed = Preferences {
            terminal_font_size: 17.0,
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
