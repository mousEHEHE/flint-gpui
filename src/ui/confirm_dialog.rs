//! A yes-or-no question before an action that loses work.

use std::rc::Rc;

use gpui::prelude::*;
use gpui::{div, App, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    dialog::{DialogAction, DialogFooter, DialogHeader, DialogTitle},
    h_flex, v_flex, WindowExt as _,
};

use crate::ui::{dialog_chrome, rems_from_px, scaled_px, DIALOG_WIDE};

pub struct Question {
    pub title: SharedString,
    pub message: SharedString,
    /// Shown under the message in the terminal font, for text the user sends.
    pub preview: Option<SharedString>,
    pub confirm_label: SharedString,
    pub is_danger: bool,
}

pub fn ask(
    question: Question,
    window: &mut Window,
    cx: &mut App,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
) {
    let on_confirm = Rc::new(on_confirm);
    let question = Rc::new(question);
    window.open_dialog(cx, move |dialog, _, cx| {
        let on_confirm = on_confirm.clone();
        let confirm = Button::new("confirm-ok")
            .child(question.confirm_label.clone())
            .on_click(move |_, window, cx| {
                window.close_dialog(cx);
                on_confirm(window, cx);
            });
        let confirm = if question.is_danger {
            confirm.danger()
        } else {
            confirm.primary()
        };
        dialog_chrome(dialog).w(scaled_px(DIALOG_WIDE, cx)).child(
            v_flex()
                .p(rems_from_px(12.))
                .gap(rems_from_px(8.))
                .child(DialogHeader::new().child(DialogTitle::new().child(question.title.clone())))
                .child(
                    div()
                        .text_size(rems_from_px(12.5))
                        .child(question.message.clone()),
                )
                .when_some(question.preview.clone(), |el, preview| {
                    el.child(
                        div()
                            .p(rems_from_px(8.))
                            .rounded(rems_from_px(6.))
                            .bg(crate::theme::field())
                            .font_family(crate::theme::MONO_FONT)
                            .text_size(rems_from_px(12.))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(preview),
                    )
                })
                .child(
                    DialogFooter::new().child(
                        DialogAction::new().child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("confirm-cancel")
                                        .ghost()
                                        .child("Cancel")
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(confirm),
                        ),
                    ),
                ),
        )
    });
}
