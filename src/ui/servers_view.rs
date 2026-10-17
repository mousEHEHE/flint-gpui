use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::theme as flint_theme;
use crate::ui::context_menu::{ContextMenuItem, PopoverMenu};
use gpui::prelude::*;
use gpui::{
    div, px, svg, Anchor, ClipboardItem, Context, Entity, EventEmitter, Focusable, MouseButton,
    MouseDownEvent, Window,
};
use gpui_component::{
    h_flex,
    input::{Input, InputState},
    v_flex, ActiveTheme as _, Icon, IconName,
};
use uuid::Uuid;

use crate::ui::host_sharing;
use crate::ui::VaultState;
use crate::vault::ServerEntrySummary;

const SERVER_ROW_GROUP: &str = "server-row";

#[cfg(target_os = "macos")]
const ADD_HOST_HINT: &str = "or press Cmd+T to add a host";
#[cfg(not(target_os = "macos"))]
const ADD_HOST_HINT: &str = "or press Ctrl+Shift+T to add a host";

/// The empty-state line that offers the hosts of `~/.ssh/config`.
fn ssh_config_offer(host_count: usize) -> Option<String> {
    match host_count {
        0 => None,
        1 => Some("Found 1 host in your SSH config".to_string()),
        count => Some(format!("Found {count} hosts in your SSH config")),
    }
}

pub enum ServersViewEvent {
    OpenServer(ServerEntrySummary),
    AddServerRequested,
    ImportRequested,
    EditServerRequested(Uuid),
    DuplicateServerRequested(Uuid),
    DeleteServerRequested(Uuid),
}

pub struct ServersView {
    vault_state: Entity<VaultState>,
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
}

impl EventEmitter<ServersViewEvent> for ServersView {}

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
            vault_state,
            search_input,
            search: String::new(),
            open_menu_for: None,
            menu_dismissed_at: None,
            connected_ids: std::collections::HashSet::new(),
            group_filter: None,
            tag_filter: std::collections::HashSet::new(),
            ssh_config_host_count: 0,
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

        let trigger = div()
            .id(("server-row-more-btn", ix))
            .size(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(3.))
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
            .child(Icon::new(IconName::Ellipsis).size(px(12.)));

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

        PopoverMenu::new(("server-row-more", ix), is_menu_open, trigger)
            .anchor(Anchor::TopRight)
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
            .items(sharing_items)
            .item(
                ContextMenuItem::new(
                    "row-menu-delete",
                    "Delete",
                    cx.listener(move |this, _, _, cx| {
                        this.open_menu_for = None;
                        cx.emit(ServersViewEvent::DeleteServerRequested(id));
                        cx.notify();
                    }),
                )
                .danger(),
            )
            .on_dismiss(cx.listener(move |this, _, _, cx| {
                this.menu_dismissed_at = Some((id, Instant::now()));
                this.open_menu_for = None;
                cx.notify();
            }))
    }

    /// Section heading for a host folder: name + count, quiet and small.
    fn group_header(&self, name: &str, count: usize) -> gpui::AnyElement {
        let text_dim = flint_theme::text_dim();
        h_flex()
            .w_full()
            .h(px(22.))
            .mt(px(6.))
            .px(px(8.))
            .gap(px(6.))
            .items_center()
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(text_dim)
                    .child(name.to_uppercase()),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(text_dim)
                    .child(format!("{count}")),
            )
            .into_any_element()
    }

    /// One host in the list. A single 26px line: status, name, address, and
    /// the OS/tag chips pushed to the right.
    fn server_row(
        &self,
        ix: usize,
        entry: ServerEntrySummary,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let text_dim = flint_theme::text_dim();
        let list_hover = theme.list_hover;
        let status_online = flint_theme::status_online();
        let status_off = flint_theme::status_off();

        let id = entry.id;
        let name = if entry.label.is_empty() {
            entry.host.clone()
        } else {
            entry.label.clone()
        };
        let addr = format!("{}@{}:{}", entry.username, entry.host, entry.port);
        // Detected OS (filled on connect); nothing shows until then.
        let os = entry.os.clone().filter(|os| !os.trim().is_empty());
        let tag_names = entry.tag_names.clone();
        let team_name = entry.team_name.clone();
        let is_menu_open = self.open_menu_for == Some(id);
        let is_connected = self.connected_ids.contains(&id.to_string());
        let group_id: gpui::SharedString = format!("{SERVER_ROW_GROUP}-{ix}").into();

        h_flex()
            .id(("srow", ix))
            .group(group_id.clone())
            .w_full()
            .h(px(26.))
            .px(px(8.))
            .gap(px(8.))
            .items_center()
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|s| s.bg(list_hover))
            .when(is_menu_open, |el| el.bg(list_hover))
            .active(|s| s.bg(flint_theme::element_active()))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(ServersViewEvent::OpenServer(entry.clone()));
            }))
            .child(
                div()
                    .size(px(5.))
                    .flex_none()
                    .rounded_full()
                    .bg(if is_connected {
                        status_online
                    } else {
                        status_off
                    }),
            )
            .child(
                div()
                    .w(px(200.))
                    .flex_none()
                    .text_size(px(12.5))
                    .text_color(fg)
                    .truncate()
                    .child(name),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.))
                    .text_color(text_dim)
                    .truncate()
                    .child(addr),
            )
            .children(os.map(|os| {
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(text_dim)
                    .child(os)
            }))
            .children(team_name.map(|team| {
                h_flex()
                    .flex_none()
                    .gap(px(3.))
                    .items_center()
                    .text_size(px(10.5))
                    .text_color(text_dim)
                    .child(Icon::new(IconName::User).size(px(10.)))
                    .child(team)
            }))
            .children(tag_names.into_iter().map(|t| {
                div()
                    .flex_none()
                    .text_size(px(10.5))
                    .text_color(text_dim)
                    .child(format!("#{t}"))
            }))
            .child(
                div()
                    .flex_none()
                    .when(!is_menu_open, |el| {
                        el.invisible()
                            .group_hover(group_id.clone(), |s| s.visible())
                    })
                    .child(self.row_more_menu(ix, id, is_menu_open, cx)),
            )
            .into_any_element()
    }
}

