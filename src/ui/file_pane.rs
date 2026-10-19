use crate::ui::context_menu::{ContextMenu, ContextMenuItem};
use gpui::prelude::*;
use gpui::{
    canvas, div, px, uniform_list, App, Bounds, ClickEvent, ClipboardItem, Context, Entity,
    EntityId, EventEmitter, ExternalDragPayload, ExternalPaths, FileDragPaths, FocusHandle,
    Focusable, Hsla, KeyDownEvent, MouseButton, Pixels, Point, SharedString,
    UniformListScrollHandle, WeakEntity, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, Search as InputSearch},
    spinner::Spinner,
    v_flex, ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, StyledExt as _,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use crate::file_open;
use crate::preferences::Preferences;
use crate::shell;
use crate::ssh::{EntryKind, FileEntry};
use crate::ui::file_pane_keys::{
    self, hint, pane_command, type_ahead_match, typed_text, PaneCommand, RowMove, SwitchPane,
    TypeAhead,
};
use crate::ui::file_preview::{preview_plan, FilePreview, PreviewContent, PreviewPlan};
use crate::ui::file_selection::{ClickKind, FileSelection};

#[derive(Clone, Debug)]
pub enum SyncIndicator {
    Uploading,
    Downloading,
    Synced { at: SystemTime },
    Conflict,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PaneKind {
    Local,
    /// The server the session is connected to.
    Session,
    /// Another saved server, shown in the left pane instead of the local disk.
    Peer,
}

impl PaneKind {
    pub fn is_remote(self) -> bool {
        self != PaneKind::Local
    }
}

/// Which metadata columns the file list shows at the current pane width.
/// The NAME column is always visible; the rest drop out as the pane narrows,
/// least useful first, so file names never get squeezed onto a second line.
#[derive(Clone, Copy, PartialEq)]
struct VisibleColumns {
    type_col: bool,
    perms: bool,
    size: bool,
    modified: bool,
}

fn visible_columns(width: Pixels, is_remote: bool) -> VisibleColumns {
    let w: f32 = width.into();
    VisibleColumns {
        size: w >= 200.,
        modified: w >= 290.,
        perms: is_remote && w >= 380.,
        type_col: w >= 450.,
    }
}

/// Which column the file list is sorted by (set by clicking its header).
#[derive(Clone, Copy, PartialEq)]
enum SortKey {
    Name,
    Type,
    Perms,
    Size,
    Modified,
}

#[derive(Clone, Copy, PartialEq)]
enum SortDir {
    Asc,
    Desc,
}

/// Sort rank among non-folders (folders are grouped first separately).
fn kind_rank(k: EntryKind) -> u8 {
    match k {
        EntryKind::Folder => 0,
        EntryKind::File => 1,
        EntryKind::Symlink => 2,
        EntryKind::Other => 3,
    }
}

/// Label shown in the TYPE column.
fn kind_label(k: EntryKind) -> &'static str {
    match k {
        EntryKind::Folder => "Folder",
        EntryKind::File => "File",
        EntryKind::Symlink => "Link",
        EntryKind::Other => "Other",
    }
}

/// Compare two entries for the current sort. Folders (and links to folders)
/// always come first, whatever the key and direction. Within a group the
/// chosen key applies, with the name as the tiebreaker.
fn cmp_entries(a: &FileEntry, b: &FileEntry, key: SortKey, dir: SortDir) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let group = b.opens_as_folder().cmp(&a.opens_as_folder());
    if group != Ordering::Equal {
        return group;
    }
    let ord = match key {
        SortKey::Name => Ordering::Equal,
        SortKey::Type => kind_rank(a.kind).cmp(&kind_rank(b.kind)),
        SortKey::Perms => a.permissions.unwrap_or(0).cmp(&b.permissions.unwrap_or(0)),
        SortKey::Size => a.size.cmp(&b.size),
        SortKey::Modified => a.mtime.cmp(&b.mtime),
    };
    let ord = if dir == SortDir::Desc {
        ord.reverse()
    } else {
        ord
    };
    ord.then_with(|| {
        let by_name = a.name.cmp(&b.name);
        if dir == SortDir::Desc && key == SortKey::Name {
            by_name.reverse()
        } else {
            by_name
        }
    })
}

/// One entry named by a drag, a transfer or a delete.
#[derive(Clone, Debug, PartialEq)]
pub struct EntryRef {
    pub path: String,
    /// The entry is a real folder, not a link to one.
    pub is_dir: bool,
}

#[derive(Clone)]
pub struct DraggedEntry {
    /// The pane the drag started in.
    pub source: EntityId,
    /// The dragged row alone, or the whole selection when the row is part of
    /// it. Shared, because every row on screen holds a copy.
    pub items: Rc<[EntryRef]>,
}

pub enum FilePaneEvent {
    NavigateTo(String),
    /// The pane's directory actually changed (a navigation landed). Emitted by
    /// `set_entries` only when the new path differs, so refreshes of the same
    /// folder don't fire it. Drives per-host last-folder persistence.
    PathChanged,
    FileSelected {
        path: String,
    },
    EntryDropped(DraggedEntry),
    /// A drag started on this entry. The remote pane answers by staging a local
    /// copy: if the drag leaves the window the OS wants a real path, and there
    /// is no way to hand it one after the fact.
    DragStarted(DraggedEntry),
    /// A drag left the window but the remote entry has no local copy to hand
    /// over. `reason` is set when it never will have one; `None` means the copy
    /// is simply still in flight.
    ExternalDragUnavailable {
        name: String,
        reason: Option<String>,
    },
    ExternalDropped(Vec<PathBuf>),
    HoverEntry(Option<String>),
    OpenFile(String),
    /// Right-click "Open With…" on a remote file.
    RequestOpenWith(String),
    /// Context-menu transfer toward the opposite pane: Download on the remote
    /// pane, Upload on the local pane (the browser decides by pane kind).
    TransferRequested {
        paths: Vec<String>,
    },
    /// Create a directory at this full path (committed "New Folder…" dialog).
    CreateDir {
        path: String,
    },
    /// Rename `from` to `to` (full paths, same parent directory).
    Rename {
        from: String,
        to: String,
    },
    /// Delete these entries (confirmed by the user in the pane's dialog).
    Delete {
        items: Vec<EntryRef>,
    },
    /// chmod (remote pane only): set the same Unix mode bits on every path.
    Chmod {
        paths: Vec<String>,
        mode: u32,
    },
    /// Move this session's shell into the folder (remote pane only).
    CdTerminalHere(String),
    /// Edit the file in a new terminal tab (session pane only).
    EditInTerminal(String),
    /// The preview shows `path` and waits for its first `limit` bytes. After
    /// a move through the rows the read waits a moment, in case the user is
    /// only passing by.
    PreviewRequested {
        path: String,
        limit: usize,
        is_after_move: bool,
    },
    /// The source pill was clicked: the user wants to choose what this pane shows.
    SourcePickerRequested,
}

/// What a context menu was opened on: one row, the selected rows, or nothing.
/// No entries = empty-space menu (operations on the current directory, e.g.
/// New Folder).
#[derive(Clone)]
struct MenuTarget {
    pos: Point<Pixels>,
    entries: Vec<MenuEntry>,
}

#[derive(Clone)]
struct MenuEntry {
    path: String,
    name: String,
    /// The entry is a real folder. Decides how delete and transfers treat it.
    is_dir: bool,
    /// "Open" navigates: a real folder or a symlink to one.
    opens_as_folder: bool,
    permissions: Option<u32>,
}

impl MenuEntry {
    fn new(entry: &FileEntry, path: String) -> Self {
        Self {
            path,
            name: entry.name.clone(),
            is_dir: entry.is_dir,
            opens_as_folder: entry.opens_as_folder(),
            permissions: entry.permissions,
        }
    }
}

/// In-pane modal dialog (rename / new folder / delete confirm / chmod),
/// rendered as an overlay over the pane so no window-level dialog is needed.
enum PaneDialog {
    Rename {
        from: String,
        input: Entity<InputState>,
    },
    NewFolder {
        input: Entity<InputState>,
    },
    DeleteConfirm {
        items: Vec<EntryRef>,
        title: String,
        question: String,
        detail: String,
    },
    /// `is_dir`: every target is a folder, so the preview starts with "d".
    Chmod {
        paths: Vec<String>,
        title: String,
        is_dir: bool,
        input: Entity<InputState>,
    },
}

/// Title and body text of the delete dialog for these entries.
fn delete_prompt(entries: &[MenuEntry]) -> (String, String) {
    let folder_count = entries.iter().filter(|e| e.is_dir).count();
    if let [entry] = entries {
        let inside = if entry.is_dir {
            "It and everything inside it will be deleted. "
        } else {
            ""
        };
        return (
            format!("Delete \u{201c}{}\u{201d}?", entry.name),
            format!("{inside}This cannot be undone."),
        );
    }
    let count = entries.len();
    let inside = if folder_count > 0 {
        "The folders and everything inside them will be deleted. "
    } else {
        ""
    };
    (
        format!("Delete {count} items?"),
        format!("{inside}This cannot be undone."),
    )
}

/// What exactly a delete will remove, shown under the question: the full path
/// of one entry, or the first names of several. Enter confirms the dialog, so
/// it has to say plainly what goes.
fn delete_detail(entries: &[MenuEntry]) -> String {
    const NAMES_SHOWN: usize = 3;
    if let [entry] = entries {
        return entry.path.clone();
    }
    let names: Vec<&str> = entries
        .iter()
        .take(NAMES_SHOWN)
        .map(|e| e.name.as_str())
        .collect();
    let rest = entries.len().saturating_sub(NAMES_SHOWN);
    if rest == 0 {
        names.join(", ")
    } else {
        format!("{} and {rest} more", names.join(", "))
    }
}

/// The mode the chmod dialog starts with: the one all entries share, or
/// nothing when they differ or one is unknown, so no entry's mode is offered
/// as if it were everyone's.
fn shared_mode_text(entries: &[MenuEntry]) -> String {
    let mut modes = entries
        .iter()
        .map(|e| e.permissions.map(|mode| mode & 0o7777));
    let Some(Some(first)) = modes.next() else {
        return String::new();
    };
    if modes.all(|mode| mode == Some(first)) {
        format!("{first:o}")
    } else {
        String::new()
    }
}

/// "report.txt" for one entry, "5 items" for several.
fn items_label(items: &[EntryRef]) -> String {
    match items {
        [item] => item.path.rsplit('/').next().unwrap_or("").to_owned(),
        _ => format!("{} items", items.len()),
    }
}

/// Shift selects a range and Cmd (Ctrl off macOS) toggles one row.
fn click_kind(modifiers: gpui::Modifiers) -> ClickKind {
    if modifiers.shift {
        ClickKind::Range
    } else if modifiers.secondary() {
        ClickKind::Toggle
    } else {
        ClickKind::Single
    }
}

/// Indices into `entries` that pass the hidden-file and quick-filter rules.
fn visible_entry_indices(entries: &[FileEntry], filter: &str, show_hidden: bool) -> Vec<usize> {
    let needle = filter.to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| show_hidden || !e.name.starts_with('.'))
        .filter(|(_, e)| needle.is_empty() || e.name.to_lowercase().contains(&needle))
        .map(|(ix, _)| ix)
        .collect()
}

struct DragPreview(SharedString);

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py(px(2.))
            .rounded(px(4.))
            .bg(cx.theme().list_active)
            .border_1()
            .border_color(cx.theme().border)
            .text_sm()
            .text_color(cx.theme().foreground)
            .shadow_md()
            .child(self.0.clone())
    }
}

const SPIN_CYCLE: Duration = Duration::from_millis(800);

const ROW_HEIGHT: Pixels = px(26.);
/// Space between the pane edge and the rounded rows.
const LIST_INSET: f32 = 6.;
/// Space inside a row before its icon and after its last column.
const ROW_PADDING: f32 = 8.;

/// A selected row is accent while the keys are in its pane and grey when
/// they are elsewhere, so the selection that Enter or Delete acts on stands out.
fn selection_bg(is_selected: bool, has_focus: bool) -> Option<Hsla> {
    match (is_selected, has_focus) {
        (false, _) => None,
        (true, true) => Some(crate::theme::accent_soft()),
        (true, false) => Some(crate::theme::element()),
    }
}

