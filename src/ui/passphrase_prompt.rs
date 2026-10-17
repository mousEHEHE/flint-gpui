//! Dialog asking for a private-key passphrase right before connecting.
//! Shown only when the stored key is encrypted and the vault has no working
//! passphrase for it. Validation happens in app_view (it holds the PEM);
//! wrong answers come back via `set_error`.

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, EventEmitter, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    dialog::{DialogHeader, DialogTitle},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme as _, Sizable as _,
};
use zeroize::Zeroizing;

pub enum PassphrasePromptEvent {
    Submitted(Zeroizing<String>),
    Cancelled,
}

pub struct PassphrasePromptView {
    /// Vault name of the key ("Production key").
    key_name: String,
    /// Server the connection is for ("deploy@web.example.com").
    target: String,
    input: Entity<InputState>,
    error: Option<String>,
}

impl EventEmitter<PassphrasePromptEvent> for PassphrasePromptView {}

impl PassphrasePromptView {
    pub fn new(
        key_name: String,
        target: String,
        initial_error: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Passphrase")
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.subscribe(&input, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.submit(cx);
            }
        })
        .detach();

        Self {
            key_name,
            target,
            input,
            error: initial_error,
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let value = self.input.read(cx).value().to_string();
        if value.is_empty() {
            self.error = Some("Enter the passphrase.".to_string());
            cx.notify();
            return;
        }
        cx.emit(PassphrasePromptEvent::Submitted(Zeroizing::new(value)));
    }

    /// Called by app_view when the submitted passphrase doesn't decrypt the key.
    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.error = Some(message);
        cx.notify();
    }
}

impl Render for PassphrasePromptView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        v_flex()
            .p(px(12.))
            .gap(px(8.))
            .child(DialogHeader::new().child(DialogTitle::new().child("Key passphrase")))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "The key \"{}\" for {} is encrypted.",
                        self.key_name, self.target,
                    )),
            )
            .child(Input::new(&self.input).mask_toggle())
            .children(self.error.as_ref().map(|msg| {
                div()
                    .text_size(px(11.5))
                    .text_color(theme.danger)
                    .child(msg.clone())
            }))
            .child(
                h_flex()
                    .gap(px(8.))
                    .justify_end()
                    .child(
                        Button::new("passphrase-cancel")
                            .small()
                            .ghost()
                            .child("Cancel")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(PassphrasePromptEvent::Cancelled);
                            })),
                    )
                    .child(
                        Button::new("passphrase-connect")
                            .small()
                            .primary()
                            .child("Connect")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.submit(cx);
                            })),
                    ),
            )
    }
}
