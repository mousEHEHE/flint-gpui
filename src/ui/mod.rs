pub mod app_picker;
pub mod app_view;
pub mod command_palette;
pub mod context_menu;
pub mod create_vault_view;
pub mod feedback_view;
pub mod file_pane;
pub mod file_pane_keys;
pub mod file_preview;
pub mod file_selection;
pub mod forwards_popover;
pub mod host_icon;
pub mod host_sharing;
pub mod import_view;
pub mod keyboard_layout;
pub mod keymap;
pub mod lock_screen;
pub mod mouse_report;
pub mod onboarding_view;
pub mod passphrase_prompt;
pub mod peer_link;
pub mod phrase_panel;
pub mod phrase_view;
pub mod root_view;
pub mod server_form;
pub mod server_panel;
pub mod server_session;
pub mod servers_view;
pub mod settings;
pub mod sftp_browser;
pub mod shortcut_hints;
pub mod sidebar;
pub mod sync_service;
pub mod tasks_popover;
pub mod terminal_element;
pub mod terminal_keymap;
pub mod terminal_link;
pub mod terminal_palette;
pub mod terminal_search;
pub mod terminal_suggestions;
pub mod terminal_tabs;
pub mod terminal_view;
pub mod transfer_conflict;
pub mod transfer_rate;
pub mod unlock_view;
pub mod update_consent_view;
pub mod update_service;
pub mod update_view;
pub mod window_chrome;

/// The chrome every dialog shares.
///
/// gpui-component paints a dialog in `theme.background`, the very colour of
/// the ground behind it, with the faint `border` token and no shadow, so it
/// reads as text floating on the app rather than a surface above it. This
/// swaps in the floating-surface colour, the border meant for surfaces that
/// sit above the chrome, and the four-layer shadow Zed gives its modals.
pub fn dialog_chrome(dialog: gpui_component::dialog::Dialog) -> gpui_component::dialog::Dialog {
    use gpui::prelude::*;

    dialog
        .bg(crate::theme::pop_surface())
        .border_color(crate::theme::float_border())
        .rounded(rems_from_px(12.))
        .shadow(modal_shadow())
}

/// Corner radius of the working area, the card that holds the host list or
/// a session.
pub const PANEL_RADIUS: f32 = 10.;

/// A size that follows the interface scale; equals `px(value)` at 100%.
pub const fn rems_from_px(value: f32) -> gpui::Rems {
    gpui::rems(value / crate::theme::UI_REM_BASE)
}

/// For APIs that take only `Pixels`, such as a dialog's width.
pub fn scaled_px(value: gpui::Pixels, cx: &gpui::App) -> gpui::Pixels {
    value * crate::theme::ui_scale(cx)
}

/// Which side panel a drag resizes. Also the (empty) drag preview.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResizeEdge {
    Sidebar,
    ServerPanel,
}

impl ResizeEdge {
    /// `width` is in real pixels; it is stored in 100% units.
    pub fn set_width(self, width: f32, cx: &mut gpui::App) {
        self.set_unscaled_width(width / crate::theme::ui_scale(cx), cx);
    }

    fn set_unscaled_width(self, width: f32, cx: &mut gpui::App) {
        match self {
            Self::Sidebar => crate::preferences::Preferences::set_sidebar_width(width, cx),
            Self::ServerPanel => crate::preferences::Preferences::set_server_panel_width(width, cx),
        }
    }

    fn reset_width(self, cx: &mut gpui::App) {
        let width = match self {
            Self::Sidebar => crate::preferences::DEFAULT_SIDEBAR_WIDTH,
            Self::ServerPanel => crate::preferences::DEFAULT_SERVER_PANEL_WIDTH,
        };
        self.set_unscaled_width(width, cx);
    }
}

impl gpui::Render for ResizeEdge {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        _: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        gpui::Empty
    }
}

/// A strip on the panel's inner edge. The parent row's `on_drag_move` does the
/// resizing; a double click puts the default width back.
pub fn resize_edge(edge: ResizeEdge) -> gpui::Stateful<gpui::Div> {
    use gpui::{div, prelude::*};

    let group = match edge {
        ResizeEdge::Sidebar => "sidebar-resize",
        ResizeEdge::ServerPanel => "server-panel-resize",
    };
    let line = div()
        .absolute()
        .top_0()
        .h_full()
        .w(rems_from_px(2.))
        .group_hover(group, |s| s.bg(crate::theme::accent()));
    let (strip, line) = match edge {
        ResizeEdge::Sidebar => (div().right_0(), line.right_0()),
        ResizeEdge::ServerPanel => (div().left_0(), line.left_0()),
    };
    strip
        .id(group)
        .group(group)
        .absolute()
        .top_0()
        .h_full()
        .w(rems_from_px(6.))
        .cursor_col_resize()
        .child(line)
        .on_drag(edge, |edge, _, _, cx| cx.new(|_| *edge))
        .on_click(move |event, window, cx| {
            if event.click_count() == 2 {
                edge.reset_width(cx);
                window.refresh();
            }
        })
}

pub const DIALOG_NARROW: gpui::Pixels = gpui::px(400.);
pub const DIALOG_WIDE: gpui::Pixels = gpui::px(480.);
/// The host form: user, host and port share a row, and so do the folder and
/// the tags.
pub const DIALOG_FORM: gpui::Pixels = gpui::px(620.);

