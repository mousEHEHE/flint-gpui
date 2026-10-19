//! A read-only look at one file, over the file pane it was opened from.
//! Space opens it. The bytes stay in memory and never touch the disk.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    div, img, px, Context, Entity, Image, ImageFormat, ObjectFit, RenderImage, SharedString,
    Subscription, Task, Window,
};
use gpui_component::{
    h_flex,
    input::{Editor, EditorState},
    v_flex, ActiveTheme as _,
};

use crate::ssh::{EntryKind, FileEntry, FileHead};
use crate::ui::file_pane::{fmt_mtime, fmt_size};

/// How much of a text file is read. Enough for any config file; a log shows
/// its start.
pub const TEXT_PREVIEW_BYTES: usize = 256 * 1024;
/// Images are read whole. A decoded image takes many times its file size.
const IMAGE_PREVIEW_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// The editor lays out a line in one piece, so a minified file of one huge
/// line would stall it.
const MAX_LINE_CHARS: usize = 2000;
/// A NUL in the first bytes means a binary file. Text never has one.
const BINARY_PROBE_BYTES: usize = 8 * 1024;
/// gpui decodes every GIF frame into a full RGBA canvas, all at once and
/// without a limit, so a small file can ask for gigabytes.
const GIF_DECODED_MAX_BYTES: u64 = 256 * 1024 * 1024;

pub enum PreviewContent {
    Loading,
    Folder,
    Text {
        text: String,
        /// The file is longer than what was read.
        is_head_only: bool,
        has_long_lines_cut: bool,
    },
    Image(Arc<Image>),
    Empty,
    Binary,
    TooLarge(u64),
    NotAFile,
    Failed(String),
}

/// What the listing already tells: either the answer, or how much to read.
pub enum PreviewPlan {
    Ready(PreviewContent),
    Read { limit: usize },
}

pub fn preview_plan(entry: &FileEntry) -> PreviewPlan {
    if entry.opens_as_folder() {
        return PreviewPlan::Ready(PreviewContent::Folder);
    }
    if entry.is_link_broken {
        return PreviewPlan::Ready(PreviewContent::Failed("The link points nowhere".into()));
    }
    if entry.kind == EntryKind::Other {
        return PreviewPlan::Ready(PreviewContent::NotAFile);
    }
    if entry.size == 0 {
        return PreviewPlan::Ready(PreviewContent::Empty);
    }
    if image_format(&entry.name).is_none() {
        return PreviewPlan::Read {
            limit: TEXT_PREVIEW_BYTES,
        };
    }
    if entry.size > IMAGE_PREVIEW_MAX_BYTES {
        return PreviewPlan::Ready(PreviewContent::TooLarge(entry.size));
    }
    PreviewPlan::Read {
        limit: entry.size as usize,
    }
}

fn image_format(name: &str) -> Option<ImageFormat> {
    let extension = name.rsplit_once('.')?.1.to_lowercase();
    Some(match extension.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        "bmp" => ImageFormat::Bmp,
        "tif" | "tiff" => ImageFormat::Tiff,
        "svg" => ImageFormat::Svg,
        "ico" => ImageFormat::Ico,
        _ => return None,
    })
}

/// Turn the bytes that were read into what the preview shows.
pub fn classify(name: &str, head: FileHead) -> PreviewContent {
    if head.bytes.is_empty() {
        return PreviewContent::Empty;
    }
    if let Some(format) = image_format(name) {
        if head.size > IMAGE_PREVIEW_MAX_BYTES {
            return PreviewContent::TooLarge(head.size);
        }
        if matches!(format, ImageFormat::Gif) && !is_gif_small_enough(&head.bytes) {
            return PreviewContent::Failed("This animation is too big to preview".into());
        }
        return PreviewContent::Image(Arc::new(Image::from_bytes(format, head.bytes)));
    }
    let probe = &head.bytes[..head.bytes.len().min(BINARY_PROBE_BYTES)];
    if probe.contains(&0) {
        return PreviewContent::Binary;
    }
    let is_head_only = head.size > head.bytes.len() as u64;
    let mut has_long_lines_cut = false;
    let text = decode_text(head.bytes);
    let mut lines = Vec::new();
    for line in text.split('\n') {
        if line.chars().count() > MAX_LINE_CHARS {
            has_long_lines_cut = true;
            lines.push(line.chars().take(MAX_LINE_CHARS).collect::<String>());
        } else {
            lines.push(line.to_string());
        }
    }
    PreviewContent::Text {
        text: lines.join("\n"),
        is_head_only,
        has_long_lines_cut,
    }
}

