use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::preferences::Preferences;
use crate::theme as flint_theme;
use crate::ui::context_menu::{ContextMenuItem, PopoverMenu};
use crate::ui::{rems_from_px, scaled_px};
use gpui::prelude::*;
use gpui::{
    actions, div, px, svg, Anchor, App, ClickEvent, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent, SharedString, Window,
};
use gpui_component::{
    h_flex,
    input::{Input, InputState},
    spinner::Spinner,
    tooltip::Tooltip,
    v_flex, ActiveTheme as _, Icon, IconName, Sizable as _,
};
use uuid::Uuid;

use crate::ui::host_icon::{host_icon, TILE_SIZE};
use crate::ui::host_sharing;
use crate::ui::keymap;
use crate::ui::sync_service::SyncService;
use crate::ui::VaultState;
use crate::vault::ServerEntrySummary;

actions!(servers_view, [DeleteSelectedHosts, FocusHostSearch]);

pub const KEY_CONTEXT: &str = "HostList";

const SERVER_ROW_GROUP: &str = "server-row";
const CARD_RADIUS: f32 = 10.;
/// Where the line between two rows starts: past the host's tile, so the
/// tiles stand in one column.
const ROW_DIVIDER_INSET: f32 = 52.;

/// The empty-state line that offers the hosts of `~/.ssh/config`.
fn ssh_config_offer(host_count: usize) -> Option<String> {
    match host_count {
        0 => None,
        1 => Some("Found 1 host in your SSH config".to_string()),
        count => Some(format!("Found {count} hosts in your SSH config")),
    }
}

/// Where a row sits in its card: the end rows round their corners.
#[derive(Clone, Copy)]
struct RowPlace {
    is_first: bool,
    is_last: bool,
}

pub enum ServersViewEvent {
    OpenServer(ServerEntrySummary),
    AddServerRequested,
    ImportRequested,
    EditServerRequested(Uuid),
    DuplicateServerRequested(Uuid),
    DeleteServersRequested(Vec<Uuid>),
    ExportKeyRequested {
        host: Uuid,
        key: Uuid,
    },
    /// Open one more terminal on the host, connecting first if needed.
    ConnectInNewTab(Uuid),
    /// Show the host's files beside the session in front.
    OpenInSplit(Uuid),
    MoveToGroup {
        hosts: Vec<Uuid>,
        group: Option<Uuid>,
    },
}

/// Hosts dragged onto a folder heading.
#[derive(Clone)]
struct DraggedHosts(Vec<Uuid>);

struct DraggedHostsPreview(SharedString);

impl Render for DraggedHostsPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(rems_from_px(8.))
            .py(rems_from_px(3.))
            .rounded(rems_from_px(6.))
            .bg(flint_theme::element())
            .border_1()
            .border_color(flint_theme::border())
            .shadow_md()
            .font_family(flint_theme::UI_FONT)
            .text_size(rems_from_px(12.5))
            .text_color(cx.theme().foreground)
            .child(self.0.clone())
    }
}

/// The host an arrow key moves to: the next one in `order`, or the first
/// when none is picked yet.
fn step_host(order: &[Uuid], from: Option<Uuid>, is_down: bool) -> Option<Uuid> {
    let Some(pos) = from.and_then(|from| order.iter().position(|id| *id == from)) else {
        return order.first().copied();
    };
    let next = if is_down {
        (pos + 1).min(order.len() - 1)
    } else {
        pos.saturating_sub(1)
    };
    order.get(next).copied()
}

/// The hosts from `anchor` to `to` in list order, both ends included.
fn host_range(order: &[Uuid], anchor: Uuid, to: Uuid) -> Vec<Uuid> {
    let find = |id: Uuid| order.iter().position(|candidate| *candidate == id);
    match (find(anchor), find(to)) {
        (Some(a), Some(b)) => order[a.min(b)..=a.max(b)].to_vec(),
        _ => vec![to],
    }
}

pub struct ServersView {
    focus_handle: FocusHandle,
    vault_state: Entity<VaultState>,
    /// Picked with the keys or Cmd/Shift+click. The last one is where the
    /// arrow keys move from.
    selected: Vec<Uuid>,
    is_selection_menu_open: bool,
    search_input: Entity<InputState>,
    search: String,
    /// Which row's "more" menu is open (at most one).
    open_menu_for: Option<Uuid>,
    /// Guards the capture-phase dismiss vs. bubble-phase toggle race. See the
    /// matching field in `Sidebar` and the trigger handler below.
    menu_dismissed_at: Option<(Uuid, Instant)>,
    connected_ids: std::collections::HashSet<String>,
    /// Show only hosts in this folder (filter bar); `None` = all.
    group_filter: Option<String>,
    /// Show only hosts carrying all of these tags (filter bar).
    tag_filter: std::collections::HashSet<String>,
    /// Hosts found in `~/.ssh/config`, counted once when the view is created.
    /// The empty state offers to import them; 0 until the scan finishes.
    ssh_config_host_count: usize,
    /// The sync button was pressed and its round still runs. Rounds that
    /// start on their own do not spin it: a short twitch every couple of
    /// minutes looks like a glitch.
    is_refreshing: bool,
}

