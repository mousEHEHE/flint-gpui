//! The Vault section of Settings: lock the vault now, Touch ID, change the
//! master password or the recovery phrase, and delete the vault.

use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, EventEmitter, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _,
};
use secrecy::{ExposeSecret as _, SecretString};

use crate::account::{self, AccountError, Api};
use crate::preferences::Preferences;
use crate::ui::app_view::LOCK_SHORTCUT;
use crate::ui::VaultState;
use crate::vault::{NewPasswordSlot, VaultError, VaultManager};

pub enum VaultPaneEvent {
    LockRequested,
}

/// Why the change-password form cannot be sent yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasswordChangeProblem {
    EmptyCurrent,
    EmptyNew,
    EmptyConfirmation,
    Mismatch,
    Unchanged,
}

impl PasswordChangeProblem {
    fn message(self) -> &'static str {
        match self {
            Self::EmptyCurrent => "Enter your current password.",
            Self::EmptyNew => "Choose a new password.",
            Self::EmptyConfirmation => "Type the new password again to confirm it.",
            Self::Mismatch => "The new passwords do not match.",
            Self::Unchanged => "The new password is the same as the current one.",
        }
    }
}

/// The first thing wrong with the form, or `None` when it can be sent.
/// Whether `current` is the right password is only known after the KDF runs.
fn password_change_problem(
    current: &str,
    new: &str,
    confirmation: &str,
) -> Option<PasswordChangeProblem> {
    if current.is_empty() {
        return Some(PasswordChangeProblem::EmptyCurrent);
    }
    if new.is_empty() {
        return Some(PasswordChangeProblem::EmptyNew);
    }
    if confirmation.is_empty() {
        return Some(PasswordChangeProblem::EmptyConfirmation);
    }
    if new != confirmation {
        return Some(PasswordChangeProblem::Mismatch);
    }
    if new == current {
        return Some(PasswordChangeProblem::Unchanged);
    }
    None
}

/// The sync session that has to hear about a password change: `Ok(None)` for
/// a vault that does not sync, `Err` with the reason when the account cannot
/// take the change right now. Changing only the local password then would
/// leave the account on the old one.
fn account_session(vault: &VaultManager) -> Result<Option<String>, &'static str> {
    let Some(state) = vault.sync_state() else {
        return Ok(None);
    };
    if vault.recovery_job().is_some() {
        return Err("The account is still being updated after the recovery. Try again shortly.");
    }
    match &state.session_token {
        Some(token) => Ok(Some(token.clone())),
        None => Err("Sign in to sync again first (Settings, Sync), then change the password."),
    }
}

enum ChangeStatus {
    Idle,
    Working,
    Changed,
    Failed(String),
}

pub struct VaultPane {
    vault_state: Entity<VaultState>,
    current_password: Entity<InputState>,
    new_password: Entity<InputState>,
    confirm_password: Entity<InputState>,
    status: ChangeStatus,
    /// The password typed to confirm a new recovery phrase.
    phrase_password: Entity<InputState>,
    phrase_error: Option<String>,
    is_making_phrase: bool,
    is_confirming_delete: bool,
    delete_password: Entity<InputState>,
    delete_error: Option<String>,
    is_deleting: bool,
    touch_id_error: Option<String>,
    is_saving_touch_id: bool,
    is_touch_id_available: bool,
}

impl EventEmitter<VaultPaneEvent> for VaultPane {}

