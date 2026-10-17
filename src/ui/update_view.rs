//! The dialog behind the title bar's update mark: what is new, and the
//! download of it. It only shows what `UpdateService` knows.

use gpui::prelude::*;
use gpui::{div, px, Context, EventEmitter, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    dialog::{DialogHeader, DialogTitle},
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Sizable as _,
};

use crate::ui::file_pane::fmt_size;
use crate::ui::update_service::{Download, UpdateService};
use crate::update::Release;
use crate::version;

const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Show in Finder"
} else {
    "Show in folder"
};

pub enum UpdateViewEvent {
    Closed,
}

pub struct UpdateView;

impl EventEmitter<UpdateViewEvent> for UpdateView {}

impl UpdateView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        cx.observe(&UpdateService::global(cx), |_, _, cx| cx.notify())
            .detach();
        Self
    }

    fn close_button(label: &'static str, cx: &mut Context<Self>) -> Button {
        Button::new("update-close")
            .small()
            .child(label)
            .on_click(cx.listener(|_, _, _, cx| cx.emit(UpdateViewEvent::Closed)))
    }

    fn download_button(is_running: bool) -> Button {
        Button::new("update-download")
            .small()
            .primary()
            .child("Download")
            .loading(is_running)
            .disabled(is_running)
            .on_click(|_, _, cx| {
                UpdateService::global(cx).update(cx, |service, cx| service.start_download(cx));
            })
    }

    fn render_buttons(download: &Download, cx: &mut Context<Self>) -> impl IntoElement {
        let buttons = h_flex().gap(px(8.)).justify_end();
        match download {
            Download::Idle | Download::Failed(_) => buttons
                .child(Self::close_button("Later", cx))
                .child(Self::download_button(false)),
            Download::Running { .. } => buttons
                .child(
                    Button::new("update-cancel")
                        .small()
                        .child("Cancel")
                        .on_click(|_, _, cx| {
                            UpdateService::global(cx)
                                .update(cx, |service, cx| service.cancel_download(cx));
                        }),
                )
                .child(Self::download_button(true)),
            Download::Done(_) => buttons.child(Self::close_button("Close", cx)).child(
                Button::new("update-reveal")
                    .small()
                    .primary()
                    .child(REVEAL_LABEL)
                    .on_click(|_, _, cx| {
                        UpdateService::global(cx)
                            .update(cx, |service, cx| service.reveal_download(cx));
                    }),
            ),
        }
    }
}

fn headline(release: &Release) -> String {
    format!(
        "flint {} is available. You have {}.",
        release.version,
        version::CURRENT
    )
}

/// What the download is doing, for the line above the buttons.
fn progress_line(download: &Download) -> Option<String> {
    match download {
        Download::Idle => None,
        Download::Running { percent } => Some(format!("Downloading\u{2026} {percent}%")),
        Download::Done(path) => Some(format!("Saved to {}", path.display())),
        Download::Failed(problem) => Some(problem.clone()),
    }
}

impl Render for UpdateView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted_fg, danger, border) = (
            theme.foreground,
            theme.muted_foreground,
            theme.danger,
            theme.border,
        );
        let body = v_flex()
            .p(px(12.))
            .gap(px(8.))
            .child(DialogHeader::new().child(DialogTitle::new().child("flint update")));

        let service = UpdateService::global(cx);
        let (release, download) = {
            let service = service.read(cx);
            (service.release.clone(), service.download.clone())
        };
        // The offer can go away under an open dialog: the build was withdrawn.
        let Some(release) = release else {
            return body
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(muted_fg)
                        .child("No update available."),
                )
                .child(
                    h_flex()
                        .justify_end()
                        .child(Self::close_button("Close", cx)),
                );
        };

        let notes = release.notes.clone();
        let is_failed = matches!(download, Download::Failed(_));
        let progress = progress_line(&download);
        let buttons = Self::render_buttons(&download, cx);

        body.child(
            div()
                .text_size(px(12.))
                .text_color(fg)
                .child(headline(&release)),
        )
        .when(!notes.is_empty(), |body| {
            body.child(
                div()
                    .id("update-notes")
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(border)
                    .p(px(8.))
                    .text_size(px(11.5))
                    .text_color(muted_fg)
                    // Plain text: a string child is never read as markup.
                    .child(notes),
            )
        })
        .child(div().text_size(px(11.)).text_color(muted_fg).child(format!(
            "{}, {}",
            release.filename,
            fmt_size(release.size_bytes)
        )))
        .children(progress.map(|line| {
            div()
                .text_size(px(11.5))
                .text_color(if is_failed { danger } else { muted_fg })
                .child(line)
        }))
        .child(buttons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_headline_names_both_versions() {
        let release = Release {
            version: "0.2.0".into(),
            notes: String::new(),
            filename: "Flint-0.2.0.dmg".into(),
            size_bytes: 1,
            sha256: String::new(),
            download_path: String::new(),
        };
        assert_eq!(
            headline(&release),
            format!("flint 0.2.0 is available. You have {}.", version::CURRENT)
        );
    }

    #[test]
    fn the_progress_line_follows_the_download() {
        assert_eq!(progress_line(&Download::Idle), None);
        assert_eq!(
            progress_line(&Download::Running { percent: 42 }).as_deref(),
            Some("Downloading\u{2026} 42%")
        );
        assert_eq!(
            progress_line(&Download::Done("/home/me/Downloads/Flint-0.2.0.dmg".into())).as_deref(),
            Some("Saved to /home/me/Downloads/Flint-0.2.0.dmg")
        );
        assert_eq!(
            progress_line(&Download::Failed("Could not download: x.".into())).as_deref(),
            Some("Could not download: x.")
        );
    }
}
