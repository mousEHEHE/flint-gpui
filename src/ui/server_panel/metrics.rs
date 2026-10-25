//! The Overview tab: live metrics, then the system and how flint reaches it.

use std::time::Duration;

use gpui::prelude::*;
use gpui::{
    div, ease_out_quint, linear_color_stop, linear_gradient, px, relative, Animation,
    AnimationElement, AnimationExt as _, AnyElement, Context, Div, ElementId, Hsla, SharedString,
};
use gpui_component::{chart::AreaChart, h_flex, v_flex, ActiveTheme as _, StyledExt as _};

use super::{format_bytes, info_row, note, section_title, share, Reading, ServerPanel, TICK};
use crate::server_stats::SystemInfo;
use crate::theme;
use crate::ui::host_icon::host_icon;
use crate::vault::{AuthMethod, ProxyChoice, ServerEntry, VaultManager};

const CHART_HEIGHT: f32 = 48.;
/// Keeps a line at zero clear of the card's bottom border.
const BASELINE_GAP: f32 = 2.;
const FADE_IN: Duration = Duration::from_millis(220);
/// Holds a line's height while its text is not known yet.
const BLANK: &str = "\u{a0}";

impl ServerPanel {
    fn render_identity(&self, cx: &Context<Self>) -> AnyElement {
        let Some(info) = &self.info else {
            // The same two lines as below, so nothing moves when the info comes.
            return identity_row(None, "Reading the server\u{2026}".into(), None, cx)
                .into_any_element();
        };
        let os_label = info.os_name.clone().unwrap_or_else(|| info.kernel.clone());
        let subtitle = match self.last_sample.as_ref() {
            Some(sample) => format!(
                "{} \u{b7} up {}",
                info.hostname,
                format_uptime(sample.uptime)
            ),
            None => info.hostname.clone(),
        };
        let row = identity_row(Some(&os_label), os_label.clone().into(), Some(subtitle), cx);
        fade_in("server-identity", row).into_any_element()
    }

    pub(super) fn render_overview(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let mut children = vec![self.render_identity(cx)];
        if self.is_unsupported {
            children.push(note("Live metrics need a Linux server.").into_any_element());
        } else {
            children.extend(self.render_metrics(cx));
        }
        // Disks and System come with the host's first answer, in one fade.
        if self.info.is_some() || self.last_sample.is_some() {
            children.push(fade_in("server-details", self.render_details(cx)).into_any_element());
        }
        children
    }

    fn render_details(&self, cx: &Context<Self>) -> Div {
        let mut details = v_flex().gap(px(10.));
        if let Some(sample) = self.last_sample.as_ref().filter(|s| !s.disks.is_empty()) {
            details = details.child(section_title("Disks")).children(
                sample
                    .disks
                    .iter()
                    .map(|disk| disk_row(&disk.mount, disk.used, disk.size, cx)),
            );
        }
        let mut rows = self.info.as_ref().map(system_rows).unwrap_or_default();
        rows.extend(self.connection_rows(cx));
        details.child(section_title("System")).children(
            rows.into_iter()
                .filter(|(_, value)| !value.is_empty())
                .enumerate()
                .map(|(ix, (key, value))| info_row(ix, key, value, cx)),
        )
    }

    fn connection_rows(&self, cx: &Context<Self>) -> Vec<(&'static str, String)> {
        let Some(server_id) = self.server_id(cx) else {
            return Vec::new();
        };
        let Some(vault) = self.vault_state.read(cx).manager() else {
            return Vec::new();
        };
        match vault.get_entry(server_id) {
            Ok(entry) => connection_rows(&entry, vault),
            Err(_) => Vec::new(),
        }
    }

