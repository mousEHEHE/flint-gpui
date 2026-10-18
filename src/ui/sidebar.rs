use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    div, px, Anchor, AnyElement, Context, Entity, EventEmitter, MouseButton, MouseDownEvent,
    SharedString, Window,
};
use gpui_component::{h_flex, tooltip::Tooltip, v_flex, ActiveTheme as _, Icon, IconName};
use uuid::Uuid;

use crate::theme as flint_theme;
use crate::ui::app_view::ToggleSettings;
use crate::ui::context_menu::{ContextMenuItem, PopoverMenu};
use crate::ui::host_sharing;
use crate::ui::shortcut_hints;
use crate::ui::VaultState;
use crate::vault::ServerEntrySummary;

const SIDEBAR_WIDTH: f32 = 232.;
const ROW_HEIGHT: f32 = 28.;
const SIDEBAR_GROUP: &str = "sidebar-host-row";
/// Cap on how many closed-but-recent hosts the RECENT zone shows. The full
/// catalog lives behind the "All hosts" button, so this stays a short list.
const RECENT_LIMIT: usize = 6;

/// Live connection state of a host that has an open session. Hosts without an
/// open session are absent from the list and rendered as [`HostStatus::Offline`].
#[derive(Clone, Copy, PartialEq, Eq)]
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

pub enum SidebarEvent {
    OpenServer(Uuid),
    CloseServer(Uuid),
    AddServerRequested,
    ShowAllServers,
    EditServerRequested(Uuid),
    DeleteServerRequested(Uuid),
    SettingsRequested,
    FeedbackRequested,
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

