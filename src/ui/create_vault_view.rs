use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, FocusHandle, Focusable, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    v_flex, ActiveTheme as _, Sizable as _,
};
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::ui::VaultState;

enum Step {
    Form,
    Working,
}

pub struct CreateVaultView {
    focus_handle: FocusHandle,
    vault_state: Entity<VaultState>,
    step: Step,
    password: Entity<InputState>,
    confirm: Entity<InputState>,
    error: Option<String>,
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
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Password")
                .masked(true)
        });
        let confirm = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Confirm password")
                .masked(true)
        });

        cx.subscribe(&password, Self::on_input).detach();
        cx.subscribe(&confirm, Self::on_input).detach();

        Self {
            focus_handle: cx.focus_handle(),
            vault_state,
            step: Step::Form,
            password,
            confirm,
            error: None,
        }
    }

    /// Put the cursor in the password field. Called by `RootView` when the create
    /// screen first appears so the user can type straight away.
    pub fn focus_password(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.password
            .update(cx, |state, cx| state.focus(window, cx));
    }

    /// Empty both fields. `RootView` calls this once the vault exists, so the
    /// new master password does not stay in a form nobody will see again.
    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for input in [&self.password, &self.confirm] {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

    fn on_input(&mut self, _: Entity<InputState>, event: &InputEvent, cx: &mut Context<Self>) {
        if matches!(event, InputEvent::PressEnter { .. }) && matches!(self.step, Step::Form) {
            self.submit(cx);
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let pw = self.password.read(cx).value().to_string();
        let conf = Zeroizing::new(self.confirm.read(cx).value().to_string());

        if pw.is_empty() {
            self.error = Some("Password cannot be empty.".into());
            cx.notify();
            return;
        }
        if pw != *conf {
            self.error = Some("Passwords do not match.".into());
            cx.notify();
            return;
        }

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

impl Render for CreateVaultView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        let inner: gpui::AnyElement = match self.step {
            Step::Form => v_flex()
                .gap(px(8.))
                .child(
                    v_flex()
                        .gap(px(2.))
                        .child(div().text_lg().child("Create vault"))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.muted_foreground)
                                .child(
                                    "Choose a master password to protect your hosts and \
                                     credentials.",
                                ),
                        ),
                )
                .children(
                    self.error
                        .as_ref()
                        .map(|e| div().text_sm().text_color(theme.danger).child(e.clone())),
                )
                .child(Input::new(&self.password).mask_toggle())
                .child(Input::new(&self.confirm).mask_toggle())
                .child(
                    Button::new("create-vault")
                        .primary()
                        .child("Create vault")
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                )
                .into_any_element(),

            Step::Working => v_flex()
                .gap_3()
                .items_center()
                .child(Spinner::new().large())
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Creating vault…"),
                )
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(theme.background)
            .items_center()
            .justify_center()
            .child(v_flex().w(px(300.)).gap(px(8.)).child(inner))
    }
}
