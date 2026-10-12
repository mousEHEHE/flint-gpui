mod sync_pane;
mod teams_pane;
mod vault_pane;
pub mod view;

pub(crate) use view::set_theme_preference;
pub use view::{SettingsView, SettingsViewEvent};

use gpui::App;
use gpui_component::{Icon, IconName};

pub fn init(cx: &mut App) {
    view::init(cx);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Vault,
    Sync,
    Teams,
    Appearance,
    Terminal,
    Keys,
    Snippets,
    Network,
    About,
}

impl SettingsSection {
    pub fn label(self) -> &'static str {
        match self {
            SettingsSection::Vault => "Vault",
            SettingsSection::Sync => "Sync",
            SettingsSection::Teams => "Teams",
            SettingsSection::Appearance => "Appearance",
            SettingsSection::Terminal => "Terminal",
            SettingsSection::Keys => "SSH Keys",
            SettingsSection::Snippets => "Snippets",
            SettingsSection::Network => "Proxy",
            SettingsSection::About => "About",
        }
    }

    pub fn icon(self) -> Icon {
        let name = match self {
            SettingsSection::Vault => return Icon::empty().path("icons/lock.svg"),
            SettingsSection::Sync => IconName::Globe,
            SettingsSection::Teams => IconName::User,
            SettingsSection::Appearance => IconName::Palette,
            SettingsSection::Terminal => IconName::PanelBottom,
            SettingsSection::Keys => return Icon::empty().path("icons/key-round.svg"),
            SettingsSection::Snippets => IconName::SquareTerminal,
            SettingsSection::Network => IconName::Network,
            SettingsSection::About => IconName::Info,
        };
        Icon::new(name)
    }

    pub fn all() -> &'static [SettingsSection] {
        &[
            SettingsSection::Vault,
            SettingsSection::Sync,
            SettingsSection::Teams,
            SettingsSection::Appearance,
            SettingsSection::Terminal,
            SettingsSection::Keys,
            SettingsSection::Snippets,
            SettingsSection::Network,
            SettingsSection::About,
        ]
    }
}
