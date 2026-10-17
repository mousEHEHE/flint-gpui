//! Forwards popover: start and stop the port forwards saved on a server.
//!
//! A pill in the terminal header shows how many are up; clicking it opens a
//! panel listing every forward the host defines, with what it routes and a
//! Start/Stop button. Forwards are defined in the server form. This panel
//! only runs them.
//!
//! This is part of `impl ServerSession`, kept out of `server_session.rs` so
//! that file stays about the connection lifecycle.

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context, Hsla, SharedString};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    tooltip::Tooltip,
    v_flex, ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _,
};

use crate::connection::{is_loopback_bind, ForwardKind};
use crate::ssh::ForwardState;
use crate::theme;
use crate::ui::server_session::{ForwardRuntime, ServerSession};
use crate::ui::terminal_tabs::header_button;

const POPOVER_W: f32 = 340.0;

impl ServerSession {
    pub fn toggle_forwards(&mut self, cx: &mut Context<Self>) {
        self.forwards_open = !self.forwards_open;
        // The two popovers share a corner, so only one can be open.
        self.tasks_open = false;
        cx.notify();
    }

    fn close_forwards(&mut self, cx: &mut Context<Self>) {
        self.forwards_open = false;
        cx.notify();
    }

    /// Start a stopped forward, or stop a running one. The result comes back
    /// as a `ForwardChanged` event, so nothing is assumed here.
    fn toggle_forward(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(forward) = self.forwards.iter().find(|f| f.spec.id == id) else {
            return;
        };
        let Some(conn) = self.connection.clone() else {
            return;
        };
        let spec = forward.spec.clone();
        let is_bound = forward.is_bound();
        cx.spawn(async move |_, _| {
            if is_bound {
                conn.stop_forward(spec.id).await;
            } else {
                conn.start_forward(spec).await;
            }
        })
        .detach();
    }

    pub fn render_forward_chip(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.forwards.is_empty() {
            return None;
        }

        let bound = self.forwards.iter().filter(|f| f.is_bound()).count();
        let has_failed = self.forwards.iter().any(|f| f.failure().is_some());
        let count_color = if has_failed {
            theme::run_red()
        } else {
            theme::text_dim()
        };
        let count = format!("{bound}/{}", self.forwards.len());

        Some(
            header_button("forward-chip", cx)
                .gap(px(4.))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_forwards(cx)))
                .child("Forwards")
                .child(
                    div()
                        .text_color(count_color)
                        .child(SharedString::from(count)),
                )
                .into_any_element(),
        )
    }

    pub fn render_forwards_popover(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.forwards_open || self.forwards.is_empty() {
            return None;
        }

        let rows: Vec<AnyElement> = self
            .forwards
            .iter()
            .enumerate()
            .map(|(ix, forward)| self.render_forward_row(ix, forward, cx))
            .collect();

        let popover = v_flex()
            .occlude()
            .absolute()
            .right(px(12.))
            .bottom(px(12.))
            .w(px(POPOVER_W))
            .bg(theme::pop_surface())
            .border_1()
            .border_color(theme::border_focused())
            .rounded(px(6.))
            .shadow_lg()
            .overflow_hidden()
            .child(self.render_forwards_head(cx))
            .child(
                div()
                    .id("forwards-pop-body")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .p(px(3.))
                    .child(v_flex().gap(px(1.)).children(rows)),
            );

        Some(popover.into_any_element())
    }

    fn render_forwards_head(&self, cx: &Context<Self>) -> AnyElement {
        h_flex()
            .h(px(28.))
            .pl(px(11.))
            .pr(px(6.))
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(theme::sunken_soft())
            .child(
                div()
                    .text_size(px(10.))
                    .font_semibold()
                    .text_color(theme::muted_gray())
                    .child("PORT FORWARDING"),
            )
            .child(div().flex_1())
            .child(
                Button::new("forwards-close")
                    .ghost()
                    .icon(IconName::Close)
                    .xsmall()
                    .on_click(cx.listener(|this, _, _, cx| this.close_forwards(cx))),
            )
            .into_any_element()
    }

    fn render_forward_row(
        &self,
        index: usize,
        forward: &ForwardRuntime,
        cx: &Context<Self>,
    ) -> AnyElement {
        let is_bound = forward.is_bound();
        let is_live = self.connection.is_some();
        let id = forward.spec.id.clone();

        h_flex()
            .id(("forward-row", index))
            .h(px(40.))
            .items_center()
            .gap(px(8.))
            .px(px(8.))
            .rounded(px(3.))
            .child(forward_kind_pill(forward.spec.kind))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        h_flex()
                            .gap(px(5.))
                            .items_center()
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(px(12.))
                                    .font_semibold()
                                    .text_color(cx.theme().foreground)
                                    .truncate()
                                    .child(SharedString::from(forward.name.clone())),
                            )
                            .when(!is_loopback_bind(&forward.spec.bind_address), |el| {
                                el.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(10.))
                                        .font_semibold()
                                        .text_color(theme::warning())
                                        .child("PUBLIC"),
                                )
                            })
                            .child(div().flex_1())
                            .child({
                                let label = SharedString::from(state_label(
                                    &forward.state,
                                    forward.spec.bind_port,
                                ));
                                let tooltip_text = label.clone();
                                // A stop reason can be long; the tooltip keeps it readable.
                                div()
                                    .id(("forward-state", index))
                                    .flex_shrink_0()
                                    .max_w(px(140.))
                                    .truncate()
                                    .text_size(px(10.5))
                                    .text_color(state_color(&forward.state))
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(tooltip_text.clone()).build(window, cx)
                                    })
                                    .child(label)
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(SharedString::from(forward.spec.route_label())),
                    ),
            )
            .child(
                Button::new(SharedString::from(format!("forward-toggle-{id}")))
                    .ghost()
                    .small()
                    .child(if is_bound { "Stop" } else { "Start" })
                    .when(!is_live, |b| b.disabled(true))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_forward(id.clone(), cx);
                    })),
            )
            .into_any_element()
    }
}

/// What the forward is doing, in one short line.
fn state_label(state: &ForwardState, bind_port: u16) -> String {
    match state {
        // A remote forward on port 0 gets its port from the server, so the
        // route line does not show it.
        ForwardState::Bound { bound_port, active } if *bound_port != bind_port => {
            format!("port {bound_port} \u{00b7} {}", activity_label(*active))
        }
        ForwardState::Bound { active, .. } => activity_label(*active),
        ForwardState::Stopped { reason: None } => "stopped".to_string(),
        ForwardState::Stopped {
            reason: Some(reason),
        } => reason.clone(),
    }
}

fn activity_label(active: u32) -> String {
    match active {
        0 => "listening".to_string(),
        1 => "1 connection".to_string(),
        n => format!("{n} connections"),
    }
}

fn state_color(state: &ForwardState) -> Hsla {
    match state {
        ForwardState::Bound { .. } => theme::status_online(),
        ForwardState::Stopped { reason: None } => theme::text_dim(),
        ForwardState::Stopped { reason: Some(_) } => theme::run_red(),
    }
}

/// Small neutral pill showing a forward's kind (Local / Remote / SOCKS5).
fn forward_kind_pill(kind: ForwardKind) -> impl IntoElement {
    div()
        .flex_none()
        .h(px(18.))
        .px(px(4.))
        .flex()
        .items_center()
        .rounded(px(3.))
        .bg(theme::element())
        .text_size(px(10.))
        .text_color(theme::muted_gray())
        .child(kind.display_label())
}