    /// One host row. `compact` rows (recent hosts) are dimmed. `show_close`
    /// adds a disconnect button, for open sessions only.
    #[allow(clippy::too_many_arguments)]
    fn host_row(
        &self,
        ui_idx: usize,
        entry: &ServerEntrySummary,
        subtitle: SharedString,
        status: HostStatus,
        compact: bool,
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

        let label_color = if compact { text_muted } else { text };

        let dot_color = match status {
            HostStatus::Connected => status_online,
            HostStatus::Connecting => status_connecting,
            HostStatus::Disconnected => status_down,
            HostStatus::Offline => flint_theme::status_off(),
        };
        let dot = div().size(px(6.)).flex_none().rounded_full().bg(dot_color);

        h_flex()
            .id(("sidebar-host", ui_idx))
            .group(group_id.clone())
            .relative()
            .h(px(ROW_HEIGHT))
            .gap(px(8.))
            .px(px(8.))
            .rounded(px(6.))
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
                    .gap(px(4.))
                    .items_center()
                    .child(
                        div()
                            .min_w_0()
                            .text_color(label_color)
                            .text_size(px(13.))
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
                            .child(Icon::new(IconName::User).size(px(10.)))
                    })),
            )
            // Trailing slot: the subtitle normally, the row controls on hover.
            // The controls sit on top so the two never fight for width.
            .child(
                div()
                    .flex_none()
                    .text_color(text_dim)
                    .text_size(px(11.5))
                    .whitespace_nowrap()
                    .group_hover(group_id.clone(), |s| s.invisible())
                    .when(is_active || hint.is_some(), |el| el.invisible())
                    .child(subtitle),
            )
            .child(
                h_flex()
                    .absolute()
                    .right(px(4.))
                    .flex_none()
                    .gap(px(1.))
                    .items_center()
                    .when_some(hint.clone(), |row, keys| {
                        row.child(shortcut_hints::badge(keys))
                    })
                    .when(show_close && hint.is_none(), |row| {
                        row.child(
                            div()
                                .id(("sidebar-host-close", ui_idx))
                                .size(px(16.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(3.))
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
                                .child(Icon::new(IconName::Close).size(px(10.))),
                        )
                    })
                    .when(hint.is_none(), |row| {
                        row.child(self.host_more_menu(ui_idx, entry_id, show_close, cx))
                    }),
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarEvent::OpenServer(entry_id));
            }))
            .into_any_element()
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
        let is_active = self.active_server_id.as_deref() == Some(entry_id.to_string().as_str());
        let group_id: SharedString = format!("{SIDEBAR_GROUP}-{ui_idx}").into();

        // The trigger button. Open/pressed both get a solid highlight so the
        // control clearly reacts; the row owns hover/visibility reveal.
        let trigger = div()
            .id(("sidebar-host-more-btn", ui_idx))
            .size(px(16.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
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
            .child(Icon::new(IconName::Ellipsis).size(px(12.)));

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

        menu.items(sharing_items)
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
        .h(px(26.))
        .px(px(8.))
        .pb(px(4.))
        .flex()
        .items_end()
        .text_size(px(11.5))
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
            .h(px(ROW_HEIGHT))
            .gap(px(8.))
            .px(px(8.))
            .rounded(px(6.))
            .items_center()
            .cursor_pointer()
            .text_size(px(13.))
            .text_color(text)
            .when(is_active, |el| el.bg(flint_theme::element()))
            .when(!is_active, |el| {
                el.hover(|s| s.bg(flint_theme::field()))
                    .active(|s| s.bg(flint_theme::element_active()))
            })
            .child(
                Icon::empty()
                    .path("icons/servers.svg")
                    .size(px(14.))
                    .text_color(icon_color),
            )
            .child(div().flex_1().child("All hosts"))
            // On the selected fill the dim colour falls below AA contrast.
            .child(
                div()
                    .text_size(px(11.5))
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
        hint: Option<&'static str>,
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
            .size(px(30.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.))
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
            .child(icon.size(px(15.)))
            .when_some(hint, |el, keys| {
                el.child(shortcut_hints::corner_badge(keys))
            })
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(event())))
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let bg = theme.sidebar;
        let text = theme.foreground;
        let text_dim = flint_theme::text_dim();

        let entries = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_entries().ok())
            .unwrap_or_default();
        let now = unix_now();

        let by_id: HashMap<String, &ServerEntrySummary> =
            entries.iter().map(|e| (e.id.to_string(), e)).collect();
        let open_ids: HashSet<&str> = self.sessions.iter().map(|(id, _)| id.as_str()).collect();

        let mut recents: Vec<&ServerEntrySummary> = entries
            .iter()
            .filter(|e| e.last_opened_at.is_some() && !open_ids.contains(e.id.to_string().as_str()))
            .collect();
        recents.sort_by_key(|e| std::cmp::Reverse(e.last_opened_at));
        recents.truncate(RECENT_LIMIT);

        let mut content: Vec<AnyElement> = vec![self.all_hosts_row(entries.len(), cx)];
        let mut ui_idx = 0usize;

        // Open sessions first, in tab order.
        if !self.sessions.is_empty() {
            content.push(section_label("Open"));
        }
        // Cmd+1 to Cmd+9 follow this order: it is the tab order.
        for (tab_index, (id, status)) in self.sessions.iter().enumerate() {
            let Some(e) = by_id.get(id) else { continue };
            let sub: SharedString = match status {
                HostStatus::Connected => "",
                HostStatus::Connecting => "connecting…",
                HostStatus::Disconnected => "disconnected",
                HostStatus::Offline => "offline",
            }
            .into();
            let shortcut = shortcut_hints::tab_key(tab_index);
            content.push(self.host_row(ui_idx, e, sub, *status, false, true, shortcut, cx));
            ui_idx += 1;
        }

        if !recents.is_empty() {
            content.push(section_label("Recent"));
        }
        for e in &recents {
            let sub: SharedString = e
                .last_opened_at
                .map(|t| relative_time(t, now))
                .unwrap_or_default()
                .into();
            content.push(self.host_row(ui_idx, e, sub, HostStatus::Offline, true, false, None, cx));
            ui_idx += 1;
        }

        // Nothing open and nothing recent: a quiet hint, never the full catalog.
        if self.sessions.is_empty() && recents.is_empty() && !entries.is_empty() {
            content.push(
                div()
                    .h(px(ROW_HEIGHT))
                    .px(px(8.))
                    .mt(px(6.))
                    .flex()
                    .items_center()
                    .text_size(px(12.))
                    .text_color(text_dim)
                    .child("No recent hosts")
                    .into_any_element(),
            );
        }

        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .h_full()
            .bg(bg)
            .child(
                v_flex()
                    .id("sidebar-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .pb(px(4.))
                    .px(px(8.))
                    .children(content),
            )
            .child(
                h_flex()
                    .h(px(46.))
                    .px(px(8.))
                    .gap(px(4.))
                    .items_center()
                    .child(
                        h_flex()
                            .id("sb-btn-new-host")
                            .relative()
                            .flex_1()
                            .h(px(30.))
                            .px(px(10.))
                            .gap(px(6.))
                            .items_center()
                            .justify_center()
                            .rounded(px(7.))
                            .bg(flint_theme::field())
                            .text_size(px(13.))
                            .text_color(text)
                            .cursor_pointer()
                            .hover(|s| s.bg(flint_theme::element()))
                            .active(|s| s.bg(flint_theme::element_active()))
                            .child(Icon::new(IconName::Plus).size(px(13.)))
                            .child(div().child("New host"))
                            .when(shortcut_hints::is_shown(cx), |el| {
                                el.child(shortcut_hints::corner_badge(shortcut_hints::NEW_HOST))
                            })
                            .on_click(
                                cx.listener(|_, _, _, cx| {
                                    cx.emit(SidebarEvent::AddServerRequested)
                                }),
                            ),
                    )
                    .child(self.footer_icon_button(
                        "sb-btn-feedback",
                        Icon::empty().path("icons/message-square.svg"),
                        "Send feedback",
                        None,
                        None,
                        || SidebarEvent::FeedbackRequested,
                        cx,
                    ))
                    .child(self.footer_icon_button(
                        "sb-btn-settings",
                        Icon::new(IconName::Settings),
                        "Settings",
                        Some(&ToggleSettings),
                        Some(shortcut_hints::SETTINGS),
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
