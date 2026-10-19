//! The Teams section of Settings: make a team, invite people, let them in,
//! and choose which hosts the team sees, in which vault, and who sees it.

use std::collections::BTreeSet;

use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, ClipboardItem, Context, Div, Entity, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
};
use secrecy::SecretString;
use uuid::Uuid;

use crate::account::team::Roster;
use crate::ui::sync_service::{SyncService, TeamAction};
use crate::ui::VaultState;

use super::ago;

pub struct TeamsPane {
    vault_state: Entity<VaultState>,
    service: Entity<SyncService>,
    team_name: Entity<InputState>,
    invite_link: Entity<InputState>,
    /// For the first use of teams, next to the create and join fields.
    first_password: Entity<InputState>,
    /// For leaving or deleting, next to the team it is asked for.
    confirm_password: Entity<InputState>,
    host_filter: Entity<InputState>,
    /// For a new vault, or a vault being renamed.
    vault_name: Entity<InputState>,
    /// What is open under a vault. One thing at a time.
    open: Option<Open>,
    /// The plain members ticked in the open "Who sees it" list.
    access_draft: BTreeSet<Uuid>,
    /// The team the user asked to leave or delete and has to confirm.
    confirming: Option<Uuid>,
    /// The button that started the action now running. It shows a spinner.
    pressed: Option<SharedString>,
    /// Team and member the owner is about to hand the team to.
    handing_over: Option<(Uuid, Uuid)>,
    /// The vault whose deletion was just refused. Its form is still open and
    /// shows why, so the pane does not say it a second time at the top.
    failed_delete: Option<Uuid>,
}

#[derive(Clone, Copy, PartialEq)]
enum Open {
    /// The host list of this vault.
    Hosts(Uuid),
    Access {
        team_id: Uuid,
        vault_id: Uuid,
    },
    Rename {
        team_id: Uuid,
        vault_id: Uuid,
    },
    Delete {
        team_id: Uuid,
        vault_id: Uuid,
    },
    /// The name field for a new vault of this team.
    NewVault(Uuid),
}

/// A host that is in the vault, or could be put there.
struct HostRow {
    id: Uuid,
    label: String,
    /// `user@host`, shown next to the label.
    address: String,
    is_shared: bool,
}

/// One team as the pane shows it.
struct TeamRow {
    id: Uuid,
    /// This account.
    me: Uuid,
    name: String,
    is_owner: bool,
    is_manager: bool,
    /// User id, name as shown and role, the owner first.
    members: Vec<(Uuid, String, String)>,
    /// Invite id and link.
    invite_links: Vec<(Uuid, String)>,
    /// The vaults this account holds, in the roster's order.
    vaults: Vec<VaultRow>,
    /// The team has one vault, without a name, that every member sees. It
    /// is shown as one line, with nothing about vaults.
    is_plain: bool,
}

struct VaultRow {
    id: Uuid,
    name: String,
    /// Everyone who sees the vault.
    holders: Vec<Uuid>,
    /// The vault's hosts first, then the personal ones.
    hosts: Vec<HostRow>,
}

/// The button that deletes a vault for good, in the form under the vault.
fn delete_confirm_id(vault_id: Uuid) -> SharedString {
    SharedString::from(format!("vault-delete-confirm-{vault_id}"))
}

fn trash_icon() -> Icon {
    Icon::empty().path("icons/trash.svg")
}

fn caption_text(cx: &App, text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.5))
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn line_text(cx: &App, text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(12.5))
        .text_color(cx.theme().foreground)
        .child(text.into())
}

fn checkbox(is_ticked: bool, cx: &App) -> Div {
    let accent = cx.theme().primary;
    div()
        .w(px(13.))
        .h(px(13.))
        .flex_none()
        .rounded(px(3.))
        .border_1()
        .border_color(if is_ticked {
            accent
        } else {
            crate::theme::border_focused()
        })
        .bg(if is_ticked {
            accent
        } else {
            crate::theme::modal_rail()
        })
        .flex()
        .items_center()
        .justify_center()
        .when(is_ticked, |el| {
            el.child(Icon::new(IconName::Check).size(px(9.)))
        })
}

/// A member as the pane names them. The server tells no login for someone
/// who left the team and is still in its roster.
fn member_name(login: Option<&String>) -> String {
    match login {
        Some(login) => format!("@{login}"),
        None => "someone who left".to_string(),
    }
}

/// "1 host", "3 hosts".
fn host_count(count: usize) -> String {
    if count == 1 {
        "1 host".to_string()
    } else {
        format!("{count} hosts")
    }
}

/// Who sees a vault, in a few words.
fn audience(holders: usize, members: usize) -> String {
    if holders >= members {
        "everyone".to_string()
    } else {
        format!("{holders} of {members} people")
    }
}

/// The line a team with one plain vault shows about its hosts.
fn shared_summary(shared: &[&HostRow]) -> String {
    let count = shared.len();
    if count == 0 {
        return "No hosts shared yet.".to_string();
    }
    let names: Vec<&str> = shared
        .iter()
        .take(4)
        .map(|row| row.label.as_str())
        .collect();
    let more = if count > 4 {
        format!(" and {} more", count - 4)
    } else {
        String::new()
    };
    format!("Shared hosts: {}{more}", names.join(", "))
}