pub struct FilePane {
    focus_handle: FocusHandle,
    pub kind: PaneKind,
    /// Name in the header: "Local" or the host's name.
    pub source_label: SharedString,
    /// Verb of the transfer toward the other pane: "Upload", "Download" or "Copy".
    pub transfer_label: SharedString,
    /// The server a "Copy" goes to. `None` for an upload or a download.
    pub transfer_target: Option<SharedString>,
    pub path: String,
    pub entries: Vec<FileEntry>,
    /// A listing has landed. Until then `path` is only a placeholder and
    /// `entries` is empty whatever the folder holds.
    is_listed: bool,
    selection: FileSelection,
    type_ahead: TypeAhead,
    pub is_loading: bool,
    pub sync_states: HashMap<String, SyncIndicator>,
    sort_key: SortKey,
    sort_dir: SortDir,
    is_animating: bool,
    animation_start: Option<Instant>,
    animation_gen: u64,
    path_input: Option<Entity<InputState>>,
    is_editing_path: bool,
    context_menu: Option<MenuTarget>,
    /// Space shows the lead row's file over the list.
    pub preview: Option<Entity<FilePreview>>,
    dialog: Option<PaneDialog>,
    /// Last measured pane width, used to decide which columns fit. Updated by a
    /// canvas probe at prepaint. Starts wide so the first frame shows everything.
    list_width: Pixels,
    /// Quick-filter substring. Empty means no filter. Set via the filter field.
    filter: String,
    filter_input: Option<Entity<InputState>>,
    is_filtering: bool,
    /// Whether dotfiles are shown. Off by default, like Finder/Termius.
    show_hidden: bool,
    /// Drives keyboard-nav scroll-to-selection in the virtualized list.
    scroll_handle: UniformListScrollHandle,
    /// Remote pane only: local copies staged for outbound drags, keyed by remote
    /// path.
    ///
    /// This lives on the pane rather than the browser because the drag resolver
    /// has to answer the instant the pointer leaves the window. gpui coalesces
    /// mouse moves, so a quick flick can start the drag and cross the window
    /// edge inside a single event. Asking anyone else would arrive a frame too
    /// late.
    staged_dragout: HashMap<String, StagedFile>,
    /// A folder that could not be listed. Shown in place of the list with a
    /// Retry button; `path` and `entries` still describe the last good folder.
    list_error: Option<ListError>,
}

struct ListError {
    path: String,
    message: String,
}

/// A remote file copied to local disk for outbound drags. `size` and `mtime`
/// are what it was copied at, so a listing that no longer matches means the
/// copy is stale.
pub struct StagedFile {
    pub local: PathBuf,
    pub size: u64,
    pub mtime: u32,
}

impl EventEmitter<FilePaneEvent> for FilePane {}

impl Focusable for FilePane {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl FilePane {
    pub fn new(kind: PaneKind, path: String, cx: &mut Context<Self>) -> Self {
        let (source_label, transfer_label) = match kind {
            PaneKind::Local => ("Local", "Upload"),
            PaneKind::Session | PaneKind::Peer => ("", "Download"),
        };
        Self {
            focus_handle: cx.focus_handle(),
            kind,
            source_label: source_label.into(),
            transfer_label: transfer_label.into(),
            transfer_target: None,
            path,
            entries: Vec::new(),
            is_listed: false,
            selection: FileSelection::default(),
            type_ahead: TypeAhead::default(),
            is_loading: false,
            sync_states: HashMap::new(),
            sort_key: SortKey::Name,
            sort_dir: SortDir::Asc,
            is_animating: false,
            animation_start: None,
            animation_gen: 0,
            path_input: None,
            is_editing_path: false,
            context_menu: None,
            preview: None,
            dialog: None,
            list_width: px(600.),
            filter: String::new(),
            filter_input: None,
            is_filtering: false,
            show_hidden: false,
            scroll_handle: UniformListScrollHandle::new(),
            staged_dragout: HashMap::new(),
            list_error: None,
        }
    }

    /// Whether `path` and `entries` describe a real folder yet.
    pub fn is_listed(&self) -> bool {
        self.is_listed
    }

    pub fn set_source_label(&mut self, label: SharedString, cx: &mut Context<Self>) {
        self.source_label = label;
        cx.notify();
    }

    pub fn set_transfer_label(
        &mut self,
        label: SharedString,
        target: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.transfer_label = label;
        self.transfer_target = target;
        cx.notify();
    }

    /// Record a finished staging copy, making it available to outbound drags.
    pub fn set_staged_dragout(&mut self, remote: String, staged: StagedFile) {
        self.staged_dragout.insert(remote, staged);
    }

    /// The listing entry for a remote path shown in this pane.
    pub fn entry_at(&self, remote: &str) -> Option<&FileEntry> {
        let name = remote.rsplit('/').next()?;
        self.entries.iter().find(|e| e.name == name)
    }

    /// Whether a staged copy of `remote` exists and still matches the listing.
    pub fn has_fresh_staged(&self, remote: &str) -> bool {
        self.fresh_staged(remote).is_some()
    }

    /// The staged copy of `remote`, if it still matches the listing.
    fn fresh_staged(&self, remote: &str) -> Option<&StagedFile> {
        let staged = self.staged_dragout.get(remote)?;
        let entry = self.entry_at(remote)?;
        let is_fresh =
            entry.size == staged.size && entry.mtime == staged.mtime && staged.local.exists();
        is_fresh.then_some(staged)
    }

    /// Indices into `entries` (in sorted order) that pass the hidden-file and
    /// quick-filter rules. The virtualized list and keyboard nav both walk this.
    fn visible_indices(&self) -> Vec<usize> {
        visible_entry_indices(&self.entries, &self.filter, self.show_hidden)
    }

    fn full_path(&self, name: &str) -> String {
        format!("{}/{}", self.path.trim_end_matches('/'), name)
    }

    /// Change the selection with the names of the visible rows at hand, top to
    /// bottom.
    fn change_selection<R>(&mut self, change: impl FnOnce(&mut FileSelection, &[&str]) -> R) -> R {
        let entries = &self.entries;
        let visible: Vec<&str> = visible_entry_indices(entries, &self.filter, self.show_hidden)
            .into_iter()
            .map(|ix| entries[ix].name.as_str())
            .collect();
        change(&mut self.selection, &visible)
    }

    /// Unselect rows that are gone or hidden. Called whenever the listing, the
    /// filter or the hidden-files switch changes, so an action on the
    /// selection never reaches a row the user cannot see.
    fn prune_selection(&mut self) {
        if self.selection.is_empty() {
            return;
        }
        self.change_selection(|selection, visible| {
            let visible: HashSet<&str> = visible.iter().copied().collect();
            selection.retain(|name| visible.contains(name));
        });
    }

    /// The selected rows, top to bottom as shown.
    fn selected_entries(&self) -> Vec<&FileEntry> {
        self.visible_indices()
            .into_iter()
            .map(|ix| &self.entries[ix])
            .filter(|e| self.selection.contains(&e.name))
            .collect()
    }

    fn selected_items(&self) -> Vec<EntryRef> {
        self.selected_entries()
            .into_iter()
            .map(|e| EntryRef {
                path: self.full_path(&e.name),
                is_dir: e.is_dir,
            })
            .collect()
    }

    /// The selected rows in the shape the dialogs and the context menu take.
    fn selected_menu_entries(&self) -> Vec<MenuEntry> {
        self.selected_entries()
            .into_iter()
            .map(|e| MenuEntry::new(e, self.full_path(&e.name)))
            .collect()
    }

    /// Move the lead row through the visible rows and scroll it into view.
    /// With nothing selected, a step lands on the first or the last visible
    /// row. `is_extending` keeps the rows passed over.
    fn move_selection(&mut self, to: RowMove, is_extending: bool, cx: &mut Context<Self>) {
        let page = self.rows_per_page();
        let moved = self.change_selection(|selection, visible| match to {
            RowMove::Up => selection.move_lead(visible, -1, is_extending),
            RowMove::Down => selection.move_lead(visible, 1, is_extending),
            RowMove::PageUp => selection.move_lead(visible, -page, is_extending),
            RowMove::PageDown => selection.move_lead(visible, page, is_extending),
            RowMove::First => selection.move_lead_to(visible, 0, is_extending),
            RowMove::Last => selection.move_lead_to(visible, usize::MAX, is_extending),
        });
        let Some(position) = moved else { return };
        self.scroll_to_row(position);
        cx.notify();
    }

    /// Rows that fit in the list, less one so a page step keeps a row in view.
    fn rows_per_page(&self) -> i32 {
        let height = self
            .scroll_handle
            .0
            .borrow()
            .base_handle
            .bounds()
            .size
            .height;
        ((height / ROW_HEIGHT) as i32 - 1).max(1)
    }

    /// Scroll the visible row at `position` into view.
    fn scroll_to_row(&self, position: usize) {
        let show_parent = !is_at_root(&self.path);
        self.scroll_handle
            .scroll_to_item(position + show_parent as usize, gpui::ScrollStrategy::Top);
    }

    /// Select the first row whose name starts with what was typed. Returns
    /// whether the key was taken as typing.
    fn type_to_select(
        &mut self,
        event: &KeyDownEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Typing in the path or the filter field also passes through here.
        if !self.focus_handle.is_focused(window) || self.list_error.is_some() {
            return false;
        }
        let Some(text) = typed_text(&event.keystroke, event.prefer_character_input) else {
            return false;
        };
        let Some(typed) = self
            .type_ahead
            .push(text, Instant::now())
            .map(str::to_owned)
        else {
            // A space that does not continue a typed name.
            if text == " " && !self.is_loading {
                self.open_preview(false, cx);
                return true;
            }
            return false;
        };
        let position = self.change_selection(|selection, visible| {
            let position = type_ahead_match(visible, &typed)?;
            selection.select_only(visible[position]);
            Some(position)
        });
        if let Some(position) = position {
            self.scroll_to_row(position);
            cx.notify();
        }
        true
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.change_selection(|selection, visible| selection.select_all(visible));
        cx.notify();
    }

    /// A drag began on the row `name`. Dragging a row outside the selection
    /// moves the selection to it, so what is highlighted is what is dragged.
    fn drag_started(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.selection.contains(name) {
            self.selection.select_only(name);
            cx.notify();
        }
    }

    /// Right-click on the row `name`: the menu acts on the whole selection when
    /// the row is part of it, otherwise the row becomes the selection.
    fn open_row_menu(&mut self, name: &str, pos: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.selection.contains(name) {
            self.selection.select_only(name);
        }
        let entries = self.selected_menu_entries();
        if entries.is_empty() {
            return;
        }
        self.context_menu = Some(MenuTarget { pos, entries });
        cx.notify();
    }

