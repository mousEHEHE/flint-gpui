//! The Sync section of Settings: sign in with GitHub, see whether the vault
//! is in step with the account, sign out.

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
};
use secrecy::SecretString;
use uuid::Uuid;

use crate::account::api::DeviceSession;
use crate::account::{self, AccountError, Api};
use crate::preferences::Preferences;
use crate::ui::sync_service::{SignIn, SyncService};
use crate::ui::VaultState;
use crate::vault::VaultError;

use super::ago;

pub struct SyncPane {
    vault_state: Entity<VaultState>,
    service: Entity<SyncService>,
    password: Entity<InputState>,
    /// The devices signed in to the account; `None` until the server answered.
    devices: Option<Vec<DeviceSession>>,
    /// The device being signed out right now.
    signing_out: Option<Uuid>,
    is_loading_devices: bool,
    device_error: Option<String>,
    is_confirming_delete: bool,
    delete_password: Entity<InputState>,
    is_deleting: bool,
    delete_error: Option<String>,
}

impl SyncPane {
    pub fn new(
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let service = SyncService::global(cx);
        cx.observe(&service, |this, _, cx| {
            // Signed out: the list belonged to that session.
            if this.session_token(cx).is_none() {
                this.devices = None;
                this.device_error = None;
            }
            // Signed in while the pane was open: the list is still missing.
            // A failed load waits for the Retry button.
            let is_missing = this.devices.is_none() && this.device_error.is_none();
            if is_missing && !this.is_loading_devices {
                this.load_devices(None, cx);
            }
            cx.notify();
        })
        .detach();
        cx.observe(&vault_state, |_, _, cx| cx.notify()).detach();

        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Master password")
                .masked(true)
        });
        cx.subscribe_in(
            &password,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_password(window, cx);
                }
            },
        )
        .detach();

        let delete_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Master password")
                .masked(true)
        });
        cx.subscribe_in(
            &delete_password,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.delete_account(window, cx);
                }
            },
        )
        .detach();

        let mut this = Self {
            vault_state,
            service,
            password,
            devices: None,
            signing_out: None,
            is_loading_devices: false,
            device_error: None,
            is_confirming_delete: false,
            delete_password,
            is_deleting: false,
            delete_error: None,
        };
        this.load_devices(None, cx);
        this
    }

    fn session_token(&self, cx: &Context<Self>) -> Option<String> {
        self.vault_state
            .read(cx)
            .manager()?
            .sync_state()?
            .session_token
            .clone()
    }

    /// Fetch the list of devices, after signing `sign_out` out of the account
    /// when one is given.
    fn load_devices(&mut self, sign_out: Option<Uuid>, cx: &mut Context<Self>) {
        let Some(token) = self.session_token(cx) else {
            return;
        };
        self.signing_out = sign_out;
        self.is_loading_devices = true;
        self.device_error = None;
        cx.notify();

        let asked_with = token.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let api = Api::shared();
            let devices = sign_out
                .map_or(Ok(()), |id| api.revoke_session(&token, id))
                .and_then(|()| api.sessions(&token));
            let _ = tx.try_send(devices);
        });
        cx.spawn(async move |this, cx| {
            let Ok(devices) = rx.recv().await else { return };
            this.update(cx, |this, cx| {
                this.signing_out = None;
                this.is_loading_devices = false;
                // Signed out or in again since: the answer is about another session.
                if this.session_token(cx).as_ref() != Some(&asked_with) {
                    this.load_devices(None, cx);
                    return;
                }
                match devices {
                    Ok(devices) => this.devices = Some(devices),
                    Err(e) => this.device_error = Some(e.to_string()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Delete the account on the server, then forget it here. The records
    /// stay in this vault.
    fn delete_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_deleting {
            return;
        }
        let password = self.delete_password.read(cx).value().to_string();
        let (Some(token), false) = (self.session_token(cx), password.is_empty()) else {
            return;
        };
        let Some(slot) = self
            .vault_state
            .read(cx)
            .manager()
            .map(|vault| vault.password_slot())
        else {
            return;
        };
        self.delete_password
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.is_deleting = true;
        self.delete_error = None;
        cx.notify();

        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let password = SecretString::new(password);
            let _ = tx.try_send(account::delete_account(
                &Api::shared(),
                &token,
                &slot,
                &password,
            ));
        });
        cx.spawn(async move |this, cx| {
            let Ok(deleted) = rx.recv().await else { return };
            this.update(cx, |this, cx| {
                this.is_deleting = false;
                match deleted {
                    Ok(()) => {
                        tracing::info!("sync account deleted");
                        this.is_confirming_delete = false;
                        this.devices = None;
                        Preferences::update(cx, |prefs| prefs.has_account = false);
                        this.service.update(cx, |service, cx| service.sign_out(cx));
                    }
                    Err(e) => this.delete_error = Some(delete_problem(&e)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn submit_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let password = self.password.read(cx).value().to_string();
        if password.is_empty() {
            return;
        }
        self.password
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.service.update(cx, |service, cx| {
            service.submit_password(SecretString::new(password), cx);
        });
    }
}

fn delete_problem(error: &AccountError) -> String {
    match error {
        AccountError::Vault(VaultError::InvalidPassword) => "Wrong master password.".into(),
        AccountError::Api(api) if api.code() == Some("owns_teams") => {
            "You own a team. Delete it or hand it over first (Settings, Teams).".into()
        }
        other => format!("The account was not deleted: {other}"),
    }
}

impl SyncPane {
    fn render_devices(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted_fg, danger) = (theme.foreground, theme.muted_foreground, theme.danger);
        let mut section = v_flex().gap(px(6.)).mt(px(6.)).child(
            div()
                .text_size(px(13.))
                .font_semibold()
                .text_color(fg)
                .child("Devices"),
        );

        let Some(devices) = &self.devices else {
            let Some(error) = self.device_error.clone() else {
                return section.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(muted_fg)
                        .child("Loading…"),
                );
            };
            return section
                .child(div().text_size(px(12.)).text_color(danger).child(error))
                .child(
                    h_flex().child(
                        Button::new("devices-retry")
                            .xsmall()
                            .child("Retry")
                            .on_click(cx.listener(|this, _, _, cx| this.load_devices(None, cx))),
                    ),
                );
        };
        for device in devices.clone() {
            let id = device.id;
            // "2026-10-11T08:15:00Z": the day is enough here.
            let day: String = device.last_seen_at.chars().take(10).collect();
            let mut row = h_flex()
                .h(px(24.))
                .gap(px(8.))
                .items_center()
                .child(
                    Icon::new(IconName::HardDrive)
                        .size(px(12.))
                        .text_color(muted_fg),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(fg)
                        .child(format!("{} · {}", device.device_name, device.platform)),
                )
                .when(!device.is_current, |row| {
                    row.child(
                        div()
                            .text_size(px(11.5))
                            .text_color(muted_fg)
                            .child(format!("last seen {day}")),
                    )
                })
                .child(div().flex_1());
            row = if device.is_current {
                row.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(muted_fg)
                        .child("this device"),
                )
            } else {
                row.child(
                    Button::new(SharedString::from(format!("device-{id}")))
                        .xsmall()
                        .ghost()
                        .child("Sign out")
                        .loading(self.signing_out == Some(id))
                        .disabled(self.signing_out.is_some())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.load_devices(Some(id), cx);
                        })),
                )
            };
            section = section.child(row);
        }
        section.children(
            self.device_error
                .clone()
                .map(|e| div().text_size(px(12.)).text_color(danger).child(e)),
        )
    }

    fn render_delete(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted_fg, danger) = (theme.muted_foreground, theme.danger);
        let section = v_flex().gap(px(6.)).mt(px(6.));
        let trash = || Icon::empty().path("icons/trash.svg");

        if !self.is_confirming_delete {
            return section.child(
                h_flex().child(
                    Button::new("account-delete")
                        .small()
                        .danger()
                        .outline()
                        .child("Delete account…")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.is_confirming_delete = true;
                            this.delete_password
                                .update(cx, |state, cx| state.focus(window, cx));
                            cx.notify();
                        })),
                ),
            );
        }
        section
            .w(px(340.))
            .child(div().text_size(px(11.5)).text_color(muted_fg).child(
                "This removes the account and everything stored for it on the server, and signs \
                 out every device. The records in this vault stay. Enter your master password \
                 to confirm.",
            ))
            .child(Input::new(&self.delete_password).small().mask_toggle())
            .children(
                self.delete_error
                    .clone()
                    .map(|e| div().text_size(px(12.)).text_color(danger).child(e)),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("account-delete-confirm")
                            .small()
                            .danger()
                            .icon(trash())
                            .child("Delete account")
                            .loading(self.is_deleting)
                            .disabled(self.is_deleting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.delete_account(window, cx);
                            })),
                    )
                    .child(
                        Button::new("account-delete-cancel")
                            .small()
                            .child("Cancel")
                            .disabled(self.is_deleting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.is_confirming_delete = false;
                                this.delete_error = None;
                                this.delete_password
                                    .update(cx, |state, cx| state.set_value("", window, cx));
                                cx.notify();
                            })),
                    ),
            )
    }
}

