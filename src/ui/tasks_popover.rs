//! Tasks popover: the running transfers of a server session and their history.
//!
//! Small pills in the terminal header show the running transfers; clicking one
//! opens a popover that lists them with live progress, and the finished ones
//! with their result. Several transfers can run at once.
//!
//! This is part of `impl ServerSession`, kept out of `server_session.rs` so
//! that file stays about the connection lifecycle.

use gpui::prelude::*;
use gpui::{div, linear_color_stop, linear_gradient, px, AnyElement, Context, Hsla, SharedString};
use gpui_component::{
    h_flex, progress::ProgressCircle, spinner::Spinner, v_flex, ActiveTheme as _, Sizable as _,
    StyledExt as _,
};

use crate::ssh::TransferOutcome;
use crate::theme;
use crate::ui::file_pane::fmt_size;
use crate::ui::server_session::{ServerSession, TransferTask};
use crate::ui::sftp_browser::TransferKind;

const POPOVER_W: f32 = 340.0;

impl ServerSession {
    pub fn toggle_tasks(&mut self, cx: &mut Context<Self>) {
        self.tasks_open = !self.tasks_open;
        // The two popovers share a corner, so only one can be open.
        self.forwards_open = false;
        cx.notify();
    }

    fn close_tasks(&mut self, cx: &mut Context<Self>) {
        self.tasks_open = false;
        cx.notify();
    }

    /// Drop all finished transfers from history. If nothing is still active,
    /// the popover has nothing left to show, so close it too.
    fn clear_finished_tasks(&mut self, cx: &mut Context<Self>) {
        self.transfer_history.clear();
        if self.transfers.is_empty() {
            self.tasks_open = false;
        }
        cx.notify();
    }

    /// Flag a transfer for cancellation. The worker notices between chunks and
    /// reports back via `TransferFinished`, which moves the row to history.
    fn cancel_transfer(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(t) = self.transfers.iter().find(|t| t.id == id) {
            t.request_cancel();
            cx.notify();
        }
    }

    fn toggle_transfer_expand(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(t) = self
            .transfers
            .iter_mut()
            .chain(self.transfer_history.iter_mut())
            .find(|t| t.id == id)
        {
            t.expanded = !t.expanded;
            cx.notify();
        }
    }

    pub fn render_task_pills(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.transfers.is_empty() {
            return None;
        }

        let mut pills: Vec<AnyElement> = Vec::new();
        for transfer in self.transfers.iter().take(3) {
            pills.push(self.render_transfer_pill(transfer, cx));
        }
        let extra = self.transfers.len().saturating_sub(3);
        if extra > 0 {
            pills.push(self.render_glyph_pill(format!("+{extra}"), theme::accent(), cx));
        }

        Some(
            h_flex()
                .gap(px(7.))
                .items_center()
                .children(pills)
                .into_any_element(),
        )
    }