    /// Enter on the selected row: navigate into a directory, or open a file the
    /// same way a double-click does (remote files open externally). Does
    /// nothing while several rows are selected.
    fn activate_selection(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.selection.only() else {
            return;
        };
        let Some(entry) = self.entries.iter().find(|e| e.name == name) else {
            return;
        };
        if entry.opens_as_folder() {
            let path = format!("{}/{}", self.path.trim_end_matches('/'), entry.name);
            self.begin_load(cx);
            cx.emit(FilePaneEvent::NavigateTo(path));
        } else if self.kind == PaneKind::Session {
            let path = format!("{}/{}", self.path.trim_end_matches('/'), entry.name);
            open_file(path, cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let command = pane_command(&event.keystroke.key, &event.keystroke.modifiers);
        // A held key repeats. Only a move through the rows may: a repeated F5
        // would queue the same transfer again, and an Enter held a moment too
        // long after a rename would open the row behind the dialog.
        if event.is_held && !matches!(command, Some(PaneCommand::Move { .. })) {
            return;
        }
        let is_handled = if self.context_menu.is_some() || self.dialog.is_some() {
            self.run_overlay_command(command, window, cx)
        } else if self.preview.is_some() {
            self.run_preview_key(event, command, window, cx)
        } else if let Some(command) = command {
            self.run_command(command, window, cx)
        } else {
            self.type_to_select(event, window, cx)
        };
        if is_handled {
            cx.stop_propagation();
        }
    }

    /// Space shows the lead row without opening it. Folders get a note: the
    /// preview stays up while the arrows pass over them.
    fn open_preview(&mut self, is_after_move: bool, cx: &mut Context<Self>) {
        // Dropping the old preview cancels its read.
        self.preview = None;
        cx.notify();
        // The lead can sit on a row a Cmd/Ctrl-click just unselected.
        let Some(name) = self
            .selection
            .lead()
            .filter(|name| self.selection.contains(name))
        else {
            return;
        };
        let Some(entry) = self.entries.iter().find(|e| e.name == name).cloned() else {
            return;
        };
        let path = format!("{}/{}", self.path.trim_end_matches('/'), entry.name);
        let (content, limit) = match preview_plan(&entry) {
            PreviewPlan::Ready(content) => (content, None),
            PreviewPlan::Read { limit } => (PreviewContent::Loading, Some(limit)),
        };
        let preview_path = path.clone();
        self.preview = Some(cx.new(|cx| FilePreview::new(preview_path, entry, content, cx)));
        if let Some(limit) = limit {
            cx.emit(FilePaneEvent::PreviewRequested {
                path,
                limit,
                is_after_move,
            });
        }
    }

    fn close_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview = None;
        // The text view may have had the keys.
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// The previewed file was deleted or renamed.
    fn close_preview_if_gone(&mut self, cx: &App) {
        let is_gone = self.preview.as_ref().is_some_and(|preview| {
            !self
                .entries
                .iter()
                .any(|e| e.name == preview.read(cx).name())
        });
        if is_gone {
            self.preview = None;
        }
    }

    /// Keys while the preview is up. Space and Escape close it, the arrows
    /// move to the next file, Enter opens the row. Other list commands would
    /// act on rows the user cannot see, so they are swallowed.
    fn run_preview_key(
        &mut self,
        event: &KeyDownEvent,
        command: Option<PaneCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // The text view has the keys: it moves its own cursor and selects.
        if !self.focus_handle.is_focused(window) {
            if command == Some(PaneCommand::Cancel) {
                self.close_preview(window, cx);
                return true;
            }
            return false;
        }
        let is_space = event.keystroke.key == "space" && !event.keystroke.modifiers.modified();
        if is_space || command == Some(PaneCommand::Cancel) {
            self.close_preview(window, cx);
            return true;
        }
        match command {
            Some(PaneCommand::Move { to, is_extending }) => {
                self.move_selection(to, is_extending, cx);
                self.open_preview(true, cx);
            }
            Some(PaneCommand::Open) => {
                self.close_preview(window, cx);
                self.activate_selection(cx);
            }
            Some(_) => {}
            None => return typed_text(&event.keystroke, event.prefer_character_input).is_some(),
        }
        true
    }

    /// A menu or a dialog is on top of the list. Escape closes it and Enter
    /// confirms a delete; no other key may reach the rows behind it (Enter
    /// would open the folder that the delete dialog is asking about). The
    /// dialogs with a text field commit on Enter through the field itself.
    fn run_overlay_command(
        &mut self,
        command: Option<PaneCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match command {
            Some(PaneCommand::Cancel) => {
                self.context_menu = None;
                self.cancel_dialog(window, cx);
                cx.notify();
                true
            }
            Some(PaneCommand::Open)
                if self.context_menu.is_none()
                    && matches!(self.dialog, Some(PaneDialog::DeleteConfirm { .. })) =>
            {
                self.commit_dialog(window, cx);
                true
            }
            _ => false,
        }
    }

    /// Run a list command. Returns whether the key was used. Keys also reach
    /// here from the path and filter fields, which pass on what they have no
    /// use for.
    fn run_command(
        &mut self,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if command == PaneCommand::Cancel {
            return self.cancel_topmost(window, cx);
        }
        // Enter in the path field goes to the typed folder, not into the
        // selected row.
        if self.is_editing_path {
            return false;
        }
        // The list is hidden behind the error, so only leaving makes sense.
        let leaves_error = matches!(command, PaneCommand::GoUp | PaneCommand::Refresh);
        if self.list_error.is_some() && !leaves_error {
            return false;
        }
        match command {
            PaneCommand::Move { to, is_extending } => self.move_selection(to, is_extending, cx),
            PaneCommand::Open => self.activate_selection(cx),
            PaneCommand::GoUp => self.go_up(cx),
            PaneCommand::Refresh => self.refresh(cx),
            PaneCommand::ToggleFilter => self.focus_or_close_filter(window, cx),
            PaneCommand::ToggleHidden => self.toggle_hidden(cx),
            PaneCommand::SelectAll if self.focus_handle.is_focused(window) => self.select_all(cx),
            PaneCommand::SelectAll | PaneCommand::Cancel => return false,
            PaneCommand::Rename
            | PaneCommand::Delete
            | PaneCommand::Transfer
            | PaneCommand::NewFolder => return self.run_file_command(command, window, cx),
        }
        true
    }

    /// Rename, delete, transfer or New Folder from the keyboard. Not while a
    /// listing is on its way: the rows may be about to change, which is also
    /// why they have no context menu then.
    fn run_file_command(
        &mut self,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.is_loading {
            return false;
        }
        if command == PaneCommand::NewFolder {
            self.open_new_folder_dialog(window, cx);
            return true;
        }
        let entries = self.selected_menu_entries();
        match (command, entries.as_slice()) {
            (_, []) => return false,
            (PaneCommand::Rename, [entry]) => self.open_rename_dialog(entry, window, cx),
            (PaneCommand::Delete, entries) => self.open_delete_dialog(entries, window, cx),
            (PaneCommand::Transfer, entries) => {
                let paths = entries.iter().map(|e| e.path.clone()).collect();
                cx.emit(FilePaneEvent::TransferRequested { paths });
            }
            // Rename is about one entry.
            _ => return false,
        }
        true
    }

    /// Escape with no menu or dialog open: leave the path field, then close
    /// the filter, then drop the selection.
    fn cancel_topmost(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.is_editing_path {
            self.cancel_edit_path(cx);
            window.focus(&self.focus_handle, cx);
            return true;
        }
        if self.is_filtering {
            self.toggle_filter(window, cx);
            return true;
        }
        if self.focus_handle.is_focused(window) && !self.selection.is_empty() {
            self.selection.clear();
            cx.notify();
            return true;
        }
        false
    }

    /// Tab moves to the other pane. From the path or the filter field it goes
    /// to this pane's list first, and a dialog keeps it.
    fn on_switch_pane(&mut self, _: &SwitchPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            return;
        }
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle, cx);
            return;
        }
        if self.context_menu.take().is_some() {
            cx.notify();
        }
        // The browser owns both panes and does the switch.
        cx.propagate();
    }

    /// Cmd/Ctrl+F typed in the filter field closes it. The field binds the
    /// chord itself (text search, off for one line) and swallows it, so the
    /// key never arrives as a key press; it is caught on its way down.
    fn on_input_search(&mut self, _: &InputSearch, window: &mut Window, cx: &mut Context<Self>) {
        let is_in_filter = self.is_filtering
            && self
                .filter_input
                .as_ref()
                .is_some_and(|input| input.focus_handle(cx).is_focused(window));
        if is_in_filter {
            self.toggle_filter(window, cx);
            cx.stop_propagation();
        }
    }