/// The four-layer shadow Zed gives its modals, shared by dialogs, the palette
/// and the settings card so they all sit at the same height.
pub fn modal_shadow() -> Vec<gpui::BoxShadow> {
    let is_light = crate::theme::is_light();
    let layer = |y: f32, blur: f32, alpha: f32| gpui::BoxShadow {
        color: gpui::hsla(0., 0., 0., alpha),
        offset: gpui::point(gpui::px(0.), gpui::px(y)),
        blur_radius: gpui::px(blur),
        spread_radius: gpui::px(0.),
        inset: false,
    };
    vec![
        layer(2., 3., if is_light { 0.06 } else { 0.12 }),
        layer(3., 6., if is_light { 0.06 } else { 0.08 }),
        layer(6., 12., 0.04),
        // Contact shadow: unblurred, it draws the panel's own edge.
        layer(1., 0., if is_light { 0.04 } else { 0.12 }),
    ]
}

/// How tall a dialog's content may grow before it has to scroll.
///
/// gpui-component pins a dialog's top at a tenth of the window and gives it no
/// height cap of its own, so tall content just runs off the bottom, taking the
/// confirm buttons with it. This leaves room for that top offset, the dialog's
/// own 16px padding on each side and a matching gap at the bottom.
pub fn dialog_max_height(window: &gpui::Window) -> gpui::Pixels {
    let viewport = window.viewport_size().height;
    (viewport * 0.9 - gpui::px(48.)).max(gpui::px(200.))
}

/// `user@host`, with the port only when it is not 22.
pub fn ssh_address(username: &str, host: &str, port: u16) -> String {
    let host = bracket_ipv6(host);
    if port == 22 {
        format!("{username}@{host}")
    } else {
        format!("{username}@{host}:{port}")
    }
}

/// An IPv6 host needs brackets, or its colons read as the port separator.
pub fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

pub enum VaultState {
    Locked,
    AwaitingPhraseConfirmation {
        manager: crate::vault::VaultManager,
        phrase: crate::vault::RecoveryPhrase,
        is_rotation: bool,
    },
    Unlocked {
        manager: crate::vault::VaultManager,
    },
}

impl VaultState {
    pub fn manager(&self) -> Option<&crate::vault::VaultManager> {
        match self {
            VaultState::Unlocked { manager } => Some(manager),
            _ => None,
        }
    }

    pub fn manager_mut(&mut self) -> Option<&mut crate::vault::VaultManager> {
        match self {
            VaultState::Unlocked { manager } => Some(manager),
            _ => None,
        }
    }

    /// Go from `Unlocked` back to `Locked`, wiping the DEK and dropping the
    /// decrypted records. Any other state is left alone: a recovery phrase
    /// still waiting to be confirmed must not be thrown away.
    pub fn lock(&mut self) {
        if !matches!(self, VaultState::Unlocked { .. }) {
            return;
        }
        if let VaultState::Unlocked { manager } = std::mem::replace(self, VaultState::Locked) {
            manager.lock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::format::KdfParams;
    use crate::vault::{RecoveryPhrase, VaultManager};
    use secrecy::SecretString;

    fn new_vault(dir: &std::path::Path) -> (VaultManager, RecoveryPhrase) {
        VaultManager::create_with_params(
            &dir.join("vault.bin"),
            &SecretString::new("hunter2".to_string()),
            KdfParams {
                m_cost: 256,
                t_cost: 1,
                p_cost: 1,
            },
        )
        .unwrap()
    }

    #[test]
    fn ssh_address_brackets_ipv6_and_hides_port_22() {
        assert_eq!(ssh_address("root", "example.com", 22), "root@example.com");
        assert_eq!(
            ssh_address("root", "example.com", 2222),
            "root@example.com:2222"
        );
        assert_eq!(ssh_address("root", "::1", 22), "root@[::1]");
        assert_eq!(ssh_address("root", "fe80::1", 2222), "root@[fe80::1]:2222");
        assert_eq!(
            ssh_address("root", "[fe80::1]", 2222),
            "root@[fe80::1]:2222"
        );
    }

    #[test]
    fn locking_an_unlocked_vault_drops_the_manager() {
        let dir = tempfile::tempdir().unwrap();
        let (manager, _) = new_vault(dir.path());
        let mut state = VaultState::Unlocked { manager };

        state.lock();

        assert!(matches!(state, VaultState::Locked));
        assert!(state.manager().is_none());
    }

    #[test]
    fn locking_keeps_a_recovery_phrase_that_is_not_confirmed_yet() {
        let dir = tempfile::tempdir().unwrap();
        let (manager, phrase) = new_vault(dir.path());
        let mut state = VaultState::AwaitingPhraseConfirmation {
            manager,
            phrase,
            is_rotation: false,
        };

        state.lock();

        assert!(matches!(
            state,
            VaultState::AwaitingPhraseConfirmation { .. }
        ));
    }

    #[test]
    fn locking_a_locked_vault_changes_nothing() {
        let mut state = VaultState::Locked;
        state.lock();
        assert!(matches!(state, VaultState::Locked));
    }
}