impl TeamsPane {
    pub fn new(
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let service = SyncService::global(cx);
        cx.observe(&service, |this, service, cx| {
            let service = service.read(cx);
            if !service.is_team_busy {
                this.note_delete_outcome(service.team_error.is_some());
                this.pressed = None;
            }
            cx.notify();
        })
        .detach();
        cx.observe(&vault_state, |_, _, cx| cx.notify()).detach();

        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let masked = |window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Master password")
                    .masked(true)
            })
        };

        let host_filter = input("Filter hosts…", window, cx);
        cx.subscribe(&host_filter, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        let confirm_password = masked(window, cx);
        cx.subscribe_in(
            &confirm_password,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if !matches!(event, InputEvent::PressEnter { .. }) {
                    return;
                }
                match this.open {
                    Some(Open::Delete { .. }) => this.delete_vault(window, cx),
                    _ => this.confirm(window, cx),
                }
            },
        )
        .detach();
        let vault_name = input("Vault name", window, cx);
        cx.subscribe_in(
            &vault_name,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_vault_name(window, cx);
                }
            },
        )
        .detach();

        Self {
            team_name: input("Team name", window, cx),
            invite_link: input("flint://invite/…", window, cx),
            first_password: masked(window, cx),
            confirm_password,
            host_filter,
            vault_name,
            vault_state,
            service,
            open: None,
            access_draft: BTreeSet::new(),
            confirming: None,
            pressed: None,
            handing_over: None,
            failed_delete: None,
        }
    }

    /// A team action ended. If it was the deletion of the vault whose form
    /// is open, the form closes, or stays to say what went wrong: a wrong
    /// password is reported where it was typed.
    fn note_delete_outcome(&mut self, has_failed: bool) {
        let Some(Open::Delete { vault_id, .. }) = self.open else {
            return;
        };
        if self.pressed != Some(delete_confirm_id(vault_id)) {
            return;
        }
        if has_failed {
            self.failed_delete = Some(vault_id);
        } else {
            self.open = None;
        }
    }

    /// Start a team action from the button `pressed`.
    fn run(
        &mut self,
        pressed: impl Into<SharedString>,
        action: TeamAction,
        password: Option<SecretString>,
        cx: &mut Context<Self>,
    ) {
        self.pressed = Some(pressed.into());
        self.failed_delete = None;
        self.service
            .update(cx, |service, cx| service.team_action(action, password, cx));
        cx.notify();
    }

    /// Take what is typed in a password field and empty the field.
    fn take_password(
        field: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<SecretString> {
        let password = field.read(cx).value().to_string();
        field.update(cx, |state, cx| state.set_value("", window, cx));
        (!password.is_empty()).then(|| SecretString::new(password))
    }

    /// Whether the account has made its team keys. Until then the first
    /// action needs the master password.
    fn has_team_keys(&self, cx: &Context<Self>) -> bool {
        let vault_state = self.vault_state.read(cx);
        let state = vault_state.manager().and_then(|vault| vault.sync_state());
        state.is_some_and(|state| state.key_seed.is_some())
    }

    fn create_team(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.team_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        self.team_name
            .update(cx, |state, cx| state.set_value("", window, cx));
        let password = Self::take_password(&self.first_password, window, cx);
        self.run("team-create", TeamAction::Create { name }, password, cx);
    }

    fn join_team(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.invite_link.read(cx).value().trim().to_string();
        if link.is_empty() {
            return;
        }
        self.invite_link
            .update(cx, |state, cx| state.set_value("", window, cx));
        let password = Self::take_password(&self.first_password, window, cx);
        self.run("team-join", TeamAction::Join { link }, password, cx);
    }

    /// Leave or delete the team being confirmed, with the password typed
    /// next to it.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(team_id) = self.confirming else {
            return;
        };
        if self.is_busy(cx) {
            return;
        }
        let Some(password) = Self::take_password(&self.confirm_password, window, cx) else {
            return;
        };
        let is_owner = self
            .teams(cx)
            .is_some_and(|teams| teams.iter().any(|team| team.id == team_id && team.is_owner));
        let action = if is_owner {
            TeamAction::Delete { team_id }
        } else {
            TeamAction::Leave { team_id }
        };
        self.confirming = None;
        self.run(format!("leave-{team_id}"), action, Some(password), cx);
    }

    fn teams(&self, cx: &Context<Self>) -> Option<Vec<TeamRow>> {
        let vault = self.vault_state.read(cx).manager()?;
        let state = vault.sync_state()?;
        state.session_token.as_ref()?;
        let hosts = vault.list_entries().unwrap_or_default();

        let rows = state
            .teams
            .iter()
            .filter_map(|(id, team)| {
                let roster = Roster::parse(&team.roster)?;
                let mut members: Vec<(Uuid, String, String)> = roster
                    .members
                    .iter()
                    .map(|member| {
                        let name = member_name(team.logins.get(&member.user_id));
                        (member.user_id, name, member.role.clone())
                    })
                    .collect();
                members.sort_by_key(|(_, login, role)| (role != "owner", login.clone()));
                Some(TeamRow {
                    id: *id,
                    me: state.user_id,
                    name: team.name.clone(),
                    is_owner: roster.owner() == Some(state.user_id),
                    is_manager: roster.is_manager(state.user_id),
                    members,
                    invite_links: state
                        .invites
                        .iter()
                        .filter(|(_, invite)| invite.team_id == *id)
                        .map(|(invite_id, invite)| (*invite_id, invite.link.clone()))
                        .collect(),
                    is_plain: is_plain(&roster),
                    vaults: vault
                        .team_vaults(*id)
                        .into_iter()
                        .map(|(vault_id, name)| VaultRow {
                            id: vault_id,
                            name,
                            holders: roster.holders(vault_id),
                            hosts: host_rows(&hosts, |host_id| vault.vault_of(host_id), vault_id),
                        })
                        .collect(),
                })
            })
            .collect();
        Some(rows)
    }

    /// Put a host into a team vault, or with `None` make it personal again.
    fn set_shared(&mut self, host_id: Uuid, vault_id: Option<Uuid>, cx: &mut Context<Self>) {
        self.vault_state.update(cx, |vault_state, cx| {
            let Some(vault) = vault_state.manager_mut() else {
                return;
            };
            let saved = match vault_id {
                Some(vault_id) => vault.share_entry(host_id, vault_id),
                None => vault.unshare_entry(host_id),
            };
            if let Err(e) = saved {
                tracing::error!(?e, "failed to save the vault after sharing a host");
            }
            cx.notify();
        });
        self.service.update(cx, |service, cx| service.sync_now(cx));
    }

    /// Open one panel and close whatever else was open, with its fields.
    fn open_panel(&mut self, open: Option<Open>, window: &mut Window, cx: &mut Context<Self>) {
        self.open = open;
        self.confirming = None;
        self.failed_delete = None;
        for field in [&self.confirm_password, &self.vault_name, &self.host_filter] {
            field.update(cx, |state, cx| state.set_value("", window, cx));
        }
        match open {
            Some(Open::Delete { .. }) => {
                self.confirm_password
                    .update(cx, |state, cx| state.focus(window, cx));
            }
            Some(Open::NewVault(_) | Open::Rename { .. }) => {
                self.vault_name
                    .update(cx, |state, cx| state.focus(window, cx));
            }
            _ => {}
        }
        cx.notify();
    }

    /// Create the vault being named, or rename the one being renamed.
    fn submit_vault_name(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.vault_name.read(cx).value().trim().to_string();
        if name.is_empty() || self.is_busy(cx) {
            return;
        }
        let (pressed, action) = match self.open {
            Some(Open::NewVault(team_id)) => (
                format!("vault-new-{team_id}"),
                TeamAction::AddVault { team_id, name },
            ),
            Some(Open::Rename { team_id, vault_id }) => (
                format!("vault-rename-{vault_id}"),
                TeamAction::RenameVault {
                    team_id,
                    vault_id,
                    name,
                },
            ),
            _ => return,
        };
        self.open_panel(None, window, cx);
        self.run(pressed, action, None, cx);
    }

    fn delete_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Open::Delete { team_id, vault_id }) = self.open else {
            return;
        };
        if self.is_busy(cx) {
            return;
        }
        let Some(password) = Self::take_password(&self.confirm_password, window, cx) else {
            return;
        };
        // The form stays open until the action is through.
        let action = TeamAction::DeleteVault { team_id, vault_id };
        self.run(delete_confirm_id(vault_id), action, Some(password), cx);
    }

    fn save_access(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Open::Access { team_id, vault_id }) = self.open else {
            return;
        };
        let user_ids = self.access_draft.iter().copied().collect();
        self.open_panel(None, window, cx);
        let action = TeamAction::SetAccess {
            team_id,
            vault_id,
            user_ids,
        };
        self.run(format!("vault-access-{vault_id}"), action, None, cx);
    }

    /// A team action is running. Enter in a field must wait like the buttons
    /// do: the service takes one action at a time and drops the rest.
    fn is_busy(&self, cx: &App) -> bool {
        self.service.read(cx).is_team_busy
    }

    fn is_spinning(&self, id: &SharedString, cx: &App) -> bool {
        self.service.read(cx).is_team_busy && self.pressed.as_ref() == Some(id)
    }

    /// The vaults of a team card, each with whatever is open under it.
    fn vaults_section(&self, team: &TeamRow, cx: &mut Context<Self>) -> Vec<AnyElement> {
        if team.vaults.is_empty() {
            let text = "No vault of this team is shared with you.";
            return vec![caption_text(cx, text).into_any_element()];
        }
        let mut parts = Vec::new();
        if !team.is_plain {
            parts.push(caption_text(cx, "Vaults").into_any_element());
        }
        for vault in &team.vaults {
            parts.push(self.vault_row(team, vault, cx));
            parts.extend(self.vault_panel(team, vault, cx));
        }
        if self.open == Some(Open::NewVault(team.id)) {
            let save_id = SharedString::from(format!("vault-create-{}", team.id));
            parts.push(self.name_form(save_id, "Create vault", cx));
        }
        parts
    }

    fn vault_row(&self, team: &TeamRow, vault: &VaultRow, cx: &mut Context<Self>) -> AnyElement {
        let (team_id, vault_id) = (team.id, vault.id);
        let is_busy = self.service.read(cx).is_team_busy;
        let shared: Vec<&HostRow> = vault.hosts.iter().filter(|row| row.is_shared).collect();
        let is_choosing = self.open == Some(Open::Hosts(vault_id));

        let mut row = h_flex().h(px(24.)).gap(px(8.)).items_center();
        row = if team.is_plain {
            row.child(line_text(cx, shared_summary(&shared)).min_w_0().truncate())
        } else {
            let about = format!(
                "{} · {}",
                host_count(shared.len()),
                audience(vault.holders.len(), team.members.len())
            );
            row.child(line_text(cx, vault.name.clone()))
                .child(caption_text(cx, about))
        };
        row = row.child(div().flex_1()).child(
            Button::new(SharedString::from(format!("hosts-toggle-{vault_id}")))
                .ghost()
                .xsmall()
                .child(if is_choosing {
                    "Done"
                } else {
                    "Choose hosts…"
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    let open = if is_choosing {
                        None
                    } else {
                        Some(Open::Hosts(vault_id))
                    };
                    this.open_panel(open, window, cx);
                })),
        );
        if !team.is_manager || team.is_plain {
            return row.into_any_element();
        }

        let access_id = SharedString::from(format!("vault-access-{vault_id}"));
        let rename_id = SharedString::from(format!("vault-rename-{vault_id}"));
        let delete_id = SharedString::from(format!("vault-delete-{vault_id}"));
        let ticked: BTreeSet<Uuid> = vault.holders.iter().copied().collect();
        let name = vault.name.clone();
        row = row
            .child(
                Button::new(access_id.clone())
                    .ghost()
                    .xsmall()
                    .child("Who sees it…")
                    .loading(self.is_spinning(&access_id, cx))
                    .disabled(is_busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_panel(Some(Open::Access { team_id, vault_id }), window, cx);
                        this.access_draft = ticked.clone();
                    })),
            )
            .child(
                Button::new(rename_id.clone())
                    .ghost()
                    .xsmall()
                    .child("Rename")
                    .loading(self.is_spinning(&rename_id, cx))
                    .disabled(is_busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_panel(Some(Open::Rename { team_id, vault_id }), window, cx);
                        this.vault_name
                            .update(cx, |state, cx| state.set_value(name.clone(), window, cx));
                    })),
            );
        // The first vault stays: it is the one new members get.
        if team
            .vaults
            .first()
            .is_some_and(|first| first.id != vault_id)
        {
            row = row.child(
                Button::new(delete_id)
                    .ghost()
                    .xsmall()
                    .child("Delete")
                    .disabled(is_busy)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_panel(Some(Open::Delete { team_id, vault_id }), window, cx);
                    })),
            );
        }
        row.into_any_element()
    }

    /// What is open under this vault, if anything.
    fn vault_panel(
        &self,
        team: &TeamRow,
        vault: &VaultRow,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (team_id, vault_id) = (team.id, vault.id);
        match self.open? {
            Open::Hosts(id) if id == vault_id => Some(self.host_picker(vault, cx)),
            open if open == (Open::Access { team_id, vault_id }) => {
                Some(self.access_list(team, cx))
            }
            open if open == (Open::Rename { team_id, vault_id }) => {
                let save_id = SharedString::from(format!("vault-rename-save-{vault_id}"));
                Some(self.name_form(save_id, "Rename", cx))
            }
            open if open == (Open::Delete { team_id, vault_id }) => {
                Some(self.delete_form(vault_id, cx))
            }
            _ => None,
        }
    }

    fn host_picker(&self, vault: &VaultRow, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, muted_fg) = (theme.foreground, theme.muted_foreground);
        let (border, list_hover) = (theme.title_bar_border, theme.list_hover);
        let vault_id = vault.id;
        let filter = self.host_filter.read(cx).value().to_string();
        let rows: Vec<&HostRow> = vault
            .hosts
            .iter()
            .filter(|row| matches_filter(row, &filter))
            .collect();

        let mut list = v_flex()
            .id(SharedString::from(format!("hosts-{vault_id}")))
            .max_h(px(220.))
            .overflow_y_scroll()
            .rounded(px(4.))
            .border_1()
            .border_color(border)
            .py(px(2.));
        if rows.is_empty() {
            list = list.child(
                div()
                    .px(px(8.))
                    .py(px(6.))
                    .child(caption_text(cx, "No hosts match.")),
            );
        }
        for row in rows {
            let (host_id, is_shared) = (row.id, row.is_shared);
            let target = if is_shared { None } else { Some(vault_id) };
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("host-{vault_id}-{host_id}")))
                    .h(px(24.))
                    .px(px(8.))
                    .gap(px(8.))
                    .items_center()
                    .cursor_pointer()
                    .hover(|style| style.bg(list_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_shared(host_id, target, cx);
                    }))
                    .child(checkbox(is_shared, cx))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(220.))
                            .truncate()
                            .text_size(px(12.5))
                            .text_color(fg)
                            .child(row.label.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(muted_fg)
                            .child(row.address.clone()),
                    ),
            );
        }
        v_flex()
            .w(px(460.))
            .gap(px(6.))
            .child(Input::new(&self.host_filter).small())
            .child(list)
            .child(caption_text(
                cx,
                "A ticked host, and the key it logs in with, is visible to everyone who sees \
                 this vault. They can also edit it.",
            ))
            .into_any_element()
    }

    /// The members of the team, ticked when they see the open vault.
    fn access_list(&self, team: &TeamRow, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, list_hover) = (theme.title_bar_border, theme.list_hover);
        let mut list = v_flex()
            .rounded(px(4.))
            .border_1()
            .border_color(border)
            .py(px(2.));
        for (user_id, login, role) in &team.members {
            let user_id = *user_id;
            let is_always = role != "member";
            let is_ticked = is_always || self.access_draft.contains(&user_id);
            let mut row = h_flex()
                .id(SharedString::from(format!("access-{}-{user_id}", team.id)))
                .h(px(24.))
                .px(px(8.))
                .gap(px(8.))
                .items_center()
                .child(checkbox(is_ticked, cx).when(is_always, |el| el.opacity(0.5)))
                .child(line_text(cx, login.clone()));
            if is_always {
                row = row.child(caption_text(cx, format!("{role}, always")));
            } else {
                row = row
                    .cursor_pointer()
                    .hover(|style| style.bg(list_hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.access_draft.remove(&user_id) {
                            this.access_draft.insert(user_id);
                        }
                        cx.notify();
                    }));
            }
            list = list.child(row);
        }

        let save_id = SharedString::from(format!("access-save-{}", team.id));
        v_flex()
            .w(px(340.))
            .gap(px(6.))
            .child(list)
            .child(caption_text(
                cx,
                "Someone you untick keeps what they already had, as personal copies. The \
                 vault gets a new key, so they see nothing that changes after this.",
            ))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new(save_id)
                            .small()
                            .primary()
                            .child("Save")
                            .disabled(self.service.read(cx).is_team_busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_access(window, cx)),
                            ),
                    )
                    .child(self.cancel_button(cx)),
            )
            .into_any_element()
    }

    /// The name field for a new vault or a rename, with its two buttons.
    fn name_form(
        &self,
        save_id: SharedString,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w(px(340.))
            .gap(px(8.))
            .child(Input::new(&self.vault_name).small())
            .child(
                Button::new(save_id)
                    .small()
                    .primary()
                    .child(label)
                    .disabled(self.service.read(cx).is_team_busy)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.submit_vault_name(window, cx)),
                    ),
            )
            .child(self.cancel_button(cx))
            .into_any_element()
    }

    fn delete_form(&self, vault_id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let delete_id = delete_confirm_id(vault_id);
        let service = self.service.read(cx);
        let error = service
            .team_error
            .clone()
            .filter(|_| self.failed_delete == Some(vault_id));
        let danger = cx.theme().danger;
        v_flex()
            .w(px(340.))
            .gap(px(6.))
            .child(caption_text(
                cx,
                "Deleting the vault removes it for everyone. Its hosts stay with everyone who \
                 could see them, as personal copies. Enter your master password to confirm.",
            ))
            .child(Input::new(&self.confirm_password).small().mask_toggle())
            .children(error.map(|text| div().text_size(px(12.)).text_color(danger).child(text)))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new(delete_id.clone())
                            .small()
                            .danger()
                            .icon(trash_icon())
                            .child("Delete vault")
                            .loading(self.is_spinning(&delete_id, cx))
                            .disabled(service.is_team_busy)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.delete_vault(window, cx)),
                            ),
                    )
                    .child(self.cancel_button(cx)),
            )
            .into_any_element()
    }

    fn cancel_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("vault-panel-cancel")
            .small()
            .child("Cancel")
            .on_click(cx.listener(|this, _, window, cx| this.open_panel(None, window, cx)))
    }
}