    /// Cmd/Ctrl+F: open the filter, go back to it when the list has the
    /// focus, and close it from inside the field.
    fn focus_or_close_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unfocused_input = self
            .filter_input
            .clone()
            .filter(|input| self.is_filtering && !input.focus_handle(cx).is_focused(window));
        match unfocused_input {
            Some(input) => input.update(cx, |state, cx| state.focus(window, cx)),
            None => self.toggle_filter(window, cx),
        }
    }

    fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.show_hidden = !self.show_hidden;
        self.prune_selection();
        cx.notify();
    }

    /// Show/hide the quick-filter field. Opening focuses it; closing clears the
    /// active filter so the full listing returns, and hands the keys back to
    /// the list.
    fn toggle_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_filtering {
            self.is_filtering = false;
            self.filter.clear();
            self.prune_selection();
            if let Some(input) = self.filter_input.as_ref().cloned() {
                input.update(cx, |state, cx| state.set_value("", window, cx));
            }
            window.focus(&self.focus_handle, cx);
            cx.notify();
            return;
        }

        if self.filter_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter\u{2026}"));
            cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.filter = input.read(cx).value().to_string();
                    this.prune_selection();
                    cx.notify();
                }
            })
            .detach();
            self.filter_input = Some(input);
        }
        let input = self
            .filter_input
            .as_ref()
            .expect("initialized just above")
            .clone();
        input.update(cx, |state, cx| state.focus(window, cx));
        self.is_filtering = true;
        cx.notify();
    }

    pub fn set_sync_states(
        &mut self,
        states: HashMap<String, SyncIndicator>,
        cx: &mut Context<Self>,
    ) {
        self.sync_states = states;
        cx.notify();
    }

    fn sort_entries(&mut self) {
        let (key, dir) = (self.sort_key, self.sort_dir);
        self.entries.sort_by(|a, b| cmp_entries(a, b, key, dir));
    }

    /// Header click: re-sort by `key`. Clicking the active column flips the
    /// direction; a new column starts ascending. The selection is kept by name,
    /// so it survives the re-sort.
    fn toggle_sort(&mut self, key: SortKey, cx: &mut Context<Self>) {
        if self.sort_key == key {
            self.sort_dir = match self.sort_dir {
                SortDir::Asc => SortDir::Desc,
                SortDir::Desc => SortDir::Asc,
            };
        } else {
            self.sort_key = key;
            self.sort_dir = SortDir::Asc;
        }
        self.sort_entries();
        cx.notify();
    }

    pub fn set_entries(&mut self, path: String, entries: Vec<FileEntry>, cx: &mut Context<Self>) {
        let path_changed = self.path != path;
        self.path = path;
        self.entries = entries;
        self.is_listed = true;
        self.is_loading = false;
        self.list_error = None;
        // Keep the user's chosen sort across navigation and refresh.
        self.sort_entries();
        // A reload of the same folder keeps the selection; a new folder drops it.
        if path_changed {
            self.selection.clear();
            self.type_ahead.clear();
            self.preview = None;
        } else {
            self.prune_selection();
            self.close_preview_if_gone(cx);
        }

        if let Some(start) = self.animation_start.take() {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            let remaining_ms =
                SPIN_CYCLE.as_millis() as u64 - (elapsed_ms % SPIN_CYCLE.as_millis() as u64);
            let remaining = Duration::from_millis(remaining_ms);
            let gen = self.animation_gen;
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(remaining).await;
                this.update(cx, |pane, cx| {
                    if pane.animation_gen == gen {
                        pane.is_animating = false;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        } else {
            self.is_animating = false;
        }
        if path_changed {
            cx.emit(FilePaneEvent::PathChanged);
        }
        cx.notify();
    }

    /// Take a fresh listing of the folder already on screen. Does nothing when
    /// it matches what is shown, and never touches the loading state, so it is
    /// safe while the user is waiting on another folder.
    pub fn refresh_entries(&mut self, mut entries: Vec<FileEntry>, cx: &mut Context<Self>) {
        let (key, dir) = (self.sort_key, self.sort_dir);
        entries.sort_by(|a, b| cmp_entries(a, b, key, dir));
        let is_error_here = self
            .list_error
            .as_ref()
            .is_some_and(|e| e.path == self.path);
        if entries == self.entries && !is_error_here {
            return;
        }
        self.entries = entries;
        self.prune_selection();
        self.close_preview_if_gone(cx);
        if is_error_here {
            self.list_error = None;
        }
        cx.notify();
    }

    /// Show that `path` could not be listed. The pane stays in its last good
    /// folder and becomes usable again.
    pub fn set_list_error(&mut self, path: String, message: String, cx: &mut Context<Self>) {
        self.is_loading = false;
        self.is_animating = false;
        self.animation_start = None;
        self.animation_gen = self.animation_gen.wrapping_add(1);
        self.list_error = Some(ListError { path, message });
        cx.notify();
    }

    fn retry_failed_list(&mut self, cx: &mut Context<Self>) {
        let Some(error) = self.list_error.take() else {
            return;
        };
        self.begin_load(cx);
        cx.emit(FilePaneEvent::NavigateTo(error.path));
    }

    pub fn set_loading(&mut self, cx: &mut Context<Self>) {
        self.begin_load(cx);
    }

    fn begin_load(&mut self, cx: &mut Context<Self>) {
        self.is_loading = true;
        self.list_error = None;
        self.is_animating = true;
        self.animation_start = Some(Instant::now());
        self.animation_gen = self.animation_gen.wrapping_add(1);
        cx.notify();
    }

    /// A plain click opens a folder and selects a file. With Shift or Cmd/Ctrl
    /// the click only changes the selection, which is also how a folder gets
    /// selected.
    fn row_clicked(&mut self, ix: usize, kind: ClickKind, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else {
            return;
        };
        let name = entry.name.clone();
        let path = self.full_path(&name);

        if kind == ClickKind::Single && entry.opens_as_folder() {
            self.begin_load(cx);
            cx.emit(FilePaneEvent::NavigateTo(path));
            return;
        }
        self.change_selection(|selection, visible| selection.click(visible, &name, kind));
        if kind == ClickKind::Single {
            cx.emit(FilePaneEvent::FileSelected { path });
        }
        cx.notify();
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let parent = PathBuf::from(&self.path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "/".to_string());
        self.begin_load(cx);
        cx.emit(FilePaneEvent::NavigateTo(parent));
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.begin_load(cx);
        cx.emit(FilePaneEvent::NavigateTo(self.path.clone()));
    }

    fn start_edit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx));
            cx.subscribe_in(
                &input,
                window,
                |this, _input, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.commit_path(window, cx),
                    InputEvent::Blur => this.cancel_edit_path(cx),
                    _ => {}
                },
            )
            .detach();
            self.path_input = Some(input);
        }

        let path = self.path.clone();
        let input = self
            .path_input
            .as_ref()
            .expect("initialized just above")
            .clone();
        input.update(cx, |state, cx| {
            state.set_value(path, window, cx);
            state.focus(window, cx);
        });
        self.is_editing_path = true;
        cx.notify();
    }

    fn commit_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self
            .path_input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let path = path.trim().to_string();
        self.is_editing_path = false;
        // The field is gone from the next frame; without this the keys would
        // go nowhere.
        window.focus(&self.focus_handle, cx);
        if !path.is_empty() {
            self.begin_load(cx);
            cx.emit(FilePaneEvent::NavigateTo(path));
        }
        cx.notify();
    }

    fn cancel_edit_path(&mut self, cx: &mut Context<Self>) {
        if self.is_editing_path {
            self.is_editing_path = false;
            cx.notify();
        }
    }

    /// Build a focused single-line input prefilled with `value`. Enter commits
    /// the active dialog; changes repaint (the chmod preview reads the value).
    fn dialog_input(
        &mut self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
            state.focus(window, cx);
        });
        cx.subscribe_in(
            &input,
            window,
            |this, _input, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_dialog(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        )
        .detach();
        input
    }

    fn open_rename_dialog(
        &mut self,
        entry: &MenuEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.dialog_input(&entry.name, window, cx);
        self.dialog = Some(PaneDialog::Rename {
            from: entry.path.clone(),
            input,
        });
        cx.notify();
    }

    fn open_new_folder_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.dialog_input("", window, cx);
        self.dialog = Some(PaneDialog::NewFolder { input });
        cx.notify();
    }

    fn open_delete_dialog(
        &mut self,
        entries: &[MenuEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, question) = delete_prompt(entries);
        let detail = delete_detail(entries);
        let items = entries
            .iter()
            .map(|e| EntryRef {
                path: e.path.clone(),
                is_dir: e.is_dir,
            })
            .collect();
        self.dialog = Some(PaneDialog::DeleteConfirm {
            items,
            title,
            question,
            detail,
        });
        // This dialog has no text field, so Enter and Escape come through the pane.
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn open_chmod_dialog(
        &mut self,
        entries: &[MenuEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = self.dialog_input(&shared_mode_text(entries), window, cx);
        let title = match entries {
            [entry] => format!("Permissions: {}", entry.name),
            _ => format!("Permissions: {} items", entries.len()),
        };
        let is_dir = entries.iter().all(|e| e.is_dir);
        let paths = entries.iter().map(|e| e.path.clone()).collect();
        self.dialog = Some(PaneDialog::Chmod {
            paths,
            title,
            is_dir,
            input,
        });
        cx.notify();
    }

    /// Commit whichever dialog is open. Invalid input (empty name, separator
    /// in a name, bad octal) keeps the dialog open instead of doing damage.
    /// A closed dialog hands the keys back to the list.
    fn commit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.dialog.take() {
            None => return,
            Some(PaneDialog::Rename { from, input }) => {
                let new_name = input.read(cx).value().trim().to_string();
                if !is_valid_entry_name(&new_name) {
                    self.dialog = Some(PaneDialog::Rename { from, input });
                    return;
                }
                let to = format!("{}/{}", parent_dir(&from).trim_end_matches('/'), new_name);
                if to != from {
                    self.begin_load(cx);
                    cx.emit(FilePaneEvent::Rename { from, to });
                }
            }
            Some(PaneDialog::NewFolder { input }) => {
                let name = input.read(cx).value().trim().to_string();
                if !is_valid_entry_name(&name) {
                    self.dialog = Some(PaneDialog::NewFolder { input });
                    return;
                }
                let path = format!("{}/{}", self.path.trim_end_matches('/'), name);
                self.begin_load(cx);
                cx.emit(FilePaneEvent::CreateDir { path });
            }
            Some(PaneDialog::DeleteConfirm { items, .. }) => {
                self.begin_load(cx);
                cx.emit(FilePaneEvent::Delete { items });
            }
            Some(PaneDialog::Chmod {
                paths,
                title,
                is_dir,
                input,
            }) => {
                let value = input.read(cx).value().trim().to_string();
                match parse_octal_mode(&value) {
                    Some(mode) => {
                        self.begin_load(cx);
                        cx.emit(FilePaneEvent::Chmod { paths, mode });
                    }
                    None => {
                        self.dialog = Some(PaneDialog::Chmod {
                            paths,
                            title,
                            is_dir,
                            input,
                        });
                        return;
                    }
                }
            }
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn cancel_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some() {
            self.dialog = None;
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }
}

/// Parent of a path that may use either separator (local Windows paths mix
/// `\` and `/`). For a root-level remote entry ("/file") returns "/".
fn parent_dir(path: &str) -> &str {
    match path.rfind(['/', '\\']) {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "",
    }
}

/// A single path component: non-empty, no separators, not "." / "..".
fn is_valid_entry_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && name != "." && name != ".."
}

/// Parse a chmod octal string ("755", "0644", "1777") into mode bits.
fn parse_octal_mode(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 4 {
        return None;
    }
    u32::from_str_radix(s, 8).ok().filter(|m| *m <= 0o7777)
}

/// "drwxr-xr-x"-style mode column. Special bits (setuid/setgid/sticky) are
/// not rendered; the octal value in the chmod dialog still round-trips them.
pub(crate) fn unix_mode_string(mode: u32, is_dir: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if is_dir { 'd' } else { '-' });
    for shift in [6u32, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        s.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        s.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        s.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Menu text of a transfer, e.g. "Upload 3 items" or "Copy to web-01".
fn transfer_menu_label(verb: &str, target: Option<&str>, count: usize) -> String {
    let what = if count == 1 {
        verb.to_string()
    } else {
        format!("{verb} {count} items")
    };
    match target {
        Some(target) => format!("{what} to {target}"),
        None => what,
    }
}

/// Double-click or Enter on a file of the session pane. Binary files open on
/// this computer even when files are set to open in the terminal.
fn open_file(path: String, cx: &mut Context<FilePane>) {
    if Preferences::is_editor_default_open(cx) && !file_open::has_binary_extension(&path) {
        cx.emit(FilePaneEvent::EditInTerminal(path));
    } else {
        cx.emit(FilePaneEvent::OpenFile(path));
    }
}

/// "cd Terminal Here": the session's shell changes into `path`.
fn cd_terminal_menu_item(path: String, cx: &mut Context<FilePane>) -> ContextMenuItem {
    ContextMenuItem::new(
        "ctx-cd-terminal",
        "cd Terminal Here",
        cx.listener(move |this, _, _, cx| {
            this.context_menu = None;
            cx.emit(FilePaneEvent::CdTerminalHere(path.clone()));
            cx.notify();
        }),
    )
    .icon("icons/terminal.svg")
}

impl FilePane {
    /// Context-menu rows for the current [`MenuTarget`]. Items differ by pane
    /// kind (transfer direction, chmod is remote-only), by entry type, and by
    /// how many entries the menu is for.
    fn menu_items(&self, target: &MenuTarget, cx: &mut Context<Self>) -> Vec<ContextMenuItem> {
        let is_remote = self.kind.is_remote();
        let is_session = self.kind == PaneKind::Session;
        let entries = target.entries.as_slice();
        let mut items: Vec<ContextMenuItem> = Vec::new();

        match entries {
            [] => return self.folder_menu_items(cx),
            [entry] => {
                items.extend(self.open_menu_items(entry, cx));
                if is_session && entry.opens_as_folder {
                    items.push(cd_terminal_menu_item(entry.path.clone(), cx));
                }
                items.push(self.transfer_menu_item(entries, cx));
                items.push(
                    ContextMenuItem::new(
                        "ctx-rename",
                        "Rename\u{2026}",
                        cx.listener({
                            let entry = entry.clone();
                            move |this, _, window, cx| {
                                this.context_menu = None;
                                this.open_rename_dialog(&entry, window, cx);
                            }
                        }),
                    )
                    .icon("icons/pen-line.svg")
                    .shortcut(hint::RENAME),
                );
            }
            // Open, Rename and Copy Path are about one entry.
            _ => items.push(self.transfer_menu_item(entries, cx)),
        }

        if is_remote {
            items.push(
                ContextMenuItem::new(
                    "ctx-chmod",
                    "Permissions\u{2026}",
                    cx.listener({
                        let entries = entries.to_vec();
                        move |this, _, window, cx| {
                            this.context_menu = None;
                            this.open_chmod_dialog(&entries, window, cx);
                        }
                    }),
                )
                .icon("icons/lock.svg"),
            );
        }

        if let [entry] = entries {
            items.push(
                ContextMenuItem::new(
                    "ctx-copy-path",
                    "Copy Path",
                    cx.listener({
                        let path = entry.path.clone();
                        move |this, _, _, cx| {
                            this.context_menu = None;
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()));
                            cx.notify();
                        }
                    }),
                )
                .icon("icons/clipboard-copy.svg"),
            );
        }

        let delete_label = match entries {
            [_] => "Delete\u{2026}".to_string(),
            _ => format!("Delete {} items\u{2026}", entries.len()),
        };
        items.push(
            ContextMenuItem::new(
                "ctx-delete",
                delete_label,
                cx.listener({
                    let entries = entries.to_vec();
                    move |this, _, window, cx| {
                        this.context_menu = None;
                        this.open_delete_dialog(&entries, window, cx);
                    }
                }),
            )
            .icon("icons/trash.svg")
            .shortcut(hint::DELETE)
            .danger(),
        );
        items
    }

    /// "Open" for one entry, and "Open With…" for a remote file.
    fn open_menu_items(&self, entry: &MenuEntry, cx: &mut Context<Self>) -> Vec<ContextMenuItem> {
        let path = entry.path.clone();
        if entry.opens_as_folder {
            return vec![ContextMenuItem::new(
                "ctx-open",
                "Open",
                cx.listener(move |this, _, _, cx| {
                    this.context_menu = None;
                    this.begin_load(cx);
                    cx.emit(FilePaneEvent::NavigateTo(path.clone()));
                }),
            )
            .icon("icons/folder-open.svg")
            .shortcut(hint::OPEN)];
        }
        let preview = ContextMenuItem::new(
            "ctx-preview",
            "Preview",
            cx.listener({
                let name = entry.name.clone();
                move |this, _, _, cx| {
                    this.context_menu = None;
                    this.selection.select_only(&name);
                    this.open_preview(false, cx);
                }
            }),
        )
        .icon("icons/eye.svg")
        .shortcut(hint::PREVIEW);
        if self.kind != PaneKind::Session {
            return vec![preview];
        }
        let open = ContextMenuItem::new(
            "ctx-open",
            "Open",
            cx.listener({
                let path = path.clone();
                move |this, _, _, cx| {
                    this.context_menu = None;
                    cx.emit(FilePaneEvent::OpenFile(path.clone()));
                    cx.notify();
                }
            }),
        )
        .icon("icons/file.svg");
        let open_with = ContextMenuItem::new(
            "ctx-open-with",
            "Open With\u{2026}",
            cx.listener({
                let path = path.clone();
                move |this, _, _, cx| {
                    this.context_menu = None;
                    cx.emit(FilePaneEvent::RequestOpenWith(path.clone()));
                    cx.notify();
                }
            }),
        )
        .icon("icons/external-link.svg");
        if file_open::has_binary_extension(&path) {
            return vec![open.shortcut(hint::OPEN), preview, open_with];
        }

        let command = Preferences::editor_command(cx);
        let editor = shell::editor_name(&command).unwrap_or("Terminal");
        let edit = ContextMenuItem::new(
            "ctx-edit-in-terminal",
            format!("Edit in {editor}"),
            cx.listener(move |this, _, _, cx| {
                this.context_menu = None;
                cx.emit(FilePaneEvent::EditInTerminal(path.clone()));
                cx.notify();
            }),
        )
        .icon("icons/square-terminal.svg");
        // The Enter hint goes on the item that Enter and double-click run.
        if Preferences::is_editor_default_open(cx) {
            vec![edit.shortcut(hint::OPEN), preview, open, open_with]
        } else {
            vec![open.shortcut(hint::OPEN), preview, open_with, edit]
        }
    }

    /// The transfer toward the other pane. Folders go along: every direction
    /// copies a whole tree.
    fn transfer_menu_item(&self, entries: &[MenuEntry], cx: &mut Context<Self>) -> ContextMenuItem {
        let label = transfer_menu_label(
            &self.transfer_label,
            self.transfer_target.as_deref(),
            entries.len(),
        );
        let paths: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        ContextMenuItem::new(
            "ctx-transfer",
            label,
            cx.listener(move |this, _, _, cx| {
                this.context_menu = None;
                cx.emit(FilePaneEvent::TransferRequested {
                    paths: paths.clone(),
                });
                cx.notify();
            }),
        )
        .icon(match (self.kind, &self.transfer_target) {
            (PaneKind::Local, _) => "icons/upload.svg",
            (_, Some(_)) => "icons/copy.svg",
            (_, None) => "icons/download.svg",
        })
        .shortcut(hint::TRANSFER)
    }

    /// Empty-space menu: operations on the folder the pane shows.
    fn folder_menu_items(&self, cx: &mut Context<Self>) -> Vec<ContextMenuItem> {
        let mut items = vec![
            ContextMenuItem::new(
                "ctx-new-folder",
                "New Folder\u{2026}",
                cx.listener(|this, _, window, cx| {
                    this.context_menu = None;
                    this.open_new_folder_dialog(window, cx);
                }),
            )
            .icon("icons/folder-plus.svg")
            .shortcut(hint::NEW_FOLDER),
            ContextMenuItem::new(
                "ctx-refresh",
                "Refresh",
                cx.listener(|this, _, _, cx| {
                    this.context_menu = None;
                    this.refresh(cx);
                }),
            )
            .icon("icons/rotate-cw.svg")
            .shortcut(hint::REFRESH),
            ContextMenuItem::new(
                "ctx-copy-path",
                "Copy Path",
                cx.listener(|this, _, _, cx| {
                    this.context_menu = None;
                    cx.write_to_clipboard(ClipboardItem::new_string(this.path.clone()));
                    cx.notify();
                }),
            )
            .icon("icons/clipboard-copy.svg"),
        ];
        if self.kind == PaneKind::Session {
            items.push(cd_terminal_menu_item(self.path.clone(), cx));
        }
        items
    }

    /// Centered in-pane modal for the active [`PaneDialog`].
    /// Covers the whole pane, so nothing behind it can be clicked or dropped on.
    fn render_preview(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let preview = self.preview.clone()?;
        Some(
            div()
                .id("file-preview")
                .absolute()
                .inset_0()
                .flex()
                .occlude()
                .bg(cx.theme().background)
                .child(preview)
                .child(
                    div().absolute().top(px(7.)).right(px(6.)).child(
                        Button::new("close-preview")
                            .ghost()
                            .icon(Icon::empty().path("icons/x.svg"))
                            .xsmall()
                            .tooltip("Close preview (Space)")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_preview(window, cx)),
                            ),
                    ),
                )
                .into_any_element(),
        )
    }

    fn render_dialog(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let theme = cx.theme();

        let (title, body, confirm_label, confirm_danger): (String, gpui::AnyElement, &str, bool) =
            match dialog {
                PaneDialog::Rename { from, input } => (
                    "Rename".to_string(),
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(from.clone()),
                        )
                        .child(Input::new(input).small())
                        .into_any_element(),
                    "Rename",
                    false,
                ),
                PaneDialog::NewFolder { input } => (
                    "New folder".to_string(),
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(format!("in {}", self.path)),
                        )
                        .child(Input::new(input).small())
                        .into_any_element(),
                    "Create",
                    false,
                ),
                PaneDialog::DeleteConfirm {
                    title,
                    question,
                    detail,
                    ..
                } => (
                    title.clone(),
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.foreground)
                                .child(question.clone()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(detail.clone()),
                        )
                        .into_any_element(),
                    "Delete",
                    true,
                ),
                PaneDialog::Chmod {
                    title,
                    is_dir,
                    input,
                    ..
                } => {
                    let value = input.read(cx).value().trim().to_string();
                    let preview = parse_octal_mode(&value)
                        .map(|m| unix_mode_string(m, *is_dir))
                        .unwrap_or_else(|| "invalid mode".to_string());
                    (
                        title.clone(),
                        v_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Octal mode (e.g. 644, 755):"),
                            )
                            .child(Input::new(input).small())
                            .child(
                                div()
                                    .font_family("Cascadia Code")
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(preview),
                            )
                            .into_any_element(),
                        "Apply",
                        false,
                    )
                }
            };

        let confirm = if confirm_danger {
            Button::new("dialog-confirm").danger().child(confirm_label)
        } else {
            Button::new("dialog-confirm").primary().child(confirm_label)
        };

        Some(
            div()
                .absolute()
                .inset_0()
                .bg(theme.overlay)
                .flex()
                .items_center()
                .justify_center()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.cancel_dialog(window, cx)),
                )
                .on_mouse_down(MouseButton::Right, |_, _, _| {})
                .child(
                    v_flex()
                        .occlude()
                        .w(px(300.))
                        .p(px(12.))
                        .gap(px(8.))
                        .bg(crate::theme::pop_surface())
                        .rounded(px(6.))
                        .border_1()
                        .border_color(crate::theme::border_focused())
                        .shadow_md()
                        .on_mouse_down(MouseButton::Left, |_, _, _| {})
                        .child(
                            div()
                                .text_size(px(12.5))
                                .font_semibold()
                                .text_color(theme.foreground)
                                .child(title),
                        )
                        .child(body)
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("dialog-cancel")
                                        .ghost()
                                        .small()
                                        .child("Cancel")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.cancel_dialog(window, cx);
                                        })),
                                )
                                .child(confirm.small().on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.commit_dialog(window, cx);
                                    },
                                ))),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl FilePane {
    /// One clickable column header with a sort-direction arrow on the active
    /// column. `width` = None makes it the flexible NAME column (left-aligned);
    /// a fixed width right-aligns it to match the metadata columns.
    fn sort_header(
        &self,
        id: &'static str,
        label: &'static str,
        key: SortKey,
        width: Option<Pixels>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let muted = crate::theme::text_dim();
        let fg = cx.theme().foreground;
        let active = self.sort_key == key;
        let arrow = if active {
            match self.sort_dir {
                SortDir::Asc => " \u{25B2}",
                SortDir::Desc => " \u{25BC}",
            }
        } else {
            ""
        };

        let mut el = div()
            .id(id)
            .flex()
            .items_center()
            .cursor_pointer()
            .text_color(if active { fg } else { muted })
            .hover(move |s| s.text_color(fg))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_sort(key, cx)));
        el = match width {
            Some(w) => el.w(w).flex_shrink_0().justify_end(),
            None => el.flex_1().min_w_0(),
        };
        el.child(format!("{label}{arrow}")).into_any_element()
    }
}