    /// The three cards. They keep their size from the start; the numbers come
    /// with the first reading and a graph needs two.
    fn render_metrics(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let latest = self.history.back().copied();
        let sample = self.last_sample.as_ref().filter(|_| latest.is_some());
        let theme = cx.theme();
        let (rx_color, tx_color) = (theme.chart_5, theme.chart_3);
        let points = (self.history.len() >= 2).then(|| self.chart_points());

        let cpu = latest.zip(sample).map(|(reading, sample)| {
            let load = sample.load;
            let caption_text = format!(
                "load {:.2}  {:.2}  {:.2} \u{b7} {} cores",
                load[0], load[1], load[2], sample.cpu_count
            );
            (format!("{:.0}%", reading.cpu), caption(caption_text))
        });
        let memory = latest.zip(sample).map(|(reading, sample)| {
            let memory = sample.memory;
            let mut caption_text = format!(
                "{} of {}",
                format_bytes(memory.used()),
                format_bytes(memory.total)
            );
            if memory.swap_total > 0 {
                caption_text.push_str(&format!(
                    " \u{b7} swap {}",
                    format_bytes(memory.swap_used())
                ));
            }
            (format!("{:.0}%", reading.memory), caption(caption_text))
        });
        let network = latest.map(|reading| {
            let legend = h_flex()
                .gap(px(12.))
                .child(legend_item("in", reading.rx, rx_color))
                .child(legend_item("out", reading.tx, tx_color));
            (
                format_rate(reading.rx + reading.tx),
                legend.into_any_element(),
            )
        });

        vec![
            metric_card(
                "cpu",
                "CPU",
                cpu,
                points.clone().map(|points| {
                    percent_chart("server-cpu-chart", points, |p| p.reading.cpu, theme.chart_1)
                }),
                cx,
            ),
            metric_card(
                "memory",
                "Memory",
                memory,
                points.clone().map(|points| {
                    percent_chart(
                        "server-memory-chart",
                        points,
                        |p| p.reading.memory,
                        theme.chart_4,
                    )
                }),
                cx,
            ),
            metric_card(
                "network",
                "Network",
                network,
                points.map(|points| network_chart(points, rx_color, tx_color)),
                cx,
            ),
        ]
    }

    fn chart_points(&self) -> Vec<ChartPoint> {
        let last = self.history.len().saturating_sub(1);
        self.history
            .iter()
            .enumerate()
            .map(|(ix, reading)| {
                let seconds_ago = (last - ix) as u64 * TICK.as_secs();
                let label = match seconds_ago {
                    0 => "now".to_string(),
                    seconds => format!("{seconds} s ago"),
                };
                ChartPoint {
                    label: label.into(),
                    reading: *reading,
                }
            })
            .collect()
    }
}

/// Fades the element in when it first shows, so data does not pop in.
fn fade_in<E: Styled + IntoElement + 'static>(
    id: impl Into<ElementId>,
    element: E,
) -> AnimationElement<E> {
    element.with_animation(
        id,
        Animation::new(FADE_IN).with_easing(ease_out_quint()),
        |element, delta| element.opacity(delta),
    )
}