/// Whether a team is shown without its vaults: it has one, without a name,
/// and every member sees it.
fn is_plain(roster: &Roster) -> bool {
    match roster.vaults.as_slice() {
        [only] => only.name.is_none() && roster.holders(only.id).len() == roster.members.len(),
        _ => false,
    }
}

/// The hosts a vault's list shows: its own first, then the personal ones.
/// A host shared in another vault is left out; the host's menu moves it.
fn host_rows(
    hosts: &[crate::vault::ServerEntrySummary],
    vault_of: impl Fn(Uuid) -> Option<Uuid>,
    vault_id: Uuid,
) -> Vec<HostRow> {
    let mut rows: Vec<HostRow> = hosts
        .iter()
        .filter_map(|host| {
            let home = vault_of(host.id);
            if home.is_some_and(|home| home != vault_id) {
                return None;
            }
            let address = format!("{}@{}", host.username, host.host);
            // A host saved without a name goes by its address.
            let label = if host.label.trim().is_empty() {
                host.host.clone()
            } else {
                host.label.clone()
            };
            Some(HostRow {
                id: host.id,
                label,
                address,
                is_shared: home.is_some(),
            })
        })
        .collect();
    rows.sort_by_key(|row| (!row.is_shared, row.label.to_lowercase()));
    rows
}