    fn render_transfer_pill(&self, t: &TransferTask, cx: &Context<Self>) -> AnyElement {
        let determinate = t.total > 0;
        let accent = theme::status_online();
        let id = t.id as usize;
        let indicator: AnyElement = if t.is_queued {
            queued_mark(px(11.))
        } else if determinate {
            ProgressCircle::new(("tr-pill-ring", id))
                .value(t.pct() as f32)
                .color(accent)
                .size(px(12.))
                .into_any_element()
        } else {
            Spinner::new()
                .with_size(px(11.))
                .color(accent)
                .into_any_element()
        };
        let value = transfer_value_text(t);
        let arrow = t.kind.arrow();

        div()
            .id(("tr-pill", id))
            .flex()
            .items_center()
            .gap(px(5.))
            .h(px(19.))
            .pl(px(4.))
            .pr(px(7.))
            .rounded(px(3.))
            .border_1()
            .border_color(theme::accent_line())
            .bg(theme::accent_soft())
            .text_size(px(10.5))
            .text_color(cx.theme().foreground)
            .cursor_pointer()
            .hover(|s| s.bg(theme::element_hover()))
            .active(|s| s.bg(theme::element_active()))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_tasks(cx)))
            .child(indicator)
            .child(div().text_color(accent).child(arrow))
            .child(
                div()
                    .max_w(px(96.))
                    .truncate()
                    .text_ellipsis_middle()
                    .font_semibold()
                    .child(t.name.clone()),
            )
            .child(
                div()
                    .text_color(accent)
                    .font_semibold()
                    .child(SharedString::from(value)),
            )
            .into_any_element()
    }

    fn render_glyph_pill(&self, label: String, color: Hsla, cx: &Context<Self>) -> AnyElement {
        div()
            .id(SharedString::from(format!("task-glyph-{label}")))
            .flex()
            .items_center()
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
            .on_click(cx.listener(|this, _, _, cx| this.toggle_tasks(cx)))
            .child(SharedString::from(label))
            .into_any_element()
    }

    pub fn render_tasks_popover(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if !self.tasks_open {
            return None;
        }
        if self.transfers.is_empty() && self.transfer_history.is_empty() {
            return None;
        }

        let mut body: Vec<AnyElement> = Vec::new();
        for transfer in &self.transfers {
            body.push(self.render_transfer_row(transfer, cx));
        }
        if !self.transfer_history.is_empty() {
            body.push(self.render_section("EARLIER", true, cx));
            for transfer in &self.transfer_history {
                body.push(self.render_transfer_history_row(transfer, cx));
            }
        }

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
            .child(self.render_popover_head(cx))
            .child(
                div()
                    .id("tasks-pop-body")
                    .max_h(px(360.))
                    .overflow_y_scroll()
                    .p(px(3.))
                    .child(v_flex().gap(px(1.)).children(body)),
            );

        Some(popover.into_any_element())
    }

    fn render_popover_head(&self, cx: &Context<Self>) -> AnyElement {
        let active = self.transfers.len();

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
                    .child("TASKS"),
            )
            .when(active > 0, |el| {
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
                        .child(SharedString::from(active.to_string())),
                )
            })
            .child(div().flex_1())
            .child(self.head_icon_btn(
                "tasks-close",
                "✕",
                cx.listener(|this, _, _, cx| this.close_tasks(cx)),
            ))
            .into_any_element()
    }

    fn head_icon_btn(
        &self,
        id: &'static str,
        glyph: &'static str,
        on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
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
            .active(|s| s.bg(theme::element_active()).text_color(theme::text()))
            .on_click(on_click)
            .child(glyph)
    }

    fn render_section(
        &self,
        label: &'static str,
        with_clear: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        h_flex()
            .items_center()
            .gap(px(8.))
            .px(px(7.))
            .pt(px(6.))
            .pb(px(3.))
            .child(
                div()
                    .text_size(px(8.5))
                    .font_bold()
                    .text_color(theme::text_dim())
                    .child(label),
            )
            .child(div().flex_1().h(px(1.)).bg(cx.theme().border))
            .when(with_clear, |el| {
                el.child(
                    div()
                        .id("tasks-clear")
                        .flex_none()
                        .h(px(15.))
                        .px(px(5.))
                        .flex()
                        .items_center()
                        .rounded(px(3.))
                        .text_size(px(8.5))
                        .font_bold()
                        .text_color(theme::text_dim())
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::element_hover()).text_color(theme::text()))
                        .active(|s| s.bg(theme::element_active()))
                        .on_click(cx.listener(|this, _, _, cx| this.clear_finished_tasks(cx)))
                        .child("CLEAR"),
                )
            })
            .into_any_element()
    }

    fn render_transfer_row(&self, t: &TransferTask, cx: &Context<Self>) -> AnyElement {
        let determinate = t.total > 0;
        let accent = theme::status_online();
        let id = t.id;
        let arrow = t.kind.arrow();
        let value = transfer_value_text(t);
        let pace = if t.is_queued {
            String::new()
        } else {
            live_pace_text(t, std::time::Instant::now())
                .unwrap_or_else(|| elapsed_compact(t.started.elapsed().as_secs()))
        };

        let indicator: AnyElement = if t.is_queued {
            queued_mark(px(13.))
        } else if determinate {
            ProgressCircle::new(("tr-row-ring", id as usize))
                .value(t.pct() as f32)
                .color(accent)
                .size(px(15.))
                .into_any_element()
        } else {
            Spinner::new()
                .with_size(px(13.))
                .color(accent)
                .into_any_element()
        };

        let detail = t.expanded.then(|| transfer_detail_panel(t, cx));

        let row = h_flex()
            .h(px(26.))
            .px(px(7.))
            .gap(px(9.))
            .items_center()
            .rounded(px(3.))
            .bg(theme::accent().opacity(0.055))
            .child(expand_chevron(
                ("tr-row-chev", id as usize),
                t.expanded,
                cx.listener(move |this, _, _, cx| this.toggle_transfer_expand(id, cx)),
            ))
            .child(
                div()
                    .w(px(15.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(indicator),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(4.))
                    .items_center()
                    .child(div().text_size(px(11.)).text_color(accent).child(arrow))
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_ellipsis_middle()
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(cx.theme().foreground)
                            .child(t.name.clone()),
                    ),
            )
            // No bar here: the ring and the percent already say it, and the
            // speed and time left need the room, or the name is squeezed out.
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(10.5))
                    .font_semibold()
                    .text_color(accent)
                    .child(SharedString::from(value)),
            )
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(10.))
                    .text_color(theme::text_dim())
                    .child(SharedString::from(pace)),
            )
            .child(
                div()
                    .id(("tr-stop", id as usize))
                    .w(px(16.))
                    .h(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .text_size(px(9.))
                    .text_color(theme::text_dim())
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::element_hover()).text_color(theme::run_red()))
                    .active(|s| s.bg(theme::element_active()).text_color(theme::run_red()))
                    .on_click(cx.listener(move |this, _, _, cx| this.cancel_transfer(id, cx)))
                    .child("■"),
            );

        v_flex()
            .id(("tr-arow", id as usize))
            .child(row)
            .children(detail)
            .into_any_element()
    }

    fn render_transfer_history_row(&self, t: &TransferTask, cx: &Context<Self>) -> AnyElement {
        let (glyph, color, meta) = match &t.outcome {
            Some(TransferOutcome::Done { skipped_links }) => (
                "✓",
                theme::status_online(),
                done_text(t.done, *skipped_links),
            ),
            Some(TransferOutcome::Canceled) => ("✕", theme::text_dim(), "canceled".to_string()),
            // The reason is too long for this cell; the expanded row shows it.
            Some(TransferOutcome::Failed(_)) => ("⚠", theme::run_red(), "failed".to_string()),
            None => ("?", theme::text_dim(), String::new()),
        };
        let arrow = t.kind.arrow();
        let when = t
            .finished_at
            .map(|at| format!("{} ago", elapsed_compact(at.elapsed().as_secs())))
            .unwrap_or_default();
        let id = t.id;
        let expanded = t.expanded;
        let detail = expanded.then(|| transfer_detail_panel(t, cx));

        let row = h_flex()
            .id(("tr-hrow", id as usize))
            .h(px(26.))
            .px(px(7.))
            .gap(px(9.))
            .items_center()
            .rounded(px(3.))
            .cursor_pointer()
            .hover(|s| s.bg(theme::element_hover()))
            .active(|s| s.bg(theme::element_active()))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_transfer_expand(id, cx)))
            .child(chevron_glyph(expanded))
            .child(
                div()
                    .w(px(15.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(color)
                    .child(glyph),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .gap(px(4.))
                    .items_center()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::text_dim())
                            .child(arrow),
                    )
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_ellipsis_middle()
                            .text_size(px(12.))
                            .font_semibold()
                            .text_color(cx.theme().foreground)
                            .child(t.name.clone()),
                    ),
            )
            .child(
                div()
                    .max_w(px(110.))
                    .truncate()
                    .text_size(px(10.5))
                    .text_color(theme::text_dim())
                    .child(SharedString::from(meta)),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme::text_dim())
                    .child(SharedString::from(when)),
            );

        v_flex().child(row).children(detail).into_any_element()
    }
}