/// Reads only the headers: the frames are skipped, not decoded. A file whose
/// headers do not parse counts as too big, so gpui never tries it.
fn is_gif_small_enough(bytes: &[u8]) -> bool {
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    let Ok(mut decoder) = options.read_info(bytes) else {
        return false;
    };
    let canvas = u64::from(decoder.width()) * u64::from(decoder.height()) * 4;
    let mut decoded = 0u64;
    loop {
        match decoder.next_frame_info() {
            Ok(Some(_)) => decoded += canvas,
            Ok(None) => return true,
            Err(_) => return false,
        }
        if decoded > GIF_DECODED_MAX_BYTES {
            return false;
        }
    }
}

/// UTF-8 without a BOM. A character split by the read limit is dropped
/// rather than shown as a replacement mark.
fn decode_text(mut bytes: Vec<u8>) -> String {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.drain(..3);
    }
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) if e.utf8_error().error_len().is_none() => {
            let valid = e.utf8_error().valid_up_to();
            let mut bytes = e.into_bytes();
            bytes.truncate(valid);
            String::from_utf8(bytes).unwrap_or_default()
        }
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

pub struct FilePreview {
    path: String,
    entry: FileEntry,
    content: PreviewContent,
    /// The image as painted, so its place in the GPU atlas can be freed.
    rendered: Option<Arc<RenderImage>>,
    /// Made on the first render that shows text: it needs the `Window`.
    editor: Option<Entity<EditorState>>,
    /// Dropped with the preview, which cancels the read.
    _load: Option<Task<()>>,
    _release: Subscription,
}

impl FilePreview {
    pub fn new(
        path: String,
        entry: FileEntry,
        content: PreviewContent,
        cx: &mut Context<Self>,
    ) -> Self {
        // gpui keeps every decoded image in its asset cache and in the GPU
        // atlas until told to forget it.
        let release = cx.on_release(|this, cx| {
            if let Some(rendered) = this.rendered.take() {
                cx.drop_image(rendered, None);
            }
            if let PreviewContent::Image(image) = &this.content {
                image.clone().remove_asset(cx);
            }
        });
        Self {
            path,
            entry,
            content,
            rendered: None,
            editor: None,
            _load: None,
            _release: release,
        }
    }

    pub fn entry(&self) -> &FileEntry {
        &self.entry
    }

    pub fn set_load(&mut self, task: Task<()>) {
        self._load = Some(task);
    }

    pub fn show(&mut self, result: anyhow::Result<FileHead>, cx: &mut Context<Self>) {
        self.content = match result {
            Ok(head) => classify(&self.entry.name, head),
            Err(e) => {
                tracing::warn!(path = %self.path, ?e, "preview read failed");
                PreviewContent::Failed(format!("Couldn't read the file: {e}"))
            }
        };
        cx.notify();
    }