impl Render for FilePane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let popup = self.context_menu.clone().map(|target| {
            let items = self.menu_items(&target, cx);
            ContextMenu::new(
                "ctx-menu-popup",
                target.pos,
                gpui::Anchor::TopLeft,
                cx.listener(|this, _, _, cx| {
                    this.context_menu = None;
                    cx.notify();
                }),
                items,
            )
        });

        let muted_fg = cx.theme().muted_foreground;
        let fg = cx.theme().foreground;
        let field = crate::theme::field();
        let is_remote = self.kind.is_remote();
        // The pane the keys go to shows it with an accent icon in its header
        // and an accent selection.
        let has_focus = self.focus_handle.contains_focused(window, cx);
        // The session's own pane always shows its server; the other pane can
        // show this computer or another server.
        let pick_source: Option<SourcePillClick> = (self.kind != PaneKind::Session).then(|| {
            Box::new(cx.listener(|_, _: &ClickEvent, _, cx| {
                cx.emit(FilePaneEvent::SourcePickerRequested);
            })) as SourcePillClick
        });
        let pill = source_pill(
            source_icon(self.kind),
            self.source_label.clone(),
            has_focus,
            pick_source,
            cx,
        );

        let is_editing = self.is_editing_path;
        let path_input = self.path_input.clone();
        let path_disp = self.path.clone();

        let pane_path = self.path.as_str();
        // The "../" row is item 0 of the list below, except at the root.
        let show_parent = !is_at_root(pane_path);

        let path_area: gpui::AnyElement = if is_editing {
            if let Some(ref input) = path_input {
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(input).xsmall())
                    .into_any_element()
            } else {
                div().flex_1().into_any_element()
            }
        } else {
            div()
                .id("path-display")
                .flex_1()
                .min_w_0()
                .px(px(5.))
                .py(px(2.))
                .rounded(px(5.))
                .font_family(crate::theme::MONO_FONT)
                .text_size(px(12.))
                .text_color(muted_fg)
                .truncate()
                .cursor_pointer()
                .hover(move |s| s.bg(field).text_color(fg))
                .active(|s| s.bg(crate::theme::element_active()))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.start_edit_path(window, cx);
                }))
                .child(path_disp)
                .into_any_element()
        };

        let filter_field = self
            .filter_input
            .as_ref()
            .filter(|_| self.is_filtering)
            .map(|input| {
                div()
                    .w(px(150.))
                    .flex_shrink_0()
                    .child(Input::new(input).xsmall())
            });

        // A canvas that only reports the pane width at prepaint, so the next
        // frame can drop columns that no longer fit.
        let width_probe = {
            let view = cx.entity();
            canvas(
                move |bounds: Bounds<Pixels>, _, app| {
                    let w = bounds.size.width;
                    view.update(app, |this, cx| {
                        let was = visible_columns(this.list_width, is_remote);
                        this.list_width = w;
                        if visible_columns(w, is_remote) != was {
                            cx.notify();
                        }
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full()
        };

        let cols = visible_columns(self.list_width, is_remote);

        v_flex()
            .track_focus(&self.focus_handle)
            .key_context(file_pane_keys::KEY_CONTEXT)
            .on_action(cx.listener(Self::on_switch_pane))
            .capture_action(cx.listener(Self::on_input_search))
            .on_key_down(cx.listener(Self::on_key_down))
            .flex_1()
            .size_full()
            .overflow_hidden()
            // Anchor for the in-pane dialog overlay (absolute inset_0).
            .relative()
            .child(width_probe)
            .child(
                h_flex()
                    .h(px(38.))
                    .flex_none()
                    .pl(px(6.))
                    .pr(px(6.))
                    .gap(px(4.))
                    .items_center()
                    .child(pill)
                    .child(path_area)
                    .children(filter_field)
                    .child(
                        Button::new("filter")
                            .ghost()
                            .icon(IconName::Search)
                            .xsmall()
                            .selected(self.is_filtering)
                            .tooltip("Filter files")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_filter(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("show-hidden")
                            .ghost()
                            .icon(IconName::Eye)
                            .xsmall()
                            .selected(self.show_hidden)
                            .tooltip(if self.show_hidden {
                                "Hide hidden files"
                            } else {
                                "Show hidden files"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_hidden(cx))),
                    )
                    .child(
                        Button::new("go-up")
                            .ghost()
                            .icon(Icon::empty().path("icons/arrow-up.svg"))
                            .xsmall()
                            .tooltip("Parent folder")
                            .on_click(cx.listener(|this, _, _, cx| this.go_up(cx))),
                    )
                    .child(
                        Button::new("refresh")
                            .ghost()
                            .icon(Icon::empty().path("icons/rotate-cw.svg"))
                            .loading_icon(Icon::empty().path("icons/rotate-cw.svg"))
                            .loading(self.is_animating)
                            .xsmall()
                            .tooltip("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(
                h_flex()
                    .h(px(22.))
                    .flex_none()
                    .px(px(LIST_INSET + ROW_PADDING))
                    .gap(px(8.))
                    .items_center()
                    .text_size(px(11.5))
                    .child(self.sort_header("hdr-name", "Name", SortKey::Name, None, cx))
                    .when(cols.type_col, |el| {
                        el.child(self.sort_header(
                            "hdr-type",
                            "Type",
                            SortKey::Type,
                            Some(px(56.)),
                            cx,
                        ))
                    })
                    .when(cols.perms, |el| {
                        el.child(self.sort_header(
                            "hdr-perms",
                            "Perms",
                            SortKey::Perms,
                            Some(px(72.)),
                            cx,
                        ))
                    })
                    .when(cols.size, |el| {
                        el.child(self.sort_header(
                            "hdr-size",
                            "Size",
                            SortKey::Size,
                            Some(px(76.)),
                            cx,
                        ))
                    })
                    .when(cols.modified, |el| {
                        el.child(self.sort_header(
                            "hdr-modified",
                            "Modified",
                            SortKey::Modified,
                            Some(px(92.)),
                            cx,
                        ))
                    }),
            )
            .child(if let Some(error) = &self.list_error {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap(px(6.))
                    .px(px(16.))
                    .overflow_hidden()
                    .child(
                        div()
                            .max_w_full()
                            .text_size(px(12.5))
                            .text_color(fg)
                            .child(error.message.clone()),
                    )
                    .child(
                        div()
                            .max_w_full()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(error.path.clone()),
                    )
                    .child(
                        Button::new("retry-list")
                            .small()
                            .child("Retry")
                            .on_click(cx.listener(|this, _, _, cx| this.retry_failed_list(cx))),
                    )
                    .into_any_element()
            } else if self.is_loading && self.entries.is_empty() {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Spinner::new().color(muted_fg))
                    .into_any_element()
            } else {
                let list_id = match self.kind {
                    PaneKind::Local => "local-file-list",
                    PaneKind::Session => "remote-file-list",
                    PaneKind::Peer => "peer-file-list",
                };
                // Virtualized: only the rows in view are built and laid out, so a
                // hover repaint (which redraws the whole window) stays cheap even
                // with hundreds of entries. Item 0 is the "../" row when shown.
                let view = cx.entity();
                let item_count = self.visible_indices().len() + show_parent as usize;
                uniform_list(list_id, item_count, move |range, window, app| {
                    view.update(app, |this, cx| {
                        let has_focus = this.focus_handle.contains_focused(window, cx);
                        let pane_path = this.path.clone();
                        let kind = this.kind;
                        // Built once and shared: every selected row drags the
                        // whole selection.
                        let selected_items: Option<Rc<[EntryRef]>> =
                            (this.selection.len() > 1).then(|| this.selected_items().into());
                        let is_loading = this.is_loading;
                        let cols = visible_columns(this.list_width, kind.is_remote());
                        let visible = this.visible_indices();
                        let mut out = Vec::with_capacity(range.len());
                        for ix in range {
                            if show_parent && ix == 0 {
                                out.push(render_parent_row(is_loading, cols, cx));
                                continue;
                            }
                            let vis_pos = ix - show_parent as usize;
                            let Some(&entry_ix) = visible.get(vis_pos) else {
                                continue;
                            };
                            if let Some(e) = this.entries.get(entry_ix) {
                                let full =
                                    format!("{}/{}", pane_path.trim_end_matches('/'), e.name);
                                let sync = this.sync_states.get(&full).cloned();
                                let is_selected = this.selection.contains(&e.name);
                                let drag_items = match &selected_items {
                                    Some(items) if is_selected => items.clone(),
                                    _ => Rc::from([EntryRef {
                                        path: full,
                                        is_dir: e.is_dir,
                                    }]),
                                };
                                out.push(render_entry(
                                    entry_ix,
                                    e,
                                    selection_bg(is_selected, has_focus),
                                    drag_items,
                                    &pane_path,
                                    kind,
                                    is_loading,
                                    sync,
                                    cols,
                                    cx,
                                ));
                            }
                        }
                        out
                    })
                })
                .track_scroll(&self.scroll_handle)
                .flex_1()
                .px(px(LIST_INSET))
                .pb(px(LIST_INSET))
                .when(self.is_loading, |s| s.opacity(0.5))
                .on_drop(cx.listener(|_this, entry: &DraggedEntry, _window, cx| {
                    if entry.source != cx.entity_id() {
                        cx.emit(FilePaneEvent::EntryDropped(entry.clone()));
                    }
                }))
                .on_drop(cx.listener(|_this, paths: &ExternalPaths, _window, cx| {
                    cx.emit(FilePaneEvent::ExternalDropped(paths.paths().to_vec()));
                }))
                .drag_over::<DraggedEntry>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                // A click anywhere in the list, also between the rows, brings
                // the keys to this pane.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| window.focus(&this.focus_handle, cx)),
                )
                // Empty-space menu (New Folder / Refresh / Copy Path). Row
                // handlers stop propagation, so this only fires between rows.
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                        // Escape has to reach the menu.
                        window.focus(&this.focus_handle, cx);
                        this.context_menu = Some(MenuTarget {
                            pos: event.position,
                            entries: Vec::new(),
                        });
                        cx.notify();
                    }),
                )
                .into_any_element()
            })
            .children(self.render_preview(cx))
            .children(popup)
            .children(self.render_dialog(cx))
    }
}

