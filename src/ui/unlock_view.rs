use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, FocusHandle, Focusable, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    v_flex, ActiveTheme as _, Disableable as _, Sizable as _,
};
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::ui::VaultState;

const RECOVERY_WORD_COUNT: usize = 24;

/// What has to be typed before a locked vault can be deleted. There is no
/// password to ask for: this is for someone who lost it.
const DELETE_WORD: &str = "DELETE";

fn is_delete_confirmed(typed: &str) -> bool {
    typed.trim() == DELETE_WORD
}

/// Why the recovery form cannot be submitted yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryProblem {
    WordCount,
    EmptyPassword,
    EmptyConfirmation,
    PasswordMismatch,
}

impl RecoveryProblem {
    fn message(self) -> &'static str {
        match self {
            Self::WordCount => "Enter all 24 words of the recovery phrase.",
            Self::EmptyPassword => "Choose a new password.",
            Self::EmptyConfirmation => "Type the new password again to confirm it.",
            Self::PasswordMismatch => "Passwords do not match.",
        }
    }
}

fn recovery_word_count(phrase: &str) -> usize {
    phrase.split_whitespace().count()
}

/// The first thing wrong with the recovery form, or `None` when it can be sent.
fn recovery_problem(
    phrase: &str,
    new_password: &str,
    confirmation: &str,
) -> Option<RecoveryProblem> {
    if recovery_word_count(phrase) != RECOVERY_WORD_COUNT {
        return Some(RecoveryProblem::WordCount);
    }
    if new_password.is_empty() {
        return Some(RecoveryProblem::EmptyPassword);
    }
    if confirmation.is_empty() {
        return Some(RecoveryProblem::EmptyConfirmation);
    }
    if new_password != confirmation {
        return Some(RecoveryProblem::PasswordMismatch);
    }
    None
}

enum Step {
    PasswordForm,
    PasswordWorking,
    RecoveryForm,
    RecoveryWorking,
    DeleteForm,
}

pub struct UnlockView {
    focus_handle: FocusHandle,
    vault_state: Entity<VaultState>,
    step: Step,
    password: Entity<InputState>,
    phrase_input: Entity<InputState>,
    new_password: Entity<InputState>,
    confirm_password: Entity<InputState>,
    delete_word: Entity<InputState>,
    error: Option<String>,
}