fn matches_filter(row: &HostRow, filter: &str) -> bool {
    let filter = filter.trim().to_lowercase();
    filter.is_empty()
        || row.label.to_lowercase().contains(&filter)
        || row.address.to_lowercase().contains(&filter)
}

impl Render for TeamsPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let danger = theme.danger;
        let success = theme.success;
        let accent = theme.primary;

        let heading = move |text: String| {
            div()
                .text_size(px(13.))
                .font_semibold()
                .text_color(fg)
                .child(text)
        };
        let caption =
            move |text: String| div().text_size(px(11.5)).text_color(muted_fg).child(text);
        let note = move |text: String| caption(text).max_w(px(520.));
        let line = move |text: String| div().text_size(px(12.5)).text_color(fg).child(text);

        let pane = v_flex()
            .id("teams-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(20.))
            .gap(px(12.));

        let Some(teams) = self.teams(cx) else {
            // The page title above already says "Teams".
            return pane.child(caption(
                "Share hosts with other people. Sign in under Sync first: a team needs an account."
                    .into(),
            ));
        };

        let service = self.service.read(cx);
        let is_busy = service.is_team_busy;
        let error = service
            .team_error
            .clone()
            .filter(|_| self.failed_delete.is_none());
        let notice = service.team_notice.clone();
        let join_requests = service.join_requests.clone();
        let waiting = self
            .vault_state
            .read(cx)
            .manager()
            .map_or(0, |vault| vault.pending_joins());
        let needs_first_password = !self.has_team_keys(cx);
        let pressed = self.pressed.clone();
        // Every button is off while an action runs; the one that started it spins.
        let is_spinning = move |id: &SharedString| is_busy && pressed.as_ref() == Some(id);

        let status = match (&service.sync_error, service.is_syncing, service.last_synced) {
            (_, true, _) => caption("Updating…".into()),
            (Some(error), _, _) => caption(format!("Last sync failed: {error}")).text_color(danger),
            (None, _, Some(at)) => caption(format!("Updated {}", ago(at))),
            (None, _, None) => caption("Not synced yet".into()),
        };
        let refresh_row = h_flex().gap(px(10.)).items_center().child(status).child(
            Button::new("teams-refresh")
                .ghost()
                .xsmall()
                .icon(Icon::empty().path("icons/rotate-cw.svg"))
                .loading_icon(Icon::empty().path("icons/rotate-cw.svg"))
                .loading(service.is_syncing)
                .disabled(service.is_syncing || is_busy)
                .child("Refresh")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.service
                        .update(cx, |service, cx| service.refresh_all(cx));
                })),
        );

        let mut pane = pane
            .child(refresh_row)
            .child(note(
                "Share hosts, and the keys they log in with, with other accounts. They are \
                 encrypted for members only; the sync service cannot read them or add anyone."
                    .into(),
            ))
            .child(note(
                "Teams are free for now. Later the owner pays about $5 a month for up to 3 \
                 teams; members never pay."
                    .into(),
            ))
            .children(error.map(|text| div().text_size(px(12.)).text_color(danger).child(text)))
            .children(notice.map(|text| div().text_size(px(12.)).text_color(success).child(text)));
        if waiting > 0 {
            pane = pane.child(caption(
                "You asked to join a team. You are in once the person who sent the link opens \
                 flint and accepts."
                    .into(),
            ));
        }

        let mono = crate::ui::terminal_element::terminal_font_family(cx);
        for team in teams {
            let team_id = team.id;
            let role = match (team.is_owner, team.is_manager) {
                (true, _) => "owner",
                (false, true) => "admin",
                (false, false) => "member",
            };
            let mut card = v_flex().gap(px(6.)).mt(px(6.)).child(
                h_flex()
                    .gap(px(6.))
                    .items_center()
                    .child(heading(team.name.clone()))
                    .child(caption(role.into())),
            );

            for (user_id, login, member_role) in team.members.clone() {
                let is_me = user_id == team.me;
                let is_owner_row = member_role == "owner";
                let mut row = h_flex()
                    .h(px(24.))
                    .gap(px(8.))
                    .items_center()
                    .child(Icon::new(IconName::User).size(px(12.)).text_color(muted_fg))
                    .child(line(login))
                    .child(caption(member_role.clone()));
                if is_me {
                    row = row.child(caption("you".into()));
                }
                row = row.child(div().flex_1());

                // Roles and ownership are the owner's to give.
                if team.is_owner && !is_me && self.handing_over == Some((team_id, user_id)) {
                    let id = SharedString::from(format!("handover-{team_id}-{user_id}"));
                    row = row
                        .child(caption(
                            "Hand the team over? You stay in it as an admin.".into(),
                        ))
                        .child(
                            Button::new(id.clone())
                                .xsmall()
                                .danger()
                                .child("Hand over")
                                .loading(is_spinning(&id))
                                .disabled(is_busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.handing_over = None;
                                    let action = TeamAction::HandOver { team_id, user_id };
                                    this.run(id.clone(), action, None, cx);
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!(
                                "keep-owner-{team_id}-{user_id}"
                            )))
                            .xsmall()
                            .child("Cancel")
                            .on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.handing_over = None;
                                    cx.notify();
                                },
                            )),
                        );
                } else if team.is_owner && !is_me {
                    let (role_label, role) = if member_role == "admin" {
                        ("Make member", "member")
                    } else {
                        ("Make admin", "admin")
                    };
                    let role_id = SharedString::from(format!("role-{team_id}-{user_id}"));
                    row = row
                        .child(
                            Button::new(role_id.clone())
                                .ghost()
                                .xsmall()
                                .child(role_label)
                                .loading(is_spinning(&role_id))
                                .disabled(is_busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let action = TeamAction::SetRole {
                                        team_id,
                                        user_id,
                                        role,
                                    };
                                    this.run(role_id.clone(), action, None, cx);
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("owner-{team_id}-{user_id}")))
                                .ghost()
                                .xsmall()
                                .child("Make owner…")
                                .disabled(is_busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.handing_over = Some((team_id, user_id));
                                    cx.notify();
                                })),
                        );
                }
                if team.is_manager && !is_me && !is_owner_row {
                    let id = SharedString::from(format!("remove-{team_id}-{user_id}"));
                    row = row.child(
                        Button::new(id.clone())
                            .ghost()
                            .xsmall()
                            .child("Remove")
                            .loading(is_spinning(&id))
                            .disabled(is_busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let action = TeamAction::Remove {
                                    team_id,
                                    user_ids: vec![user_id],
                                };
                                this.run(id.clone(), action, None, cx);
                            })),
                    );
                }
                card = card.child(row);
            }

            for (invite_id, request) in join_requests.get(&team_id).cloned().unwrap_or_default() {
                let login = request.github_login.clone();
                let id = SharedString::from(format!("accept-{team_id}-{}", request.user_id));
                card = card.child(
                    h_flex()
                        .gap(px(8.))
                        .items_center()
                        .child(Icon::new(IconName::User).size(px(12.)).text_color(accent))
                        .child(line(format!("@{login} asks to join")))
                        .child(
                            Button::new(id.clone())
                                .xsmall()
                                .primary()
                                .icon(IconName::Check)
                                .child("Accept")
                                .loading(is_spinning(&id))
                                .disabled(is_busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let action = TeamAction::Accept {
                                        team_id,
                                        invite_id,
                                        request: request.clone(),
                                    };
                                    this.run(id.clone(), action, None, cx);
                                })),
                        ),
                );
            }

            for (invite_id, link) in team.invite_links.clone() {
                let id = SharedString::from(format!("revoke-{invite_id}"));
                card = card.child(
                    h_flex()
                        .h(px(24.))
                        .gap(px(8.))
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(muted_fg)
                                .font_family(mono)
                                .child(link.clone()),
                        )
                        .child(
                            Button::new(SharedString::from(format!("copy-{invite_id}")))
                                .ghost()
                                .xsmall()
                                .child("Copy link")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                                })),
                        )
                        .child(
                            Button::new(id.clone())
                                .ghost()
                                .xsmall()
                                .child("Revoke")
                                .loading(is_spinning(&id))
                                .disabled(is_busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let action = TeamAction::Revoke { team_id, invite_id };
                                    this.run(id.clone(), action, None, cx);
                                })),
                        ),
                );
            }

            if !team.invite_links.is_empty() {
                card = card.child(caption(
                    "Anyone with a link can ask to join. Links expire after 72 hours.".into(),
                ));
            }

            card = card.children(self.vaults_section(&team, cx));

            let leave_label = if team.is_owner {
                "Delete team"
            } else {
                "Leave team"
            };
            let leave_id = SharedString::from(format!("leave-{team_id}"));
            if self.confirming == Some(team_id) {
                let warning = if team.is_owner {
                    "Deleting the team removes it for every member. The hosts it shared stay \
                     in each member's vault as personal copies. Enter your master password to \
                     confirm."
                } else {
                    "The hosts the team shared stay in your vault as personal copies. Enter \
                     your master password to confirm."
                };
                card = card.child(
                    v_flex()
                        .w(px(340.))
                        .gap(px(6.))
                        .child(caption(warning.into()))
                        .child(Input::new(&self.confirm_password).small().mask_toggle())
                        .child(
                            h_flex()
                                .gap(px(8.))
                                .child(
                                    Button::new(leave_id.clone())
                                        .small()
                                        .danger()
                                        .icon(trash_icon())
                                        .child(leave_label)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.confirm(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("keep-{team_id}")))
                                        .small()
                                        .child("Cancel")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.confirming = None;
                                            this.confirm_password.update(cx, |state, cx| {
                                                state.set_value("", window, cx)
                                            });
                                            cx.notify();
                                        })),
                                ),
                        ),
                );
            } else {
                let mut actions = h_flex().gap(px(8.));
                if team.is_manager {
                    let id = SharedString::from(format!("invite-{team_id}"));
                    actions = actions.child(
                        Button::new(id.clone())
                            .small()
                            .icon(IconName::Plus)
                            .child("New invite link")
                            .loading(is_spinning(&id))
                            .disabled(is_busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.run(id.clone(), TeamAction::Invite { team_id }, None, cx);
                            })),
                    );
                    let id = SharedString::from(format!("vault-new-{team_id}"));
                    actions = actions.child(
                        Button::new(id.clone())
                            .small()
                            .icon(IconName::Plus)
                            .child("New vault…")
                            .loading(is_spinning(&id))
                            .disabled(is_busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_panel(Some(Open::NewVault(team_id)), window, cx);
                            })),
                    );
                }
                actions = actions.child(
                    Button::new(leave_id.clone())
                        .small()
                        .danger()
                        .outline()
                        .child(leave_label)
                        .loading(is_spinning(&leave_id))
                        .disabled(is_busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_panel(None, window, cx);
                            this.confirming = Some(team_id);
                            this.confirm_password
                                .update(cx, |state, cx| state.focus(window, cx));
                            cx.notify();
                        })),
                );
                card = card.child(actions);
            }
            pane = pane.child(card);
        }

        let create_id = SharedString::from("team-create");
        let join_id = SharedString::from("team-join");
        let mut forms = v_flex().gap(px(8.)).mt(px(6.)).w(px(340.));
        if needs_first_password {
            forms = forms
                .child(caption(
                    "The first time you create or join a team, flint makes the keys your \
                     account signs with. Enter your master password once to confirm."
                        .into(),
                ))
                .child(Input::new(&self.first_password).small().mask_toggle());
        }
        pane.child(
            forms
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(Input::new(&self.team_name).small())
                        .child(
                            Button::new(create_id.clone())
                                .small()
                                .primary()
                                .icon(IconName::Plus)
                                .child("Create team")
                                .loading(is_spinning(&create_id))
                                .disabled(is_busy)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.create_team(window, cx)),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(Input::new(&self.invite_link).small())
                        .child(
                            Button::new(join_id.clone())
                                .small()
                                .icon(IconName::ArrowRight)
                                .child("Join")
                                .loading(is_spinning(&join_id))
                                .disabled(is_busy)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.join_team(window, cx)),
                                ),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{ProxyChoice, ServerEntrySummary};

    fn host(label: &str, address: &str) -> ServerEntrySummary {
        ServerEntrySummary {
            id: Uuid::new_v4(),
            label: label.into(),
            host: address.into(),
            port: 22,
            username: "root".into(),
            os: None,
            last_opened_at: None,
            group_name: None,
            tag_names: Vec::new(),
            proxy: ProxyChoice::Default,
            team_name: None,
        }
    }

    #[test]
    fn a_teams_hosts_come_first_and_other_teams_hosts_are_left_out() {
        let (team, other_team) = (Uuid::new_v4(), Uuid::new_v4());
        let hosts = [
            host("zeta", "z"),
            host("alpha", "a"),
            host("mine", "m"),
            host("theirs", "t"),
        ];
        let (shared, elsewhere) = (hosts[0].id, hosts[3].id);
        let team_of = |id| match id {
            id if id == shared => Some(team),
            id if id == elsewhere => Some(other_team),
            _ => None,
        };

        let rows = host_rows(&hosts, team_of, team);
        let labels: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(labels, ["zeta", "alpha", "mine"]);
        assert!(rows[0].is_shared && !rows[1].is_shared);
    }

    #[test]
    fn a_host_without_a_name_goes_by_its_address() {
        let rows = host_rows(&[host("  ", "10.0.0.5")], |_| None, Uuid::new_v4());
        assert_eq!(rows[0].label, "10.0.0.5");
        assert_eq!(rows[0].address, "root@10.0.0.5");
    }

    #[test]
    fn a_member_without_a_login_is_not_shown_as_one() {
        assert_eq!(member_name(Some(&"octocat".to_string())), "@octocat");
        assert_eq!(member_name(None), "someone who left");
    }

    #[test]
    fn the_filter_looks_at_the_name_and_the_address() {
        let rows = host_rows(&[host("Web", "10.0.0.5")], |_| None, Uuid::new_v4());
        assert!(matches_filter(&rows[0], ""));
        assert!(matches_filter(&rows[0], " web "));
        assert!(matches_filter(&rows[0], "root@10"));
        assert!(!matches_filter(&rows[0], "db"));
    }
}
