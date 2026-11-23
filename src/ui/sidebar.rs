use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    div, px, relative, Anchor, AnyElement, Context, Entity, EventEmitter, MouseButton,
    MouseDownEvent, SharedString, Window,
};
use gpui_component::{h_flex, tooltip::Tooltip, v_flex, ActiveTheme as _, Icon, IconName};
use uuid::Uuid;

use crate::preferences::{HostSubtitle, Preferences};
use crate::theme as flint_theme;
use crate::ui::app_view::ToggleSettings;
use crate::ui::context_menu::{ContextMenuItem, PopoverMenu};
use crate::ui::host_sharing;
use crate::ui::rems_from_px;
use crate::ui::shortcut_hints;
use crate::ui::VaultState;
use crate::vault::ServerEntrySummary;

const ROW_HEIGHT: f32 = 28.;
const COMPACT_ROW_HEIGHT: f32 = 24.;
const SIDEBAR_GROUP: &str = "sidebar-host-row";

/// Live connection state of a host that has an open session. Hosts without an
/// open session are absent from the list and rendered as [`HostStatus::Offline`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostStatus {
    /// No open session.
    Offline,
    /// Session opened, SSH handshake in progress.
    Connecting,
    /// Live SSH connection.
    Connected,
    /// Session open but the connection dropped.
    Disconnected,
}

/// The start of a local terminal session's id; see `ServerSession::start_local`.
pub const LOCAL_SESSION_PREFIX: &str = "local-";

pub enum SidebarEvent {
    OpenServer(Uuid),
    CloseServer(Uuid),
    AddServerRequested,
    ShowAllServers,
    EditServerRequested(Uuid),
    DeleteServerRequested(Uuid),
    SettingsRequested,
    FeedbackRequested,
    ExportKeyRequested {
        host: Uuid,
        key: Uuid,
    },
    /// A tab that is not a saved host: a local terminal. Carries its session id.
    ShowTab(String),
    CloseTab(String),
}