fn identity_row(
    os_label: Option<&str>,
    title: SharedString,
    subtitle: Option<String>,
    cx: &Context<ServerPanel>,
) -> Div {
    h_flex()
        .gap(px(10.))
        .items_center()
        .child(host_icon(os_label, false, cx.theme().background, 32.))
        .child(
            v_flex()
                .min_w_0()
                .gap(px(1.))
                .child(
                    div()
                        .truncate()
                        .text_size(px(13.))
                        .font_semibold()
                        .text_color(if subtitle.is_some() {
                            cx.theme().foreground
                        } else {
                            theme::text_dim()
                        })
                        .child(title),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(11.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(subtitle.unwrap_or_else(|| BLANK.to_string())),
                ),
        )
}

#[derive(Clone)]
struct ChartPoint {
    label: SharedString,
    reading: Reading,
}

fn area_fill(color: Hsla) -> gpui::Background {
    linear_gradient(
        180.,
        linear_color_stop(color.opacity(0.32), 0.),
        linear_color_stop(color.opacity(0.), 1.),
    )
}

fn percent_chart(
    id: &'static str,
    points: Vec<ChartPoint>,
    value: fn(&ChartPoint) -> f64,
    color: Hsla,
) -> AnyElement {
    AreaChart::new(points)
        .id(id)
        .x(|point: &ChartPoint| point.label.clone())
        .y(value)
        .stroke(color)
        .fill(area_fill(color))
        .linear()
        .x_axis(false)
        .grid(false)
        .y_domain(0., 100.)
        .y_padding(10., BASELINE_GAP)
        .tooltip_value(|_, _, value| format!("{value:.0}%").into())
        .into_any_element()
}

fn network_chart(points: Vec<ChartPoint>, rx_color: Hsla, tx_color: Hsla) -> AnyElement {
    AreaChart::new(points)
        .id("server-network-chart")
        .x(|point: &ChartPoint| point.label.clone())
        .y(|point: &ChartPoint| point.reading.rx)
        .stroke(rx_color)
        .fill(area_fill(rx_color))
        .linear()
        .name("In")
        .y(|point: &ChartPoint| point.reading.tx)
        .stroke(tx_color)
        .fill(area_fill(tx_color))
        .linear()
        .name("Out")
        .x_axis(false)
        .grid(false)
        .y_padding(10., BASELINE_GAP)
        .tooltip_value(|_, _, value| format_rate(value).into())
        .into_any_element()
}

/// A metric's card. Before its data comes, blank lines and an empty graph
/// area hold its size; then each part fades in.
fn metric_card(
    id: &'static str,
    title: &'static str,
    data: Option<(String, AnyElement)>,
    chart: Option<AnyElement>,
    cx: &Context<ServerPanel>,
) -> AnyElement {
    let (value, caption_element) = match data {
        Some((value, caption_element)) => (Some(value), Some(caption_element)),
        None => (None, None),
    };
    let value_text = div()
        .flex_1()
        .min_w_0()
        .truncate()
        .text_right()
        .text_size(px(13.))
        .font_semibold()
        .text_color(cx.theme().foreground);
    let value = match value {
        Some(value) => fade_in(format!("{id}-value"), value_text.child(value)).into_any_element(),
        None => value_text.child(BLANK).into_any_element(),
    };
    let caption_element = match caption_element {
        Some(element) => fade_in(format!("{id}-caption"), div().child(element)).into_any_element(),
        None => caption(BLANK.to_string()),
    };
    let header = h_flex()
        .gap(px(8.))
        .items_center()
        .child(
            div()
                .flex_none()
                .text_size(px(11.5))
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
        .child(value);
    // flex_none: a clipped card would otherwise shrink to nothing in the column.
    v_flex()
        .flex_none()
        .rounded(px(8.))
        .border_1()
        .border_color(theme::border())
        .bg(theme::card())
        .overflow_hidden()
        .child(
            v_flex()
                .px(px(10.))
                .pt(px(10.))
                .gap(px(4.))
                .child(header)
                .child(caption_element),
        )
        // Edge to edge: the graph runs to the card's sides and bottom.
        .child(div().mt(px(8.)).h(px(CHART_HEIGHT)).children(
            chart.map(|chart| fade_in(format!("{id}-chart"), div().size_full().child(chart))),
        ))
        .into_any_element()
}

fn caption(text: String) -> AnyElement {
    div()
        .truncate()
        .text_size(px(11.))
        .text_color(theme::text_dim())
        .child(text)
        .into_any_element()
}

/// A dot in the series' colour, its name and its latest rate.
fn legend_item(name: &'static str, rate: f64, color: Hsla) -> Div {
    let rate = format_rate(rate);
    h_flex()
        .gap(px(5.))
        .items_center()
        .text_size(px(11.))
        .text_color(theme::text_dim())
        .child(div().size(px(6.)).rounded_full().bg(color))
        .child(format!("{name} {rate}"))
}

fn disk_row(mount: &str, used: u64, size: u64, cx: &Context<ServerPanel>) -> Div {
    let fraction = share(used, size) / 100.;
    let color = match fraction {
        f if f >= 0.9 => cx.theme().danger,
        f if f >= 0.8 => cx.theme().warning,
        _ => cx.theme().chart_1,
    };
    v_flex()
        .gap(px(5.))
        .child(
            h_flex()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.))
                        .text_color(cx.theme().foreground)
                        .child(mount.to_string()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.5))
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{} / {}", format_bytes(used), format_bytes(size))),
                ),
        )
        .child(
            div().h(px(4.)).rounded_full().bg(theme::field()).child(
                div()
                    .h_full()
                    .rounded_full()
                    .w(relative(fraction.clamp(0., 1.) as f32))
                    .bg(color),
            ),
        )
}