impl Render for SyncPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let danger = theme.danger;

        let heading = |text: String| {
            div()
                .text_size(px(13.))
                .font_semibold()
                .text_color(fg)
                .child(text)
        };
        let caption = |text: String| div().text_size(px(11.5)).text_color(muted_fg).child(text);
        let problem = |text: String| div().text_size(px(12.)).text_color(danger).child(text);

        let account = self.vault_state.read(cx).manager().and_then(|vault| {
            let state = vault.sync_state()?;
            Some((state.github_login.clone(), state.session_token.is_some()))
        });
        let service = self.service.read(cx);

        let sign_in_button = |label: &'static str, cx: &mut Context<Self>| {
            Button::new("sync-sign-in")
                .primary()
                .small()
                .child(label)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.service
                        .update(cx, |service, cx| service.start_sign_in(cx))
                }))
        };
        let cancel_button = |cx: &mut Context<Self>| {
            Button::new("sync-cancel")
                .small()
                .child("Cancel")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.service
                        .update(cx, |service, cx| service.cancel_sign_in(cx))
                }))
        };
        let sign_out_button = |label: &'static str, cx: &mut Context<Self>| {
            Button::new("sync-sign-out")
                .small()
                .child(label)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.service.update(cx, |service, cx| service.sign_out(cx))
                }))
        };

        let sign_in_error = service.sign_in_error.clone().map(problem);
        let body = v_flex().gap(px(8.));

        let body = match (&service.sign_in, account) {
            (SignIn::Browser, _) => body
                .child(heading("Sign in with GitHub".into()))
                .child(caption(
                    "Finish signing in from your browser, then come back here. GitHub only tells \
                     flint who you are; it gets no access to your repositories."
                        .into(),
                ))
                .child(h_flex().child(cancel_button(cx))),

            (SignIn::Password(step) | SignIn::Working(step), _) => {
                let is_working = matches!(service.sign_in, SignIn::Working(_));
                let (title, text, action) = if step.is_new {
                    (
                        format!("Create a sync account for @{}", step.github_login),
                        "Your master password will protect the synced copy as well: enter it to \
                         confirm. You will get a new recovery phrase, and the current one stops \
                         working.",
                        "Create account",
                    )
                } else {
                    (
                        format!("Sign in as @{}", step.github_login),
                        "Enter the master password of your flint account. This vault switches \
                         to it: from now on it opens with that password and that account's \
                         recovery phrase. Hosts already here are kept and added to the account.",
                        "Sign in",
                    )
                };
                body.child(heading(title))
                    .child(caption(text.into()))
                    .child(
                        div()
                            .w(px(300.))
                            .child(Input::new(&self.password).small().mask_toggle()),
                    )
                    .children(sign_in_error)
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                Button::new("sync-submit-password")
                                    .primary()
                                    .small()
                                    .child(action)
                                    .loading(is_working)
                                    .disabled(is_working)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_password(window, cx);
                                    })),
                            )
                            .child(cancel_button(cx).disabled(is_working)),
                    )
            }

            // The page title above already says "Sync".
            (SignIn::Idle, None) => body
                .child(caption(
                    "Keep hosts, keys and snippets the same on all your devices. Everything is \
                     encrypted on this device before it is sent, with a key that only your \
                     master password or recovery phrase can unlock, so the server cannot read \
                     any of it."
                        .into(),
                ))
                .children(sign_in_error)
                .child(h_flex().child(sign_in_button("Sign in with GitHub", cx))),

            (SignIn::Idle, Some((login, false))) => body
                .child(heading(format!("Signed out of @{login}")))
                .child(caption(
                    "The account ended this device's session: its password was changed or the \
                     device was signed out from another one. Nothing was removed from this \
                     vault. Sign in again to continue syncing."
                        .into(),
                ))
                .children(sign_in_error)
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(sign_in_button("Sign in again", cx))
                        .child(sign_out_button("Forget account", cx)),
                ),

            (SignIn::Idle, Some((login, true))) => {
                let status = match (&service.sync_error, service.is_syncing, service.last_synced) {
                    (_, true, _) => caption("Syncing…".into()),
                    (Some(error), _, _) => problem(format!("Last sync failed: {error}")),
                    (None, _, Some(at)) => caption(format!("Synced {}.", ago(at))),
                    (None, _, None) => caption("Not synced yet.".into()),
                };
                body.child(heading(format!("Signed in as @{login}")))
                    .child(caption(
                        "Changes sync on their own within a couple of minutes. Signing out keeps \
                         every record in this vault and in the account."
                            .into(),
                    ))
                    .child(status)
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                Button::new("sync-now")
                                    .small()
                                    .child("Sync now")
                                    .disabled(service.is_syncing)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.service
                                            .update(cx, |service, cx| service.refresh_all(cx));
                                    })),
                            )
                            .child(sign_out_button("Sign out", cx)),
                    )
                    .child(self.render_devices(cx))
                    .child(self.render_delete(cx))
            }
        };

        v_flex()
            .id("sync-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(20.))
            .child(body)
    }
}
