//! The "write to the developer" dialog: a bug, an idea or anything else,
//! sent to the sync server under the signed-in account.

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, EventEmitter, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    dialog::{DialogHeader, DialogTitle},
    h_flex,
    input::{Textarea, TextareaState},
    v_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _,
};

use crate::account::api::FeedbackKind;
use crate::account::{Api, ApiError};
use crate::ui::VaultState;

/// The server takes at most this many characters.
const MAX_MESSAGE_CHARS: usize = 4000;

pub enum FeedbackEvent {
    Closed,
}

enum Status {
    Writing,
    Sending,
    Sent,
    Failed(String),
}

pub struct FeedbackView {
    vault_state: Entity<VaultState>,
    message: Entity<TextareaState>,
    kind: FeedbackKind,
    status: Status,
}

impl EventEmitter<FeedbackEvent> for FeedbackView {}

impl FeedbackView {
    pub fn new(
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let message = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(8)
                .placeholder("What happened, or what would you like to see?")
        });
        message.update(cx, |state, cx| state.focus(window, cx));
        Self {
            vault_state,
            message,
            kind: FeedbackKind::Bug,
            status: Status::Writing,
        }
    }

    /// The session a message is sent under. `None` when the vault is not
    /// signed in to sync: a message has to come from an account.
    fn session_token(&self, cx: &Context<Self>) -> Option<String> {
        self.vault_state
            .read(cx)
            .manager()?
            .sync_state()?
            .session_token
            .clone()
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Sending | Status::Sent) {
            return;
        }
        let Some(token) = self.session_token(cx) else {
            return;
        };
        let text = self.message.read(cx).value().trim().to_string();
        if text.is_empty() {
            self.status = Status::Failed("Write a message first.".into());
            cx.notify();
            return;
        }
        if text.chars().count() > MAX_MESSAGE_CHARS {
            self.status = Status::Failed(format!(
                "The message is too long. Keep it under {MAX_MESSAGE_CHARS} characters."
            ));
            cx.notify();
            return;
        }
        self.status = Status::Sending;
        cx.notify();

        let kind = self.kind;
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.try_send(Api::new().send_feedback(&token, kind, &text));
        });
        cx.spawn(async move |this, cx| {
            let Ok(sent) = rx.recv().await else { return };
            this.update(cx, |this, cx| {
                this.status = match sent {
                    Ok(()) => {
                        tracing::info!("feedback sent");
                        Status::Sent
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "feedback was not sent");
                        Status::Failed(send_problem(&e))
                    }
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

fn send_problem(error: &ApiError) -> String {
    match error.code() {
        Some("rate_limited") => "That is a lot of messages. Try again in an hour.".into(),
        Some("unauthorized") => {
            "This device was signed out of sync. Sign in again under Settings, Sync.".into()
        }
        _ => format!("The message was not sent: {error}"),
    }
}

impl Render for FeedbackView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted_fg, danger, success) = (
            theme.foreground,
            theme.muted_foreground,
            theme.danger,
            theme.success,
        );
        let caption =
            move |text: &'static str| div().text_size(px(11.)).text_color(muted_fg).child(text);
        let close = |label: &'static str, cx: &mut Context<Self>| {
            Button::new("feedback-close")
                .small()
                .ghost()
                .child(label)
                .on_click(cx.listener(|_, _, _, cx| cx.emit(FeedbackEvent::Closed)))
        };

        let body = v_flex()
            .p(px(12.))
            .gap(px(8.))
            .child(DialogHeader::new().child(DialogTitle::new().child("Write to the developer")));

        if self.session_token(cx).is_none() {
            return body
                .child(div().text_size(px(12.)).text_color(fg).child(
                    "Messages are sent from your sync account. Sign in under Settings, Sync first.",
                ))
                .child(h_flex().justify_end().child(close("Close", cx)));
        }
        if matches!(self.status, Status::Sent) {
            return body
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(success)
                        .child("Sent. Thank you."),
                )
                .child(h_flex().justify_end().child(close("Close", cx)));
        }

        let is_sending = matches!(self.status, Status::Sending);
        let kinds = [
            (FeedbackKind::Bug, "Bug"),
            (FeedbackKind::Idea, "Idea"),
            (FeedbackKind::Other, "Other"),
        ];
        let kind_buttons = kinds.into_iter().map(|(kind, label)| {
            Button::new(SharedString::from(format!("feedback-kind-{label}")))
                .xsmall()
                .ghost()
                .selected(self.kind == kind)
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.kind = kind;
                    cx.notify();
                }))
        });

        body.child(h_flex().gap(px(6.)).children(kind_buttons))
            .child(Textarea::new(&self.message).h(px(150.)))
            .child(caption(
                "Sent with your GitHub login, the app version and your OS.",
            ))
            .children(match &self.status {
                Status::Failed(problem) => Some(
                    div()
                        .text_size(px(11.5))
                        .text_color(danger)
                        .child(problem.clone()),
                ),
                _ => None,
            })
            .child(
                h_flex()
                    .gap(px(8.))
                    .justify_end()
                    .child(close("Cancel", cx).disabled(is_sending))
                    .child(
                        Button::new("feedback-send")
                            .small()
                            .primary()
                            .child("Send")
                            .loading(is_sending)
                            .disabled(is_sending)
                            .on_click(cx.listener(|this, _, _, cx| this.send(cx))),
                    ),
            )
    }
}