impl Render for ServersView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self
            .vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_entries().ok())
            .unwrap_or_default();

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

        let search_lower = self.search.to_lowercase();
        let filtered: Vec<ServerEntrySummary> = entries
            .into_iter()
            .filter(|e| {
                self.group_filter
                    .as_ref()
                    .is_none_or(|g| e.group_name.as_deref() == Some(g.as_str()))
            })
            .filter(|e| self.tag_filter.iter().all(|t| e.tag_names.contains(t)))
            .filter(|e| {
                if search_lower.is_empty() {
                    return true;
                }
                let name = if e.label.is_empty() {
                    &e.host
                } else {
                    &e.label
                };
                name.to_lowercase().contains(&search_lower)
                    || e.host.to_lowercase().contains(&search_lower)
                    || e.username.to_lowercase().contains(&search_lower)
                    || e.tag_names
                        .iter()
                        .any(|t| t.to_lowercase().contains(&search_lower))
                    || e.group_name
                        .as_ref()
                        .is_some_and(|g| g.to_lowercase().contains(&search_lower))
            })
            .collect();

        let is_search_empty = self.search.is_empty();
        let search_text = self.search.clone();

        let theme = cx.theme();
        let bg = theme.background;
        let border = theme.title_bar_border;
        let fg = theme.foreground;
        let text_dim = flint_theme::text_dim();
        let muted_fg = theme.muted_foreground;
        let bg_element = theme.input;
        let accent = theme.primary;
        let accent_soft = flint_theme::accent_soft();
        let accent_line = flint_theme::accent_line();
        let border_focused = flint_theme::border_focused();

        let add_server_btn = |id: &'static str| {
            h_flex()
                .id(id)
                .h(px(26.))
                .pl(px(9.))
                .pr(px(11.))
                .gap(px(6.))
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .border_1()
                .border_color(accent_line)
                .bg(accent_soft)
                .text_size(px(11.5))
                .text_color(accent)
                .cursor_pointer()
                .hover(move |s| s.bg(accent_soft).border_color(accent))
                .active(move |s| s.bg(flint_theme::accent_soft_press()).border_color(accent))
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.emit(ServersViewEvent::AddServerRequested);
                }))
                .child(Icon::new(IconName::Plus).size(px(12.)).text_color(accent))
                .child(div().child("New host"))
        };

        let search_focused = self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let search_field = h_flex()
            .flex_1()
            .h(px(26.))
            .pl(px(9.))
            .pr(px(6.))
            .gap(px(7.))
            .items_center()
            .rounded(px(4.))
            .bg(bg_element)
            .border_1()
            .border_color(if search_focused {
                accent
            } else {
                border_focused
            })
            .child(
                Icon::new(IconName::Search)
                    .size(px(12.))
                    .text_color(text_dim),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.search_input).appearance(false)),
            );

        let import_btn = |id: &'static str| {
            h_flex()
                .id(id)
                .h(px(26.))
                .px(px(10.))
                .gap(px(6.))
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .border_1()
                .border_color(border_focused)
                .bg(bg_element)
                .text_size(px(11.5))
                .text_color(muted_fg)
                .cursor_pointer()
                .hover(move |s| s.bg(flint_theme::element_hover()).text_color(fg))
                .active(move |s| s.bg(flint_theme::element_active()).text_color(fg))
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.emit(ServersViewEvent::ImportRequested);
                }))
        };

        let header = h_flex()
            .w_full()
            .px(px(8.))
            .py(px(8.))
            .gap(px(6.))
            .child(search_field)
            .child(import_btn("import-hosts-header").child("Import"))
            .child(add_server_btn("add-server-header"));

        // "All" resets, one group at a time, tags stack.
        let filter_bar = (!all_groups.is_empty() || !all_tags.is_empty()).then(|| {
            let chip = |active: bool| {
                h_flex()
                    .h(px(20.))
                    .px(px(7.))
                    .gap(px(4.))
                    .items_center()
                    .flex_none()
                    .rounded(px(3.))
                    .border_1()
                    .text_size(px(10.5))
                    .cursor_pointer()
                    .map(move |el| {
                        if active {
                            el.bg(accent_soft)
                                .border_color(accent_line)
                                .text_color(accent)
                        } else {
                            el.bg(bg_element)
                                .border_color(border)
                                .text_color(muted_fg)
                                .hover(move |s| s.text_color(fg).border_color(border_focused))
                        }
                    })
            };

            let mut bar = h_flex()
                .w_full()
                .px(px(8.))
                .pb(px(8.))
                .gap(px(4.))
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
                                .size(px(10.))
                                .text_color(if active { accent } else { text_dim }),
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

        let body = if filtered.is_empty() && is_search_empty && !has_filters {
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(fg)
                        .child("No hosts yet"),
                )
                .map(|el| match ssh_config_offer(self.ssh_config_host_count) {
                    Some(offer) => el
                        .child(div().text_size(px(12.)).text_color(muted_fg).child(offer))
                        .child(
                            h_flex()
                                .gap(px(6.))
                                .child(import_btn("import-hosts-empty").child("Import them"))
                                .child(add_server_btn("add-server-empty")),
                        ),
                    None => el.child(add_server_btn("add-server-empty")),
                })
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(text_dim)
                        .child(ADD_HOST_HINT),
                )
                .into_any_element()
        } else if filtered.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.))
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

            let mut content: Vec<gpui::AnyElement> = Vec::new();
            let mut ix = 0usize;
            for (group_name, entries) in grouped {
                content.push(self.group_header(&group_name, entries.len()));
                for entry in entries {
                    content.push(self.server_row(ix, entry, cx));
                    ix += 1;
                }
            }

            // Ungrouped hosts get a heading only when folders render above.
            if has_groups && !ungrouped.is_empty() {
                content.push(self.group_header("Ungrouped", ungrouped.len()));
            }
            for entry in ungrouped {
                content.push(self.server_row(ix, entry, cx));
                ix += 1;
            }

            div()
                .id("servers-list")
                .flex_1()
                .overflow_y_scroll()
                .px(px(8.))
                .pb(px(8.))
                .child(v_flex().w_full().children(content))
                .into_any_element()
        };

        div().size_full().bg(bg).child(
            v_flex()
                .size_full()
                .child(header)
                .children(filter_bar)
                .child(body),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