pub struct Sidebar {
    vault_state: Entity<VaultState>,
    active_server_id: Option<String>,
    /// Open sessions in tab order. A `Vec`, not a map, so rows don't jump as
    /// sessions come and go.
    sessions: Vec<(String, HostStatus)>,
    /// Which host's "more" menu is open (at most one).
    open_menu_for: Option<Uuid>,
    /// The menu the last outside click closed, and when. `on_mouse_down_out`
    /// runs before the button's own handler and clears `open_menu_for`, so
    /// without this a click on the button of an open menu would reopen it.
    menu_dismissed_at: Option<(Uuid, Instant)>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(
        vault_state: Entity<VaultState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&vault_state, |_, _, cx| cx.notify()).detach();
        cx.observe_global::<Preferences>(|_, cx| cx.notify())
            .detach();
        Self {
            vault_state,
            active_server_id: None,
            sessions: Vec::new(),
            open_menu_for: None,
            menu_dismissed_at: None,
        }
    }

    pub fn set_active_server(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.active_server_id = id;
        cx.notify();
    }

    /// Replace the ordered list of open sessions. The caller (app_view) owns
    /// tab order, so the OPEN zone mirrors it exactly.
    pub fn set_sessions(&mut self, sessions: Vec<(String, HostStatus)>, cx: &mut Context<Self>) {
        if self.sessions != sessions {
            self.sessions = sessions;
            cx.notify();
        }
    }

    /// One host row. `is_dimmed` rows (closed hosts) are dimmed. `show_close`
    /// adds a disconnect button, for open sessions only.
    #[allow(clippy::too_many_arguments)]
    fn host_row(
        &self,
        ui_idx: usize,
        entry: &ServerEntrySummary,
        subtitle: SharedString,
        status: HostStatus,
        is_dimmed: bool,
        show_close: bool,
        shortcut: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // While the shortcut hints show, the tab number takes the place of the
        // subtitle and the row controls.
        let hint = shortcut.filter(|_| shortcut_hints::is_shown(cx));
        let theme = cx.theme();
        let bg_element_hover = theme.list_hover;
        let bg_element_active = theme.list_active;
        let text = theme.foreground;
        let text_muted = theme.muted_foreground;
        let text_dim = flint_theme::text_dim();
        let row_hover = flint_theme::field();
        let row_selected = flint_theme::element();
        let status_online = flint_theme::status_online();
        let status_connecting = theme.warning;
        let status_down = theme.danger;

        let entry_id = entry.id;
        // An imported host can have an empty label; its address stands in.
        let label: SharedString = if entry.label.is_empty() {
            entry.host.clone()
        } else {
            entry.label.clone()
        }
        .into();
        let team_name = entry.team_name.clone();
        let entry_id_str = entry_id.to_string();
        let is_active = self.active_server_id.as_deref() == Some(entry_id_str.as_str());
        let group_id: SharedString = format!("{SIDEBAR_GROUP}-{ui_idx}").into();

        let label_color = if is_dimmed { text_muted } else { text };

        let dot_color = match status {
            HostStatus::Connected => status_online,
            HostStatus::Connecting => status_connecting,
            HostStatus::Disconnected => status_down,
            HostStatus::Offline => flint_theme::status_off(),
        };
        let dot = div()
            .size(rems_from_px(6.))
            .flex_none()
            .rounded_full()
            .bg(dot_color);

        h_flex()
            .id(("sidebar-host", ui_idx))
            .group(group_id.clone())
            .relative()
            .h(rems_from_px(row_height(cx)))
            .gap(rems_from_px(8.))
            .px(rems_from_px(8.))
            .rounded(rems_from_px(6.))
            .items_center()
            .cursor_pointer()
            .when(is_active, |el| el.bg(row_selected))
            .when(!is_active, |el| {
                el.hover(|s| s.bg(row_hover))
                    .active(|s| s.bg(bg_element_active))
            })
            .child(dot)
            // The name, and right after it the mark of a host shared with a
            // team. The mark stays with the name: further right it would hang
            // in the space the subtitle leaves empty while the row is hovered.
            // The row is too narrow for the team's name, so that is a tooltip.
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(rems_from_px(4.))
                    .items_center()
                    .child(
                        div()
                            .min_w_0()
                            .text_color(label_color)
                            .text_size(rems_from_px(13.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .truncate()
                            .child(label),
                    )
                    .children(team_name.map(|team| {
                        div()
                            .id(("sidebar-host-team", ui_idx))
                            .flex_none()
                            .text_color(text_dim)
                            .tooltip(move |window, cx| {
                                Tooltip::new(format!("Shared with {team}")).build(window, cx)
                            })
                            .child(Icon::new(IconName::User).size(rems_from_px(10.)))
                    })),
            )
            // Trailing slot: the subtitle normally, the row controls on hover.
            // The controls sit on top so the two never fight for width.
            .child(
                div()
                    .flex_none()
                    .text_color(text_dim)
                    .text_size(rems_from_px(11.5))
                    .max_w(relative(0.5))
                    .truncate()
                    .group_hover(group_id.clone(), |s| s.invisible())
                    .when(is_active || hint.is_some(), |el| el.invisible())
                    .child(subtitle),
            )
            .child(
                h_flex()
                    .absolute()
                    .right(rems_from_px(4.))
                    .flex_none()
                    .gap(px(1.))
                    .items_center()
                    .when_some(hint.clone(), |row, keys| {
                        row.child(shortcut_hints::badge(keys))
                    })
                    // Star, close, more: the menu stays at the edge, and the
                    // harmless toggle sits furthest from it.
                    .when(hint.is_none(), |row| {
                        row.child(self.host_pin_button(ui_idx, entry_id, is_active, cx))
                    })
                    .when(show_close && hint.is_none(), |row| {
                        row.child(
                            div()
                                .id(("sidebar-host-close", ui_idx))
                                .size(rems_from_px(16.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(rems_from_px(3.))
                                .text_color(text_dim)
                                .invisible()
                                .when(is_active, |el| el.visible())
                                .group_hover(group_id.clone(), |s| s.visible())
                                .hover(|s| s.bg(bg_element_hover).text_color(text))
                                .active(|s| {
                                    s.bg(bg_element_active).text_color(flint_theme::red_error())
                                })
                                .tooltip(|window, cx| {
                                    Tooltip::new("Close session").build(window, cx)
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(SidebarEvent::CloseServer(entry_id));
                                    }),
                                )
                                .child(Icon::new(IconName::Close).size(rems_from_px(10.))),
                        )
                    })
                    .when(hint.is_none(), |row| {
                        row.child(self.host_more_menu(ui_idx, entry_id, show_close, cx))
                    }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_menu_for = Some(entry_id);
                    cx.notify();
                }),
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::OpenServer(entry_id));
            }))
            .into_any_element()
    }

    /// The row of a local terminal tab: no host behind it, so no menu.
    fn local_row(
        &self,
        ui_idx: usize,
        session_id: String,
        shortcut: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hint = shortcut.filter(|_| shortcut_hints::is_shown(cx));
        let theme = cx.theme();
        let (bg_element_hover, bg_element_active, text) =
            (theme.list_hover, theme.list_active, theme.foreground);
        let text_dim = flint_theme::text_dim();
        let is_active = self.active_server_id.as_deref() == Some(session_id.as_str());
        let group_id: SharedString = format!("{SIDEBAR_GROUP}-{ui_idx}").into();
        let (show_id, close_id) = (session_id.clone(), session_id);

        h_flex()
            .id(("sidebar-local", ui_idx))
            .group(group_id.clone())
            .relative()
            .h(rems_from_px(row_height(cx)))
            .gap(rems_from_px(8.))
            .px(rems_from_px(8.))
            .rounded(rems_from_px(6.))
            .items_center()
            .cursor_pointer()
            .when(is_active, |el| el.bg(flint_theme::element()))
            .when(!is_active, |el| {
                el.hover(|s| s.bg(flint_theme::field()))
                    .active(|s| s.bg(bg_element_active))
            })
            .child(
                Icon::new(IconName::SquareTerminal)
                    .size(rems_from_px(12.))
                    .text_color(text_dim),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(rems_from_px(13.))
                    .text_color(text)
                    .truncate()
                    .child("Local terminal"),
            )
            .child(
                h_flex()
                    .absolute()
                    .right(rems_from_px(4.))
                    .gap(px(1.))
                    .items_center()
                    .when_some(hint.clone(), |row, keys| {
                        row.child(shortcut_hints::badge(keys))
                    })
                    .when(hint.is_none(), |row| {
                        row.child(
                            div()
                                .id(("sidebar-local-close", ui_idx))
                                .size(rems_from_px(16.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(rems_from_px(3.))
                                .text_color(text_dim)
                                .invisible()
                                .when(is_active, |el| el.visible())
                                .group_hover(group_id, |s| s.visible())
                                .hover(|s| s.bg(bg_element_hover).text_color(text))
                                .tooltip(|window, cx| {
                                    Tooltip::new("Close terminal").build(window, cx)
                                })
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(SidebarEvent::CloseTab(close_id.clone()));
                                    }),
                                )
                                .child(Icon::new(IconName::Close).size(rems_from_px(10.))),
                        )
                    }),
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::ShowTab(show_id.clone()));
            }))
            .into_any_element()
    }

    /// Pin or unpin from the row. The star is filled on a pinned host.
    fn host_pin_button(
        &self,
        ui_idx: usize,
        entry_id: Uuid,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_pinned = Preferences::is_pinned(entry_id, cx);
        let text = cx.theme().foreground;
        let bg_hover = cx.theme().list_hover;
        let group_id: SharedString = format!("{SIDEBAR_GROUP}-{ui_idx}").into();
        let (icon, tip) = if is_pinned {
            ("icons/star-filled.svg", "Unpin")
        } else {
            ("icons/star.svg", "Pin")
        };
        div()
            .id(("sidebar-host-pin", ui_idx))
            .size(rems_from_px(16.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(rems_from_px(3.))
            .text_color(flint_theme::text_dim())
            .invisible()
            .when(is_active, |el| el.visible())
            .group_hover(group_id, |s| s.visible())
            .hover(move |s| s.bg(bg_hover).text_color(text))
            .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |_, _: &MouseDownEvent, _, cx| {
                    // The row under it would open the host.
                    cx.stop_propagation();
                    Preferences::set_pinned(entry_id, !is_pinned, cx);
                    cx.notify();
                }),
            )
            .child(Icon::empty().path(icon).size(rems_from_px(11.)))
    }

    /// The "more" trigger and its dropdown for a host row. The [`PopoverMenu`]
    /// drops below the trigger and never covers it, so a second click on the
    /// trigger closes the menu.
    fn host_more_menu(
        &self,
        ui_idx: usize,
        entry_id: Uuid,
        has_session: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let bg_element_hover = theme.list_hover;
        let bg_element_active = theme.list_active;
        let text = theme.foreground;
        let text_dim = flint_theme::text_dim();

        let is_menu_open = self.open_menu_for == Some(entry_id);
        let is_pinned = Preferences::is_pinned(entry_id, cx);
        let is_active = self.active_server_id.as_deref() == Some(entry_id.to_string().as_str());
        let group_id: SharedString = format!("{SIDEBAR_GROUP}-{ui_idx}").into();

        // The trigger button. Open/pressed both get a solid highlight so the
        // control clearly reacts; the row owns hover/visibility reveal.
        let trigger = div()
            .id(("sidebar-host-more-btn", ui_idx))
            .size(rems_from_px(16.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(rems_from_px(3.))
            .text_color(if is_menu_open { text } else { text_dim })
            .when(is_menu_open, |el| el.bg(bg_element_active).visible())
            .when(!is_menu_open, |el| {
                el.invisible()
                    .when(is_active, |el| el.visible())
                    .group_hover(group_id.clone(), |s| s.visible())
                    .hover(|s| s.bg(bg_element_hover).text_color(text))
                    // Pressed feedback before the menu paints.
                    .active(|s| s.bg(bg_element_active).text_color(text))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    // The menu's outside-click dismiss fires in the capture phase,
                    // before this bubble-phase handler, and clears `open_menu_for`.
                    // If it just closed *this* row's menu, treat the click as a
                    // toggle-off and stay closed; otherwise open this row's menu
                    // (covers both first-open and switching between rows).
                    let toggled_off = this.menu_dismissed_at.take().is_some_and(|(id, at)| {
                        id == entry_id && at.elapsed() < Duration::from_millis(200)
                    });
                    if toggled_off || this.open_menu_for == Some(entry_id) {
                        this.open_menu_for = None;
                    } else {
                        this.open_menu_for = Some(entry_id);
                    }
                    cx.notify();
                }),
            )
            .child(Icon::new(IconName::Ellipsis).size(rems_from_px(12.)));

        let sharing_items: Vec<ContextMenuItem> =
            host_sharing::menu_choices(&self.vault_state, entry_id, cx)
                .into_iter()
                .enumerate()
                .map(|(choice_ix, (label, target))| {
                    ContextMenuItem::new(
                        ("sidebar-menu-share", choice_ix),
                        label,
                        cx.listener(move |this, _, _, cx| {
                            this.open_menu_for = None;
                            host_sharing::set_shared(&this.vault_state, entry_id, target, cx);
                            cx.notify();
                        }),
                    )
                })
                .collect();

        let export_key = host_sharing::export_key_item(
            "sidebar-menu-export-key",
            &self.vault_state,
            cx,
            |key| {
                Box::new(cx.listener(move |this, _, _, cx| {
                    this.open_menu_for = None;
                    cx.emit(SidebarEvent::ExportKeyRequested {
                        host: entry_id,
                        key,
                    });
                    cx.notify();
                }))
            },
        );

        let mut menu = PopoverMenu::new(("sidebar-host-more", ui_idx), is_menu_open, trigger)
            .anchor(Anchor::TopRight)
            .item(
                ContextMenuItem::new(
                    "sidebar-menu-edit",
                    "Edit",
                    cx.listener(move |this, _, _, cx| {
                        this.open_menu_for = None;
                        cx.emit(SidebarEvent::EditServerRequested(entry_id));
                        cx.notify();
                    }),
                )
                .icon("icons/pen-line.svg"),
            )
            .item(
                ContextMenuItem::new(
                    "sidebar-menu-pin",
                    if is_pinned { "Unpin" } else { "Pin" },
                    cx.listener(move |this, _, _, cx| {
                        this.open_menu_for = None;
                        Preferences::set_pinned(entry_id, !is_pinned, cx);
                        cx.notify();
                    }),
                )
                .icon("icons/star.svg"),
            );

        if has_session {
            menu = menu.item(
                ContextMenuItem::new(
                    "sidebar-menu-close",
                    "Close session",
                    cx.listener(move |this, _, _, cx| {
                        this.open_menu_for = None;
                        cx.emit(SidebarEvent::CloseServer(entry_id));
                        cx.notify();
                    }),
                )
                .icon("icons/x.svg"),
            );
        }

        menu.items(export_key.map(|item| item.icon("icons/key-round.svg")))
            .items(sharing_items)
            .item(
                ContextMenuItem::new(
                    "sidebar-menu-delete",
                    "Delete",
                    cx.listener(move |this, _, _, cx| {
                        this.open_menu_for = None;
                        cx.emit(SidebarEvent::DeleteServerRequested(entry_id));
                        cx.notify();
                    }),
                )
                .icon("icons/trash.svg")
                .danger(),
            )
            .on_dismiss(cx.listener(move |this, _, _, cx| {
                // Record which menu we just closed so the trigger's bubble-phase
                // handler can tell a toggle-off from a row switch (see trigger).
                this.menu_dismissed_at = Some((entry_id, Instant::now()));
                this.open_menu_for = None;
                cx.notify();
            }))
    }
}

