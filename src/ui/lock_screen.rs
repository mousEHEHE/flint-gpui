use gpui::prelude::*;
use gpui::{div, AnyElement, App, Div, ElementId, Hsla, Rems, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    spinner::Spinner,
    v_flex, ActiveTheme as _, Icon, Sizable as _,
};

use crate::ui::rems_from_px;
use crate::ui::window_chrome::{self, TITLE_BAR_HEIGHT};

const COLUMN_WIDTH: Rems = rems_from_px(300.);
const NOTE_TEXT_SIZE: Rems = rems_from_px(11.);

/// The whole window while the vault is locked: a bare title bar, and the
/// content centred below it.
pub fn screen(content: impl IntoElement, window: &Window, cx: &App) -> Div {
    v_flex()
        .size_full()
        .bg(cx.theme().background)
        .child(window_chrome::bare_title_bar(window))
        .child(
            v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                // Balances the title bar, so the content sits in the middle of
                // the window and not of the area below the bar.
                .pb(TITLE_BAR_HEIGHT)
                .child(content),
        )
}

/// The column the lock-screen forms are laid out in.
pub fn column() -> Div {
    v_flex().w(COLUMN_WIDTH).gap(rems_from_px(8.))
}

pub fn header(title: impl Into<SharedString>, subtitle: Option<&'static str>, cx: &App) -> Div {
    v_flex()
        .gap(rems_from_px(4.))
        .pb(rems_from_px(4.))
        .child(
            div()
                .text_size(rems_from_px(16.))
                .font_weight(gpui::FontWeight::MEDIUM)
                .child(title.into()),
        )
        .when_some(subtitle, |el, subtitle| {
            el.child(note(subtitle, cx.theme().muted_foreground))
        })
}

/// Small text under or between the fields: hints, counters and errors.
pub fn note(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .text_size(NOTE_TEXT_SIZE)
        .text_color(color)
        .child(text.into())
}

/// Shown wherever sync is offered: what people ask first is "does it cost" and "can you read it".
pub fn sync_promise(cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .w_full()
        .gap(rems_from_px(8.))
        .px(rems_from_px(10.))
        .py(rems_from_px(8.))
        .rounded(rems_from_px(6.))
        .bg(crate::theme::element())
        .child(
            Icon::empty()
                .path("icons/shield-check.svg")
                .size(rems_from_px(16.))
                .text_color(theme.primary),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(rems_from_px(12.))
                .text_color(theme.foreground)
                .child("Free. Encrypted on your device, so even our server can't read it."),
        )
}

pub fn error(text: &str, cx: &App) -> Div {
    note(text.to_string(), cx.theme().danger)
}

/// The secondary actions under the main button, one per line, centred.
pub fn links() -> Div {
    v_flex()
        .items_center()
        .gap(rems_from_px(2.))
        .pt(rems_from_px(4.))
}

pub fn link(id: impl Into<ElementId>, label: &'static str) -> Button {
    Button::new(id).ghost().xsmall().child(label)
}

/// What shows while the vault is being created or opened.
pub fn working(label: impl Into<SharedString>, cx: &App) -> AnyElement {
    v_flex()
        .gap(rems_from_px(8.))
        .items_center()
        .child(Spinner::new())
        .child(note(label, cx.theme().muted_foreground))
        .into_any_element()
}