fn system_rows(info: &SystemInfo) -> Vec<(&'static str, String)> {
    let mut rows: Vec<(&'static str, String)> = vec![("Hostname", info.hostname.clone())];
    if let Some(os) = &info.os_name {
        rows.push(("OS", os.clone()));
    }
    rows.push(("Kernel", info.kernel.clone()));
    rows.push(("Architecture", info.arch.clone()));
    if let Some(model) = &info.cpu_model {
        rows.push(("CPU", model.clone()));
    }
    if let Some(count) = info.cpu_count {
        rows.push(("Cores", count.to_string()));
    }
    rows.extend(
        info.addresses
            .iter()
            .map(|address| ("Address", address.clone())),
    );
    rows
}

/// How flint reaches the host, as the vault has it.
fn connection_rows(entry: &ServerEntry, vault: &VaultManager) -> Vec<(&'static str, String)> {
    let mut rows = vec![(
        "SSH",
        format!("{}@{}:{}", entry.username, entry.host, entry.port),
    )];
    let login = match &entry.auth {
        AuthMethod::Password(_) => "Password".to_string(),
        AuthMethod::PrivateKey { key_id } => match vault.get_key(*key_id) {
            Ok(key) => format!("Key \u{b7} {}", key.name),
            Err(_) => "Key (missing)".to_string(),
        },
        AuthMethod::Agent => "SSH agent".to_string(),
    };
    rows.push(("Login", login));
    if let Some(jump_id) = entry.jump_host {
        let jump = vault
            .get_entry(jump_id)
            .map(|jump| jump.label)
            .unwrap_or_else(|_| "(missing)".to_string());
        rows.push(("Jump host", jump));
    }
    let proxy = match entry.proxy {
        ProxyChoice::Default => None,
        ProxyChoice::Direct => Some("Direct".to_string()),
        ProxyChoice::Use(id) => Some(
            vault
                .get_proxy(id)
                .map(|proxy| proxy.name.clone())
                .unwrap_or_else(|_| "(missing)".to_string()),
        ),
    };
    if let Some(proxy) = proxy {
        rows.push(("Proxy", proxy));
    }
    if let Some(fingerprint) = &entry.host_key_fingerprint {
        rows.push(("Host key", fingerprint.clone()));
    }
    rows
}

fn format_rate(bytes_per_second: f64) -> String {
    format!("{}/s", format_bytes(bytes_per_second.max(0.0) as u64))
}

fn format_uptime(uptime: Duration) -> String {
    let minutes = uptime.as_secs() / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_shows_the_two_largest_units() {
        assert_eq!(format_uptime(Duration::from_secs(59)), "0m");
        assert_eq!(
            format_uptime(Duration::from_secs(3 * 3600 + 12 * 60)),
            "3h 12m"
        );
        assert_eq!(
            format_uptime(Duration::from_secs(4 * 86400 + 5 * 3600)),
            "4d 5h"
        );
    }
}