/// Small heading over a group of sidebar rows.
fn section_label(text: &'static str) -> AnyElement {
    div()
        .flex_none()
        .h(rems_from_px(26.))
        .px(rems_from_px(8.))
        .pb(rems_from_px(4.))
        .flex()
        .items_end()
        .text_size(rems_from_px(11.5))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(flint_theme::text_dim())
        .child(text)
        .into_any_element()
}

impl Sidebar {
    /// The way back to the full list of hosts. Selected while no session is
    /// in front.
    fn all_hosts_row(&self, host_count: usize, cx: &mut Context<Self>) -> AnyElement {
        let is_active = self.active_server_id.is_none();
        let text = cx.theme().foreground;
        let icon_color = if is_active {
            flint_theme::accent()
        } else {
            cx.theme().muted_foreground
        };
        h_flex()
            .id("sidebar-all-hosts")
            .h(rems_from_px(row_height(cx)))
            .gap(rems_from_px(8.))
            .px(rems_from_px(8.))
            .rounded(rems_from_px(6.))
            .items_center()
            .cursor_pointer()
            .text_size(rems_from_px(13.))
            .text_color(text)
            .when(is_active, |el| el.bg(flint_theme::element()))
            .when(!is_active, |el| {
                el.hover(|s| s.bg(flint_theme::field()))
                    .active(|s| s.bg(flint_theme::element_active()))
            })
            .child(
                Icon::empty()
                    .path("icons/servers.svg")
                    .size(rems_from_px(14.))
                    .text_color(icon_color),
            )
            .child(div().flex_1().child("All hosts"))
            // On the selected fill the dim colour falls below AA contrast.
            .child(
                div()
                    .text_size(rems_from_px(11.5))
                    .text_color(if is_active {
                        cx.theme().muted_foreground
                    } else {
                        flint_theme::text_dim()
                    })
                    .child(host_count.to_string()),
            )
            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::ShowAllServers)))
            .into_any_element()
    }

    /// A square, quiet icon button for the footer. With `action` the tooltip
    /// also shows that action's shortcut.
    fn footer_icon_button(
        &self,
        id: &'static str,
        icon: Icon,
        tooltip: &'static str,
        action: Option<&'static dyn gpui::Action>,
        hint: Option<SharedString>,
        event: fn() -> SidebarEvent,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hint = hint.filter(|_| shortcut_hints::is_shown(cx));
        let muted = cx.theme().muted_foreground;
        let text = cx.theme().foreground;
        div()
            .id(id)
            .relative()
            .flex_none()
            .size(rems_from_px(30.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(rems_from_px(7.))
            .text_color(muted)
            .cursor_pointer()
            .hover(move |s| s.bg(flint_theme::field()).text_color(text))
            .active(|s| s.bg(flint_theme::element_active()))
            .tooltip(move |window, cx| {
                let tip = Tooltip::new(tooltip);
                match action {
                    Some(action) => tip.action(action, None),
                    None => tip,
                }
                .build(window, cx)
            })
            .child(icon.size(rems_from_px(15.)))
            .when_some(hint, |el, keys| {
                el.child(shortcut_hints::corner_badge(keys))
            })
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(event())))
    }
}

