use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, EventEmitter, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    dialog::{DialogAction, DialogFooter, DialogHeader, DialogTitle},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme as _, Sizable as _,
};
use std::path::PathBuf;

use crate::app_discovery::{discover_apps, DiscoveredApp};
use crate::file_associations::SharedFileAssociations;
use crate::file_open::open_in_specific_app;

pub struct AppPicker {
    extension: String,
    file_to_open: PathBuf,
    /// `None` until the list of installed apps has been read.
    apps: Option<Vec<DiscoveredApp>>,
    // Created lazily on first render since Window is needed for InputState::new.
    filter_input: Option<Entity<InputState>>,
    file_associations: SharedFileAssociations,
}

pub enum AppPickerEvent {
    Done,
    /// The chosen app could not be started; the text is for the user.
    OpenFailed(String),
}

impl EventEmitter<AppPickerEvent> for AppPicker {}

impl AppPicker {
    pub fn new(
        extension: String,
        file_to_open: PathBuf,
        file_associations: SharedFileAssociations,
        cx: &mut Context<Self>,
    ) -> Self {
        // Finding the apps reads several folders (the registry on Windows).
        cx.spawn(async move |this, cx| {
            let apps = cx.background_spawn(async { discover_apps() }).await;
            this.update(cx, |this, cx| {
                this.apps = Some(apps);
                cx.notify();
            })
            .ok();
        })
        .detach();

        Self {
            extension,
            file_to_open,
            apps: None,
            filter_input: None,
            file_associations,
        }
    }

    fn ensure_filter_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        if let Some(fi) = &self.filter_input {
            return fi.clone();
        }
        let fi = cx.new(|cx| InputState::new(window, cx).placeholder("Search apps\u{2026}"));
        cx.subscribe(&fi, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        self.filter_input = Some(fi.clone());
        fi
    }

    fn filtered_apps<'a>(&'a self, cx: &App) -> Vec<&'a DiscoveredApp> {
        let filter = self
            .filter_input
            .as_ref()
            .map(|fi| fi.read(cx).value().to_lowercase())
            .unwrap_or_default();
        self.apps
            .iter()
            .flatten()
            .filter(|a| {
                filter.is_empty()
                    || a.name.to_lowercase().contains(&filter)
                    || a.path.to_string_lossy().to_lowercase().contains(&filter)
            })
            .collect()
    }

    fn confirm_with(&mut self, app_path: PathBuf, cx: &mut Context<Self>) {
        self.file_associations
            .borrow_mut()
            .set(&self.extension, app_path.clone());
        if let Err(e) = self.file_associations.borrow().save() {
            tracing::warn!(?e, "failed to save file associations");
        }
        if let Err(e) = open_in_specific_app(&app_path, &self.file_to_open) {
            tracing::warn!(?e, app = %app_path.display(), "failed to open file with selected app");
            cx.emit(AppPickerEvent::OpenFailed(format!(
                "Could not open the file in the chosen app: {e}"
            )));
            return;
        }
        cx.emit(AppPickerEvent::Done);
    }

    fn choose_another(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            #[cfg(target_os = "macos")]
            let extensions: &[&str] = &["app"];
            #[cfg(target_os = "windows")]
            let extensions: &[&str] = &["exe"];
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            let extensions: &[&str] = &[];

            let picked = rfd::AsyncFileDialog::new()
                .set_title("Choose application")
                .add_filter("Application", extensions)
                .pick_file()
                .await;
            let Some(handle) = picked else { return };
            let app_path = handle.path().to_path_buf();
            this.update(cx, |this, cx| this.confirm_with(app_path, cx))
                .ok();
        })
        .detach();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(AppPickerEvent::Done);
    }
}

impl Render for AppPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter_input = self.ensure_filter_input(window, cx);
        let filtered = self.filtered_apps(cx);
        let empty_text = if self.apps.is_some() {
            "No apps found"
        } else {
            "Looking for apps\u{2026}"
        };
        let hover_bg = cx.theme().list_hover;
        let fg = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        // Title from the file itself: ".yml" for extensioned files, otherwise
        // the bare name (Dockerfile, Makefile) since `extension` would just be
        // the lowercased file name.
        let title = match self.file_to_open.extension().and_then(|s| s.to_str()) {
            Some(ext) if !ext.is_empty() => format!("Open .{ext} files with"),
            _ => {
                let name = self
                    .file_to_open
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file");
                format!("Open {name} with")
            }
        };

        let border = cx.theme().border;
        let radius = cx.theme().radius;

        v_flex()
            .p(px(12.))
            .gap(px(8.))
            .child(DialogHeader::new().child(DialogTitle::new().child(title)))
            .child(Input::new(&filter_input).small())
            .child(if filtered.is_empty() {
                div()
                    .py_3()
                    .text_sm()
                    .text_color(muted)
                    .child(empty_text)
                    .into_any_element()
            } else {
                v_flex()
                    .id("picker-app-list")
                    .max_h(px(240.))
                    .overflow_y_scroll()
                    .border_1()
                    .border_color(border)
                    .rounded(radius)
                    .children(filtered.into_iter().enumerate().map(|(i, app)| {
                        let app_path = app.path.clone();
                        let name = app.name.clone();
                        h_flex()
                            .id(("app-row", i))
                            .h(px(24.))
                            .px(px(8.))
                            .gap(px(6.))
                            .items_center()
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover_bg))
                            .active(|s| s.bg(crate::theme::element_active()))
                            .child(div().flex_1().text_size(px(12.)).text_color(fg).child(name))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.confirm_with(app_path.clone(), cx);
                            }))
                    }))
                    .into_any_element()
            })
            .child(
                DialogFooter::new().child(
                    DialogAction::new().child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("picker-cancel")
                                    .ghost()
                                    .child("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                            )
                            .child(
                                Button::new("picker-choose-another")
                                    .child("Choose another\u{2026}")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.choose_another(cx)),
                                    ),
                            ),
                    ),
                ),
            )
    }
}