pub type SourcePillClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// The icon before a pane's name: this computer or a server.
pub fn source_icon(kind: PaneKind) -> &'static str {
    match kind {
        PaneKind::Local if cfg!(target_os = "macos") => "icons/laptop.svg",
        PaneKind::Local => "icons/monitor.svg",
        PaneKind::Session | PaneKind::Peer => "icons/server.svg",
    }
}

/// The title at the start of a pane header that says what the pane shows.
/// With `on_click` it works as a dropdown that opens the source picker.
pub fn source_pill(
    icon: &'static str,
    label: SharedString,
    has_focus: bool,
    on_click: Option<SourcePillClick>,
    cx: &App,
) -> impl IntoElement + use<> {
    let theme = cx.theme();
    let fg = theme.foreground;
    let muted_fg = theme.muted_foreground;
    let icon_color = if has_focus {
        crate::theme::accent()
    } else {
        muted_fg
    };
    let is_clickable = on_click.is_some();

    h_flex()
        .id("source-pill")
        .h(px(26.))
        .max_w(px(180.))
        .flex_shrink_0()
        .gap(px(6.))
        .items_center()
        .px(px(6.))
        .rounded(px(6.))
        .text_size(px(13.))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(fg)
        .child(
            Icon::empty()
                .path(icon)
                .size(px(14.))
                .flex_shrink_0()
                .text_color(icon_color),
        )
        .child(div().min_w_0().truncate().child(label))
        .when(is_clickable, |el| {
            el.cursor_pointer()
                .hover(|s| s.bg(crate::theme::field()))
                .active(|s| s.bg(crate::theme::element_active()))
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(12.))
                        .flex_shrink_0()
                        .text_color(muted_fg),
                )
        })
        .when_some(on_click, |el, on_click| el.on_click(on_click))
}

// True when `path` has no parent to walk up to: the Unix root "/" or a
// Windows drive root like "C:\". The "../" row is hidden there.
fn is_at_root(path: &str) -> bool {
    if path.trim_end_matches('/').is_empty() {
        return true;
    }
    PathBuf::from(path)
        .parent()
        .map(|p| p.as_os_str().is_empty())
        .unwrap_or(true)
}

// The "../" pseudo-entry pinned to the top of the list. Clicking it navigates
// to the parent directory. Laid out to line up with the real entry columns.
fn render_parent_row(
    is_loading: bool,
    cols: VisibleColumns,
    cx: &mut Context<FilePane>,
) -> gpui::AnyElement {
    let muted_fg = cx.theme().muted_foreground;

    h_flex()
        .id("entry-parent")
        .w_full()
        .h(ROW_HEIGHT)
        .px(px(ROW_PADDING))
        .gap(px(8.))
        .items_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|s| s.bg(crate::theme::field()))
        .active(|s| s.bg(crate::theme::element_active()))
        .when(!is_loading, |s| {
            s.on_click(cx.listener(|this, _, _, cx| this.go_up(cx)))
        })
        .child(
            Icon::empty()
                .path("icons/corner-left-up.svg")
                .size(px(15.))
                .flex_shrink_0()
                .text_color(muted_fg),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(13.))
                .text_color(muted_fg)
                .child(".."),
        )
        .when(cols.type_col, |el| {
            el.child(div().w(px(56.)).flex_shrink_0())
        })
        .when(cols.perms, |el| el.child(div().w(px(72.)).flex_shrink_0()))
        .when(cols.size, |el| el.child(div().w(px(76.)).flex_shrink_0()))
        .when(cols.modified, |el| {
            el.child(div().w(px(92.)).flex_shrink_0())
        })
        .into_any_element()
}

/// Largest remote file copied locally on the chance a drag leaves the window.
/// Every hover, click and drag pays for this speculatively, so the ceiling is
/// about bandwidth spent on files that are never dragged out, not disk space.
pub const DRAGOUT_MAX_BYTES: u64 = 64 * 1024 * 1024;

const PEER_DRAGOUT_REFUSAL: &str = "download it to this computer first";
const MANY_DRAGOUT_REFUSAL: &str = "drop them on the other pane instead";

/// Why a remote entry will never be draggable into another app, if it won't be.
/// Waiting will not change these, so a drag says so instead of looking stalled.
pub fn dragout_refusal(entry: &FileEntry) -> Option<String> {
    if entry.opens_as_folder() {
        Some("drag folders to the other pane instead".to_string())
    } else if entry.size > DRAGOUT_MAX_BYTES {
        Some(format!(
            "over the {} drag-out limit, download it instead",
            fmt_size(DRAGOUT_MAX_BYTES)
        ))
    } else {
        None
    }
}

/// What to hand the OS when a drag started in a file pane leaves the window.
///
/// Local entries are already real paths. A remote entry needs a staged copy
/// that still matches the listing: gpui hands the platform paths rather than
/// promises, so a file that is still downloading has nothing to offer and the
/// drag stays inside the window.
fn external_drag_payload(
    entry: &DraggedEntry,
    pane: &WeakEntity<FilePane>,
    cx: &mut App,
) -> Option<ExternalDragPayload> {
    let pane_entity = pane.upgrade()?;
    match pane_entity.read(cx).kind {
        PaneKind::Local => {
            return Some(ExternalDragPayload::Files(FileDragPaths::new(
                entry
                    .items
                    .iter()
                    .map(|item| (PathBuf::from(&item.path), item.is_dir)),
            )));
        }
        // Nothing of the other server is staged on this disk: its cache
        // folder would be shared with that server's own tab.
        PaneKind::Peer => {
            let name = items_label(&entry.items);
            let reason = Some(PEER_DRAGOUT_REFUSAL.to_string());
            pane_entity.update(cx, |_, cx| {
                cx.emit(FilePaneEvent::ExternalDragUnavailable { name, reason })
            });
            return None;
        }
        PaneKind::Session => {}
    }

    // Staging copies one file per drag, so several remote entries only go to
    // the other pane.
    let [entry] = &*entry.items else {
        let name = items_label(&entry.items);
        let reason = Some(MANY_DRAGOUT_REFUSAL.to_string());
        pane_entity.update(cx, |_, cx| {
            cx.emit(FilePaneEvent::ExternalDragUnavailable { name, reason })
        });
        return None;
    };
    let pane = pane_entity.read(cx);
    if let Some(staged) = pane.fresh_staged(&entry.path) {
        let local = staged.local.clone();
        tracing::debug!(target: "flint::dragout", path = %entry.path, "handing staged copy to the OS");
        return Some(ExternalDragPayload::Files(FileDragPaths::new([(
            local, false,
        )])));
    }

    let reason = pane.entry_at(&entry.path).and_then(dragout_refusal);
    let name = entry.path.rsplit('/').next().unwrap_or("").to_owned();
    tracing::debug!(target: "flint::dragout", path = %entry.path, ?reason, "no staged copy to hand over");
    pane_entity.update(cx, |_, cx| {
        cx.emit(FilePaneEvent::ExternalDragUnavailable { name, reason })
    });
    None
}

