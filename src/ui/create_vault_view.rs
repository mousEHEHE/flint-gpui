use std::mem;

use gpui::prelude::*;
use gpui::{AnyElement, App, Context, Entity, FocusHandle, Focusable, Subscription, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    ActiveTheme as _,
};
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::account::GitHubStep;
use crate::ui::sync_service::{SignIn, SyncService};
use crate::ui::{lock_screen, VaultState};

enum Step {
    Form,
    Working,
}

enum Mode {
    Plain,
    Account,
}

/// `SignIn` without the GitHub answer.
#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Idle,
    Browser,
    Password,
    Working,
}

fn stage(sign_in: &SignIn) -> Stage {
    match sign_in {
        SignIn::Idle => Stage::Idle,
        SignIn::Browser => Stage::Browser,
        SignIn::Password(_) => Stage::Password,
        SignIn::Working(_) => Stage::Working,
    }
}

fn is_new_account(sign_in: &SignIn) -> bool {
    match sign_in {
        SignIn::Password(step) | SignIn::Working(step) => step.is_new,
        SignIn::Idle | SignIn::Browser => false,
    }
}

pub struct CreateVaultView {
    focus_handle: FocusHandle,
    vault_state: Entity<VaultState>,
    step: Step,
    mode: Mode,
    password: Entity<InputState>,
    confirm: Entity<InputState>,
    account_password: Entity<InputState>,
    error: Option<String>,
    /// So focus moves only when the stage changes.
    last_stage: Stage,
    _sync_observer: Subscription,
}

impl Focusable for CreateVaultView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl CreateVaultView {
    pub fn new(
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let password = masked_input("Password", window, cx);
        let confirm = masked_input("Confirm password", window, cx);
        let account_password = masked_input("Account password", window, cx);

        cx.subscribe_in(&password, window, Self::on_input).detach();
        cx.subscribe_in(&confirm, window, Self::on_input).detach();
        cx.subscribe_in(&account_password, window, Self::on_input)
            .detach();

        let service = SyncService::global(cx);
        let last_stage = stage(&service.read(cx).sign_in);
        let sync_observer = cx.observe_in(&service, window, Self::on_sync_changed);

        Self {
            focus_handle: cx.focus_handle(),
            vault_state,
            step: Step::Form,
            mode: Mode::Plain,
            password,
            confirm,
            account_password,
            error: None,
            last_stage,
            _sync_observer: sync_observer,
        }
    }

    /// Put the cursor in the password field. Called by `RootView` when the create
    /// screen first appears so the user can type straight away.
    pub fn focus_password(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.password
            .update(cx, |state, cx| state.focus(window, cx));
    }