    fn ensure_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_some() {
            return;
        }
        let PreviewContent::Text { text, .. } = &mut self.content else {
            return;
        };
        let text = std::mem::take(text);
        self.editor = Some(cx.new(|cx| {
            let mut state = EditorState::new(window, cx).soft_wrap(false);
            state.set_value(text, window, cx);
            state
        }));
    }

    fn render_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let muted_fg = cx.theme().muted_foreground;
        let message = |text: SharedString| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .px(px(16.))
                .text_size(px(12.5))
                .text_color(muted_fg)
                .child(text)
                .into_any_element()
        };
        match &self.content {
            PreviewContent::Loading => message("Loading\u{2026}".into()),
            PreviewContent::Text { .. } => match &self.editor {
                Some(editor) => div()
                    .size_full()
                    .child(
                        Editor::new(editor)
                            .readonly(true)
                            .bordered(false)
                            .h_full()
                            // The size of the file list behind it.
                            .text_size(px(12.)),
                    )
                    .into_any_element(),
                None => message("Loading\u{2026}".into()),
            },
            // Out of the flow, the box takes the body's final size, so the
            // image has a limit to fit into.
            PreviewContent::Image(image) => div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .p(px(12.))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    // Natural size, but never larger than the pane. With a
                    // size in percent the image keeps its aspect ratio and
                    // takes the full width, so a tall one runs off the
                    // bottom.
                    img(image.clone())
                        .id("file-preview-image")
                        .max_w_full()
                        .max_h_full()
                        .object_fit(ObjectFit::Contain)
                        .with_loading(move || {
                            div()
                                .text_size(px(12.5))
                                .text_color(muted_fg)
                                .child("Loading\u{2026}")
                                .into_any_element()
                        })
                        .with_fallback(move || {
                            div()
                                .text_size(px(12.5))
                                .text_color(muted_fg)
                                .child("Couldn't decode the image")
                                .into_any_element()
                        }),
                )
                .into_any_element(),
            PreviewContent::Folder => message("Folder. Enter opens it.".into()),
            PreviewContent::Empty => message("Empty file".into()),
            PreviewContent::Binary => message("Binary file, no preview".into()),
            PreviewContent::TooLarge(size) => message(
                format!(
                    "Too large to preview ({}, the limit is {})",
                    fmt_size(*size),
                    fmt_size(IMAGE_PREVIEW_MAX_BYTES)
                )
                .into(),
            ),
            PreviewContent::NotAFile => message("No preview for this kind of file".into()),
            PreviewContent::Failed(error) => message(error.clone().into()),
        }
    }
}

impl FilePreview {
    /// Size, date and what was left out, as one line for the header.
    fn details(&self) -> String {
        let entry = &self.entry;
        let mut parts = Vec::new();
        if !entry.opens_as_folder() && !entry.is_link_broken {
            parts.push(fmt_size(entry.size));
        }
        let mtime = fmt_mtime(entry.mtime);
        if !mtime.is_empty() {
            parts.push(mtime);
        }
        if let PreviewContent::Text {
            is_head_only,
            has_long_lines_cut,
            ..
        } = self.content
        {
            if is_head_only {
                parts.push(format!(
                    "first {} shown",
                    fmt_size(TEXT_PREVIEW_BYTES as u64)
                ));
            }
            if has_long_lines_cut {
                parts.push("long lines cut".into());
            }
        }
        parts.join(" \u{00B7} ")
    }
}

