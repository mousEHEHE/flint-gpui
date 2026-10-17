use std::path::Path;
use std::time::Duration;

use gpui::prelude::*;
use gpui::{div, px, App, ClipboardItem, Context, Entity, FocusHandle, Focusable, Task, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, ActiveTheme as _,
};
use zeroize::Zeroizing;

use crate::ui::phrase_panel::{phrase_file_text, phrase_panel};
use crate::ui::{lock_screen, VaultState};

const COPIED_LABEL_DURATION: Duration = Duration::from_secs(2);
const PHRASE_FILE_NAME: &str = "flint-recovery-phrase.txt";

pub struct PhraseView {
    focus_handle: FocusHandle,
    vault_state: Entity<VaultState>,
    is_copied: bool,
    copied_reset: Option<Task<()>>,
    /// Outcome of the last "Save as text file": the saved path, or the error.
    save_result: Option<Result<String, String>>,
    is_save_dialog_open: bool,
    /// Counts the phrases this view has shown. A save that finishes after its
    /// phrase was confirmed must not report on the next phrase's screen.
    phrase_generation: u64,
}

impl Focusable for PhraseView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Writes the phrase file. On Unix only the current user can read it.
fn write_phrase_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    // The open mode only applies to a new file. An overwritten file keeps its
    // old mode, so narrow it before the words are written.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(text.as_bytes())
}

impl PhraseView {
    pub fn new(
        vault_state: Entity<VaultState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            vault_state,
            is_copied: false,
            copied_reset: None,
            save_result: None,
            is_save_dialog_open: false,
            phrase_generation: 0,
        }
    }

    fn phrase_words(&self, cx: &App) -> Option<Zeroizing<String>> {
        match self.vault_state.read(cx) {
            VaultState::AwaitingPhraseConfirmation { phrase, .. } => {
                Some(Zeroizing::new(phrase.expose_words().to_string()))
            }
            _ => None,
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        // This view outlives the phrase, so a later phrase must start clean.
        self.is_copied = false;
        self.copied_reset = None;
        self.save_result = None;
        self.phrase_generation += 1;
        self.vault_state.update(cx, |vs, cx| {
            let taken = std::mem::replace(vs, VaultState::Locked);
            match taken {
                VaultState::AwaitingPhraseConfirmation { manager, .. } => {
                    *vs = VaultState::Unlocked { manager };
                }
                other => *vs = other,
            }
            cx.notify();
        });
    }

    fn copy_phrase(&mut self, cx: &mut Context<Self>) {
        let Some(words) = self.phrase_words(cx) else {
            return;
        };
        let plain = words.split_whitespace().collect::<Vec<_>>().join(" ");
        cx.write_to_clipboard(ClipboardItem::new_string(plain));
        self.is_copied = true;
        // Replacing the task drops the old timer, so a second click restarts it.
        self.copied_reset = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_LABEL_DURATION).await;
            this.update(cx, |this, cx| {
                this.is_copied = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn save_phrase(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(words) = self.phrase_words(cx) else {
            return;
        };
        if self.is_save_dialog_open {
            return;
        }
        self.is_save_dialog_open = true;
        let generation = self.phrase_generation;
        let text = Zeroizing::new(phrase_file_text(&words));
        cx.spawn_in(window, async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .set_file_name(PHRASE_FILE_NAME)
                .add_filter("Text", &["txt"])
                .save_file()
                .await;
            let Some(handle) = handle else {
                this.update(cx, |this, _| this.is_save_dialog_open = false)
                    .ok();
                return;
            };
            let path = handle.path().to_path_buf();
            let shown_path = path.display().to_string();
            let written = cx
                .background_executor()
                .spawn(async move { write_phrase_file(&path, &text) })
                .await;
            let result = match written {
                Ok(()) => {
                    tracing::info!(path = %shown_path, "recovery phrase saved to file");
                    Ok(shown_path)
                }
                Err(e) => {
                    tracing::error!(?e, path = %shown_path, "failed to save recovery phrase");
                    Err(format!("Could not save the file: {e}"))
                }
            };
            this.update(cx, |this, cx| {
                this.is_save_dialog_open = false;
                if this.phrase_generation == generation {
                    this.save_result = Some(result);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for PhraseView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_rotation = matches!(
            self.vault_state.read(cx),
            VaultState::AwaitingPhraseConfirmation {
                is_rotation: true,
                ..
            }
        );
        let Some(words) = self.phrase_words(cx) else {
            return div().into_any_element();
        };

        let theme = cx.theme();

        let subtitle: &'static str = if is_rotation {
            "Your previous phrase no longer works. This one is shown only once."
        } else {
            "Shown only once. If you forget your password, these 24 words are the only way back into this vault."
        };

        let save_message = self.save_result.as_ref().map(|result| match result {
            Ok(path) => lock_screen::note(format!("Saved to {path}"), theme.muted_foreground),
            Err(error) => lock_screen::error(error, cx),
        });

        lock_screen::screen(
            v_flex()
                .w(px(480.))
                .gap_3()
                .child(phrase_panel(
                    &words,
                    "Save your recovery phrase",
                    subtitle,
                    cx,
                ))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("copy-phrase")
                                .child(if self.is_copied { "Copied" } else { "Copy" })
                                .on_click(cx.listener(|this, _, _, cx| this.copy_phrase(cx))),
                        )
                        .child(
                            Button::new("save-phrase")
                                .child("Save as text file")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.save_phrase(window, cx)),
                                ),
                        ),
                )
                .children(save_message)
                .child(lock_screen::note(
                    "Copied words stay on the clipboard, and the text file is not encrypted.",
                    theme.muted_foreground,
                ))
                .child(
                    Button::new("confirm-phrase")
                        .primary()
                        .child("I have saved this phrase")
                        .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                ),
            window,
            cx,
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_phrase_file_replaces_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phrase.txt");
        std::fs::write(&path, "an older and much longer file body").unwrap();

        write_phrase_file(&path, "alpha beta\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha beta\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_phrase_file_narrows_the_mode_of_an_existing_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phrase.txt");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_phrase_file(&path, "alpha beta\n").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_phrase_file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phrase.txt");
        write_phrase_file(&path, "alpha beta\n").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