impl Focusable for UnlockView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl UnlockView {
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
        let phrase_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("word1 word2 word3 … (all 24 words)")
        });
        let new_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("New password")
                .masked(true)
        });
        let confirm_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Confirm new password")
                .masked(true)
        });

        let delete_word = cx.new(|cx| {
            InputState::new(window, cx).placeholder(format!("Type {DELETE_WORD} to confirm"))
        });

        cx.subscribe_in(
            &password,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. })
                    && matches!(this.step, Step::PasswordForm)
                {
                    this.submit_password(window, cx);
                }
            },
        )
        .detach();

        cx.subscribe(&phrase_input, Self::on_recovery_input)
            .detach();

        cx.subscribe(&new_password, Self::on_recovery_input)
            .detach();

        cx.subscribe(&confirm_password, Self::on_recovery_input)
            .detach();

        cx.subscribe(&delete_word, Self::on_delete_input).detach();

        Self {
            focus_handle: cx.focus_handle(),
            vault_state,
            step: Step::PasswordForm,
            password,
            phrase_input,
            new_password,
            confirm_password,
            delete_word,
            error: None,
        }
    }

    /// Put the cursor in the password field. Called by `RootView` whenever the
    /// unlock screen appears so the user can type straight away.
    pub fn focus_password(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.password
            .update(cx, |state, cx| state.focus(window, cx));
    }

    /// Back to an empty password form. `RootView` calls this once the vault
    /// opens: this view lives for the whole run and shows again on every lock,
    /// so it must not still hold the master password or a spinner by then.
    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.step = Step::PasswordForm;
        self.error = None;
        for input in [
            &self.password,
            &self.phrase_input,
            &self.new_password,
            &self.confirm_password,
            &self.delete_word,
        ] {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
        cx.notify();
    }

    fn submit_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pw = self.password.read(cx).value().to_string();

        if pw.is_empty() {
            self.error = Some("Password cannot be empty.".into());
            cx.notify();
            return;
        }

        self.step = Step::PasswordWorking;
        self.error = None;
        cx.notify();

        let (tx, rx) = async_channel::bounded::<
            Result<crate::vault::VaultManager, crate::vault::VaultError>,
        >(1);

        std::thread::spawn(move || {
            let result = (|| -> Result<_, crate::vault::VaultError> {
                let path = crate::vault::default_vault_path()?;
                let secret = SecretString::new(pw);
                crate::vault::VaultManager::unlock_with_password(&path, &secret)
            })();
            let _ = tx.try_send(result);
        });

        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            this.update_in(cx, |this, window, cx| match result {
                Ok(manager) => {
                    this.vault_state.update(cx, |vs, cx| {
                        *vs = VaultState::Unlocked { manager };
                        cx.notify();
                    });
                }
                Err(crate::vault::VaultError::InvalidPassword) => {
                    this.step = Step::PasswordForm;
                    this.error = Some("Incorrect password.".into());
                    this.password
                        .update(cx, |state, cx| state.focus(window, cx));
                    cx.notify();
                }
                Err(e) => {
                    tracing::error!(?e, "vault unlock failed");
                    this.step = Step::PasswordForm;
                    this.error = Some(format!("Failed to open vault: {e}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_recovery_input(
        &mut self,
        _: Entity<InputState>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.step, Step::RecoveryForm) {
            return;
        }
        match event {
            InputEvent::PressEnter { .. } => self.submit_recovery(cx),
            // The message describes what was typed before; it is stale now.
            InputEvent::Change => {
                self.error = None;
                cx.notify();
            }
            _ => {}
        }
    }

    fn recovery_problem(&self, cx: &App) -> Option<RecoveryProblem> {
        recovery_problem(
            self.phrase_input.read(cx).value().as_ref(),
            self.new_password.read(cx).value().as_ref(),
            self.confirm_password.read(cx).value().as_ref(),
        )
    }

    fn submit_recovery(&mut self, cx: &mut Context<Self>) {
        // Enter in any field lands here too, so the checks behind the disabled
        // Recover button must be repeated.
        if let Some(problem) = self.recovery_problem(cx) {
            self.error = Some(problem.message().into());
            cx.notify();
            return;
        }

        let phrase_text = Zeroizing::new(self.phrase_input.read(cx).value().to_string());
        let new_pw = self.new_password.read(cx).value().to_string();

        // Collapse any whitespace (spaces, newlines, tabs) between words.
        let normalized =
            Zeroizing::new(phrase_text.split_whitespace().collect::<Vec<_>>().join(" "));

        let phrase = match crate::vault::RecoveryPhrase::from_words(&normalized) {
            Ok(p) => p,
            Err(_) => {
                self.error = Some(
                    "Invalid recovery phrase. Check that you have exactly 24 valid BIP39 words."
                        .into(),
                );
                cx.notify();
                return;
            }
        };

        self.step = Step::RecoveryWorking;
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
                let secret = SecretString::new(new_pw);
                crate::vault::VaultManager::unlock_with_recovery(&path, &phrase, &secret)
            })();
            let _ = tx.try_send(result);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.recv().await else { return };
            this.update(cx, |this, cx| match result {
                Ok((manager, new_phrase)) => {
                    this.vault_state.update(cx, |vs, cx| {
                        *vs = VaultState::AwaitingPhraseConfirmation {
                            manager,
                            phrase: new_phrase,
                            is_rotation: true,
                        };
                        cx.notify();
                    });
                    this.step = Step::RecoveryForm;
                }
                Err(crate::vault::VaultError::InvalidRecoveryPhrase) => {
                    this.step = Step::RecoveryForm;
                    this.error = Some(
                        "Recovery phrase is incorrect. Check the words and their order.".into(),
                    );
                    cx.notify();
                }
                Err(e) => {
                    tracing::error!(?e, "vault recovery failed");
                    this.step = Step::RecoveryForm;
                    this.error = Some(format!("Recovery failed: {e}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_delete_input(
        &mut self,
        _: Entity<InputState>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.step, Step::DeleteForm) {
            return;
        }
        match event {
            InputEvent::PressEnter { .. } => self.delete_vault(cx),
            // The Delete button follows what is typed.
            InputEvent::Change => cx.notify(),
            _ => {}
        }
    }

    fn delete_vault(&mut self, cx: &mut Context<Self>) {
        if !is_delete_confirmed(self.delete_word.read(cx).value().as_ref()) {
            return;
        }
        // On success `RootView` clears this view and shows the create screen.
        if let Err(e) = crate::ui::root_view::delete_vault(&self.vault_state, cx) {
            self.error = Some(format!("The vault was not deleted: {e}"));
            cx.notify();
        }
    }

    fn render_delete_form(&self, cx: &Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let is_confirmed = is_delete_confirmed(self.delete_word.read(cx).value().as_ref());
        let note = |text: &'static str| {
            div()
                .text_size(px(11.5))
                .text_color(theme.muted_foreground)
                .child(text)
        };

        v_flex()
            .gap(px(8.))
            .child(div().text_lg().child("Delete vault"))
            .child(note(
                "Without the password or the recovery phrase this vault cannot be opened. \
                 Deleting it removes every host, key and snippet stored in it on this device \
                 and closes every open session. Then you can create a new vault.",
            ))
            .child(note(
                "If this vault synced, the account and its data stay on the server, but only \
                 signing in from a new vault with the same password brings them back. The \
                 recovery phrase works on this vault file alone and is of no use once it is \
                 deleted.",
            ))
            .children(
                self.error
                    .as_ref()
                    .map(|e| div().text_sm().text_color(theme.danger).child(e.clone())),
            )
            .child(Input::new(&self.delete_word))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("delete-vault-cancel")
                            .ghost()
                            .xsmall()
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.step = Step::RecoveryForm;
                                this.error = None;
                                this.delete_word
                                    .update(cx, |state, cx| state.set_value("", window, cx));
                                this.phrase_input
                                    .update(cx, |state, cx| state.focus(window, cx));
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("delete-vault")
                            .danger()
                            .child("Delete vault")
                            .disabled(!is_confirmed)
                            .on_click(cx.listener(|this, _, _, cx| this.delete_vault(cx))),
                    ),
            )
            .into_any_element()
    }
}

impl Render for UnlockView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        let inner: gpui::AnyElement = match self.step {
            Step::PasswordForm => v_flex()
                .gap(px(8.))
                .child(
                    v_flex()
                        .gap(px(2.))
                        .child(div().text_lg().child("Unlock vault"))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.muted_foreground)
                                .child("Enter your master password to continue."),
                        ),
                )
                .children(
                    self.error
                        .as_ref()
                        .map(|e| div().text_sm().text_color(theme.danger).child(e.clone())),
                )
                .child(Input::new(&self.password).mask_toggle())
                .child(
                    Button::new("unlock-vault")
                        .primary()
                        .child("Unlock")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.submit_password(window, cx)),
                        ),
                )
                .child(
                    Button::new("to-recovery")
                        .ghost()
                        .xsmall()
                        .child("Forgot password? Use recovery phrase")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.step = Step::RecoveryForm;
                            this.error = None;
                            this.phrase_input
                                .update(cx, |state, cx| state.focus(window, cx));
                            cx.notify();
                        })),
                )
                .into_any_element(),

            Step::PasswordWorking => v_flex()
                .gap_3()
                .items_center()
                .child(Spinner::new().large())
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Unlocking…"),
                )
                .into_any_element(),

            Step::RecoveryForm => {
                let problem = self.recovery_problem(cx);
                let word_count = recovery_word_count(self.phrase_input.read(cx).value().as_ref());
                let word_count_color = if word_count == RECOVERY_WORD_COUNT {
                    theme.success
                } else {
                    theme.muted_foreground
                };
                // The red error line already says it after a blocked Enter.
                let hint = problem
                    .map(RecoveryProblem::message)
                    .filter(|hint| self.error.as_deref() != Some(*hint));

                v_flex()
                    .gap(px(8.))
                    .child(
                        v_flex()
                            .gap(px(2.))
                            .child(div().text_lg().child("Recover vault"))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(theme.muted_foreground)
                                    .child(
                                        "Enter your 24-word recovery phrase and choose a new \
                                         password.",
                                    ),
                            ),
                    )
                    .children(
                        self.error
                            .as_ref()
                            .map(|e| div().text_sm().text_color(theme.danger).child(e.clone())),
                    )
                    .child(Input::new(&self.phrase_input))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(word_count_color)
                            .child(format!("{word_count} / {RECOVERY_WORD_COUNT} words")),
                    )
                    .child(Input::new(&self.new_password).mask_toggle())
                    .child(Input::new(&self.confirm_password).mask_toggle())
                    .children(hint.map(|hint| {
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.muted_foreground)
                            .child(hint)
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("back-to-pw")
                                    .ghost()
                                    .xsmall()
                                    .child("Back to password unlock")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.step = Step::PasswordForm;
                                        this.error = None;
                                        this.password
                                            .update(cx, |state, cx| state.focus(window, cx));
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("recover")
                                    .primary()
                                    .child("Recover")
                                    .disabled(problem.is_some())
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.submit_recovery(cx)),
                                    ),
                            ),
                    )
                    .child(
                        h_flex().child(
                            Button::new("to-delete-vault")
                                .ghost()
                                .xsmall()
                                .child("Lost the recovery phrase too? Delete this vault")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.step = Step::DeleteForm;
                                    this.error = None;
                                    this.delete_word
                                        .update(cx, |state, cx| state.focus(window, cx));
                                    cx.notify();
                                })),
                        ),
                    )
                    .into_any_element()
            }

            Step::RecoveryWorking => v_flex()
                .gap_3()
                .items_center()
                .child(Spinner::new().large())
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Recovering…"),
                )
                .into_any_element(),

            Step::DeleteForm => self.render_delete_form(cx),
        };

        v_flex()
            .size_full()
            .bg(theme.background)
            .items_center()
            .justify_center()
            .child(v_flex().w(px(300.)).gap(px(8.)).child(inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phrase(words: usize) -> String {
        vec!["abandon"; words].join(" ")
    }

    #[test]
    fn a_complete_form_has_no_problem() {
        assert_eq!(recovery_problem(&phrase(24), "secret", "secret"), None);
    }

    #[test]
    fn the_phrase_must_have_exactly_24_words() {
        for words in [0, 23, 25] {
            assert_eq!(
                recovery_problem(&phrase(words), "secret", "secret"),
                Some(RecoveryProblem::WordCount),
                "{words} words"
            );
        }
    }

    #[test]
    fn words_are_counted_across_any_whitespace() {
        let pasted = phrase(24).replace(' ', " \n\t ");
        assert_eq!(recovery_word_count(&format!("  {pasted}\n")), 24);
    }

    #[test]
    fn an_empty_new_password_is_a_problem_even_when_both_fields_agree() {
        assert_eq!(
            recovery_problem(&phrase(24), "", ""),
            Some(RecoveryProblem::EmptyPassword)
        );
    }

    #[test]
    fn the_new_password_must_be_confirmed() {
        assert_eq!(
            recovery_problem(&phrase(24), "secret", ""),
            Some(RecoveryProblem::EmptyConfirmation)
        );
        assert_eq!(
            recovery_problem(&phrase(24), "secret", "secert"),
            Some(RecoveryProblem::PasswordMismatch)
        );
    }

    #[test]
    fn deleting_takes_the_exact_word() {
        assert!(is_delete_confirmed("DELETE"));
        assert!(is_delete_confirmed(" DELETE\n"));
        for typed in ["", "delete", "DELET", "DELETE it"] {
            assert!(!is_delete_confirmed(typed), "{typed:?}");
        }
    }

    #[test]
    fn the_phrase_is_reported_before_the_passwords() {
        assert_eq!(
            recovery_problem(&phrase(3), "", "other"),
            Some(RecoveryProblem::WordCount)
        );
    }
}