impl EventEmitter<ServersViewEvent> for ServersView {}

impl Focusable for ServersView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ServersView {
    pub fn new(
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search hosts…"));

        cx.subscribe(
            &search_input,
            |this, input_entity, event: &gpui_component::input::InputEvent, cx| {
                if matches!(event, gpui_component::input::InputEvent::Change) {
                    this.search = input_entity.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )
        .detach();

        cx.observe(&vault_state, |_, _, cx| cx.notify()).detach();
        cx.observe(&SyncService::global(cx), |this, service, cx| {
            if !service.read(cx).is_syncing {
                this.is_refreshing = false;
            }
            cx.notify();
        })
        .detach();

        // The scan reads the config and every file it includes.
        cx.spawn(async move |this, cx| {
            let count = cx
                .background_executor()
                .spawn(async { crate::import::scan_ssh_config().len() })
                .await;
            tracing::debug!(count, "counted ssh config hosts for the empty state");
            this.update(cx, |this, cx| {
                this.ssh_config_host_count = count;
                cx.notify();
            })
            .ok();
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            vault_state,
            selected: Vec::new(),
            is_selection_menu_open: false,
            search_input,
            search: String::new(),
            open_menu_for: None,
            menu_dismissed_at: None,
            connected_ids: std::collections::HashSet::new(),
            group_filter: None,
            tag_filter: std::collections::HashSet::new(),
            ssh_config_host_count: 0,
            is_refreshing: false,
        }
    }

    pub fn set_connected_ids(
        &mut self,
        ids: std::collections::HashSet<String>,
        cx: &mut Context<Self>,
    ) {
        if self.connected_ids != ids {
            self.connected_ids = ids;
            cx.notify();
        }
    }

    fn copy_ssh_command(&self, id: Uuid, cx: &mut Context<Self>) {
        let Some(command) = self
            .vault_state
            .read(cx)
            .manager()
            .map(|vm| vm.ssh_command_for(id))
        else {
            return;
        };
        match command {
            Ok(command) => cx.write_to_clipboard(ClipboardItem::new_string(command)),
            Err(e) => tracing::warn!(error = %e, "could not build the ssh command to copy"),
        }
    }

    fn all_entries(&self, cx: &App) -> Vec<ServerEntrySummary> {
        let mut entries = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_entries().ok())
            .unwrap_or_default();
        entries.retain(|e| !crate::ui::hidden_hosts::is_hidden(e.id, cx));
        entries
    }

    /// The hosts the filters and the search leave, in the order they show:
    /// folders by name, then the ungrouped ones.
    fn visible_entries(&self, entries: Vec<ServerEntrySummary>) -> Vec<ServerEntrySummary> {
        let search = self.search.trim().to_lowercase();
        let mut visible: Vec<ServerEntrySummary> = entries
            .into_iter()
            .filter(|e| {
                self.group_filter
                    .as_ref()
                    .is_none_or(|g| e.group_name.as_deref() == Some(g.as_str()))
            })
            .filter(|e| self.tag_filter.iter().all(|t| e.tag_names.contains(t)))
            .filter(|e| crate::ui::command_palette::host_matches(e, &search))
            .collect();
        visible.sort_by(|a, b| {
            (a.group_name.is_none(), &a.group_name).cmp(&(b.group_name.is_none(), &b.group_name))
        });
        visible
    }

    fn host_order(&self, cx: &App) -> Vec<Uuid> {
        self.visible_entries(self.all_entries(cx))
            .iter()
            .map(|e| e.id)
            .collect()
    }

    /// The row's own host, or every picked one when the row is among them.
    fn hosts_for_row(&self, id: Uuid) -> Vec<Uuid> {
        if self.selected.contains(&id) {
            self.selected.clone()
        } else {
            vec![id]
        }
    }

    fn row_clicked(
        &mut self,
        entry: &ServerEntrySummary,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.modifiers();
        if modifiers.secondary() {
            if !self.selected.contains(&entry.id) {
                self.selected.push(entry.id);
            } else {
                self.selected.retain(|id| *id != entry.id);
            }
        } else if modifiers.shift {
            let anchor = self.selected.first().copied().unwrap_or(entry.id);
            let mut range = host_range(&self.host_order(cx), anchor, entry.id);
            // The anchor stays first, the clicked host last, for the arrow keys.
            range.retain(|id| *id != anchor && *id != entry.id);
            self.selected = std::iter::once(anchor)
                .chain(range)
                .chain((anchor != entry.id).then_some(entry.id))
                .collect();
        } else {
            self.selected.clear();
            cx.emit(ServersViewEvent::OpenServer(entry.clone()));
            return;
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let search = self.search_input.read(cx).focus_handle(cx);
        // Down from the search field goes on into the list.
        if search.is_focused(window) {
            if keystroke.key == "down" && !keystroke.modifiers.modified() {
                window.focus(&self.focus_handle, cx);
                self.selected = self.host_order(cx).first().copied().into_iter().collect();
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        if !self.focus_handle.is_focused(window) || keystroke.modifiers.modified() {
            return;
        }
        match keystroke.key.as_str() {
            "up" | "down" => {
                let order = self.host_order(cx);
                let next = step_host(
                    &order,
                    self.selected.last().copied(),
                    keystroke.key == "down",
                );
                self.selected = next.into_iter().collect();
            }
            "enter" => {
                let Some(id) = self.selected.last().copied() else {
                    return;
                };
                let entry = self.all_entries(cx).into_iter().find(|e| e.id == id);
                if let Some(entry) = entry {
                    self.selected.clear();
                    cx.emit(ServersViewEvent::OpenServer(entry));
                }
            }
            "escape" => self.selected.clear(),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        let hosts = std::mem::take(&mut self.selected);
        cx.emit(ServersViewEvent::DeleteServersRequested(hosts));
        cx.notify();
    }

    /// "Move to group…": every folder, and out of any folder.
    fn group_menu(
        &self,
        id: &'static str,
        hosts: Vec<Uuid>,
        cx: &mut Context<Self>,
    ) -> Option<ContextMenuItem> {
        let groups = self
            .vault_state
            .read(cx)
            .manager()
            .map(|vm| vm.list_group_paths())
            .unwrap_or_default();
        if groups.is_empty() {
            return None;
        }
        let choices = groups
            .into_iter()
            .map(|(group, name)| (Some(group), name))
            .chain(std::iter::once((None, "No group".to_string())));
        let items = choices
            .enumerate()
            .map(|(ix, (group, name))| {
                let hosts = hosts.clone();
                ContextMenuItem::new(
                    (id, ix),
                    name,
                    cx.listener(move |this, _, _, cx| {
                        this.open_menu_for = None;
                        this.is_selection_menu_open = false;
                        this.selected.clear();
                        cx.emit(ServersViewEvent::MoveToGroup {
                            hosts: hosts.clone(),
                            group,
                        });
                        cx.notify();
                    }),
                )
            })
            .collect();
        Some(ContextMenuItem::new(id, "Move to group\u{2026}", |_, _, _| {}).submenu(items))
    }

    /// Shown while more than one host is picked.
    fn selection_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.selected.len() < 2 {
            return None;
        }
        let is_menu_open = self.is_selection_menu_open;
        let button = |id: &'static str, label: &'static str| {
            h_flex()
                .id(id)
                .h(rems_from_px(24.))
                .px(rems_from_px(9.))
                .items_center()
                .rounded(rems_from_px(6.))
                .border_1()
                .border_color(flint_theme::border())
                .bg(flint_theme::field())
                .text_size(rems_from_px(12.5))
                .cursor_pointer()
                .hover(|s| s.bg(flint_theme::element_hover()))
                .child(label)
        };
        let move_trigger = button("selection-move", "Move to group\u{2026}").on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                this.is_selection_menu_open = !this.is_selection_menu_open;
                cx.notify();
            }),
        );
        let move_menu = self
            .group_menu("selection-move-menu", self.selected.clone(), cx)
            .map(|item| {
                PopoverMenu::new("selection-move-popover", is_menu_open, move_trigger)
                    .anchor(Anchor::TopLeft)
                    .items(item.submenu)
                    .on_dismiss(cx.listener(|this, _, _, cx| {
                        this.is_selection_menu_open = false;
                        cx.notify();
                    }))
            });
        let bar = h_flex()
            .w_full()
            .px(rems_from_px(20.))
            .pb(rems_from_px(6.))
            .gap(rems_from_px(6.))
            .items_center()
            .child(
                div()
                    .text_size(rems_from_px(12.5))
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{} selected", self.selected.len())),
            )
            .children(move_menu)
            .child(
                button("selection-delete", "Delete")
                    .text_color(cx.theme().danger)
                    .on_click(cx.listener(|this, _, _, cx| this.delete_selected(cx))),
            )
            .child(
                button("selection-clear", "Clear").on_click(cx.listener(|this, _, _, cx| {
                    this.selected.clear();
                    cx.notify();
                })),
            );
        Some(bar.into_any_element())
    }

    /// The "more" trigger and dropdown of a host row. Same anchored
    /// [`PopoverMenu`] as the sidebar's host menu.
    fn row_more_menu(
        &self,
        ix: usize,
        id: Uuid,
        is_menu_open: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let text_dim = flint_theme::text_dim();
        let bg_element_hover = theme.list_hover;
        let bg_element_active = theme.list_active;
        let border_focused = flint_theme::border_focused();
        let is_pinned = Preferences::is_pinned(id, cx);

        let trigger = div()
            .id(("server-row-more-btn", ix))
            .size(rems_from_px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(rems_from_px(6.))
            .border_1()
            .border_color(gpui::transparent_black())
            .text_color(if is_menu_open { fg } else { text_dim })
            .when(is_menu_open, |el| {
                el.bg(bg_element_active).border_color(border_focused)
            })
            .when(!is_menu_open, |el| {
                el.hover(|s| s.bg(bg_element_hover).text_color(fg))
                    .active(|s| s.bg(bg_element_active).text_color(fg))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let toggled_off = this.menu_dismissed_at.take().is_some_and(|(eid, at)| {
                        eid == id && at.elapsed() < Duration::from_millis(200)
                    });
                    if toggled_off || this.open_menu_for == Some(id) {
                        this.open_menu_for = None;
                    } else {
                        this.open_menu_for = Some(id);
                    }
                    cx.notify();
                }),
            )
            .child(Icon::new(IconName::Ellipsis).size(rems_from_px(14.)));

        let sharing_items: Vec<ContextMenuItem> =
            host_sharing::menu_choices(&self.vault_state, id, cx)
                .into_iter()
                .enumerate()
                .map(|(choice_ix, (label, target))| {
                    ContextMenuItem::new(
                        ("row-menu-share", choice_ix),
                        label,
                        cx.listener(move |this, _, _, cx| {
                            this.open_menu_for = None;
                            host_sharing::set_shared(&this.vault_state, id, target, cx);
                            cx.notify();
                        }),
                    )
                })
                .collect();

        let export_key =
            host_sharing::export_key_item("row-menu-export-key", &self.vault_state, cx, |key| {
                Box::new(cx.listener(move |this, _, _, cx| {
                    this.open_menu_for = None;
                    cx.emit(ServersViewEvent::ExportKeyRequested { host: id, key });
                    cx.notify();
                }))
            });

        let move_to_group = self.group_menu("row-menu-move", self.hosts_for_row(id), cx);
        let delete_hosts = self.hosts_for_row(id);

        PopoverMenu::new(("server-row-more", ix), is_menu_open, trigger)
            .anchor(Anchor::TopRight)
            .item(ContextMenuItem::new(
                "row-menu-new-tab",
                "Connect in new tab",
                cx.listener(move |this, _, _, cx| {
                    this.open_menu_for = None;
                    cx.emit(ServersViewEvent::ConnectInNewTab(id));
                    cx.notify();
                }),
            ))
            .item(ContextMenuItem::new(
                "row-menu-split",
                "Open in split",
                cx.listener(move |this, _, _, cx| {
                    this.open_menu_for = None;
                    cx.emit(ServersViewEvent::OpenInSplit(id));
                    cx.notify();
                }),
            ))
            .item(ContextMenuItem::new(
                "row-menu-edit",
                "Edit",
                cx.listener(move |this, _, _, cx| {
                    this.open_menu_for = None;
                    cx.emit(ServersViewEvent::EditServerRequested(id));
                    cx.notify();
                }),
            ))
            .item(ContextMenuItem::new(
                "row-menu-pin",
                if is_pinned { "Unpin" } else { "Pin" },
                cx.listener(move |this, _, _, cx| {
                    this.open_menu_for = None;
                    Preferences::set_pinned(id, !is_pinned, cx);
                    cx.notify();
                }),
            ))
            .item(ContextMenuItem::new(
                "row-menu-duplicate",
                "Duplicate",
                cx.listener(move |this, _, _, cx| {
                    this.open_menu_for = None;
                    cx.emit(ServersViewEvent::DuplicateServerRequested(id));
                    cx.notify();
                }),
            ))
            .item(ContextMenuItem::new(
                "row-menu-copy-ssh",
                "Copy SSH command",
                cx.listener(move |this, _, _, cx| {
                    this.open_menu_for = None;
                    this.copy_ssh_command(id, cx);
                    cx.notify();
                }),
            ))
            .items(move_to_group)
            .items(export_key)
            .items(sharing_items)
            .item(
                ContextMenuItem::new(
                    "row-menu-delete",
                    if delete_hosts.len() > 1 {
                        "Delete selected"
                    } else {
                        "Delete"
                    },
                    cx.listener(move |this, _, _, cx| {
                        this.open_menu_for = None;
                        this.selected.clear();
                        cx.emit(ServersViewEvent::DeleteServersRequested(
                            delete_hosts.clone(),
                        ));
                        cx.notify();
                    }),
                )
                .bound_shortcut(keymap::label("hosts.delete", cx))
                .danger(),
            )
            .on_dismiss(cx.listener(move |this, _, _, cx| {
                this.menu_dismissed_at = Some((id, Instant::now()));
                this.open_menu_for = None;
                cx.notify();
            }))
    }

    fn is_signed_in(&self, cx: &App) -> bool {
        self.vault_state
            .read(cx)
            .manager()
            .and_then(|vault| vault.sync_state())
            .is_some_and(|state| state.session_token.is_some())
    }

    fn is_loading_hosts(&self, cx: &App) -> bool {
        self.is_signed_in(cx) && SyncService::global(cx).read(cx).is_first_sync_running()
    }

    /// Only with a sync account.
    fn render_sync_button(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.is_signed_in(cx) {
            return None;
        }
        let service = SyncService::global(cx).read(cx);
        let is_spinning = self.is_refreshing && service.is_syncing;
        let tooltip: gpui::SharedString = match &service.sync_error {
            _ if is_spinning => "Syncing…".into(),
            Some(error) => format!("Last sync failed: {error}").into(),
            None => "Sync now".into(),
        };
        let icon = Icon::empty().path("icons/rotate-cw.svg");
        let icon = if is_spinning {
            Spinner::new()
                .icon(icon)
                .with_size(scaled_px(px(14.), cx))
                .color(cx.theme().foreground)
                .into_any_element()
        } else {
            let color = if service.sync_error.is_some() {
                cx.theme().danger
            } else {
                cx.theme().foreground
            };
            icon.size(rems_from_px(14.))
                .text_color(color)
                .into_any_element()
        };
        let button = h_flex()
            .id("sync-hosts")
            .size(rems_from_px(28.))
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(rems_from_px(7.))
            .border_1()
            .border_color(flint_theme::border())
            .bg(flint_theme::field())
            .when(!is_spinning, |el| {
                el.cursor_pointer()
                    .hover(|s| s.bg(flint_theme::element_hover()))
                    .active(|s| s.bg(flint_theme::element_active()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let service = SyncService::global(cx);
                        service.update(cx, |service, cx| service.refresh_all(cx));
                        this.is_refreshing = service.read(cx).is_syncing;
                        cx.notify();
                    }))
            })
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .child(icon);
        Some(button.into_any_element())
    }

    /// Section heading for a host folder: name + count, quiet and small.
    /// Hosts dropped on it move into the folder; `group` is `None` for the
    /// ungrouped ones.
    fn group_header(
        &self,
        name: &str,
        group: Option<Uuid>,
        count: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        h_flex()
            .id(SharedString::from(format!("group-header-{name}")))
            .w_full()
            .rounded(rems_from_px(6.))
            .drag_over::<DraggedHosts>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(cx.listener(move |this, dragged: &DraggedHosts, _, cx| {
                this.selected.clear();
                cx.emit(ServersViewEvent::MoveToGroup {
                    hosts: dragged.0.clone(),
                    group,
                });
            }))
            .h(rems_from_px(32.))
            .mt(rems_from_px(8.))
            .px(rems_from_px(4.))
            .gap(rems_from_px(6.))
            .items_center()
            .child(
                div()
                    .text_size(rems_from_px(12.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child(name.to_string()),
            )
            .child(
                div()
                    .text_size(rems_from_px(12.))
                    .text_color(flint_theme::text_dim())
                    .child(format!("{count}")),
            )
            .into_any_element()
    }

    /// The hosts of one folder as a card, one row each.
    fn host_card(
        &self,
        entries: Vec<ServerEntrySummary>,
        first_ix: usize,
        now: i64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let count = entries.len();
        let mut card = v_flex()
            .w_full()
            .rounded(rems_from_px(CARD_RADIUS))
            .border_1()
            .border_color(flint_theme::border())
            .bg(flint_theme::card());
        for (pos, entry) in entries.into_iter().enumerate() {
            let place = RowPlace {
                is_first: pos == 0,
                is_last: pos + 1 == count,
            };
            card = card.child(self.server_row(first_ix + pos, entry, place, now, cx));
        }
        card.into_any_element()
    }

    /// One host in a card: the system's mark with the session dot, name,
    /// address, OS, tags, and when it was last opened. The "more" menu takes
    /// the place of the time while the row is hovered.
    fn server_row(
        &self,
        ix: usize,
        entry: ServerEntrySummary,
        place: RowPlace,
        now: i64,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let text_dim = flint_theme::text_dim();
        let row_hover = flint_theme::field();
        // A hovered row's fill is square, and gpui does not clip it to the
        // card's corners, so the end rows round their own.
        let inner_radius = rems_from_px(CARD_RADIUS - 1.);

        let id = entry.id;
        let name = if entry.label.is_empty() {
            entry.host.clone()
        } else {
            entry.label.clone()
        };
        let addr = crate::ui::ssh_address(&entry.username, &entry.host, entry.port);
        let last_opened = entry
            .last_opened_at
            .map(|at| crate::ui::sidebar::relative_time(at, now))
            .unwrap_or_default();
        // Detected OS (filled on connect); nothing shows until then.
        let os = entry.os.clone().filter(|os| !os.trim().is_empty());
        let tag_names = entry.tag_names.clone();
        let team_name = entry.team_name.clone();
        let is_menu_open = self.open_menu_for == Some(id);
        let is_selected = self.selected.contains(&id);
        let is_connected = self.connected_ids.contains(&id.to_string());
        let dragged = DraggedHosts(self.hosts_for_row(id));
        let drag_label: SharedString = match dragged.0.len() {
            1 => name.clone().into(),
            count => format!("{count} hosts").into(),
        };
        let group_id: gpui::SharedString = format!("{SERVER_ROW_GROUP}-{ix}").into();

        h_flex()
            .id(("srow", ix))
            .group(group_id.clone())
            .relative()
            .w_full()
            .h(rems_from_px(44.))
            .px(rems_from_px(12.))
            .gap(rems_from_px(12.))
            .items_center()
            .when(place.is_first, |row| row.rounded_t(inner_radius))
            .when(place.is_last, |row| row.rounded_b(inner_radius))
            .cursor_pointer()
            .hover(|s| s.bg(row_hover))
            .when(is_menu_open, |el| el.bg(row_hover))
            .when(is_selected, |el| el.bg(flint_theme::element()))
            .active(|s| s.bg(flint_theme::element_active()))
            .on_drag(dragged, move |_, _, _, cx| {
                cx.new(|_| DraggedHostsPreview(drag_label.clone()))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_menu_for = Some(id);
                    cx.notify();
                }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.row_clicked(&entry, event, window, cx);
            }))
            .when(!place.is_first, |row| {
                row.child(
                    div()
                        .absolute()
                        .top_0()
                        .left(rems_from_px(ROW_DIVIDER_INSET))
                        .right_0()
                        .h(px(1.))
                        .bg(flint_theme::border()),
                )
            })
            .child(host_icon(
                os.as_deref(),
                is_connected,
                flint_theme::card(),
                TILE_SIZE,
            ))
            // Fixed widths keep the columns lined up from row to row; the
            // spare room collects in the tags column, not between the texts.
            .child(
                div()
                    .w(rems_from_px(200.))
                    .flex_shrink(1.)
                    .min_w(rems_from_px(96.))
                    .text_size(rems_from_px(13.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .truncate()
                    .child(name),
            )
            .child(
                div()
                    .w(rems_from_px(240.))
                    .flex_shrink(1.)
                    .min_w(rems_from_px(96.))
                    .font_family(flint_theme::MONO_FONT)
                    .text_size(rems_from_px(12.))
                    .text_color(text_dim)
                    .truncate()
                    .child(addr),
            )
            .child(
                div()
                    .w(rems_from_px(130.))
                    .flex_shrink(1.)
                    .min_w(rems_from_px(60.))
                    .text_size(rems_from_px(12.))
                    .text_color(text_dim)
                    .truncate()
                    .children(os),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(rems_from_px(6.))
                    .items_center()
                    .overflow_hidden()
                    .children(tag_names.into_iter().map(|t| {
                        div()
                            .flex_none()
                            .px(rems_from_px(6.))
                            .rounded(rems_from_px(5.))
                            .border_1()
                            .border_color(flint_theme::border())
                            .bg(row_hover)
                            .text_size(rems_from_px(11.5))
                            .text_color(muted_fg)
                            .child(format!("#{t}"))
                    })),
            )
            .children(team_name.map(|team| {
                h_flex()
                    .flex_none()
                    .gap(rems_from_px(4.))
                    .items_center()
                    .text_size(rems_from_px(12.))
                    .text_color(text_dim)
                    .child(Icon::new(IconName::User).size(rems_from_px(11.)))
                    .child(team)
            }))
            // Trailing slot: when it was last opened, or the menu on hover.
            .child(
                div()
                    .relative()
                    .w(rems_from_px(64.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .items_center()
                    .child(
                        div()
                            .text_size(rems_from_px(12.))
                            .text_color(text_dim)
                            .whitespace_nowrap()
                            .when(is_menu_open, |el| el.invisible())
                            .group_hover(group_id.clone(), |s| s.invisible())
                            .child(last_opened),
                    )
                    .child(
                        div()
                            .absolute()
                            .right_0()
                            .when(!is_menu_open, |el| {
                                el.invisible()
                                    .group_hover(group_id.clone(), |s| s.visible())
                            })
                            .child(self.row_more_menu(ix, id, is_menu_open, cx)),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ServersView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.all_entries(cx);
        let host_count = entries.len();
        self.selected
            .retain(|id| entries.iter().any(|entry| entry.id == *id));

        // Every group and tag in use, for the filter bar's buttons.
        let mut all_groups: Vec<String> = entries
            .iter()
            .filter_map(|e| e.group_name.clone())
            .collect();
        all_groups.sort();
        all_groups.dedup();
        let mut all_tags: Vec<String> = entries
            .iter()
            .flat_map(|e| e.tag_names.iter().cloned())
            .collect();
        all_tags.sort();
        all_tags.dedup();

        // Drop filters whose group/tag no longer exists (host deleted/edited)
        // so the view can't get stuck empty.
        if self
            .group_filter
            .as_ref()
            .is_some_and(|g| !all_groups.contains(g))
        {
            self.group_filter = None;
        }
        self.tag_filter.retain(|t| all_tags.contains(t));
        let has_filters = self.group_filter.is_some() || !self.tag_filter.is_empty();

        let search_lower = self.search.trim().to_lowercase();
        let filtered = self.visible_entries(entries);

        let is_search_empty = search_lower.is_empty();
        let search_text = self.search.clone();

        let theme = cx.theme();
        let fg = theme.foreground;
        let text_dim = flint_theme::text_dim();
        let muted_fg = theme.muted_foreground;
        let accent = theme.primary;
        let accent_hover = theme.primary_hover;
        let accent_active = theme.primary_active;
        let accent_ink = theme.primary_foreground;
        let field = flint_theme::field();
        let selected_chip = flint_theme::element();

        let sync_button = self.render_sync_button(cx);

        // The one filled button on the screen.
        let add_server_btn = |id: &'static str| {
            h_flex()
                .id(id)
                .h(rems_from_px(28.))
                .pl(rems_from_px(10.))
                .pr(rems_from_px(12.))
                .gap(rems_from_px(6.))
                .items_center()
                .justify_center()
                .rounded(rems_from_px(7.))
                .bg(accent)
                .text_size(rems_from_px(13.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(accent_ink)
                .cursor_pointer()
                .hover(move |s| s.bg(accent_hover))
                .active(move |s| s.bg(accent_active))
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.emit(ServersViewEvent::AddServerRequested);
                }))
                .child(
                    Icon::new(IconName::Plus)
                        .size(rems_from_px(13.))
                        .text_color(accent_ink),
                )
                .child(div().child("New host"))
        };

        let search_focused = self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let search_field = h_flex()
            .w(rems_from_px(240.))
            .flex_shrink(1.)
            .min_w(rems_from_px(140.))
            .h(rems_from_px(28.))
            .pl(rems_from_px(10.))
            .pr(rems_from_px(6.))
            .gap(rems_from_px(7.))
            .items_center()
            .rounded(rems_from_px(7.))
            .bg(field)
            .border_1()
            .border_color(if search_focused {
                accent
            } else {
                flint_theme::border()
            })
            .child(
                Icon::new(IconName::Search)
                    .size(rems_from_px(13.))
                    .text_color(text_dim),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.search_input).appearance(false)),
            );

        // Filled and outlined at rest, so it reads as a button before the
        // pointer gets there.
        let import_btn = |id: &'static str| {
            h_flex()
                .id(id)
                .h(rems_from_px(28.))
                .px(rems_from_px(10.))
                .gap(rems_from_px(6.))
                .items_center()
                .justify_center()
                .rounded(rems_from_px(7.))
                .border_1()
                .border_color(flint_theme::border())
                .bg(field)
                .text_size(rems_from_px(13.))
                .text_color(fg)
                .cursor_pointer()
                .hover(|s| s.bg(flint_theme::element_hover()))
                .active(|s| s.bg(flint_theme::element_active()))
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.emit(ServersViewEvent::ImportRequested);
                }))
        };