    /// Called once the vault exists, so the master password does not linger.
    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_inputs(window, cx);
        self.mode = Mode::Plain;
        self.step = Step::Form;
        self.error = None;
    }

    /// Also opens GitHub in the browser.
    pub fn start_account_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = Mode::Account;
        self.error = None;
        self.clear_inputs(window, cx);
        SyncService::global(cx).update(cx, |service, cx| service.start_sign_in(cx));
        cx.notify();
    }

    fn leave_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        SyncService::global(cx).update(cx, |service, cx| service.cancel_sign_in(cx));
        self.mode = Mode::Plain;
        self.error = None;
        self.clear_inputs(window, cx);
        self.focus_password(window, cx);
        cx.notify();
    }

    fn clear_inputs(&self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.password, &self.confirm, &self.account_password] {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

    fn on_sync_changed(
        &mut self,
        service: Entity<SyncService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let service = service.read(cx);
        let stage = stage(&service.sign_in);
        let is_new = is_new_account(&service.sign_in);
        let has_error = service.sign_in_error.is_some();
        let was = mem::replace(&mut self.last_stage, stage);
        // A sign-in from Settings must not focus this hidden view.
        if !matches!(self.mode, Mode::Account) {
            return;
        }

        let reached_password = stage == Stage::Password && was != Stage::Password;
        let failed = stage == Stage::Password && was == Stage::Working;
        if reached_password || failed {
            self.focus_account_field(is_new, window, cx);
        }
        // Sign-in cancelled in the browser.
        if stage == Stage::Idle && !has_error && was != Stage::Working {
            self.mode = Mode::Plain;
            self.focus_password(window, cx);
        }
        cx.notify();
    }

    fn focus_account_field(&self, is_new: bool, window: &mut Window, cx: &mut Context<Self>) {
        let input = if is_new {
            &self.password
        } else {
            &self.account_password
        };
        input.update(cx, |state, cx| state.focus(window, cx));
    }

    fn on_input(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, InputEvent::PressEnter { .. }) {
            return;
        }
        match self.mode {
            Mode::Plain if matches!(self.step, Step::Form) => self.submit(cx),
            Mode::Account => self.submit_account(window, cx),
            Mode::Plain => {}
        }
    }

    fn checked_new_password(&self, cx: &App) -> Result<String, &'static str> {
        let password = self.password.read(cx).value().to_string();
        let confirm = Zeroizing::new(self.confirm.read(cx).value().to_string());
        if password.is_empty() {
            return Err("Password cannot be empty.");
        }
        if password != *confirm {
            return Err("Passwords do not match.");
        }
        Ok(password)
    }

    fn checked_account_password(&self, cx: &App) -> Result<String, &'static str> {
        let password = self.account_password.read(cx).value().to_string();
        if password.is_empty() {
            return Err("Password cannot be empty.");
        }
        Ok(password)
    }

    fn submit_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let service = SyncService::global(cx);
        let SignIn::Password(step) = &service.read(cx).sign_in else {
            return;
        };
        let checked = if step.is_new {
            self.checked_new_password(cx)
        } else {
            self.checked_account_password(cx)
        };
        self.error = None;
        let password = match checked {
            Ok(password) => password,
            Err(e) => {
                self.error = Some(e.into());
                cx.notify();
                return;
            }
        };
        service.update(cx, |service, cx| {
            service.create_vault_with_account(SecretString::new(password), cx);
        });
        self.clear_inputs(window, cx);
        cx.notify();
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let pw = match self.checked_new_password(cx) {
            Ok(pw) => pw,
            Err(e) => {
                self.error = Some(e.into());
                cx.notify();
                return;
            }
        };

        self.step = Step::Working;
        self.error = None;
        cx.notify();

        let (tx, rx) = async_channel::bounded::<
            Result<
                (crate::vault::VaultManager, crate::vault::RecoveryPhrase),
                crate::vault::VaultError,
            >,
        >(1);

        std::thread::spawn(move || {
            let result = (|| -> Result<_, crate::vault::VaultError> {
                let path = crate::vault::default_vault_path()?;
                let secret = SecretString::new(pw);
                crate::vault::VaultManager::create(&path, &secret)
            })();
            let _ = tx.try_send(result);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            this.update(cx, |this, cx| match result {
                Ok((manager, phrase)) => {
                    this.vault_state.update(cx, |vs, cx| {
                        *vs = VaultState::AwaitingPhraseConfirmation {
                            manager,
                            phrase,
                            is_rotation: false,
                        };
                        cx.notify();
                    });
                    this.step = Step::Form;
                }
                Err(e) => {
                    tracing::error!(?e, "vault creation failed");
                    this.step = Step::Form;
                    this.error = Some(format!("Failed to create vault: {e}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

fn masked_input(
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .masked(true)
    })
}

impl CreateVaultView {
    fn render_plain(&self, cx: &mut Context<Self>) -> AnyElement {
        if matches!(self.step, Step::Working) {
            return lock_screen::working("Creating vault\u{2026}", cx);
        }
        lock_screen::column()
            .child(lock_screen::header(
                "Create vault",
                Some("Choose a master password to protect your hosts and credentials."),
                cx,
            ))
            .child(Input::new(&self.password).mask_toggle())
            .child(Input::new(&self.confirm).mask_toggle())
            .children(self.error.as_deref().map(|e| lock_screen::error(e, cx)))
            .child(
                Button::new("create-vault")
                    .primary()
                    .child("Create vault")
                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
            )
            .child(lock_screen::links().child(
                lock_screen::link("have-account", "I already have a flint account").on_click(
                    cx.listener(|this, _, window, cx| this.start_account_sign_in(window, cx)),
                ),
            ))
            .into_any_element()
    }

    fn render_account(&self, cx: &mut Context<Self>) -> AnyElement {
        let service = SyncService::global(cx).read(cx);
        let error = self.error.clone().or_else(|| service.sign_in_error.clone());
        match &service.sign_in {
            SignIn::Browser => self.render_browser(cx),
            SignIn::Password(step) if step.is_new => {
                let login = step.github_login.clone();
                self.render_new_account(&login, error, cx)
            }
            SignIn::Password(step) => {
                let login = step.github_login.clone();
                self.render_existing_account(&login, error, cx)
            }
            SignIn::Working(step) => lock_screen::working(working_label(step), cx),
            SignIn::Idle => match error {
                Some(error) => self.render_retry(&error, cx),
                // The sync observer goes back to the plain form.
                None => self.render_plain(cx),
            },
        }
    }

    fn render_browser(&self, cx: &mut Context<Self>) -> AnyElement {
        lock_screen::column()
            .child(lock_screen::header(
                "Continue in your browser",
                Some(
                    "Sign in to GitHub there, then come back. GitHub only tells flint who you are.",
                ),
                cx,
            ))
            .child(lock_screen::links().child(self.back_link("account-cancel", "Cancel", cx)))
            .into_any_element()
    }

    fn render_existing_account(
        &self,
        login: &str,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        lock_screen::column()
            .child(lock_screen::header(
                format!("Sign in as @{login}"),
                Some(
                    "Enter your flint account password. This device's vault will open with it, \
                     and your account's recovery phrase works here too.",
                ),
                cx,
            ))
            .child(Input::new(&self.account_password).mask_toggle())
            .children(error.as_deref().map(|e| lock_screen::error(e, cx)))
            .child(self.submit_button("account-sign-in", "Sign in", cx))
            .child(lock_screen::note(
                "Don't have your recovery phrase? After signing in, make a new one in \
                 Settings, Vault.",
                muted,
            ))
            .child(lock_screen::links().child(self.back_link("account-back", "Back", cx)))
            .into_any_element()
    }

    fn render_new_account(
        &self,
        login: &str,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        lock_screen::column()
            .child(lock_screen::header(
                format!("Create a flint account for @{login}"),
                Some(
                    "Choose a master password. It protects this vault and its synced copy. \
                     Your recovery phrase comes next.",
                ),
                cx,
            ))
            .child(Input::new(&self.password).mask_toggle())
            .child(Input::new(&self.confirm).mask_toggle())
            .children(error.as_deref().map(|e| lock_screen::error(e, cx)))
            .child(self.submit_button("account-create", "Create account", cx))
            .child(lock_screen::links().child(self.back_link("account-back", "Back", cx)))
            .into_any_element()
    }

    fn render_retry(&self, error: &str, cx: &mut Context<Self>) -> AnyElement {
        let column = lock_screen::column()
            .child(lock_screen::header("Sign in with GitHub", None, cx))
            .child(lock_screen::error(error, cx));
        // Only a retry: the new account's recovery phrase lives only in this
        // unsaved vault.
        if SyncService::global(cx).read(cx).has_unsaved_vault() {
            return column
                .child(
                    Button::new("account-save-again")
                        .primary()
                        .child("Try again")
                        .on_click(cx.listener(|_, _, _, cx| {
                            SyncService::global(cx)
                                .update(cx, |service, cx| service.save_unsaved_vault(cx));
                        })),
                )
                .into_any_element();
        }
        column
            .child(
                Button::new("account-retry")
                    .primary()
                    .child("Try again")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.start_account_sign_in(window, cx)),
                    ),
            )
            .child(lock_screen::links().child(self.back_link(
                "account-plain",
                "Create a vault without an account",
                cx,
            )))
            .into_any_element()
    }

    fn submit_button(
        &self,
        id: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .primary()
            .child(label)
            .on_click(cx.listener(|this, _, window, cx| this.submit_account(window, cx)))
    }

    fn back_link(&self, id: &'static str, label: &'static str, cx: &mut Context<Self>) -> Button {
        lock_screen::link(id, label)
            .on_click(cx.listener(|this, _, window, cx| this.leave_account(window, cx)))
    }
}

fn working_label(step: &GitHubStep) -> &'static str {
    if step.is_new {
        "Creating your account\u{2026}"
    } else {
        "Signing in\u{2026}"
    }
}

impl Render for CreateVaultView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.mode {
            Mode::Plain => self.render_plain(cx),
            Mode::Account => self.render_account(cx),
        };
        lock_screen::screen(content, window, cx)
    }
}