impl Sidebar {
    /// "All hosts", then the Pinned, Open and Recent sections.
    fn host_list(&self, entries: &[ServerEntrySummary], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let recent_limit = if Preferences::is_recent_hosts_shown(cx) {
            Preferences::recent_hosts_limit(cx)
        } else {
            0
        };
        let pinned_ids = Preferences::pinned_hosts(cx);
        let rows = split_rows(entries, &self.sessions, &pinned_ids, recent_limit);
        // Cmd+1 to Cmd+9 count the open rows from the top, as tab switching does.
        let session_ids: Vec<&str> = self.sessions.iter().map(|(id, _)| id.as_str()).collect();
        let shown_pins: Vec<Uuid> = rows.pinned.iter().map(|(e, _)| e.id).collect();
        let order = tab_order(&session_ids, &shown_pins);
        let tab_key = |tab_index: usize, cx: &gpui::App| {
            let position = order.iter().position(|ix| *ix == tab_index)?;
            shortcut_hints::tab_key(position, cx)
        };
        let subtitle_mode = Preferences::host_subtitle(cx);
        let now = unix_now();
        let by_id: HashMap<String, &ServerEntrySummary> =
            entries.iter().map(|e| (e.id.to_string(), e)).collect();

        let mut content: Vec<AnyElement> = vec![self.all_hosts_row(entries.len(), cx)];
        let mut ui_idx = 0usize;

        if !rows.pinned.is_empty() {
            content.push(section_label("Pinned"));
        }
        for (e, session) in &rows.pinned {
            let row = match session {
                Some((tab_index, status)) => {
                    let sub = open_subtitle(subtitle_mode, e, *status, now);
                    let shortcut = tab_key(*tab_index, cx);
                    self.host_row(ui_idx, e, sub, *status, false, true, shortcut, cx)
                }
                None => {
                    let sub = idle_subtitle(subtitle_mode, e, false, now).into();
                    self.host_row(ui_idx, e, sub, HostStatus::Offline, true, false, None, cx)
                }
            };
            content.push(row);
            ui_idx += 1;
        }

        if !rows.open.is_empty() {
            content.push(section_label("Open"));
        }
        for (tab_index, id, status) in &rows.open {
            let shortcut = tab_key(*tab_index, cx);
            if id.starts_with(LOCAL_SESSION_PREFIX) {
                content.push(self.local_row(ui_idx, id.to_string(), shortcut, cx));
                ui_idx += 1;
                continue;
            }
            let Some(e) = by_id.get(*id) else { continue };
            let sub = open_subtitle(subtitle_mode, e, *status, now);
            content.push(self.host_row(ui_idx, e, sub, *status, false, true, shortcut, cx));
            ui_idx += 1;
        }

        if !rows.recent.is_empty() {
            content.push(section_label("Recent"));
        }
        for e in &rows.recent {
            let sub = idle_subtitle(subtitle_mode, e, false, now).into();
            content.push(self.host_row(ui_idx, e, sub, HostStatus::Offline, true, false, None, cx));
            ui_idx += 1;
        }

        // Nothing to list: a quiet hint, never the full catalog.
        let is_empty = rows.pinned.is_empty() && rows.open.is_empty() && rows.recent.is_empty();
        if is_empty && !entries.is_empty() {
            let hint = if recent_limit > 0 {
                "No recent hosts"
            } else {
                "No open hosts"
            };
            content.push(
                div()
                    .h(rems_from_px(row_height(cx)))
                    .px(rems_from_px(8.))
                    .mt(rems_from_px(6.))
                    .flex()
                    .items_center()
                    .text_size(rems_from_px(12.))
                    .text_color(flint_theme::text_dim())
                    .child(hint)
                    .into_any_element(),
            );
        }
        content
    }
}