impl VaultPane {
    pub fn new(
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let masked_input =
            |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .masked(true)
                });
                cx.subscribe_in(&input, window, Self::on_input).detach();
                input
            };

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
                    this.delete_vault(window, cx);
                }
            },
        )
        .detach();

        Self {
            vault_state,
            current_password: masked_input("Current password", window, cx),
            new_password: masked_input("New password", window, cx),
            confirm_password: masked_input("Confirm new password", window, cx),
            status: ChangeStatus::Idle,
            phrase_password: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Master password")
                    .masked(true)
            }),
            phrase_error: None,
            is_making_phrase: false,
            is_confirming_delete: false,
            delete_password,
            delete_error: None,
            is_deleting: false,
            touch_id_error: None,
            is_saving_touch_id: false,
            is_touch_id_available: crate::touch_id::is_available(),
        }
    }

    fn on_input(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::PressEnter { .. } => self.submit(window, cx),
            // The message describes what was typed before; it is stale now.
            InputEvent::Change if !matches!(self.status, ChangeStatus::Working) => {
                self.status = ChangeStatus::Idle;
                cx.notify();
            }
            _ => {}
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.status, ChangeStatus::Working) {
            return;
        }

        let secret =
            |input: &Entity<InputState>| SecretString::new(input.read(cx).value().to_string());
        let current = secret(&self.current_password);
        let new = secret(&self.new_password);
        let confirmation = secret(&self.confirm_password);

        if let Some(problem) = password_change_problem(
            current.expose_secret(),
            new.expose_secret(),
            confirmation.expose_secret(),
        ) {
            self.status = ChangeStatus::Failed(problem.message().into());
            cx.notify();
            return;
        }

        let Some(vault) = self.vault_state.read(cx).manager() else {
            return;
        };
        let slot = vault.password_slot();
        let session = match account_session(vault) {
            Ok(session) => session,
            Err(reason) => {
                self.status = ChangeStatus::Failed(reason.into());
                cx.notify();
                return;
            }
        };

        self.status = ChangeStatus::Working;
        cx.notify();

        let (tx, rx) = async_channel::bounded::<Result<NewPasswordSlot, AccountError>>(1);

        // Argon2id runs twice in here, once per password.
        std::thread::spawn(move || {
            let rewrapped = slot
                .rewrap(&current, &new)
                .map_err(AccountError::from)
                .and_then(|new_slot| {
                    // The account first: if it refuses, nothing has changed anywhere.
                    if let Some(session) = session {
                        let change = new_slot.account_change(slot.kdf_params());
                        account::change_password(&Api::shared(), &session, &change)?;
                    }
                    Ok(new_slot)
                });
            let _ = tx.try_send(rewrapped);
        });

        let vault_state = self.vault_state.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(rewrapped) = rx.recv().await else {
                return;
            };
            cx.update(|window, cx| {
                // Installed here and not by the pane: closing Settings while
                // the KDF runs must not drop a change the user asked for.
                let outcome = install_password_slot(&vault_state, rewrapped, cx);
                this.update(cx, |this, cx| this.show_outcome(outcome, window, cx))
                    .ok();
            })
            .ok();
        })
        .detach();
    }

    /// Replace the recovery phrase. The new one takes over the window until
    /// the user confirms they saved it, as after a recovery.
    fn set_touch_id(&mut self, is_enabled: bool, cx: &mut Context<Self>) {
        self.touch_id_error = None;
        if !is_enabled {
            crate::touch_id::delete_key();
            Preferences::update(cx, |prefs| prefs.is_touch_id_enabled = false);
            tracing::info!("Touch ID unlock turned off");
            cx.notify();
            return;
        }
        let Some(key) = self
            .vault_state
            .read(cx)
            .manager()
            .map(|vm| vm.device_key())
        else {
            return;
        };
        self.is_saving_touch_id = true;
        cx.notify();
        let saving = cx
            .background_executor()
            .spawn(async move { crate::touch_id::save_key(&key) });
        cx.spawn(async move |this, cx| {
            let result = saving.await;
            this.update(cx, |this, cx| {
                this.is_saving_touch_id = false;
                match result {
                    Ok(()) => {
                        Preferences::update(cx, |prefs| prefs.is_touch_id_enabled = true);
                        tracing::info!("Touch ID unlock turned on");
                    }
                    Err(e) => {
                        tracing::error!(?e, "failed to turn on Touch ID unlock");
                        this.touch_id_error = Some(format!("Touch ID was not turned on: {e}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_touch_id(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .gap(px(8.))
            .child(
                Checkbox::new("vault-touch-id")
                    .checked(Preferences::is_touch_id_enabled(cx))
                    .disabled(self.is_saving_touch_id)
                    .label("Unlock with Touch ID")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_touch_id(*checked, cx);
                    })),
            )
            .children(self.touch_id_error.clone().map(|message| {
                div()
                    .text_size(px(12.))
                    .text_color(cx.theme().danger)
                    .child(message)
            }))
    }

    fn make_new_phrase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_making_phrase {
            return;
        }
        let password = SecretString::new(self.phrase_password.read(cx).value().to_string());
        if password.expose_secret().is_empty() {
            self.phrase_error = Some("Enter your master password.".into());
            cx.notify();
            return;
        }
        let Some(vault) = self.vault_state.read(cx).manager() else {
            return;
        };
        let slot = vault.password_slot();
        let session = match account_session(vault) {
            Ok(session) => session,
            Err(reason) => {
                self.phrase_error = Some(reason.into());
                cx.notify();
                return;
            }
        };
        self.phrase_password
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.is_making_phrase = true;
        self.phrase_error = None;
        cx.notify();

        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let made =
                account::new_recovery_phrase(&Api::shared(), session.as_deref(), &slot, &password);
            let _ = tx.try_send(made);
        });

        let vault_state = self.vault_state.clone();
        cx.spawn(async move |this, cx| {
            let Ok(made) = rx.recv().await else { return };
            let installed = match made {
                Ok((slot, phrase)) => vault_state.update(cx, |vault_state, cx| {
                    cx.notify();
                    show_new_phrase(vault_state, slot, phrase)
                }),
                Err(e) => Some(Err(e)),
            };
            this.update(cx, |this, cx| {
                this.is_making_phrase = false;
                this.phrase_error = match installed {
                    Some(Ok(())) => None,
                    Some(Err(AccountError::Vault(VaultError::InvalidPassword))) => {
                        Some("Wrong master password.".to_string())
                    }
                    Some(Err(e)) => Some(format!("The recovery phrase was not changed: {e}")),
                    None => Some(
                        "The vault was locked first, so the new phrase was not saved. If it \
                         syncs, the account no longer takes the old phrase: make a new \
                         phrase again."
                            .to_string(),
                    ),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Check the master password, then delete the vault. Nothing of this pane
    /// is left after a deletion: `RootView` closes Settings with the rest.
    fn delete_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_deleting {
            return;
        }
        let password = SecretString::new(self.delete_password.read(cx).value().to_string());
        if password.expose_secret().is_empty() {
            self.delete_error = Some("Enter your master password.".into());
            cx.notify();
            return;
        }
        let Some(vault) = self.vault_state.read(cx).manager() else {
            return;
        };
        let slot = vault.password_slot();
        self.delete_password
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.is_deleting = true;
        self.delete_error = None;
        cx.notify();

        let (tx, rx) = async_channel::bounded(1);
        // Argon2id is slow, so the check runs off the main thread.
        std::thread::spawn(move || {
            let checked = slot.open(&password).map(|_| ());
            let _ = tx.try_send(checked);
        });

        cx.spawn(async move |this, cx| {
            let Ok(checked) = rx.recv().await else { return };
            this.update(cx, |this, cx| {
                this.is_deleting = false;
                this.delete_error = this.delete_checked_vault(checked, cx).err();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The second half of `delete_vault`, back on the main thread.
    fn delete_checked_vault(
        &mut self,
        checked: Result<(), VaultError>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        match checked {
            Ok(()) => {}
            Err(VaultError::InvalidPassword) => return Err("Wrong master password.".into()),
            Err(e) => return Err(format!("The vault was not deleted: {e}")),
        }
        // The lock screen has its own way to delete; this one was asked for
        // on a vault that has been locked since, so it is dropped.
        if self.vault_state.read(cx).manager().is_none() {
            return Err("The vault was locked first, so it was not deleted.".into());
        }
        crate::ui::root_view::delete_vault(&self.vault_state, cx)
            .map_err(|e| format!("The vault was not deleted: {e}"))
    }

    fn render_delete(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let section = v_flex().gap(px(8.));
        let trash = || Icon::empty().path("icons/trash.svg");

        if !self.is_confirming_delete {
            return section.child(
                h_flex().child(
                    Button::new("vault-delete")
                        .small()
                        .danger()
                        .outline()
                        .child("Delete vault…")
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
            .w(px(300.))
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme.muted_foreground)
                    .child("Enter your master password to confirm."),
            )
            .child(Input::new(&self.delete_password).small().mask_toggle())
            .children(self.delete_error.clone().map(|message| {
                div()
                    .text_size(px(12.))
                    .text_color(theme.danger)
                    .child(message)
            }))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        Button::new("vault-delete-confirm")
                            .small()
                            .danger()
                            .icon(trash())
                            .child("Delete vault")
                            .loading(self.is_deleting)
                            .disabled(self.is_deleting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.delete_vault(window, cx);
                            })),
                    )
                    .child(
                        Button::new("vault-delete-cancel")
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

    /// `None` means the vault was locked before the change could be saved.
    fn show_outcome(
        &mut self,
        outcome: Option<Result<(), AccountError>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The local check runs first, so a refusal from the account means the
        // two no longer share a password: an earlier change reached the
        // account and was not saved here.
        let is_out_of_step = |e: &AccountError| matches!(e, AccountError::Api(api) if api.code() == Some("wrong_password"));
        match outcome {
            Some(Ok(())) => {
                for input in [
                    &self.current_password,
                    &self.new_password,
                    &self.confirm_password,
                ] {
                    input.update(cx, |state, cx| state.set_value("", window, cx));
                }
                self.status = ChangeStatus::Changed;
            }
            Some(Err(AccountError::Vault(VaultError::InvalidPassword))) => {
                self.status = ChangeStatus::Failed("Wrong current password.".into());
                self.current_password
                    .update(cx, |state, cx| state.focus(window, cx));
            }
            Some(Err(e)) if is_out_of_step(&e) => {
                self.status = ChangeStatus::Failed(
                    "The sync account has a different password than this vault. Sign out \
                     under Settings, Sync and sign in again with the account's password."
                        .into(),
                );
            }
            Some(Err(AccountError::Api(api))) if api.is_session_ended() => {
                self.status = ChangeStatus::Failed(
                    "The sync account signed this device out. Sign in again (Settings, Sync), \
                     then change the password."
                        .into(),
                );
            }
            Some(Err(e)) => {
                self.status = ChangeStatus::Failed(format!("The password was not changed: {e}"));
            }
            None => {
                self.status = ChangeStatus::Failed(
                    "The vault was locked first, so its password was not changed. If it \
                     syncs, the account already has the new one: sign out under Settings, \
                     Sync and sign in again with the new password."
                        .into(),
                );
            }
        }
        cx.notify();
    }
}

/// Save the new recovery slot and hand the window to the phrase screen.
/// `None` when the vault is no longer open; nothing is written then.
fn show_new_phrase(
    vault_state: &mut VaultState,
    slot: crate::vault::NewRecoverySlot,
    phrase: crate::vault::RecoveryPhrase,
) -> Option<Result<(), AccountError>> {
    let vault = vault_state.manager_mut()?;
    if let Err(e) = vault.install_recovery_slot(slot) {
        return Some(Err(e.into()));
    }
    if let VaultState::Unlocked { manager } = std::mem::replace(vault_state, VaultState::Locked) {
        *vault_state = VaultState::AwaitingPhraseConfirmation {
            manager,
            phrase,
            is_rotation: true,
        };
    }
    Some(Ok(()))
}

/// Save a re-wrapped password slot into the open vault. `None` when the vault
/// is no longer open; nothing is written then and the old password stays.
fn install_password_slot(
    vault_state: &Entity<VaultState>,
    rewrapped: Result<NewPasswordSlot, AccountError>,
    cx: &mut App,
) -> Option<Result<(), AccountError>> {
    let outcome = match rewrapped {
        Ok(slot) => vault_state.update(cx, |vault_state, _| {
            let vault = vault_state.manager_mut()?;
            Some(
                vault
                    .install_password_slot(slot)
                    .map_err(AccountError::from),
            )
        }),
        Err(e) => Some(Err(e)),
    };
    match &outcome {
        Some(Ok(())) => tracing::info!("master password changed"),
        Some(Err(AccountError::Vault(VaultError::InvalidPassword))) => {
            tracing::debug!("master password change refused: wrong current password");
        }
        Some(Err(e)) => tracing::error!(?e, "failed to change the master password"),
        None => tracing::warn!("vault locked before the new master password was saved"),
    }
    outcome
}

impl Render for VaultPane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let fg = theme.foreground;
        let muted_fg = theme.muted_foreground;
        let is_working = matches!(self.status, ChangeStatus::Working);

        let heading = |text: &'static str| {
            div()
                .text_size(px(13.))
                .font_semibold()
                .text_color(fg)
                .child(text)
        };
        let caption = |text: &'static str| {
            div()
                .max_w(px(520.))
                .text_size(px(11.5))
                .text_color(muted_fg)
                .child(text)
        };
        let labeled = |label: &'static str, input: &Entity<InputState>| {
            v_flex()
                .gap(px(4.))
                .child(caption(label))
                .child(Input::new(input).small().mask_toggle())
        };

        let status_line = match &self.status {
            ChangeStatus::Idle | ChangeStatus::Working => None,
            ChangeStatus::Changed => Some(
                div()
                    .text_size(px(12.))
                    .text_color(theme.success)
                    .child("Master password changed. Use it the next time you unlock."),
            ),
            ChangeStatus::Failed(message) => Some(
                div()
                    .text_size(px(12.))
                    .text_color(theme.danger)
                    .child(message.clone()),
            ),
        };

        v_flex()
            .id("vault-scroll")
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(20.))
            .gap(px(12.))
            .child(heading("Lock"))
            .child(caption(
                "Clears the vault key from memory. Open sessions stay connected behind the lock \
                 screen.",
            ))
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        Button::new("vault-lock-now")
                            .small()
                            .child("Lock now")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(VaultPaneEvent::LockRequested);
                            })),
                    )
                    .child(caption(LOCK_SHORTCUT)),
            )
            .when(
                self.is_touch_id_available || Preferences::is_touch_id_enabled(cx),
                |el| {
                    el.child(heading("Touch ID").mt(px(6.)))
                        .child(caption(
                            "The vault key stays in this Mac's keychain, and only a finger \
                             enrolled now opens it. After a new finger is added, unlock once \
                             with the master password.",
                        ))
                        .child(self.render_touch_id(cx))
                },
            )
            .child(heading("Master password").mt(px(6.)))
            .child(caption(
                "Hosts, keys and snippets are not re-encrypted, and the recovery phrase keeps \
                 working. A synced account takes the new password; other devices must sign in \
                 again.",
            ))
            .child(
                v_flex()
                    .w(px(300.))
                    .gap(px(8.))
                    .child(labeled("Current password", &self.current_password))
                    .child(labeled("New password", &self.new_password))
                    .child(labeled("Confirm new password", &self.confirm_password))
                    .children(status_line)
                    .child(
                        h_flex().child(
                            Button::new("vault-change-password")
                                .primary()
                                .small()
                                .child(if is_working {
                                    "Changing…"
                                } else {
                                    "Change password"
                                })
                                .loading(is_working)
                                .disabled(is_working)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit(window, cx);
                                })),
                        ),
                    ),
            )
            .child(heading("Recovery phrase").mt(px(6.)))
            .child(caption(
                "Makes a new 24-word phrase and shows it once. The old phrase stops working, \
                 on the sync account too.",
            ))
            .child(
                v_flex()
                    .w(px(300.))
                    .gap(px(8.))
                    .child(Input::new(&self.phrase_password).small().mask_toggle())
                    .children(self.phrase_error.clone().map(|message| {
                        div()
                            .text_size(px(12.))
                            .text_color(theme.danger)
                            .child(message)
                    }))
                    .child(
                        h_flex().child(
                            Button::new("vault-new-phrase")
                                .small()
                                .child("Make a new recovery phrase")
                                .loading(self.is_making_phrase)
                                .disabled(self.is_making_phrase)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.make_new_phrase(window, cx);
                                })),
                        ),
                    ),
            )
            .child(heading("Delete vault").mt(px(6.)))
            .child(caption(
                "Deletes every host, key and snippet on this device and closes all sessions. A \
                 synced account keeps its copy. Without sync this cannot be undone.",
            ))
            .child(self.render_delete(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_form_has_no_problem() {
        assert_eq!(password_change_problem("old", "new", "new"), None);
    }

    #[test]
    fn the_current_password_is_asked_for_first() {
        assert_eq!(
            password_change_problem("", "", "other"),
            Some(PasswordChangeProblem::EmptyCurrent)
        );
    }

    #[test]
    fn an_empty_new_password_is_a_problem_even_when_both_fields_agree() {
        assert_eq!(
            password_change_problem("old", "", ""),
            Some(PasswordChangeProblem::EmptyNew)
        );
    }

    #[test]
    fn the_new_password_must_be_confirmed() {
        assert_eq!(
            password_change_problem("old", "new", ""),
            Some(PasswordChangeProblem::EmptyConfirmation)
        );
        assert_eq!(
            password_change_problem("old", "new", "nwe"),
            Some(PasswordChangeProblem::Mismatch)
        );
    }

    #[test]
    fn the_new_password_must_differ_from_the_current_one() {
        assert_eq!(
            password_change_problem("same", "same", "same"),
            Some(PasswordChangeProblem::Unchanged)
        );
    }

    #[test]
    fn a_typo_in_the_confirmation_is_reported_before_an_unchanged_password() {
        assert_eq!(
            password_change_problem("same", "same", "sane"),
            Some(PasswordChangeProblem::Mismatch)
        );
    }
}
