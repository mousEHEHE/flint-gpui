use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, Div, ElementId, Hsla, Pixels, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    spinner::Spinner,
    v_flex, ActiveTheme as _, Sizable as _,
};

use crate::ui::window_chrome::{self, TITLE_BAR_HEIGHT};

const COLUMN_WIDTH: Pixels = px(300.);
const NOTE_TEXT_SIZE: Pixels = px(11.);

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
    v_flex().w(COLUMN_WIDTH).gap(px(8.))
}

pub fn header(title: impl Into<SharedString>, subtitle: Option<&'static str>, cx: &App) -> Div {
    v_flex()
        .gap(px(4.))
        .pb(px(4.))
        .child(
            div()
                .text_size(px(16.))
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

pub fn error(text: &str, cx: &App) -> Div {
    note(text.to_string(), cx.theme().danger)
}

/// The secondary actions under the main button, one per line, centred.
pub fn links() -> Div {
    v_flex().items_center().gap(px(2.)).pt(px(4.))
}

pub fn link(id: impl Into<ElementId>, label: &'static str) -> Button {
    Button::new(id).ghost().xsmall().child(label)
}

/// What shows while the vault is being created or opened.
pub fn working(label: &'static str, cx: &App) -> AnyElement {
    v_flex()
        .gap(px(8.))
        .items_center()
        .child(Spinner::new())
        .child(note(label, cx.theme().muted_foreground))
        .into_any_element()
}