/// Compact duration: "8s", "1m04s", "2h13m".
fn elapsed_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Interactive expand/collapse chevron for rows that carry their own buttons
/// (so the whole row isn't a single click target).
fn expand_chevron(
    id: impl Into<gpui::ElementId>,
    expanded: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(12.))
        .h(px(16.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(9.))
        .text_color(theme::text_dim())
        .cursor_pointer()
        .hover(|s| s.text_color(theme::text()))
        .on_click(move |ev, window, cx| on_click(ev, window, cx))
        .child(if expanded { "▾" } else { "▸" })
}

/// Non-interactive chevron for rows whose whole body already toggles.
fn chevron_glyph(expanded: bool) -> impl IntoElement {
    div()
        .w(px(12.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(9.))
        .text_color(theme::text_dim())
        .child(if expanded { "▾" } else { "▸" })
}

/// Expanded detail for a single SFTP transfer: direction, size, %, speed, time.
fn transfer_detail_panel(t: &TransferTask, cx: &Context<ServerSession>) -> impl IntoElement {
    let dim = theme::text_dim();
    let fg = cx.theme().foreground;

    let dur = match t.finished_at {
        Some(end) => end.saturating_duration_since(t.started),
        None => t.started.elapsed(),
    };
    let now = std::time::Instant::now();
    let is_running = t.outcome.is_none() && !t.is_queued;
    let secs = dur.as_secs_f64();
    // A running transfer shows what it does now, a finished one its average.
    let speed = if is_running {
        t.rate.bytes_per_sec(now).map(speed_text)
    } else if secs > 0.5 && !t.is_queued {
        Some(speed_text((t.done as f64 / secs) as u64))
    } else {
        None
    };
    let speed = speed.unwrap_or_else(|| "—".to_string());
    let time_left = if is_running {
        seconds_left(t, now)
    } else {
        None
    };
    let error = match &t.outcome {
        Some(TransferOutcome::Failed(error)) => Some(error.clone()),
        _ => None,
    };
    let size = if t.total > 0 {
        format!(
            "{} / {} ({}%)",
            fmt_size(t.done),
            fmt_size(t.total),
            t.pct()
        )
    } else {
        fmt_size(t.done)
    };
    let direction = direction_text(t.kind, t.route.as_deref());
    let elapsed = if t.is_queued {
        "waiting for a free slot".to_string()
    } else {
        elapsed_compact(dur.as_secs())
    };

    let kv = |label: &'static str, value: String| {
        h_flex()
            .gap(px(8.))
            .items_baseline()
            .text_size(px(10.5))
            .child(div().w(px(58.)).flex_none().text_color(dim).child(label))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_ellipsis_middle()
                    .text_color(fg)
                    .child(SharedString::from(value)),
            )
    };

    div()
        .id(("tr-detail", t.id as usize))
        .mx(px(2.))
        .mb(px(3.))
        .p(px(7.))
        .rounded(px(3.))
        .bg(theme::sunken())
        .border_1()
        .border_color(cx.theme().border)
        .child(
            v_flex()
                .gap(px(3.))
                .child(kv("File", t.name.clone()))
                .child(kv("Direction", direction))
                .child(kv("Transferred", size))
                .child(kv("Speed", speed))
                .child(kv("Elapsed", elapsed))
                .children(time_left.map(|secs| kv("Time left", elapsed_compact(secs))))
                .children(error.map(|error| error_row(error, dim))),
        )
}