fn row_height(cx: &gpui::App) -> f32 {
    if Preferences::is_sidebar_compact(cx) {
        COMPACT_ROW_HEIGHT
    } else {
        ROW_HEIGHT
    }
}

/// Tab indexes from the top of the sidebar down: open pinned hosts in pin
/// order, then the other open tabs in tab order. Tab switching and Cmd+1 to
/// Cmd+9 follow it, so "next" always goes down the list.
pub(crate) fn tab_order(session_ids: &[&str], pinned_ids: &[Uuid]) -> Vec<usize> {
    let pinned: Vec<String> = pinned_ids.iter().map(Uuid::to_string).collect();
    let is_pinned = |id: &str| pinned.iter().any(|pin| pin == id);
    let mut order: Vec<usize> = pinned
        .iter()
        .filter_map(|pin| session_ids.iter().position(|id| id == pin))
        .collect();
    order.extend((0..session_ids.len()).filter(|ix| !is_pinned(session_ids[*ix])));
    order
}

/// The sidebar's sections. A pinned host that is open is listed once, under
/// Pinned, and keeps its tab index for the Cmd+N hint.
struct SidebarRows<'a> {
    /// Each pinned host with its tab index and status when it is open.
    pinned: Vec<(&'a ServerEntrySummary, Option<(usize, HostStatus)>)>,
    /// Open sessions that are not pinned, with their tab index.
    open: Vec<(usize, &'a str, HostStatus)>,
    recent: Vec<&'a ServerEntrySummary>,
}

fn split_rows<'a>(
    entries: &'a [ServerEntrySummary],
    sessions: &'a [(String, HostStatus)],
    pinned_ids: &[Uuid],
    recent_limit: usize,
) -> SidebarRows<'a> {
    let tab_of = |id: &str| {
        sessions
            .iter()
            .position(|(session_id, _)| session_id == id)
            .map(|ix| (ix, sessions[ix].1))
    };
    // Pins of deleted hosts drop out here.
    let pinned: Vec<_> = pinned_ids
        .iter()
        .filter_map(|id| entries.iter().find(|e| e.id == *id))
        .map(|e| (e, tab_of(&e.id.to_string())))
        .collect();
    let pinned_set: HashSet<String> = pinned.iter().map(|(e, _)| e.id.to_string()).collect();

    let open = sessions
        .iter()
        .enumerate()
        .filter(|(_, (id, _))| !pinned_set.contains(id))
        .map(|(ix, (id, status))| (ix, id.as_str(), *status))
        .collect();

    let open_ids: HashSet<&str> = sessions.iter().map(|(id, _)| id.as_str()).collect();
    let mut recent: Vec<&ServerEntrySummary> = entries
        .iter()
        .filter(|e| {
            let id = e.id.to_string();
            e.last_opened_at.is_some()
                && !open_ids.contains(id.as_str())
                && !pinned_set.contains(&id)
        })
        .collect();
    recent.sort_by_key(|e| std::cmp::Reverse(e.last_opened_at));
    recent.truncate(recent_limit);

    SidebarRows {
        pinned,
        open,
        recent,
    }
}