impl Render for FilePreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_editor(window, cx);
        if let (PreviewContent::Image(image), None) = (&self.content, &self.rendered) {
            self.rendered = image.clone().get_render_image(window, cx);
        }
        let theme = cx.theme();
        let (fg, muted_fg) = (theme.foreground, theme.muted_foreground);
        let details = self.details();

        // Stretched by the pane's overlay. A size in percent would not get
        // the overlay's height.
        v_flex()
            .flex_1()
            .min_w_0()
            .child(
                h_flex()
                    .h(px(38.))
                    .flex_none()
                    // The pane draws its close button over the right end of
                    // this row.
                    .pl(px(12.))
                    .pr(px(32.))
                    .gap(px(8.))
                    .items_center()
                    .border_b_1()
                    .border_color(crate::theme::border())
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .text_color(fg)
                            .child(self.entry.name.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.))
                            .text_color(muted_fg)
                            .child(details),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.render_body(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(bytes: &[u8], size: u64) -> FileHead {
        FileHead {
            bytes: bytes.to_vec(),
            size,
        }
    }

    /// The text, whether only the head was read, whether long lines were cut.
    fn text_of(content: PreviewContent) -> (String, bool, bool) {
        match content {
            PreviewContent::Text {
                text,
                is_head_only,
                has_long_lines_cut,
            } => (text, is_head_only, has_long_lines_cut),
            _ => panic!("not text"),
        }
    }

    #[test]
    fn a_nul_byte_means_binary() {
        assert!(matches!(
            classify("app", head(b"ELF\0\x01", 5)),
            PreviewContent::Binary
        ));
    }

    #[test]
    fn text_drops_a_bom_and_a_character_split_by_the_limit() {
        let (text, is_head_only, has_long_lines_cut) =
            text_of(classify("a.txt", head(b"\xEF\xBB\xBFhi \xD0", 100)));
        assert_eq!(text, "hi ");
        assert!(is_head_only);
        assert!(!has_long_lines_cut);
        let (text, is_head_only, _) = text_of(classify("a.txt", head("привет".as_bytes(), 12)));
        assert_eq!(text, "привет");
        assert!(!is_head_only);
    }

    #[test]
    fn long_lines_are_cut() {
        let line = "x".repeat(MAX_LINE_CHARS + 5);
        let (text, is_head_only, has_long_lines_cut) =
            text_of(classify("min.js", head(line.as_bytes(), line.len() as u64)));
        assert_eq!(text.len(), MAX_LINE_CHARS);
        assert!(!is_head_only);
        assert!(has_long_lines_cut);
    }

    fn gif_bytes(width: u16, height: u16, frames: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = gif::Encoder::new(&mut bytes, width, height, &[0, 0, 0]).unwrap();
            for _ in 0..frames {
                let frame = gif::Frame {
                    width: 1,
                    height: 1,
                    buffer: std::borrow::Cow::Borrowed(&[0]),
                    ..gif::Frame::default()
                };
                encoder.write_frame(&frame).unwrap();
            }
        }
        bytes
    }

    #[test]
    fn a_gif_that_decodes_too_big_is_not_shown() {
        let small = gif_bytes(4, 4, 3);
        assert!(matches!(
            classify("a.gif", head(&small, small.len() as u64)),
            PreviewContent::Image(_)
        ));
        // 4000 x 4000 x 4 bytes is 64 MB a frame, so five frames pass the cap.
        let bomb = gif_bytes(4000, 4000, 5);
        assert!(bomb.len() < 1024);
        assert!(matches!(
            classify("a.gif", head(&bomb, bomb.len() as u64)),
            PreviewContent::Failed(_)
        ));
        assert!(matches!(
            classify("a.gif", head(b"GIF89a broken", 13)),
            PreviewContent::Failed(_)
        ));
    }

    #[test]
    fn image_extensions_ignore_case() {
        assert!(image_format("Logo.PNG").is_some());
        assert!(image_format("photo.jpeg").is_some());
        assert!(image_format("nginx.conf").is_none());
        assert!(image_format("png").is_none());
    }

    fn entry(name: &str, size: u64, kind: EntryKind) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir: false,
            size,
            mtime: 0,
            permissions: None,
            kind,
            link_target: None,
            is_link_to_dir: false,
            is_link_broken: false,
        }
    }

    #[test]
    fn the_listing_decides_before_any_read() {
        let ready = |e: &FileEntry| match preview_plan(e) {
            PreviewPlan::Ready(content) => Some(content),
            PreviewPlan::Read { .. } => None,
        };
        assert!(matches!(
            ready(&entry("a", 0, EntryKind::File)),
            Some(PreviewContent::Empty)
        ));
        assert!(matches!(
            ready(&entry("fifo", 0, EntryKind::Other)),
            Some(PreviewContent::NotAFile)
        ));
        assert!(matches!(
            ready(&entry(
                "big.png",
                IMAGE_PREVIEW_MAX_BYTES + 1,
                EntryKind::File
            )),
            Some(PreviewContent::TooLarge(_))
        ));
        let mut broken = entry("link", 5, EntryKind::Symlink);
        broken.is_link_broken = true;
        assert!(matches!(ready(&broken), Some(PreviewContent::Failed(_))));
        assert!(matches!(
            preview_plan(&entry("a.conf", 10, EntryKind::File)),
            PreviewPlan::Read {
                limit: TEXT_PREVIEW_BYTES
            }
        ));
    }
}