// One row reads all of this to draw itself; a struct would only move the list.
#[allow(clippy::too_many_arguments)]
fn render_entry(
    ix: usize,
    entry: &FileEntry,
    selected_bg: Option<Hsla>,
    drag_items: Rc<[EntryRef]>,
    pane_path: &str,
    kind: PaneKind,
    is_loading: bool,
    sync: Option<SyncIndicator>,
    cols: VisibleColumns,
    cx: &mut Context<FilePane>,
) -> gpui::AnyElement {
    let muted_fg = cx.theme().muted_foreground;
    let dim_fg = crate::theme::text_dim();
    let fg = cx.theme().foreground;
    let danger = cx.theme().danger;
    let primary = cx.theme().primary;

    let opens_as_folder = entry.opens_as_folder();
    let icon = if opens_as_folder {
        IconName::FolderClosed
    } else {
        IconName::File
    };
    let icon_color = if opens_as_folder { primary } else { dim_fg };
    let size_str: SharedString = if opens_as_folder || entry.is_link_broken {
        "--".into()
    } else {
        fmt_size(entry.size).into()
    };
    let modified_str: SharedString = fmt_mtime(entry.mtime).into();
    let name = entry.name.clone();
    let link_target: Option<SharedString> = entry
        .link_target
        .as_ref()
        .map(|target| format!("\u{2192} {target}").into());
    let target_color = if entry.is_link_broken {
        danger
    } else {
        muted_fg
    };
    let is_dim = name.starts_with('.');
    let name_color = if is_dim { muted_fg } else { fg };

    let full_path = format!("{}/{}", pane_path.trim_end_matches('/'), entry.name);
    let is_remote = kind.is_remote();
    let is_session = kind == PaneKind::Session;
    let is_dir = entry.is_dir;
    // Every remote row reports hover: directories get their listing prefetched,
    // files get a head start on the local copy an outbound drag will need.
    let hover_path = is_remote.then(|| full_path.clone());
    let drag_value = DraggedEntry {
        source: cx.entity_id(),
        items: drag_items,
    };
    let pane = cx.entity().downgrade();

    let sync_elem: Option<gpui::AnyElement> = sync.map(|state| {
        let (label, color) = match state {
            SyncIndicator::Uploading => ("syncing".to_string(), None),
            SyncIndicator::Downloading => ("reloading".to_string(), None),
            SyncIndicator::Synced { at } => (format!("synced {}", fmt_sync_time(at)), None),
            SyncIndicator::Conflict => ("conflict".to_string(), Some(crate::theme::conflict())),
            SyncIndicator::Error => ("sync failed".to_string(), Some(danger)),
        };
        div()
            .text_xs()
            .text_color(muted_fg)
            .when_some(color, |el, c| el.text_color(c))
            .flex_shrink_0()
            .child(label)
            .into_any_element()
    });

    let mut el = h_flex()
        .id(("entry", ix))
        .w_full()
        .h(ROW_HEIGHT)
        .px(px(ROW_PADDING))
        .gap(px(8.))
        .items_center()
        .rounded(px(6.))
        .cursor_pointer()
        .when_some(selected_bg, |s, bg| s.bg(bg))
        .when(selected_bg.is_none(), |s| {
            s.hover(|s| s.bg(crate::theme::field()))
        })
        .active(|s| s.bg(crate::theme::element_active()))
        .on_drag(drag_value, {
            let pane = pane.clone();
            let row_name = entry.name.clone();
            move |entry, _, _, cx| {
                pane.update(cx, |pane, cx| {
                    pane.drag_started(&row_name, cx);
                    cx.emit(FilePaneEvent::DragStarted(entry.clone()))
                })
                .ok();
                let label: SharedString = items_label(&entry.items).into();
                cx.new(|_| DragPreview(label))
            }
        })
        .external_drag_payload::<DraggedEntry>(move |entry, _, cx| {
            external_drag_payload(entry, &pane, cx)
        })
        .when(!is_loading, |s| {
            s.on_click(
                cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    let kind = click_kind(event.modifiers());
                    this.row_clicked(ix, kind, cx);
                    if kind == ClickKind::Single && event.click_count() == 2 && is_session {
                        let Some(e) = this.entries.get(ix) else {
                            return;
                        };
                        if !e.opens_as_folder() {
                            let path = format!("{}/{}", this.path.trim_end_matches('/'), e.name);
                            open_file(path, cx);
                        }
                    }
                }),
            )
        })
        .child(
            Icon::new(icon)
                .size(px(15.))
                .flex_shrink_0()
                .text_color(icon_color),
        )
        .child(match link_target {
            None => div()
                .flex_1()
                .min_w_0()
                .text_size(px(13.))
                .text_color(name_color)
                .overflow_hidden()
                .whitespace_nowrap()
                .truncate()
                .text_ellipsis_middle()
                .child(name)
                .into_any_element(),
            // A link shows where it points, dimmed, after its own name.
            Some(target) => h_flex()
                .flex_1()
                .min_w_0()
                .gap(px(5.))
                .overflow_hidden()
                .text_size(px(13.))
                // The name keeps its width; only the target gives way, so a
                // long target never squeezes the name down to an ellipsis.
                .child(
                    div()
                        .flex_shrink_0()
                        .max_w_full()
                        .truncate()
                        .text_color(name_color)
                        .child(name),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(target_color)
                        .child(target),
                )
                .into_any_element(),
        })
        .children(sync_elem)
        .when(cols.type_col, |el| {
            el.child(
                div()
                    .w(px(56.))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .text_size(px(12.))
                    .text_color(dim_fg)
                    .child(kind_label(entry.kind)),
            )
        })
        .when(cols.perms, |el| {
            let perms_str: SharedString = entry
                .permissions
                .map(|m| unix_mode_string(m, is_dir))
                .unwrap_or_else(|| "--".to_string())
                .into();
            el.child(
                div()
                    .w(px(72.))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .font_family(crate::theme::MONO_FONT)
                    .text_size(px(11.))
                    .text_color(dim_fg)
                    .child(perms_str),
            )
        })
        .when(cols.size, |el| {
            el.child(
                div()
                    .w(px(76.))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .text_size(px(12.))
                    .text_color(dim_fg)
                    .child(size_str),
            )
        })
        .when(cols.modified, |el| {
            el.child(
                div()
                    .w(px(92.))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .text_size(px(12.))
                    .text_color(dim_fg)
                    .child(modified_str),
            )
        });

    if let Some(hover_path) = hover_path {
        if !is_loading {
            el = el.on_hover(cx.listener(move |_this, hovered: &bool, _window, cx| {
                cx.emit(FilePaneEvent::HoverEntry(if *hovered {
                    Some(hover_path.clone())
                } else {
                    None
                }));
            }));
        }
    }

    // stop_propagation keeps the list's empty-space menu from also opening
    // for the same click.
    if !is_loading {
        let row_name = entry.name.clone();
        el = el.on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                cx.stop_propagation();
                // The row gets selected, so the list keys must follow it.
                window.focus(&this.focus_handle, cx);
                this.open_row_menu(&row_name, event.position, cx);
            }),
        );
    }
    el.into_any_element()
}

/// Local wall-clock time of a sync, "HH:MM".
fn fmt_sync_time(at: SystemTime) -> String {
    use std::time::UNIX_EPOCH;
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    fmt_clock(secs + local_offset_secs(secs))
}

fn fmt_clock(local_secs: i64) -> String {
    let of_day = local_secs.rem_euclid(86_400);
    format!("{:02}:{:02}", of_day / 3600, of_day % 3600 / 60)
}

/// Seconds the local clock is ahead of UTC at Unix time `ts`. Zero when the
/// system cannot tell.
#[cfg(unix)]
fn local_offset_secs(ts: i64) -> i64 {
    let time = ts as libc::time_t;
    // SAFETY: `tm` is plain data, so all zeroes is a valid value, and both
    // pointers are valid for the whole call.
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&time, &mut local) }.is_null() {
        return 0;
    }
    local.tm_gmtoff as i64
}

/// Seconds the local clock is ahead of UTC. Windows answers with the offset
/// in force today, also for a `ts` on the other side of a clock change.
#[cfg(windows)]
fn local_offset_secs(ts: i64) -> i64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;

    // FILETIME counts 100 ns ticks from the year 1601.
    const TICKS_PER_SEC: i64 = 10_000_000;
    const SECS_1601_TO_1970: i64 = 11_644_473_600;

    let ticks = (ts + SECS_1601_TO_1970) * TICKS_PER_SEC;
    let utc = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut local = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    // SAFETY: both pointers are valid for the whole call.
    if unsafe { FileTimeToLocalFileTime(&utc, &mut local) }.is_err() {
        return 0;
    }
    let local_ticks = (i64::from(local.dwHighDateTime) << 32) | i64::from(local.dwLowDateTime);
    (local_ticks - ticks) / TICKS_PER_SEC
}

pub(crate) fn fmt_mtime(mtime: u32) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    fmt_mtime_at(mtime, now, local_offset_secs(i64::from(mtime)))
}

/// `fmt_mtime` with the clock and the time zone passed in.
fn fmt_mtime_at(mtime: u32, now: u64, offset_secs: i64) -> String {
    // Past this age "Mar 5" could mean this year or an earlier one.
    const YEAR_SHOWN_AFTER_SECS: u64 = 86_400 * 330;

    if mtime == 0 {
        return String::new();
    }
    let ts = mtime as u64;
    if now < ts {
        return "just now".into();
    }
    let diff = now - ts;
    if diff < 60 {
        return "just now".into();
    }
    if diff < 3600 {
        return format!("{}m ago", diff / 60);
    }
    if diff < 86_400 {
        return format!("{}h ago", diff / 3600);
    }
    if diff < 86_400 * 30 {
        return format!("{}d ago", diff / 86_400);
    }
    let (year, month, day) = unix_to_ymd(ts as i64 + offset_secs);
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .get((month - 1) as usize)
    .copied()
    .unwrap_or("?");
    if diff < YEAR_SHOWN_AFTER_SECS {
        format!("{month_name} {day}")
    } else {
        format!("{month_name} {day} {year}")
    }
}