/// An open host's subtitle: its connection state, or the chosen detail
/// while it is connected.
fn open_subtitle(
    mode: HostSubtitle,
    entry: &ServerEntrySummary,
    status: HostStatus,
    now: i64,
) -> SharedString {
    match status {
        HostStatus::Connected => idle_subtitle(mode, entry, true, now).into(),
        HostStatus::Connecting => "connecting…".into(),
        HostStatus::Disconnected => "disconnected".into(),
        HostStatus::Offline => "offline".into(),
    }
}

/// The subtitle of a row with no status text.
fn idle_subtitle(
    mode: HostSubtitle,
    entry: &ServerEntrySummary,
    is_open: bool,
    now: i64,
) -> String {
    match mode {
        // "just now" says nothing about a host that is open.
        HostSubtitle::LastOpened if is_open => String::new(),
        HostSubtitle::LastOpened => entry
            .last_opened_at
            .map(|t| relative_time(t, now))
            .unwrap_or_default(),
        HostSubtitle::Address if entry.port == 22 => crate::ui::bracket_ipv6(&entry.host),
        HostSubtitle::Address => format!("{}:{}", crate::ui::bracket_ipv6(&entry.host), entry.port),
        HostSubtitle::UserAtHost => {
            crate::ui::ssh_address(&entry.username, &entry.host, entry.port)
        }
        HostSubtitle::None => String::new(),
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg = theme.sidebar;
        let text = theme.foreground;

        let entries = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_entries().ok())
            .unwrap_or_default();
        let content = self.host_list(&entries, cx);

        v_flex()
            .w_full()
            .flex_none()
            .h_full()
            .bg(bg)
            .child(
                v_flex()
                    .id("sidebar-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .pb(rems_from_px(4.))
                    .px(rems_from_px(8.))
                    .children(content),
            )
            .child(
                h_flex()
                    .h(rems_from_px(46.))
                    .px(rems_from_px(8.))
                    .gap(rems_from_px(4.))
                    .items_center()
                    .child(
                        h_flex()
                            .id("sb-btn-new-host")
                            .relative()
                            .flex_1()
                            .h(rems_from_px(30.))
                            .px(rems_from_px(10.))
                            .gap(rems_from_px(6.))
                            .items_center()
                            .justify_center()
                            .rounded(rems_from_px(7.))
                            .bg(flint_theme::field())
                            .text_size(rems_from_px(13.))
                            .text_color(text)
                            .cursor_pointer()
                            .hover(|s| s.bg(flint_theme::element()))
                            .active(|s| s.bg(flint_theme::element_active()))
                            .child(Icon::new(IconName::Plus).size(rems_from_px(13.)))
                            .child(div().child("New host"))
                            .when_some(
                                shortcut_hints::key("app.new_host", cx)
                                    .filter(|_| shortcut_hints::is_shown(cx)),
                                |el, hint| el.child(shortcut_hints::corner_badge(hint)),
                            )
                            .on_click(
                                cx.listener(|_, _, _, cx| {
                                    cx.emit(SidebarEvent::AddServerRequested)
                                }),
                            ),
                    )
                    .when(Preferences::is_feedback_button_shown(cx), |el| {
                        el.child(self.footer_icon_button(
                            "sb-btn-feedback",
                            Icon::empty().path("icons/message-square.svg"),
                            "Send feedback",
                            None,
                            None,
                            || SidebarEvent::FeedbackRequested,
                            cx,
                        ))
                    })
                    .child(self.footer_icon_button(
                        "sb-btn-settings",
                        Icon::new(IconName::Settings),
                        "Settings",
                        Some(&ToggleSettings),
                        shortcut_hints::key("app.settings", cx),
                        || SidebarEvent::SettingsRequested,
                        cx,
                    )),
            )
    }
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Compact relative-time label ("just now", "5m ago", "yesterday", "3d ago").
pub(crate) fn relative_time(ts: i64, now: i64) -> String {
    let d = (now - ts).max(0);
    if d < 60 {
        "just now".to_string()
    } else if d < 3_600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3_600)
    } else if d < 172_800 {
        "yesterday".to_string()
    } else if d < 604_800 {
        format!("{}d ago", d / 86_400)
    } else {
        format!("{}w ago", d / 604_800)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_go_in_sidebar_order_pinned_first() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (a_id, b_id, c_id) = (a.to_string(), b.to_string(), c.to_string());
        // Opened A, B, C; C and B are pinned, in that order. A pin that is not open
        // takes no place.
        let sessions = [a_id.as_str(), b_id.as_str(), c_id.as_str()];
        assert_eq!(tab_order(&sessions, &[c, Uuid::new_v4(), b]), [2, 1, 0]);
        assert_eq!(tab_order(&sessions, &[]), [0, 1, 2]);
        assert_eq!(tab_order(&[], &[a]), Vec::<usize>::new());
    }

    fn host(label: &str, last_opened_at: Option<i64>) -> ServerEntrySummary {
        ServerEntrySummary {
            id: Uuid::new_v4(),
            label: label.to_string(),
            host: format!("{label}.example.com"),
            port: 22,
            username: "root".to_string(),
            os: None,
            last_opened_at,
            group_name: None,
            tag_names: Vec::new(),
            team_name: None,
            proxy: crate::vault::ProxyChoice::Default,
        }
    }

    fn labels(hosts: &[&ServerEntrySummary]) -> Vec<String> {
        hosts.iter().map(|e| e.label.clone()).collect()
    }

    #[test]
    fn an_open_pinned_host_is_listed_once_with_its_tab_index() {
        let entries = vec![host("a", Some(1)), host("b", Some(2))];
        let sessions = vec![
            (entries[0].id.to_string(), HostStatus::Connected),
            (entries[1].id.to_string(), HostStatus::Connecting),
        ];
        let rows = split_rows(&entries, &sessions, &[entries[1].id], 6);

        assert_eq!(rows.pinned.len(), 1);
        assert_eq!(rows.pinned[0].0.label, "b");
        assert_eq!(rows.pinned[0].1, Some((1, HostStatus::Connecting)));
        let open: Vec<usize> = rows.open.iter().map(|(ix, _, _)| *ix).collect();
        assert_eq!(open, vec![0]);
        assert!(rows.recent.is_empty());
    }

    #[test]
    fn pins_keep_their_order_and_skip_deleted_hosts() {
        let entries = vec![host("a", None), host("b", None)];
        let deleted = Uuid::new_v4();
        let rows = split_rows(&entries, &[], &[entries[1].id, deleted, entries[0].id], 6);

        let pinned: Vec<&ServerEntrySummary> = rows.pinned.iter().map(|(e, _)| *e).collect();
        assert_eq!(labels(&pinned), vec!["b", "a"]);
        assert!(rows.pinned.iter().all(|(_, session)| session.is_none()));
    }

    #[test]
    fn recent_skips_pinned_and_open_hosts_and_keeps_the_limit() {
        let entries = vec![
            host("open", Some(50)),
            host("pinned", Some(40)),
            host("old", Some(10)),
            host("new", Some(30)),
            host("mid", Some(20)),
            host("never", None),
        ];
        let sessions = vec![(entries[0].id.to_string(), HostStatus::Connected)];
        let rows = split_rows(&entries, &sessions, &[entries[1].id], 2);
        assert_eq!(labels(&rows.recent), vec!["new", "mid"]);

        let rows = split_rows(&entries, &sessions, &[entries[1].id], 0);
        assert!(rows.recent.is_empty());
    }

    #[test]
    fn local_sessions_stay_in_open() {
        let entries = vec![host("a", None)];
        let sessions = vec![
            (format!("{LOCAL_SESSION_PREFIX}1"), HostStatus::Connected),
            (entries[0].id.to_string(), HostStatus::Connected),
        ];
        let rows = split_rows(&entries, &sessions, &[entries[0].id], 6);
        assert_eq!(rows.open.len(), 1);
        assert_eq!(rows.open[0].0, 0);
        assert_eq!(rows.pinned[0].1, Some((1, HostStatus::Connected)));
    }

    #[test]
    fn idle_subtitle_follows_the_chosen_mode() {
        let mut entry = host("a", Some(0));
        entry.host = "::1".to_string();
        entry.port = 2222;
        let now = 120;
        assert_eq!(
            idle_subtitle(HostSubtitle::LastOpened, &entry, false, now),
            "2m ago"
        );
        assert_eq!(
            idle_subtitle(HostSubtitle::LastOpened, &entry, true, now),
            ""
        );
        assert_eq!(
            idle_subtitle(HostSubtitle::Address, &entry, false, now),
            "[::1]:2222"
        );
        assert_eq!(
            idle_subtitle(HostSubtitle::UserAtHost, &entry, false, now),
            "root@[::1]:2222"
        );
        assert_eq!(idle_subtitle(HostSubtitle::None, &entry, false, now), "");
        entry.port = 22;
        assert_eq!(
            idle_subtitle(HostSubtitle::Address, &entry, false, now),
            "[::1]"
        );
    }
}
