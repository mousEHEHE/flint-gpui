//! The "Check for updates?" question, put once on an OS with builds until
//! the user answers it. Closing it without an answer leaves it for the next
//! start.

use gpui::prelude::*;
use gpui::{div, Context, EventEmitter, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    dialog::{DialogHeader, DialogTitle},
    h_flex, v_flex, ActiveTheme as _, Sizable as _,
};

use crate::preferences::UpdateChecks;
use crate::ui::rems_from_px;
use crate::ui::update_service::UpdateService;

pub enum UpdateConsentEvent {
    Answered,
}

pub struct UpdateConsentView;

impl EventEmitter<UpdateConsentEvent> for UpdateConsentView {}

impl UpdateConsentView {
    fn answer_button(
        id: &'static str,
        label: &'static str,
        answer: UpdateChecks,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .small()
            .child(label)
            .on_click(cx.listener(move |_, _, _, cx| {
                UpdateService::global(cx)
                    .update(cx, |service, cx| service.set_update_checks(answer, cx));
                cx.emit(UpdateConsentEvent::Answered);
            }))
    }
}

impl Render for UpdateConsentView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted_fg) = (theme.foreground, theme.muted_foreground);

        v_flex()
            .p(rems_from_px(12.))
            .gap(rems_from_px(8.))
            .child(DialogHeader::new().child(DialogTitle::new().child("Check for updates?")))
            .child(div().text_size(rems_from_px(12.)).text_color(fg).child(
                "flint can check for a new version on start and every 6 hours. \
                 It sends only your OS name.",
            ))
            .child(
                div()
                    .text_size(rems_from_px(11.))
                    .text_color(muted_fg)
                    .child("You can change it later in Settings → About."),
            )
            .child(
                h_flex()
                    .gap(rems_from_px(8.))
                    .justify_end()
                    .child(Self::answer_button(
                        "update-consent-manual",
                        "Don't check",
                        UpdateChecks::Manual,
                        cx,
                    ))
                    .child(
                        Self::answer_button(
                            "update-consent-on-start",
                            "Check automatically",
                            UpdateChecks::OnStart,
                            cx,
                        )
                        .primary(),
                    ),
            )
    }
}
