use gpui::prelude::*;
use gpui::{px, AnyElement, App, Context, Entity, FocusHandle, Focusable, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    ActiveTheme as _, Disableable as _,
};
use secrecy::SecretString;
use zeroize::Zeroizing;

use crate::ui::{lock_screen, VaultState};

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
    phrase_input: Entity<TextareaState>,
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
                .placeholder("Master password")
                .masked(true)
        });
        let phrase_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(3)
                .placeholder("Recovery phrase")
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

        cx.subscribe(&phrase_input, |this, _, event: &InputEvent, cx| {
            this.on_recovery_event(event, cx)
        })
        .detach();

        cx.subscribe(&new_password, |this, _, event: &InputEvent, cx| {
            this.on_recovery_event(event, cx)
        })
        .detach();

        cx.subscribe(&confirm_password, |this, _, event: &InputEvent, cx| {
            this.on_recovery_event(event, cx)
        })
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
        self.phrase_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        for input in [
            &self.password,
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

    fn on_recovery_event(&mut self, event: &InputEvent, cx: &mut Context<Self>) {
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
                    "This is not a valid recovery phrase. Check the spelling and order of the \
                     words."
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

    fn render_password_form(&self, cx: &mut Context<Self>) -> AnyElement {
        lock_screen::column()
            .child(lock_screen::header("Unlock vault", None, cx))
            .child(Input::new(&self.password).mask_toggle())
            .children(self.error.as_deref().map(|e| lock_screen::error(e, cx)))
            .child(
                Button::new("unlock-vault")
                    .primary()
                    .child("Unlock")
                    .on_click(cx.listener(|this, _, window, cx| this.submit_password(window, cx))),
            )
            .child(lock_screen::links().child(
                lock_screen::link("to-recovery", "Forgot password? Use recovery phrase").on_click(
                    cx.listener(|this, _, window, cx| {
                        this.step = Step::RecoveryForm;
                        this.error = None;
                        this.phrase_input
                            .update(cx, |state, cx| state.focus(window, cx));
                        cx.notify();
                    }),
                ),
            ))
            .into_any_element()
    }

    fn render_recovery_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let problem = self.recovery_problem(cx);
        let word_count = recovery_word_count(self.phrase_input.read(cx).value().as_ref());
        let word_count_color = if word_count == RECOVERY_WORD_COUNT {
            theme.success
        } else {
            theme.muted_foreground
        };
        // The red error line already says it after a blocked Enter.
        let hint = problem
            .filter(|problem| *problem == RecoveryProblem::PasswordMismatch)
            .map(RecoveryProblem::message)
            .filter(|hint| self.error.as_deref() != Some(*hint));

        lock_screen::column()
            .child(lock_screen::header(
                "Recover vault",
                Some("Enter your 24-word recovery phrase and choose a new password."),
                cx,
            ))
            .child(Textarea::new(&self.phrase_input).h(px(72.)))
            .child(lock_screen::note(
                format!("{word_count} / {RECOVERY_WORD_COUNT} words"),
                word_count_color,
            ))
            .child(Input::new(&self.new_password).mask_toggle())
            .child(Input::new(&self.confirm_password).mask_toggle())
            .children(self.error.as_deref().map(|e| lock_screen::error(e, cx)))
            .children(hint.map(|hint| lock_screen::note(hint, theme.muted_foreground)))
            .child(
                Button::new("recover")
                    .primary()
                    .child("Recover")
                    .disabled(problem.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.submit_recovery(cx))),
            )
            .child(
                lock_screen::links()
                    .child(
                        lock_screen::link("back-to-pw", "Back to password unlock").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.step = Step::PasswordForm;
                                this.error = None;
                                this.password
                                    .update(cx, |state, cx| state.focus(window, cx));
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        lock_screen::link(
                            "to-delete-vault",
                            "Lost the recovery phrase too? Delete this vault",
                        )
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

    fn render_delete_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let is_confirmed = is_delete_confirmed(self.delete_word.read(cx).value().as_ref());

        lock_screen::column()
            .child(lock_screen::header(
                "Delete vault",
                Some(
                    "Without the password or the recovery phrase this vault cannot be opened. \
                     Deleting it removes its hosts, keys and snippets from this device and \
                     closes open sessions.",
                ),
                cx,
            ))
            .child(lock_screen::note(
                "Synced data stays in your account. To get it back, create a new vault with \
                 the same password and sign in to sync. The recovery phrase only opens this \
                 vault file, so it is of no use once the vault is deleted.",
                muted,
            ))
            .child(Input::new(&self.delete_word))
            .children(self.error.as_deref().map(|e| lock_screen::error(e, cx)))
            .child(
                Button::new("delete-vault")
                    .danger()
                    .child("Delete vault")
                    .disabled(!is_confirmed)
                    .on_click(cx.listener(|this, _, _, cx| this.delete_vault(cx))),
            )
            .child(lock_screen::links().child(
                lock_screen::link("delete-vault-cancel", "Cancel").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.step = Step::RecoveryForm;
                        this.error = None;
                        this.delete_word
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        this.phrase_input
                            .update(cx, |state, cx| state.focus(window, cx));
                        cx.notify();
                    },
                )),
            ))
            .into_any_element()
    }
}

impl Render for UnlockView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.step {
            Step::PasswordForm => self.render_password_form(cx),
            Step::PasswordWorking => lock_screen::working("Unlocking\u{2026}", cx),
            Step::RecoveryForm => self.render_recovery_form(cx),
            Step::RecoveryWorking => lock_screen::working("Recovering\u{2026}", cx),
            Step::DeleteForm => self.render_delete_form(cx),
        };

        lock_screen::screen(content, window, cx)
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
