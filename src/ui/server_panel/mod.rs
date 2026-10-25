//! The panel on the right of a session: what the server is and how busy it
//! is, and what the vault keeps for it. It reads the host over an exec
//! channel every two seconds, and only while it is on screen.

mod metrics;
mod snippets;

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::future::Either;
use futures::StreamExt as _;
use gpui::prelude::*;
use gpui::{
    div, px, BackgroundExecutor, ClipboardItem, Context, Div, Entity, EventEmitter, Task,
    WeakEntity, Window,
};
use gpui_component::{h_flex, tooltip::Tooltip, v_flex, ActiveTheme as _, StyledExt as _};

use crate::server_stats::{self, Sample, SystemInfo};
use crate::ssh::SshConnection;
use crate::theme;
use crate::ui::server_session::ServerSession;
use crate::ui::VaultState;

pub const PANEL_WIDTH: f32 = 320.;
const TICK: Duration = Duration::from_secs(2);
/// The first two readings come this fast, so the numbers and the graphs show
/// within a second of opening: each reading needs the sample before it.
const FIRST_READINGS_DELAY: Duration = Duration::from_millis(500);
/// How often to look again while the session is still connecting.
const CONNECT_RETRY: Duration = Duration::from_millis(500);
/// Two minutes of points.
const HISTORY_LEN: usize = 60;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Snippets,
}

pub enum ServerPanelEvent {
    RunSnippet(uuid::Uuid),
}

/// One point of the graphs.
#[derive(Clone, Copy)]
struct Reading {
    cpu: f64,
    memory: f64,
    rx: f64,
    tx: f64,
}

pub struct ServerPanel {
    session: WeakEntity<ServerSession>,
    vault_state: Entity<VaultState>,
    tab: Tab,
    info: Option<SystemInfo>,
    /// The host printed no `/proc/stat`: only the system info shows.
    is_unsupported: bool,
    last_sample: Option<Sample>,
    history: VecDeque<Reading>,
    snippet_editor: Option<snippets::SnippetEditor>,
    /// Polling stops when the panel is off screen: hidden, or its tab is not
    /// in front. Every poll ends in a redraw, which keeps this fresh. `None`
    /// until the first render.
    last_rendered: Option<Instant>,
    /// Wakes the poll loop when the panel comes back on screen.
    wake: mpsc::UnboundedSender<()>,
    _poll_task: Task<()>,
}

enum NextStep {
    Read(Poll),
    Wait(Duration),
}

struct Poll {
    connection: SshConnection,
    needs_info: bool,
    needs_sample: bool,
}

impl EventEmitter<ServerPanelEvent> for ServerPanel {}

impl ServerPanel {
    pub fn new(
        session: WeakEntity<ServerSession>,
        vault_state: Entity<VaultState>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The snippets and the connection rows show what the vault holds.
        cx.observe(&vault_state, |_, _, cx| cx.notify()).detach();
        let (wake, mut woken) = mpsc::unbounded();
        let poll_task = cx.spawn(async move |this, cx| {
            let mut delay = Duration::ZERO;
            loop {
                // A wake from `render` cuts the wait short.
                let timer = cx.background_executor().timer(delay);
                futures::future::select(timer, woken.next()).await;
                while woken.try_recv().is_ok() {}
                let Ok(step) = this.update(cx, |panel, cx| panel.next_step(cx)) else {
                    break;
                };
                let poll = match step {
                    NextStep::Read(poll) => poll,
                    NextStep::Wait(wait) => {
                        delay = wait;
                        continue;
                    }
                };
                let executor = cx.background_executor().clone();
                let (info, sample) = read_host(&poll, &executor).await;
                let Ok(next) = this.update(cx, |panel, cx| {
                    panel.take_output(info, sample, cx);
                    panel.delay_after_poll()
                }) else {
                    break;
                };
                delay = next;
            }
        });
        Self {
            session,
            vault_state,
            tab: Tab::Overview,
            info: None,
            is_unsupported: false,
            last_sample: None,
            history: VecDeque::with_capacity(HISTORY_LEN),
            snippet_editor: None,
            last_rendered: None,
            wake,
            _poll_task: poll_task,
        }
    }