/// Year, month (1 to 12) and day of a Unix time, in the proleptic Gregorian
/// calendar.
fn unix_to_ymd(ts: i64) -> (i64, i64, i64) {
    let z = ts.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    // The count above starts its years in March.
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

pub(crate) fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_dir_handles_remote_and_local_paths() {
        assert_eq!(parent_dir("/srv/app/file.txt"), "/srv/app");
        assert_eq!(parent_dir("/file.txt"), "/");
        assert_eq!(parent_dir("C:\\Users\\me\\f.txt"), "C:\\Users\\me");
        // Rows are built with '/' joins, so local paths can mix separators.
        assert_eq!(parent_dir("C:\\Users\\me/f.txt"), "C:\\Users\\me");
        assert_eq!(parent_dir("noseparator"), "");
    }

    #[test]
    fn entry_name_validation() {
        assert!(is_valid_entry_name("file.txt"));
        assert!(is_valid_entry_name(".hidden"));
        assert!(!is_valid_entry_name(""));
        assert!(!is_valid_entry_name("a/b"));
        assert!(!is_valid_entry_name("a\\b"));
        assert!(!is_valid_entry_name("."));
        assert!(!is_valid_entry_name(".."));
    }

    #[test]
    fn octal_mode_parsing() {
        assert_eq!(parse_octal_mode("755"), Some(0o755));
        assert_eq!(parse_octal_mode("0644"), Some(0o644));
        assert_eq!(parse_octal_mode("1777"), Some(0o1777));
        assert_eq!(parse_octal_mode(""), None);
        assert_eq!(parse_octal_mode("999"), None);
        assert_eq!(parse_octal_mode("rwx"), None);
        assert_eq!(parse_octal_mode("77777"), None);
    }

    fn entry(name: &str, is_dir: bool, size: u64, mtime: u32, kind: EntryKind) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir,
            size,
            mtime,
            permissions: None,
            kind,
            link_target: None,
            is_link_to_dir: false,
            is_link_broken: false,
        }
    }

    fn link_to_dir(name: &str, target: &str) -> FileEntry {
        FileEntry {
            link_target: Some(target.into()),
            is_link_to_dir: true,
            is_link_broken: false,
            ..entry(name, false, 0, 0, EntryKind::Symlink)
        }
    }

    #[test]
    fn link_to_a_folder_sorts_with_the_folders() {
        let mut v = [
            entry("a.txt", false, 1, 0, EntryKind::File),
            link_to_dir("current", "releases/2026-10-01"),
            entry("broken", false, 0, 0, EntryKind::Symlink),
            entry("releases", true, 0, 0, EntryKind::Folder),
        ];
        v.sort_by(|a, b| cmp_entries(a, b, SortKey::Name, SortDir::Asc));
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["current", "releases", "a.txt", "broken"]);
    }

    #[test]
    fn deleting_a_link_to_a_folder_targets_the_link_not_the_folder() {
        let menu = MenuEntry::new(
            &link_to_dir("current", "releases/2026-10-01"),
            "/var/www/current".into(),
        );
        assert!(menu.opens_as_folder);
        // `is_dir` is what the delete dialog passes on; true would start a
        // recursive delete of the release behind the link.
        assert!(!menu.is_dir);
    }

    #[test]
    fn link_to_a_folder_is_not_offered_for_drag_out() {
        assert!(dragout_refusal(&link_to_dir("current", "releases")).is_some());
        assert!(dragout_refusal(&entry("broken", false, 0, 0, EntryKind::Symlink)).is_none());
    }

    #[test]
    fn dragout_refusal_points_at_the_other_pane() {
        assert_eq!(
            dragout_refusal(&entry("www", true, 0, 0, EntryKind::Folder)).as_deref(),
            Some("drag folders to the other pane instead")
        );
        let too_big = entry("dump.sql", false, DRAGOUT_MAX_BYTES + 1, 0, EntryKind::File);
        assert_eq!(
            dragout_refusal(&too_big),
            Some(format!(
                "over the {} drag-out limit, download it instead",
                fmt_size(DRAGOUT_MAX_BYTES)
            ))
        );
        assert_eq!(MANY_DRAGOUT_REFUSAL, "drop them on the other pane instead");
        assert_eq!(PEER_DRAGOUT_REFUSAL, "download it to this computer first");
    }

    #[test]
    fn both_server_panes_count_as_remote() {
        assert!(!PaneKind::Local.is_remote());
        assert!(PaneKind::Session.is_remote());
        assert!(PaneKind::Peer.is_remote());
    }

    #[test]
    fn transfer_menu_label_counts_items_and_names_the_target() {
        assert_eq!(transfer_menu_label("Upload", None, 1), "Upload");
        assert_eq!(transfer_menu_label("Download", None, 3), "Download 3 items");
        assert_eq!(
            transfer_menu_label("Copy", Some("web-01"), 1),
            "Copy to web-01"
        );
        assert_eq!(
            transfer_menu_label("Copy", Some("web-01"), 2),
            "Copy 2 items to web-01"
        );
    }

    fn menu_entry(name: &str, is_dir: bool, permissions: Option<u32>) -> MenuEntry {
        let kind = if is_dir {
            EntryKind::Folder
        } else {
            EntryKind::File
        };
        let entry = FileEntry {
            permissions,
            ..entry(name, is_dir, 0, 0, kind)
        };
        MenuEntry::new(&entry, format!("/srv/{name}"))
    }

    #[test]
    fn delete_prompt_names_one_entry_and_counts_several() {
        let (title, question) = delete_prompt(&[menu_entry("a.txt", false, None)]);
        assert_eq!(title, "Delete \u{201c}a.txt\u{201d}?");
        assert_eq!(question, "This cannot be undone.");

        let (title, question) = delete_prompt(&[menu_entry("logs", true, None)]);
        assert_eq!(title, "Delete \u{201c}logs\u{201d}?");
        assert!(question.contains("everything inside it"));

        let files = [
            menu_entry("a.txt", false, None),
            menu_entry("b.txt", false, None),
        ];
        let (title, question) = delete_prompt(&files);
        assert_eq!(title, "Delete 2 items?");
        assert_eq!(question, "This cannot be undone.");

        let mixed = [
            menu_entry("a.txt", false, None),
            menu_entry("logs", true, None),
        ];
        assert!(delete_prompt(&mixed).1.contains("everything inside them"));
    }

    #[test]
    fn chmod_starts_with_the_mode_all_entries_share() {
        let same = [
            menu_entry("a", false, Some(0o100644)),
            menu_entry("b", false, Some(0o644)),
        ];
        assert_eq!(shared_mode_text(&same), "644");
        let mixed = [
            menu_entry("a", false, Some(0o644)),
            menu_entry("b", true, Some(0o755)),
        ];
        assert_eq!(shared_mode_text(&mixed), "");
        assert_eq!(
            shared_mode_text(&[menu_entry("a", false, Some(0o755))]),
            "755"
        );
    }

    #[test]
    fn chmod_offers_no_mode_when_one_is_unknown() {
        assert_eq!(shared_mode_text(&[menu_entry("logs", true, None)]), "");
        let half_known = [
            menu_entry("a", false, Some(0o644)),
            menu_entry("b", false, None),
        ];
        assert_eq!(shared_mode_text(&half_known), "");
        assert_eq!(shared_mode_text(&[]), "");
    }

    #[test]
    fn unix_time_splits_into_year_month_and_day() {
        assert_eq!(unix_to_ymd(0), (1970, 1, 1));
        assert_eq!(unix_to_ymd(-1), (1969, 12, 31));
        assert_eq!(unix_to_ymd(1_551_744_000), (2019, 3, 5));
        assert_eq!(unix_to_ymd(1_709_164_800), (2024, 2, 29));
        assert_eq!(unix_to_ymd(1_709_251_199), (2024, 2, 29));
        assert_eq!(unix_to_ymd(1_709_251_200), (2024, 3, 1));
    }

    #[test]
    fn modified_time_is_relative_when_recent_and_a_date_when_old() {
        // 2025-10-09 08:53:20 UTC
        const NOW: u64 = 1_760_000_000;
        let ago = |secs: u64| (NOW - secs) as u32;
        assert_eq!(fmt_mtime_at(0, NOW, 0), "");
        assert_eq!(fmt_mtime_at(ago(0) + 60, NOW, 0), "just now");
        assert_eq!(fmt_mtime_at(ago(120), NOW, 0), "2m ago");
        assert_eq!(fmt_mtime_at(ago(3 * 3600), NOW, 0), "3h ago");
        assert_eq!(fmt_mtime_at(ago(2 * 86_400), NOW, 0), "2d ago");
        assert_eq!(fmt_mtime_at(ago(40 * 86_400), NOW, 0), "Aug 30");
    }

    #[test]
    fn modified_time_of_an_old_file_names_the_year() {
        const NOW: u64 = 1_760_000_000;
        // 2019-03-05 00:00:00 UTC
        assert_eq!(fmt_mtime_at(1_551_744_000, NOW, 0), "Mar 5 2019");
    }

    #[test]
    fn modified_date_follows_the_local_time_zone() {
        const NOW: u64 = 1_760_000_000;
        assert_eq!(fmt_mtime_at(1_551_744_000, NOW, -3600), "Mar 4 2019");
        assert_eq!(fmt_mtime_at(1_551_744_000, NOW, 3600), "Mar 5 2019");
    }

    #[test]
    fn sync_clock_shows_hours_and_minutes_of_the_local_day() {
        assert_eq!(fmt_clock(0), "00:00");
        assert_eq!(fmt_clock(13 * 3600 + 5 * 60 + 59), "13:05");
        assert_eq!(fmt_clock(86_400 + 60), "00:01");
        assert_eq!(fmt_clock(-60), "23:59");
    }

    #[test]
    fn drag_label_names_one_entry_and_counts_several() {
        let one = [EntryRef {
            path: "/srv/app/a.txt".into(),
            is_dir: false,
        }];
        assert_eq!(items_label(&one), "a.txt");
        let two = [
            one[0].clone(),
            EntryRef {
                path: "/srv/app/logs".into(),
                is_dir: true,
            },
        ];
        assert_eq!(items_label(&two), "2 items");
    }

    #[test]
    fn click_modifiers_pick_the_selection_change() {
        let plain = gpui::Modifiers::default();
        assert_eq!(click_kind(plain), ClickKind::Single);
        assert_eq!(
            click_kind(gpui::Modifiers::secondary_key()),
            ClickKind::Toggle
        );
        assert_eq!(
            click_kind(gpui::Modifiers {
                shift: true,
                ..plain
            }),
            ClickKind::Range
        );
    }

    #[test]
    fn delete_detail_shows_the_path_of_one_entry_and_the_names_of_several() {
        let entry = |name: &str| menu_entry(name, false, None);
        assert_eq!(delete_detail(&[entry("a.txt")]), "/srv/a.txt");
        assert_eq!(
            delete_detail(&[entry("a"), entry("b"), entry("c")]),
            "a, b, c"
        );
        assert_eq!(
            delete_detail(&[entry("a"), entry("b"), entry("c"), entry("d"), entry("e")]),
            "a, b, c and 2 more"
        );
    }

    #[test]
    fn hidden_and_filtered_rows_are_not_visible() {
        let entries = [
            entry(".env", false, 1, 0, EntryKind::File),
            entry("App.log", false, 1, 0, EntryKind::File),
            entry("notes.txt", false, 1, 0, EntryKind::File),
        ];
        assert_eq!(visible_entry_indices(&entries, "", false), [1, 2]);
        assert_eq!(visible_entry_indices(&entries, "", true), [0, 1, 2]);
        assert_eq!(visible_entry_indices(&entries, "app", true), [1]);
    }

    #[test]
    fn sort_by_name_groups_folders_first() {
        let mut v = [
            entry("b.txt", false, 10, 0, EntryKind::File),
            entry("zdir", true, 0, 0, EntryKind::Folder),
            entry("a.txt", false, 20, 0, EntryKind::File),
            entry("adir", true, 0, 0, EntryKind::Folder),
        ];
        v.sort_by(|a, b| cmp_entries(a, b, SortKey::Name, SortDir::Asc));
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["adir", "zdir", "a.txt", "b.txt"]);
    }

    #[test]
    fn sort_by_size_desc_keeps_folders_on_top() {
        let mut v = [
            entry("small", false, 10, 0, EntryKind::File),
            entry("dir", true, 0, 0, EntryKind::Folder),
            entry("big", false, 999, 0, EntryKind::File),
        ];
        v.sort_by(|a, b| cmp_entries(a, b, SortKey::Size, SortDir::Desc));
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["dir", "big", "small"]);
    }

    #[test]
    fn type_labels() {
        assert_eq!(kind_label(EntryKind::Folder), "Folder");
        assert_eq!(kind_label(EntryKind::File), "File");
        assert_eq!(kind_label(EntryKind::Symlink), "Link");
        assert_eq!(kind_label(EntryKind::Other), "Other");
    }

    #[test]
    fn mode_string_rendering() {
        assert_eq!(unix_mode_string(0o755, true), "drwxr-xr-x");
        assert_eq!(unix_mode_string(0o644, false), "-rw-r--r--");
        assert_eq!(unix_mode_string(0o100644, false), "-rw-r--r--"); // type bits ignored
        assert_eq!(unix_mode_string(0, false), "----------");
    }
}