        let header = h_flex()
            .w_full()
            .pl(rems_from_px(20.))
            .pr(rems_from_px(16.))
            .pt(rems_from_px(14.))
            .pb(rems_from_px(10.))
            .gap(rems_from_px(8.))
            .items_center()
            .child(
                div()
                    .text_size(rems_from_px(17.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child("All hosts"),
            )
            .child(
                div()
                    .text_size(rems_from_px(13.))
                    .text_color(text_dim)
                    .child(host_count.to_string()),
            )
            .child(div().flex_1())
            .child(search_field)
            .children(sync_button)
            .child(
                import_btn("import-hosts-header")
                    .child(
                        Icon::empty()
                            .path("icons/download.svg")
                            .size(rems_from_px(14.)),
                    )
                    .child("Import"),
            )
            .child(add_server_btn("add-server-header"));

        // "All" resets, one group at a time, tags stack.
        let filter_bar = (!all_groups.is_empty() || !all_tags.is_empty()).then(|| {
            // Every chip has a fill and an outline, so the bar reads as a row
            // of buttons; the chosen one is a step lighter.
            let chip = |active: bool| {
                h_flex()
                    .h(rems_from_px(26.))
                    .px(rems_from_px(9.))
                    .gap(rems_from_px(5.))
                    .items_center()
                    .flex_none()
                    .rounded(rems_from_px(6.))
                    .border_1()
                    .text_size(rems_from_px(12.5))
                    .cursor_pointer()
                    .map(move |el| {
                        if active {
                            el.bg(selected_chip)
                                .border_color(selected_chip)
                                .text_color(fg)
                        } else {
                            el.bg(field)
                                .border_color(flint_theme::border())
                                .text_color(muted_fg)
                                .hover(move |s| s.bg(flint_theme::element_hover()).text_color(fg))
                        }
                    })
            };

            let mut bar = h_flex()
                .w_full()
                .px(rems_from_px(16.))
                .pb(rems_from_px(4.))
                .gap(rems_from_px(6.))
                .flex_wrap()
                .child(
                    chip(!has_filters)
                        .id("filter-all")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.group_filter = None;
                            this.tag_filter.clear();
                            cx.notify();
                        }))
                        .child("All"),
                );

            for (ix, group) in all_groups.iter().enumerate() {
                let active = self.group_filter.as_deref() == Some(group.as_str());
                let value = group.clone();
                bar = bar.child(
                    chip(active)
                        .id(("filter-group", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // Click again to clear; picking a folder is exclusive.
                            this.group_filter =
                                if this.group_filter.as_deref() == Some(value.as_str()) {
                                    None
                                } else {
                                    Some(value.clone())
                                };
                            cx.notify();
                        }))
                        .child(
                            svg()
                                .path("icons/folder.svg")
                                .size(rems_from_px(12.))
                                .text_color(if active { fg } else { text_dim }),
                        )
                        .child(group.clone()),
                );
            }