    fn is_on_screen(&self) -> bool {
        self.last_rendered
            .is_some_and(|rendered| rendered.elapsed() <= TICK * 2)
    }

    fn next_step(&self, cx: &Context<Self>) -> NextStep {
        // Off screen, the next render wakes the loop.
        if !self.is_on_screen() {
            return NextStep::Wait(TICK);
        }
        let connection = self
            .session
            .upgrade()
            .and_then(|session| session.read(cx).connection.clone());
        let Some(connection) = connection else {
            return NextStep::Wait(CONNECT_RETRY);
        };
        let needs_info = self.info.is_none();
        let needs_sample = !self.is_unsupported;
        if !needs_info && !needs_sample {
            return NextStep::Wait(TICK);
        }
        NextStep::Read(Poll {
            connection,
            needs_info,
            needs_sample,
        })
    }

    /// Wakes the poll loop, which waits while the panel is off screen. After
    /// a long while the graphs start over, as the old line would join the new.
    fn come_back_on_screen(&mut self) {
        let is_long_gone = self
            .last_rendered
            .is_none_or(|rendered| rendered.elapsed() > TICK * 5);
        if is_long_gone {
            self.history.clear();
        }
        self.wake.unbounded_send(()).ok();
    }

    fn delay_after_poll(&self) -> Duration {
        if self.last_sample.is_some() && self.history.len() < 2 {
            FIRST_READINGS_DELAY
        } else {
            TICK
        }
    }

    fn take_output(
        &mut self,
        info: Option<String>,
        sample: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(info) = info.filter(|info| !info.trim().is_empty()) {
            let info = server_stats::parse_info(&info);
            tracing::debug!(host = %info.hostname, os = ?info.os_name, "server panel read the system info");
            self.info = Some(info);
        }
        // Empty output is a failed read, such as a channel refused for now.
        if let Some(output) = sample.filter(|output| !output.trim().is_empty()) {
            match server_stats::parse_sample(&output) {
                Some(sample) => self.add_sample(sample),
                None => {
                    tracing::info!(
                        "the host has no /proc/stat, so the server panel shows no metrics"
                    );
                    self.is_unsupported = true;
                }
            }
        }
        cx.notify();
    }

    fn add_sample(&mut self, sample: Sample) {
        if let Some(previous) = &self.last_sample {
            // The host's own clock: the time on the wire would make the rates jump.
            let elapsed = sample.uptime.saturating_sub(previous.uptime);
            // Off screen for a while: rates over the whole gap would not be
            // what the host does now.
            if elapsed > TICK * 5 {
                self.history.clear();
                self.last_sample = Some(sample);
                return;
            }
            if self.history.len() == HISTORY_LEN {
                self.history.pop_front();
            }
            self.history.push_back(Reading {
                cpu: server_stats::cpu_percent(&previous.cpu, &sample.cpu) as f64,
                memory: share(sample.memory.used(), sample.memory.total),
                rx: server_stats::bytes_per_second(previous.net_rx, sample.net_rx, elapsed),
                tx: server_stats::bytes_per_second(previous.net_tx, sample.net_tx, elapsed),
            });
        }
        self.last_sample = Some(sample);
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        cx.notify();
    }

