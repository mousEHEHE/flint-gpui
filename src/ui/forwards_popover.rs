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
use gpui::{div, linear_color_stop, linear_gradient, px, AnyElement, Context, Hsla, SharedString};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
};

use crate::connection::{is_loopback_bind, ForwardKind};
use crate::ssh::ForwardState;
use crate::theme;
use crate::ui::server_session::{ForwardRuntime, ServerSession};

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
        let failed = self.forwards.iter().any(|f| f.failure().is_some());
        // `status_off` is the idle grey of the sidebar capsules: a fill, not
        // a text colour. On the chip's own background it reads at ~1.9:1, so
        // the idle count uses the dim text colour of the popover rows.
        let color = if failed {
            theme::run_red()
        } else if bound > 0 {
            theme::status_online()
        } else {
            theme::text_dim()
        };
        let label = format!("{bound}/{}", self.forwards.len());

        Some(
            div()
                .id("forward-chip")
                .flex()
                .items_center()
                .gap(px(5.))
                .h(px(19.))
                .px(px(8.))
                .rounded(px(3.))
                .border_1()
                .border_color(theme::border_focused())
                .bg(theme::element())
                .text_size(px(10.5))
                .font_semibold()
                .text_color(color)
                .cursor_pointer()
                .hover(|s| s.bg(theme::element_hover()))
                .active(|s| s.bg(theme::element_active()))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_forwards(cx)))
                .child("⇄")
                .child(SharedString::from(label))
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
            .bg(linear_gradient(
                180.,
                linear_color_stop(theme::pop_grad_top(), 0.),
                linear_color_stop(theme::pop_grad_bottom(), 1.),
            ))
            .border_1()
            .border_color(cx.theme().border)
            .rounded(px(4.))
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
        let bound = self.forwards.iter().filter(|f| f.is_bound()).count();

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
                    .font_bold()
                    .text_color(theme::muted_gray())
                    .child("PORT FORWARDING"),
            )
            .when(bound > 0, |el| {
                el.child(
                    h_flex()
                        .items_center()
                        .gap(px(4.))
                        .h(px(15.))
                        .pl(px(5.))
                        .pr(px(6.))
                        .rounded(px(3.))
                        .bg(theme::accent_soft())
                        .border_1()
                        .border_color(theme::accent_line())
                        .text_size(px(9.5))
                        .font_semibold()
                        .text_color(theme::accent())
                        .child(
                            div()
                                .w(px(4.))
                                .h(px(4.))
                                .rounded(px(999.))
                                .bg(theme::accent()),
                        )
                        .child(SharedString::from(bound.to_string())),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .id("forwards-close")
                    .w(px(19.))
                    .h(px(19.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .text_size(px(11.))
                    .text_color(theme::text_dim())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::element_hover()).text_color(theme::text()))
                    .active(|s| s.bg(theme::element_active()))
                    .on_click(cx.listener(|this, _, _, cx| this.close_forwards(cx)))
                    .child("✕"),
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
            .items_center()
            .gap(px(8.))
            .px(px(7.))
            .py(px(6.))
            .rounded(px(3.))
            .child(forward_kind_pill(forward.spec.kind, cx))
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
                                    .text_size(px(11.5))
                                    .font_semibold()
                                    .text_color(cx.theme().foreground)
                                    .truncate()
                                    .child(SharedString::from(forward.name.clone())),
                            )
                            .when(!is_loopback_bind(&forward.spec.bind_address), |el| {
                                el.child(
                                    div()
                                        .text_size(px(9.))
                                        .font_bold()
                                        .text_color(theme::warning())
                                        .child("PUBLIC"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(SharedString::from(forward.spec.route_label())),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(state_color(&forward.state))
                            .truncate()
                            .child(SharedString::from(state_label(&forward.state))),
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
fn state_label(state: &ForwardState) -> String {
    match state {
        ForwardState::Bound {
            bound_port,
            active: 0,
        } => format!("bound on {bound_port}"),
        ForwardState::Bound {
            bound_port,
            active: 1,
        } => {
            format!("bound on {bound_port} · 1 connection")
        }
        ForwardState::Bound { bound_port, active } => {
            format!("bound on {bound_port} · {active} connections")
        }
        ForwardState::Stopped { reason: None } => "stopped".to_string(),
        ForwardState::Stopped {
            reason: Some(reason),
        } => reason.clone(),
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
fn forward_kind_pill(kind: ForwardKind, cx: &Context<ServerSession>) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex_none()
        .h(px(18.))
        .px(px(6.))
        .flex()
        .items_center()
        .rounded(px(3.))
        .bg(theme.input)
        .border_1()
        .border_color(theme.title_bar_border)
        .text_size(px(9.5))
        .text_color(theme.foreground)
        .child(kind.display_label())
}