            for (ix, tag) in all_tags.iter().enumerate() {
                let active = self.tag_filter.contains(tag);
                let value = tag.clone();
                bar = bar.child(
                    chip(active)
                        .id(("filter-tag", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.tag_filter.remove(&value) {
                                this.tag_filter.insert(value.clone());
                            }
                            cx.notify();
                        }))
                        .child(format!("#{tag}")),
                );
            }

            bar
        });

        let is_list_empty = filtered.is_empty() && is_search_empty && !has_filters;
        let body = if is_list_empty && self.is_loading_hosts(cx) {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(rems_from_px(13.))
                .text_color(muted_fg)
                .child("Loading your hosts…")
                .into_any_element()
        } else if is_list_empty {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(rems_from_px(8.))
                .child(
                    div()
                        .text_size(rems_from_px(15.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(fg)
                        .child("No hosts yet"),
                )
                .map(|el| match ssh_config_offer(self.ssh_config_host_count) {
                    Some(offer) => el
                        .child(
                            div()
                                .text_size(rems_from_px(13.))
                                .text_color(muted_fg)
                                .child(offer),
                        )
                        .child(
                            h_flex()
                                .gap(rems_from_px(6.))
                                .child(import_btn("import-hosts-empty").child("Import them"))
                                .child(add_server_btn("add-server-empty")),
                        ),
                    None => el.child(add_server_btn("add-server-empty")),
                })
                .when_some(keymap::label("app.new_host", cx), |el, keys| {
                    el.child(
                        div()
                            .text_size(rems_from_px(12.))
                            .text_color(text_dim)
                            .child(format!("or press {keys} to add a host")),
                    )
                })
                .into_any_element()
        } else if filtered.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(rems_from_px(13.))
                .text_color(muted_fg)
                .child(if is_search_empty {
                    "No hosts match the current filters".to_string()
                } else {
                    format!("No hosts match \"{}\"", search_text)
                })
                .into_any_element()
        } else {
            // Folder sections first (alphabetical), then the ungrouped hosts.
            let mut grouped: BTreeMap<String, Vec<ServerEntrySummary>> = BTreeMap::new();
            let mut ungrouped: Vec<ServerEntrySummary> = Vec::new();
            for e in filtered {
                match e.group_name.clone() {
                    Some(g) => grouped.entry(g).or_default().push(e),
                    None => ungrouped.push(e),
                }
            }
            let has_groups = !grouped.is_empty();

            let group_ids: std::collections::HashMap<String, Uuid> = self
                .vault_state
                .read(cx)
                .manager()
                .map(|vm| {
                    vm.list_group_paths()
                        .into_iter()
                        .map(|(id, path)| (path, id))
                        .collect()
                })
                .unwrap_or_default();
            let now = crate::ui::sidebar::unix_now();
            let mut content: Vec<gpui::AnyElement> = Vec::new();
            let mut ix = 0usize;
            for (group_name, entries) in grouped {
                let group = group_ids.get(&group_name).copied();
                content.push(self.group_header(&group_name, group, entries.len(), cx));
                let count = entries.len();
                content.push(self.host_card(entries, ix, now, cx));
                ix += count;
            }

            if !ungrouped.is_empty() {
                // A heading only when folders render above; alone, the card
                // just needs room under the filters.
                if has_groups {
                    content.push(self.group_header("Ungrouped", None, ungrouped.len(), cx));
                } else {
                    content.push(div().h(rems_from_px(8.)).into_any_element());
                }
                content.push(self.host_card(ungrouped, ix, now, cx));
            }

            div()
                .id("servers-list")
                .flex_1()
                .overflow_y_scroll()
                .px(rems_from_px(16.))
                .pb(rems_from_px(16.))
                .child(v_flex().w_full().children(content))
                .into_any_element()
        };

        let selection_bar = self.selection_bar(cx);

        // No background of its own: the card it sits in paints it, rounded.
        div()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(|this, _: &DeleteSelectedHosts, _, cx| this.delete_selected(cx)))
            .on_action(cx.listener(|this, _: &FocusHostSearch, window, cx| {
                this.search_input
                    .update(cx, |input, cx| input.focus(window, cx));
            }))
            .child(
                v_flex()
                    .size_full()
                    .child(header)
                    .children(filter_bar)
                    .children(selection_bar)
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_keys_walk_the_list_and_stop_at_its_ends() {
        let [a, b, c] = [(); 3].map(|_| Uuid::new_v4());
        let order = [a, b, c];
        assert_eq!(step_host(&order, None, true), Some(a));
        assert_eq!(step_host(&order, Some(a), true), Some(b));
        assert_eq!(step_host(&order, Some(c), true), Some(c));
        assert_eq!(step_host(&order, Some(a), false), Some(a));
        assert_eq!(step_host(&order, Some(Uuid::new_v4()), false), Some(a));
        assert_eq!(step_host(&[], None, true), None);
    }

    #[test]
    fn shift_click_takes_every_host_between_both_ways() {
        let [a, b, c, d] = [(); 4].map(|_| Uuid::new_v4());
        let order = [a, b, c, d];
        assert_eq!(host_range(&order, b, d), vec![b, c, d]);
        assert_eq!(host_range(&order, c, a), vec![a, b, c]);
        assert_eq!(host_range(&order, Uuid::new_v4(), c), vec![c]);
    }

    #[test]
    fn the_ssh_config_offer_counts_hosts_and_hides_when_there_are_none() {
        assert_eq!(ssh_config_offer(0), None);
        assert_eq!(
            ssh_config_offer(1).as_deref(),
            Some("Found 1 host in your SSH config")
        );
        assert_eq!(
            ssh_config_offer(40).as_deref(),
            Some("Found 40 hosts in your SSH config")
        );
    }
}