/// The full reason a transfer failed. It wraps, unlike the other detail rows:
/// the end of the message is often the part that explains it.
fn error_row(error: String, label_color: Hsla) -> impl IntoElement {
    h_flex()
        .gap(px(8.))
        .items_start()
        .text_size(px(10.5))
        .child(
            div()
                .w(px(58.))
                .flex_none()
                .text_color(label_color)
                .child("Error"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(theme::run_red())
                .child(SharedString::from(error)),
        )
}

fn speed_text(bytes_per_sec: u64) -> String {
    format!("{}/s", fmt_size(bytes_per_sec))
}

/// The direction line of a transfer's detail. A copy names both servers; an
/// upload or a download names the left server when it is the one in use.
fn direction_text(kind: TransferKind, route: Option<&str>) -> String {
    let verb = match kind {
        TransferKind::Copy => "copy",
        TransferKind::Upload => "upload \u{2191}",
        TransferKind::Download => "download \u{2193}",
    };
    match (kind, route) {
        (_, Some(route)) => format!("{verb} {route}"),
        (TransferKind::Copy, None) => "copy \u{2192}".to_string(),
        (_, None) => verb.to_string(),
    }
}

/// Seconds a running transfer still needs, once its size and speed are known.
fn seconds_left(t: &TransferTask, now: std::time::Instant) -> Option<u64> {
    if t.total == 0 {
        return None;
    }
    t.rate.seconds_left(now, t.total.saturating_sub(t.done))
}

/// "11.2 MB/s · 40s left" for a running transfer; only the speed while the
/// time left is unknown, and `None` until there is a speed.
fn live_pace_text(t: &TransferTask, now: std::time::Instant) -> Option<String> {
    let speed = speed_text(t.rate.bytes_per_sec(now)?);
    Some(match seconds_left(t, now) {
        Some(secs) => format!("{speed} \u{00b7} {} left", elapsed_compact(secs)),
        None => speed,
    })
}

/// The short figure next to a running transfer: percent, or bytes so far when
/// the size is unknown, or that it still waits for a free slot.
fn transfer_value_text(t: &TransferTask) -> String {
    if t.is_queued {
        return "queued".to_string();
    }
    if t.total > 0 {
        return format!("{}%", t.pct());
    }
    fmt_size(t.done)
}

/// Stands where the spinner is for a transfer that has not started: a spinner
/// would say it is busy.
fn queued_mark(size: gpui::Pixels) -> AnyElement {
    div()
        .text_size(size)
        .text_color(theme::text_dim())
        .child("\u{2026}")
        .into_any_element()
}

/// Meta text of a finished transfer: its size, plus the symlinks a folder
/// transfer left out, so a missing link in the copy is not a silent surprise.
fn done_text(bytes: u64, skipped_links: usize) -> String {
    let size = fmt_size(bytes);
    match skipped_links {
        0 => size,
        1 => format!("{size} \u{00b7} 1 link skipped"),
        n => format!("{size} \u{00b7} {n} links skipped"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_text_names_both_servers_of_a_copy() {
        assert_eq!(
            direction_text(TransferKind::Upload, None),
            "upload \u{2191}"
        );
        assert_eq!(
            direction_text(TransferKind::Download, None),
            "download \u{2193}"
        );
        assert_eq!(
            direction_text(TransferKind::Copy, Some("web-02 \u{2192} web-01")),
            "copy web-02 \u{2192} web-01"
        );
    }

    #[test]
    fn direction_text_names_the_left_server_of_an_upload_or_download() {
        assert_eq!(
            direction_text(TransferKind::Upload, Some("web-02")),
            "upload \u{2191} web-02"
        );
        assert_eq!(
            direction_text(TransferKind::Download, Some("web-02")),
            "download \u{2193} web-02"
        );
    }

    #[test]
    fn done_text_mentions_skipped_links_only_when_there_are_some() {
        assert_eq!(done_text(0, 0), fmt_size(0));
        assert!(done_text(10, 1).ends_with("1 link skipped"));
        assert!(done_text(10, 3).ends_with("3 links skipped"));
    }
}