    fn render_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let hover_color = cx.theme().muted_foreground;
        let option = |id: &'static str, tab: Tab, label: &'static str| {
            let is_active = self.tab == tab;
            div()
                .id(id)
                .flex_1()
                .h(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .text_size(px(11.5))
                .cursor_pointer()
                .text_color(if is_active {
                    cx.theme().foreground
                } else {
                    theme::text_dim()
                })
                .when(is_active, |el| el.bg(theme::element()))
                .when(!is_active, |el| el.hover(|s| s.text_color(hover_color)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
        };
        h_flex()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(7.))
            .bg(theme::field())
            .child(option("server-panel-overview", Tab::Overview, "Overview"))
            .child(option("server-panel-snippets", Tab::Snippets, "Snippets"))
    }

    /// The vault entry of the session's host.
    fn server_id(&self, cx: &gpui::App) -> Option<uuid::Uuid> {
        let session = self.session.upgrade()?;
        uuid::Uuid::parse_str(&session.read(cx).server_id).ok()
    }
}

impl Render for ServerPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.is_on_screen() {
            self.come_back_on_screen();
        }
        self.last_rendered = Some(Instant::now());
        let content = match self.tab {
            Tab::Overview => self.render_overview(cx),
            Tab::Snippets => self.render_snippets(cx),
        };
        v_flex()
            .w(px(PANEL_WIDTH))
            .flex_none()
            .h_full()
            .border_l_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_none()
                    .px(px(12.))
                    .pt(px(10.))
                    .pb(px(6.))
                    .child(self.render_tabs(cx)),
            )
            .child(
                v_flex()
                    .id("server-panel-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(12.))
                    .pt(px(6.))
                    .pb(px(12.))
                    .gap(px(10.))
                    .children(content),
            )
    }
}

/// The system info and a sample, as far as `poll` asks for them: the output
/// for each, which is one and the same when both are read at once.
async fn read_host(poll: &Poll, executor: &BackgroundExecutor) -> (Option<String>, Option<String>) {
    let connection = &poll.connection;
    match (poll.needs_info, poll.needs_sample) {
        (true, true) => {
            let command = server_stats::info_and_sample_command();
            let output = run_command(connection, command, executor).await;
            (output.clone(), output)
        }
        (true, false) => {
            let output = run_command(connection, server_stats::info_command(), executor).await;
            (output, None)
        }
        (false, true) => {
            let output = run_command(connection, server_stats::sample_command(), executor).await;
            (None, output)
        }
        (false, false) => (None, None),
    }
}

/// Run `command` on the host and collect what it prints. `None` when no
/// channel could be opened or the command did not finish in time.
async fn run_command(
    connection: &SshConnection,
    command: String,
    executor: &BackgroundExecutor,
) -> Option<String> {
    let reading = async {
        let exec = connection.open_exec(command).await.ok()?;
        let output = exec.subscribe();
        let mut bytes = Vec::new();
        while let Ok(chunk) = output.recv().await {
            bytes.extend_from_slice(&chunk);
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    };
    let limit = executor.timer(COMMAND_TIMEOUT);
    match futures::future::select(Box::pin(reading), Box::pin(limit)).await {
        Either::Left((output, _)) => output,
        Either::Right(_) => None,
    }
}

fn section_title(text: &'static str) -> Div {
    div()
        .pt(px(6.))
        .text_size(px(11.5))
        .font_medium()
        .text_color(theme::text_dim())
        .child(text)
}

fn note(text: &'static str) -> Div {
    div()
        .py(px(8.))
        .text_size(px(12.))
        .text_color(theme::text_dim())
        .child(text)
}

/// A click copies the value.
fn info_row(
    ix: usize,
    key: &'static str,
    value: String,
    cx: &Context<ServerPanel>,
) -> impl IntoElement {
    let copied = value.clone();
    h_flex()
        .id(("server-info", ix))
        .gap(px(12.))
        .px(px(6.))
        .mx(px(-6.))
        .py(px(3.))
        .rounded(px(5.))
        .cursor_pointer()
        .hover(|s| s.bg(theme::field()))
        .active(|s| s.bg(theme::element_active()))
        .child(
            div()
                .flex_none()
                .text_size(px(11.5))
                .text_color(cx.theme().muted_foreground)
                .child(key),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_right()
                .text_size(px(12.))
                .text_color(cx.theme().foreground)
                .child(value),
        )
        .tooltip(|window, cx| Tooltip::new("Copy").build(window, cx))
        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copied.clone())))
}

/// `part` as a share of `whole`, 0 to 100.
fn share(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    part as f64 / whole as f64 * 100.0
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_keep_three_figures_at_most() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(3_300_000_000), "3.1 GB");
        assert_eq!(format_bytes(150 * 1024 * 1024), "150 MB");
    }
}
