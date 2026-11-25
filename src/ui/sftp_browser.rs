use gpui::prelude::*;
use gpui::{
    div, px, AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent,
    Subscription, Task, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    spinner::Spinner,
    v_flex, ActiveTheme as _, Sizable as _,
};
use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use zeroize::Zeroizing;

use crate::connection::Connection;
use crate::file_associations::SharedFileAssociations;
use crate::file_edit::{FileEditSession, SyncState};
use crate::file_open::OpenError;
use crate::preferences::{Preferences, TerminalPosition};
use crate::ssh::{
    CancelFlag, DownloadResult, FileEntry, FileHead, SftpEvent, SftpHandle, StageItem,
    TransferOutcome,
};
use crate::theme;
use crate::ui::file_pane::{
    duplicate_name, move_destination, source_icon, source_pill, DraggedEntry, EntryRef, FilePane,
    FilePaneEvent, PaneKind, SourcePillClick, StagedFile, SyncIndicator,
};
use crate::ui::file_pane_keys::{pane_command, PaneCommand, SwitchPane, KEY_CONTEXT};
use crate::ui::rems_from_px;
use crate::ui::transfer_conflict::{
    free_name, ConflictChoice, Endpoint, FileFacts, TransferConflict,
};

const MAX_VISITED: usize = 50;
/// Past this many cached folders, only the visited ones, their children and
/// the folders above them stay.
const DIR_CACHE_MAX: usize = 300;
/// Per-file ceiling on the copies made speculatively when a directory is
/// listed or a file is hovered. Bigger files still stage on click or a first drag.
const DRAGOUT_PREFETCH_MAX_FILE: u64 = 1024 * 1024;
/// Total bytes one directory listing may spend warming drag-out copies. This is
/// bandwidth spent on files that may never be dragged anywhere, so it is a
/// budget rather than a per-file rule.
const DRAGOUT_PREFETCH_BUDGET: u64 = 4 * 1024 * 1024;
/// A folder of many tiny or empty files would fit the byte budget with
/// thousands of copies, each with its own setup on the UI thread.
const DRAGOUT_PREFETCH_MAX_FILES: usize = 64;
/// What a tiny file counts against the budget: it costs a round trip and a
/// file on disk even when it holds no bytes.
const DRAGOUT_PREFETCH_MIN_COST: u64 = 4 * 1024;
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// How many folders above the one on screen are listed ahead. Going up is a
/// likely next step; far up a deep path, folders are rarely visited and can
/// be big.
const PREFETCHED_ANCESTORS: usize = 8;
/// Progress text changes faster than it can be read, and each change redraws
/// the whole window.
const STATUS_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// How many of the last visited folders the timer lists again. Older ones
/// are listed again when the user goes back to them.
const REFRESH_RECENT: usize = 10;
/// A preview reached with the arrows waits this long before it reads, so
/// passing over rows costs nothing.
const PREVIEW_MOVE_DELAY: Duration = Duration::from_millis(120);

pub enum BrowserEvent {
    StatusChanged(String),
    Error(String),
    ConflictDetected {
        local: PathBuf,
    },
    /// A transfer would land on a name that is taken. The session asks the
    /// user; the conflict itself waits in the browser.
    TransferConflict,
    /// Double-click failed with NoHandler, or user right-clicked "Open With...".
    /// App discovery and picker display are handled by app_view.
    RequestPickerForFile {
        extension: String,
        local: PathBuf,
    },
    /// A tracked transfer was queued, and the session lists it in the tasks
    /// popover. Setting `cancel` aborts it between chunks.
    /// `route` names the servers when the session's own does not say it all:
    /// both ends of a copy, or the left server of an upload or a download.
    /// `uses_session` is false for a transfer that only uses the left server,
    /// which goes on when the session's own connection drops.
    TransferStarted {
        id: u64,
        name: String,
        kind: TransferKind,
        route: Option<String>,
        uses_session: bool,
        cancel: CancelFlag,
    },
    /// Throttled byte progress for a tracked transfer (`total` 0 = unknown).
    TransferProgress {
        id: u64,
        done: u64,
        total: u64,
    },
    /// A tracked transfer waits for a free slot, or has just left the queue.
    TransferWaiting {
        id: u64,
        is_waiting: bool,
    },
    /// Terminal state of a tracked transfer.
    TransferFinished {
        id: u64,
        outcome: TransferOutcome,
    },
    /// The local and/or remote pane navigated to a new folder. Persisted per
    /// host so a restart or reconnect reopens these folders.
    PathsChanged {
        local: String,
        remote: String,
    },
    /// The user asked for the session's shell to change into this remote folder.
    CdTerminalHere(String),
    /// Edit this remote file in a new terminal tab of the session.
    EditInTerminal(String),
    /// The user wants to choose what the left pane shows.
    LeftSourcePickerRequested,
    /// Connect to the left server again, with a new password if one was typed.
    PeerRetryRequested {
        password: Option<Zeroizing<String>>,
    },
    /// Put this computer back in the left pane.
    UseLocalRequested,
}

pub struct PreservedFiles {
    pub count: usize,
    pub cache_root: PathBuf,
}

/// Folders the SFTP panes were showing, captured before a drop so an automatic
/// reconnect can land the user back in the same local + remote directories.
/// A left server needs none: its pane moves to the new browser as it is (see
/// [`LeftServerCarry`]).
#[derive(Clone, Debug)]
pub struct RestorePaths {
    pub local: String,
    pub remote: String,
}

/// Which half of the browser a server pane sits in. The right one is always
/// the session's own server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
}

/// Everything the browser keeps per server.
struct RemoteSide {
    connection: Connection,
    pane: Entity<FilePane>,
    primary: SftpHandle,
    prefetch: Option<SftpHandle>,
    dir_cache: HashMap<PathBuf, Vec<FileEntry>>,
    in_flight: HashSet<PathBuf>,
    pending_navigation: Option<PathBuf>,
    visited_paths: VecDeque<PathBuf>,
    hovering_path: Option<String>,
    pending_hover_task: Option<Task<()>>,
    /// How many deletes, renames and the like run on this server right now.
    /// Shared with those operations, which outlive a side that is dropped.
    running_ops: Rc<Cell<usize>>,
    // Held, not detached: a side that is dropped must stop listening.
    _poll_tasks: Vec<Task<()>>,
    _subscription: Subscription,
}

/// The left server's pane, SFTP channels and running transfers, taken out of
/// a browser the session is about to replace. Its connection outlived the
/// session's own, so the next browser goes on with all of it: the folder, the
/// selection and an upload in flight stay as they are.
pub struct LeftServerCarry {
    remote: RemoteSide,
    transfers: HashMap<u64, LiveTransfer>,
    ended_early: HashSet<u64>,
}

/// A server shown in the left pane instead of the local disk.
struct PeerSlot {
    connection: Connection,
    state: PeerSlotState,
}

enum PeerSlotState {
    Connecting,
    Failed {
        /// The link was up and dropped, as opposed to never getting in.
        was_connected: bool,
        message: String,
        is_password_rejected: bool,
    },
    Ready(RemoteSide),
}

/// Which way a tracked transfer moves its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    Upload,
    Download,
    /// From one server to the other, through this computer.
    Copy,
}

impl TransferKind {
    fn done_status(self) -> &'static str {
        match self {
            Self::Upload => "Upload complete",
            Self::Download => "Download complete",
            Self::Copy => "Copy complete",
        }
    }
}

/// What a transfer reads: a path on this computer or on one of the servers.
enum Source {
    Local(PathBuf),
    Server(Side, String),
}

impl Source {
    fn at(endpoint: Endpoint, path: &str) -> Self {
        match endpoint {
            Endpoint::Local => Self::Local(PathBuf::from(path)),
            Endpoint::Server(side) => Self::Server(side, path.to_string()),
        }
    }

    fn endpoint(&self) -> Endpoint {
        match self {
            Self::Local(_) => Endpoint::Local,
            Self::Server(side, _) => Endpoint::Server(*side),
        }
    }

    fn path(&self) -> String {
        match self {
            Self::Local(path) => path.to_string_lossy().into_owned(),
            Self::Server(_, path) => path.clone(),
        }
    }

    /// The last part of the path, which the copy is named after.
    fn name(&self) -> Option<String> {
        match self {
            Self::Local(path) => path.file_name().map(|n| n.to_string_lossy().into_owned()),
            Self::Server(_, path) => path
                .rsplit('/')
                .next()
                .filter(|name| !name.is_empty())
                .map(str::to_string),
        }
    }
}

/// A tracked transfer the worker has not reported the end of.
struct LiveTransfer {
    kind: TransferKind,
    from: Endpoint,
    to: Endpoint,
    /// What is sent, for a retry.
    source_path: String,
    /// The panes of both ends. A retry needs them to still show the same places.
    panes: (Option<gpui::EntityId>, Option<gpui::EntityId>),
    /// The folder at `to` it writes into, and the name it writes.
    target_dir: String,
    name: String,
    cancel: CancelFlag,
}

impl LiveTransfer {
    /// True when the server on `side` is one end of the transfer.
    fn uses(&self, side: Side) -> bool {
        is_end_of_transfer(side, self.from, self.to)
    }
}

/// Transfer ids are unique in the whole app, so a task of a browser that was
/// rebuilt on reconnect is never mistaken for a new one.
static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);

pub struct SftpBrowser {
    _refresh_task: Task<()>,
    /// When transfer progress last went to the status bar, see
    /// `STATUS_PROGRESS_INTERVAL`.
    progress_shown_at: Option<std::time::Instant>,
    /// Tracked by the left half while it shows a placeholder, not a pane.
    focus_handle: FocusHandle,
    local_pane: Entity<FilePane>,
    right: RemoteSide,
    peer: Option<PeerSlot>,
    /// The password field of a left server that rejected its password.
    /// Created on the first render that shows it, which has the `Window`.
    peer_password_input: Option<Entity<InputState>>,
    /// What the left half tracked focus with before its source changed. The
    /// next render has the `Window` to see whether the keys were there. The
    /// old password field is kept until then: focus in a dropped field is
    /// forgotten.
    left_focus_before_change: Option<(FocusHandle, Option<Entity<InputState>>)>,
    /// Tracked transfers that are still running, by id.
    transfers: HashMap<u64, LiveTransfer>,
    /// Failed transfers that can be started again, by id.
    failed_transfers: HashMap<u64, LiveTransfer>,
    /// What Cmd+C took, and the pane it was shown in.
    copied: Option<(gpui::EntityId, Vec<EntryRef>)>,
    /// The session's own connection ended. This browser stays on screen until
    /// the session is connected again, and nothing may start on the right
    /// server in the meantime.
    is_session_down: bool,
    /// Transfers that were ended as failed when their other server went
    /// away, while the worker of the server they write to still runs. Its
    /// late report lists that pane again: part of a folder may have arrived.
    ended_early: HashSet<u64>,
    in_flight_downloads: HashSet<PathBuf>,
    open_files: HashMap<PathBuf, FileEditSession>,
    file_associations: SharedFileAssociations,
    /// The listing of a local folder in flight. A newer one replaces it, so
    /// a slow folder never lands after the one asked for last.
    local_load_task: Option<Task<()>>,
    /// The single-file staging fetch in flight, if any. Holding it here means a
    /// new one replaces the old rather than racing it.
    dragout_task: Option<Task<()>>,
    /// The remote path `dragout_task` fetches, and the flag that stops it.
    dragout_staging: Option<(String, CancelFlag)>,
    /// The whole-directory staging fetch in flight, kept apart from
    /// `dragout_task` so the two never cancel each other.
    dragout_batch_task: Option<Task<()>>,
    /// Folders last announced via [`BrowserEvent::PathsChanged`], so a re-list of
    /// the same directory (refresh, file op) doesn't re-announce and re-persist.
    persisted_local: String,
    persisted_remote: String,
    /// Transfers held back because their target name is taken, oldest first.
    /// The front one is the one the dialog shows.
    transfer_conflicts: VecDeque<TransferConflict>,
    next_conflict_id: u64,
    /// The "do the same for the others" checkbox of the conflict dialog.
    pub is_conflict_choice_for_all: bool,
}

impl EventEmitter<BrowserEvent> for SftpBrowser {}

impl Focusable for SftpBrowser {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl RemoteSide {
    /// Build the pane for a server and ask for its first listing.
    fn open(
        side: Side,
        connection: Connection,
        primary: SftpHandle,
        prefetch: Option<SftpHandle>,
        start_dir: String,
        cx: &mut Context<SftpBrowser>,
    ) -> Self {
        let kind = match side {
            Side::Left => PaneKind::Peer,
            Side::Right => PaneKind::Session,
        };
        let pane = cx.new(|cx| FilePane::new(kind, "/".to_string(), cx));
        let name = connection.display_name().to_string().into();
        pane.update(cx, |pane, cx| pane.set_source_label(name, cx));
        let (subscription, poll_tasks) =
            listen_to_side(side, &pane, &primary, prefetch.as_ref(), cx);

        pane.update(cx, |pane, cx| pane.set_loading(cx));
        primary.list_dir(start_dir.clone());

        let mut remote = Self {
            connection,
            pane,
            primary,
            prefetch,
            dir_cache: HashMap::new(),
            in_flight: HashSet::from([PathBuf::from(&start_dir)]),
            pending_navigation: Some(PathBuf::from(&start_dir)),
            visited_paths: VecDeque::with_capacity(MAX_VISITED),
            hovering_path: None,
            pending_hover_task: None,
            running_ops: Rc::default(),
            _poll_tasks: poll_tasks,
            _subscription: subscription,
        };
        remote.prefetch_ancestors(&start_dir);
        remote
    }

    /// The folders to re-list in the background, with the channel to ask on.
    fn visited_refresh(&self) -> Option<(SftpHandle, Vec<String>)> {
        let prefetch = self.prefetch.as_ref()?;
        if self.visited_paths.is_empty() {
            return None;
        }
        let paths: Vec<String> = self
            .visited_paths
            .iter()
            .take(REFRESH_RECENT)
            .filter_map(|p| p.to_str().map(String::from))
            .collect();
        tracing::debug!(
            target: "flint::refresh",
            "refreshing {} visited paths",
            paths.len()
        );
        Some((prefetch.clone(), paths))
    }

    fn mark_visited(&mut self, path: &str) {
        let key = PathBuf::from(path);
        if let Some(pos) = self.visited_paths.iter().position(|p| p == &key) {
            self.visited_paths.remove(pos);
        }
        self.visited_paths.push_front(key);
        while self.visited_paths.len() > MAX_VISITED {
            self.visited_paths.pop_back();
        }
    }

    fn mark_in_flight(&mut self, path: &str) {
        self.in_flight.insert(PathBuf::from(path));
    }

    fn clear_in_flight(&mut self, path: &str) {
        self.in_flight.remove(&PathBuf::from(path));
    }

    fn is_in_flight(&self, path: &str) -> bool {
        self.in_flight.contains(&PathBuf::from(path))
    }

    fn cache_get(&self, path: &str) -> Option<&Vec<FileEntry>> {
        self.dir_cache.get(&PathBuf::from(path))
    }

    fn cache_insert(&mut self, path: &str, entries: Vec<FileEntry>) {
        self.dir_cache.insert(PathBuf::from(path), entries);
        prune_dir_cache(&mut self.dir_cache, &self.visited_paths);
    }

    /// Re-list the pane's current folder (used after file operations).
    fn refresh_dir(&mut self, cx: &App) {
        let dir = self.pane.read(cx).path.clone();
        self.mark_in_flight(&dir);
        self.pending_navigation = Some(PathBuf::from(&dir));
        self.primary.list_dir(dir);
    }

    /// List the pane's folder again in the background. Unlike `refresh_dir`
    /// it leaves a navigation the user is waiting on alone.
    fn list_again(&mut self, cx: &App) {
        let dir = self.pane.read(cx).path.clone();
        self.mark_in_flight(&dir);
        self.primary.list_dir(dir);
    }

    /// Put a fresh listing on screen: finish the navigation waiting on `path`,
    /// or update the folder the pane already shows. A listing for any other
    /// folder only goes to the cache, so a late reply never moves the pane.
    fn show_listing(&mut self, path: &str, entries: &[FileEntry], cx: &mut App) {
        if self.pending_navigation.as_deref() == Some(std::path::Path::new(path)) {
            self.pending_navigation = None;
            let entries = entries.to_vec();
            let path = path.to_string();
            self.pane.update(cx, |pane, cx| {
                pane.set_entries(path, entries, cx);
            });
            return;
        }
        if is_same_dir(&self.pane.read(cx).path, path) {
            let entries = entries.to_vec();
            self.pane
                .update(cx, |pane, cx| pane.refresh_entries(entries, cx));
        }
    }

    /// List the folders above `path` ahead, nearest first, so going up shows
    /// them at once. A session often starts deep, where nothing above is known.
    fn prefetch_ancestors(&mut self, path: &str) {
        let Some(prefetch) = self.prefetch.clone() else {
            return;
        };
        let ancestors: Vec<String> = ancestors_to_prefetch(path)
            .into_iter()
            .filter(|ancestor| !self.is_in_flight(ancestor) && self.cache_get(ancestor).is_none())
            .map(String::from)
            .collect();
        if ancestors.is_empty() {
            return;
        }
        for ancestor in &ancestors {
            self.mark_in_flight(ancestor);
        }
        prefetch.prefetch_dirs(ancestors);
    }

    fn trigger_prefetch_for(&mut self, parent: &str, entries: &[FileEntry]) {
        let Some(prefetch) = self.prefetch.clone() else {
            return;
        };
        let to_prefetch: Vec<String> = entries
            .iter()
            .filter(|e| e.opens_as_folder())
            .take(20)
            .map(|e| format!("{}/{}", parent.trim_end_matches('/'), e.name))
            .filter(|p| !self.is_in_flight(p) && self.cache_get(p).is_none())
            .collect();
        if to_prefetch.is_empty() {
            return;
        }
        for path in &to_prefetch {
            self.mark_in_flight(path);
        }
        prefetch.prefetch_dirs(to_prefetch);
    }
}

/// Make the browser of `cx` hear a server's pane and its SFTP channels.
/// Dropping what this returns stops it.
fn listen_to_side(
    side: Side,
    pane: &Entity<FilePane>,
    primary: &SftpHandle,
    prefetch: Option<&SftpHandle>,
    cx: &mut Context<SftpBrowser>,
) -> (Subscription, Vec<Task<()>>) {
    let subscription = cx.subscribe(pane, move |this, _pane, event, cx| {
        this.on_remote_event(side, event, cx);
    });
    let poll_tasks = std::iter::once(primary)
        .chain(prefetch)
        .map(|sftp| poll_sftp_events(side, sftp, cx))
        .collect();
    (subscription, poll_tasks)
}

/// Forward one SFTP channel's events to the browser, tagged with their side.
fn poll_sftp_events(side: Side, sftp: &SftpHandle, cx: &mut Context<SftpBrowser>) -> Task<()> {
    let events = sftp.subscribe();
    cx.spawn(async move |this, cx| loop {
        let Ok(event) = events.recv().await else {
            break;
        };
        if this
            .update(cx, |browser, cx| browser.handle_sftp_event(side, event, cx))
            .is_err()
        {
            break;
        }
    })
}

impl SftpBrowser {
    pub fn new(
        connection: Connection,
        sftp_primary: SftpHandle,
        sftp_prefetch: Option<SftpHandle>,
        home_dir: String,
        file_associations: SharedFileAssociations,
        restore: Option<RestorePaths>,
        cx: &mut Context<Self>,
    ) -> Self {
        let local_home = dirs::home_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());

        // Fresh connect: local home / remote home dir. Reconnect: the folders the
        // panes were showing before the drop, so the user lands back where they were.
        let (local_start, remote_start) = match restore {
            Some(r) => (r.local, r.remote),
            None => (local_home, home_dir),
        };

        let local_pane = cx.new(|cx| FilePane::new(PaneKind::Local, local_start.clone(), cx));

        cx.subscribe(&local_pane, |this, _pane, event, cx| {
            this.on_local_event(event, cx);
        })
        .detach();
        // The session draws this view from a cache, and the layout of the
        // panes comes from the preferences.
        cx.observe_global::<Preferences>(|_, cx| cx.notify())
            .detach();

        let refresh_task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(REFRESH_INTERVAL).await;
            // Nobody sees the panes. The next tick after the window shows
            // again catches up.
            if !crate::shell::is_window_visible() {
                continue;
            }

            let result = this.update(cx, |browser, _cx| {
                [Side::Left, Side::Right]
                    .into_iter()
                    .filter_map(|side| browser.side(side)?.visited_refresh())
                    .collect::<Vec<_>>()
            });
            let Ok(batches) = result else { break };
            for (prefetch, paths) in batches {
                prefetch.prefetch_dirs(paths);
            }
        });

        let right = RemoteSide::open(
            Side::Right,
            connection,
            sftp_primary,
            sftp_prefetch,
            remote_start.clone(),
            cx,
        );

        let mut browser = Self {
            _refresh_task: refresh_task,
            progress_shown_at: None,
            focus_handle: cx.focus_handle(),
            local_pane,
            right,
            peer: None,
            peer_password_input: None,
            left_focus_before_change: None,
            transfers: HashMap::new(),
            failed_transfers: HashMap::new(),
            copied: None,
            is_session_down: false,
            ended_early: HashSet::new(),
            in_flight_downloads: HashSet::new(),
            open_files: HashMap::new(),
            file_associations,
            local_load_task: None,
            dragout_task: None,
            dragout_staging: None,
            dragout_batch_task: None,
            // Seed with the start folders so the initial listings don't re-announce
            // them; only genuine navigation away emits PathsChanged.
            persisted_local: local_start.clone(),
            persisted_remote: remote_start,
            transfer_conflicts: VecDeque::new(),
            next_conflict_id: 0,
            is_conflict_choice_for_all: false,
        };

        browser.load_local_dir(&local_start, cx);

        browser
    }

    fn side(&self, side: Side) -> Option<&RemoteSide> {
        match side {
            Side::Right => Some(&self.right),
            Side::Left => match &self.peer.as_ref()?.state {
                PeerSlotState::Ready(remote) => Some(remote),
                PeerSlotState::Connecting | PeerSlotState::Failed { .. } => None,
            },
        }
    }

    fn side_mut(&mut self, side: Side) -> Option<&mut RemoteSide> {
        match side {
            Side::Right => Some(&mut self.right),
            Side::Left => match &mut self.peer.as_mut()?.state {
                PeerSlotState::Ready(remote) => Some(remote),
                PeerSlotState::Connecting | PeerSlotState::Failed { .. } => None,
            },
        }
    }

    /// The file pane in the left half. `None` while the left server is not
    /// ready and a placeholder is shown instead.
    fn left_pane(&self) -> Option<&Entity<FilePane>> {
        if self.peer.is_none() {
            return Some(&self.local_pane);
        }
        self.side(Side::Left).map(|remote| &remote.pane)
    }

    /// Show the left half if it is hidden. The choice is saved, like the
    /// header's switch.
    fn reveal_second_pane(&mut self, cx: &mut Context<Self>) {
        if Preferences::is_second_pane_shown(cx) {
            return;
        }
        Preferences::update(cx, |prefs| prefs.is_second_pane_shown = true);
        cx.notify();
    }

    /// The other server in the left half, if one was chosen instead of this
    /// computer. Named on the switch that shows the half.
    pub fn left_source_name(&self) -> Option<String> {
        let slot = self.peer.as_ref()?;
        Some(slot.connection.display_name().to_string())
    }

    pub fn list_session_pane_again(&mut self, cx: &mut Context<Self>) {
        self.right.list_again(cx);
    }

    /// The handle that holds the keys when they are in the left half.
    fn left_focus_handle(&self, cx: &App) -> FocusHandle {
        match self.left_pane() {
            Some(pane) => pane.focus_handle(cx),
            None => self.focus_handle.clone(),
        }
    }

    /// The left pane is waiting for `connection` to come up.
    pub fn show_peer_connecting(&mut self, connection: Connection, cx: &mut Context<Self>) {
        self.set_left(
            Some(PeerSlot {
                connection,
                state: PeerSlotState::Connecting,
            }),
            cx,
        );
    }

    /// The left server could not be reached, or its connection dropped.
    pub fn show_peer_failed(
        &mut self,
        connection: Connection,
        was_connected: bool,
        message: String,
        is_password_rejected: bool,
        cx: &mut Context<Self>,
    ) {
        let state = PeerSlotState::Failed {
            was_connected,
            message,
            is_password_rejected,
        };
        self.set_left(Some(PeerSlot { connection, state }), cx);
    }

    /// The left server is connected: show its files in the left pane.
    pub fn attach_peer(
        &mut self,
        connection: Connection,
        primary: SftpHandle,
        prefetch: Option<SftpHandle>,
        start_dir: String,
        cx: &mut Context<Self>,
    ) {
        let remote = RemoteSide::open(
            Side::Left,
            connection.clone(),
            primary,
            prefetch,
            start_dir,
            cx,
        );
        let (verb, target) =
            transfer_wording(PaneKind::Peer, Some(self.right.connection.display_name()));
        remote.pane.update(cx, |pane, cx| {
            pane.set_transfer_label(verb.into(), target.map(Into::into), cx);
        });
        self.set_left(
            Some(PeerSlot {
                connection,
                state: PeerSlotState::Ready(remote),
            }),
            cx,
        );
    }

    /// Put this computer back in the left pane, as it was left.
    pub fn show_local(&mut self, cx: &mut Context<Self>) {
        self.set_left(None, cx);
    }

    /// Give up the left server so the browser that replaces this one can go
    /// on with it. `None` unless its files are showing. This browser must not
    /// be used afterwards: the transfers that go along are no longer its own.
    pub fn take_left_server(&mut self) -> Option<LeftServerCarry> {
        let remote = match self.peer.take() {
            Some(PeerSlot {
                state: PeerSlotState::Ready(remote),
                ..
            }) => remote,
            other => {
                self.peer = other;
                return None;
            }
        };
        // What ran over the session's connection was dropped when it ended.
        let transfers = std::mem::take(&mut self.transfers);
        let ended_early = std::mem::take(&mut self.ended_early);
        tracing::debug!(
            target: "flint::sftp",
            server = remote.connection.display_name(),
            transfers = transfers.len(),
            "left server moves to the new browser",
        );
        Some(LeftServerCarry {
            remote,
            transfers,
            ended_early,
        })
    }

    /// Show a left server that came from the browser this one replaces.
    pub fn adopt_left_server(&mut self, carry: LeftServerCarry, cx: &mut Context<Self>) {
        let LeftServerCarry {
            mut remote,
            transfers,
            ended_early,
        } = carry;
        // The old browser was the one listening. Events that arrived in
        // between wait in the channels.
        let (subscription, poll_tasks) = listen_to_side(
            Side::Left,
            &remote.pane,
            &remote.primary,
            remote.prefetch.as_ref(),
            cx,
        );
        remote._subscription = subscription;
        remote._poll_tasks = poll_tasks;
        remote.pending_hover_task = None;
        self.transfers.extend(transfers);
        self.ended_early.extend(ended_early);
        let connection = remote.connection.clone();
        self.set_left(
            Some(PeerSlot {
                connection,
                state: PeerSlotState::Ready(remote),
            }),
            cx,
        );
    }

    /// Why the left pane must keep its server for now, if it must.
    pub fn left_busy_reason(&self) -> Option<String> {
        let name = self.peer.as_ref()?.connection.display_name();
        let running = self
            .transfers
            .values()
            .filter(|transfer| transfer.uses(Side::Left))
            .count();
        if running > 0 {
            return Some(busy_message(running, name));
        }
        // Closing the connection would cut a delete of a folder tree halfway.
        if self
            .side(Side::Left)
            .is_some_and(|remote| remote.running_ops.get() > 0)
        {
            return Some(format!("A file operation on {name} is still running."));
        }
        let is_asking = self
            .transfer_conflicts
            .iter()
            .any(|conflict| conflict.involves(Side::Left));
        is_asking.then(|| format!("Answer the question about the transfer with {name} first."))
    }

    /// The one place the left pane changes what it shows. A server that leaves
    /// takes its transfers with it: its worker is gone and will not report them.
    fn set_left(&mut self, slot: Option<PeerSlot>, cx: &mut Context<Self>) {
        // Several changes can land before one render; the first one knows
        // where the keys were.
        if self.left_focus_before_change.is_none() {
            let old_input = self.peer_password_input.take();
            self.left_focus_before_change = Some((self.left_focus_handle(cx), old_input));
        }
        self.peer_password_input = None;
        let old = std::mem::replace(&mut self.peer, slot);
        if let Some(PeerSlot {
            connection,
            state: PeerSlotState::Ready(_),
        }) = old
        {
            let is_lost = matches!(
                &self.peer,
                Some(PeerSlot {
                    state: PeerSlotState::Failed {
                        was_connected: true,
                        ..
                    },
                    ..
                })
            );
            let reason = left_server_gone_reason(is_lost, connection.display_name());
            self.fail_peer_transfers(&reason, cx);
        }
        let other_server = self
            .peer
            .as_ref()
            .map(|slot| slot.connection.display_name());
        let (verb, target) = transfer_wording(PaneKind::Session, other_server);
        self.right.pane.update(cx, |pane, cx| {
            pane.set_transfer_label(verb.into(), target.map(Into::into), cx);
        });
        cx.notify();
    }

    fn fail_peer_transfers(&mut self, reason: &str, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self
            .transfers
            .iter()
            .filter(|(_, transfer)| transfer.uses(Side::Left))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let Some(transfer) = self.transfers.remove(&id) else {
                continue;
            };
            transfer.cancel.store(true, Ordering::Relaxed);
            tracing::warn!(target: "flint::sftp", id, reason, "transfer lost its server");
            // The right server's worker still has to stop the copy.
            if transfer.to == Endpoint::Server(Side::Right) {
                self.ended_early.insert(id);
            }
            self.failed_transfers.insert(id, transfer);
            cx.emit(BrowserEvent::TransferFinished {
                id,
                outcome: TransferOutcome::Failed(reason.to_string()),
            });
        }
    }

    /// What the pane across from `endpoint` shows. `None` across from the
    /// session's server while the left server is not connected.
    fn opposite(&self, endpoint: Endpoint) -> Option<Endpoint> {
        match endpoint {
            Endpoint::Local | Endpoint::Server(Side::Left) => Some(Endpoint::Server(Side::Right)),
            Endpoint::Server(Side::Right) if self.peer.is_none() => Some(Endpoint::Local),
            Endpoint::Server(Side::Right) => {
                self.side(Side::Left).map(|_| Endpoint::Server(Side::Left))
            }
        }
    }

    fn endpoint_pane(&self, endpoint: Endpoint) -> Option<&Entity<FilePane>> {
        match endpoint {
            Endpoint::Local => Some(&self.local_pane),
            Endpoint::Server(side) => self.side(side).map(|remote| &remote.pane),
        }
    }

    /// How a place is named to the user: "this computer" or the server's name.
    fn endpoint_label(&self, endpoint: Endpoint) -> String {
        let server = match endpoint {
            Endpoint::Local => None,
            Endpoint::Server(side) => self.side(side),
        };
        match server {
            Some(remote) => remote.connection.display_name().to_string(),
            None => "this computer".to_string(),
        }
    }

    /// The folders the panes are currently showing, captured so an automatic
    /// reconnect can restore them (see [`RestorePaths`]).
    pub fn current_paths(&self, cx: &App) -> RestorePaths {
        RestorePaths {
            local: self.local_pane.read(cx).path.clone(),
            remote: self.right.pane.read(cx).path.clone(),
        }
    }

    /// Announce the current pane folders for persistence, unless nothing changed
    /// since the last announcement. Called whenever a pane reports it navigated.
    fn persist_paths(&mut self, cx: &mut Context<Self>) {
        let local = self.local_pane.read(cx).path.clone();
        let remote = self.right.pane.read(cx).path.clone();
        if local == self.persisted_local && remote == self.persisted_remote {
            return;
        }
        self.persisted_local = local.clone();
        self.persisted_remote = remote.clone();
        cx.emit(BrowserEvent::PathsChanged { local, remote });
    }

    /// The session's own connection ended. The session marks the tasks that
    /// ran over it as failed itself, so they are forgotten here and a late
    /// report from a worker changes nothing. A transfer that only uses the
    /// left server goes on.
    pub fn session_ended(&mut self) {
        self.is_session_down = true;
        // The right server's workers are gone. A copy that wrote to the left
        // server is still being stopped by that one's worker.
        self.ended_early = self
            .transfers
            .iter()
            .filter(|(_, transfer)| transfer.from == Endpoint::Server(Side::Right))
            .filter(|(_, transfer)| transfer.to == Endpoint::Server(Side::Left))
            .map(|(id, _)| *id)
            .collect();
        self.transfers
            .retain(|_, transfer| !transfer.uses(Side::Right));
        // The answers would start transfers on a dead connection, and the
        // overlay over the tab hides what they are about.
        self.transfer_conflicts.clear();
    }

    fn handle_sftp_event(&mut self, side: Side, event: SftpEvent, cx: &mut Context<Self>) {
        match event {
            SftpEvent::PrefetchComplete { path, entries } => {
                let Some(remote) = self.side_mut(side) else {
                    return;
                };
                remote.clear_in_flight(&path);
                remote.show_listing(&path, &entries, cx);
                remote.cache_insert(&path, entries);
                return;
            }

            SftpEvent::DirListed { path, entries } => {
                let Some(remote) = self.side_mut(side) else {
                    return;
                };
                remote.clear_in_flight(&path);
                // Visited first, so a full cache keeps this folder.
                remote.mark_visited(&path);
                remote.cache_insert(&path, entries.clone());
                remote.show_listing(&path, &entries, cx);
                remote.trigger_prefetch_for(&path, &entries);
                if side == Side::Right {
                    self.prefetch_dragout(&path, &entries, cx);
                }
            }

            SftpEvent::DownloadComplete { local } => {
                tracing::info!(?local, "download complete");
                let dir = local
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".to_string());
                // Reloading another folder would pull the pane back into it.
                if is_same_dir(&self.local_pane.read(cx).path, &dir) {
                    self.reload_local_dir(cx);
                }
            }

            SftpEvent::UploadComplete { remote } => {
                tracing::info!(remote, "upload complete");
                let dir = PathBuf::from(&remote)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".to_string());
                // Only re-list a folder the user is in or on the way to. The
                // listing then updates the pane in place; any other folder is
                // dropped from the cache so the next visit fetches it fresh.
                let Some(target) = self.side_mut(side) else {
                    return;
                };
                let key = PathBuf::from(&dir);
                let is_wanted = target.pending_navigation.as_ref() == Some(&key)
                    || is_same_dir(&target.pane.read(cx).path, &dir);
                if is_wanted {
                    target.mark_in_flight(&dir);
                    target.primary.list_dir(dir);
                } else {
                    target.dir_cache.remove(&key);
                }
            }

            SftpEvent::TransferProgress {
                id,
                name,
                done,
                total,
            } => {
                // Throttled by the worker (~10/s). Tracked transfers also feed
                // the tasks popover; everything updates the status bar.
                if let Some(id) = id {
                    cx.emit(BrowserEvent::TransferProgress { id, done, total });
                }
                let is_recent = self
                    .progress_shown_at
                    .is_some_and(|at| at.elapsed() < STATUS_PROGRESS_INTERVAL);
                if is_recent && done < total {
                    return;
                }
                self.progress_shown_at = Some(std::time::Instant::now());
                let msg = if total > 0 {
                    let pct = ((done as f64 / total as f64) * 100.0).min(100.0) as u32;
                    format!(
                        "{name}: {pct}% ({} / {})",
                        crate::ui::file_pane::fmt_size(done),
                        crate::ui::file_pane::fmt_size(total),
                    )
                } else {
                    format!("{name}: {}", crate::ui::file_pane::fmt_size(done))
                };
                cx.emit(BrowserEvent::StatusChanged(msg));
                return;
            }

            SftpEvent::TransferWaiting { id, is_waiting } => {
                cx.emit(BrowserEvent::TransferWaiting { id, is_waiting });
                return;
            }

            SftpEvent::TransferFinished { id, name, outcome } => {
                // The task is already closed, or it was never ours.
                let Some(transfer) = self.transfers.remove(&id) else {
                    tracing::debug!(id, name, "the end of a transfer that is no longer tracked");
                    if self.ended_early.remove(&id) {
                        if let Some(remote) = self.side_mut(side) {
                            remote.list_again(cx);
                        }
                    }
                    return;
                };
                match &outcome {
                    TransferOutcome::Done { .. } => {
                        // The pane reloads on Download/UploadComplete.
                        let status = transfer.kind.done_status();
                        cx.emit(BrowserEvent::StatusChanged(status.to_string()));
                    }
                    // What stopped halfway changed its target: a partial file
                    // is removed, a partial folder tree stays. Show the real
                    // state.
                    TransferOutcome::Canceled | TransferOutcome::Failed(_) => {
                        self.reload_after_stopped_transfer(side, &transfer, cx);
                        if let TransferOutcome::Canceled = outcome {
                            cx.emit(BrowserEvent::StatusChanged(format!("Canceled: {name}")));
                        } else {
                            self.failed_transfers.insert(id, transfer);
                        }
                    }
                }
                cx.emit(BrowserEvent::TransferFinished { id, outcome });
            }

            SftpEvent::ListFailed {
                path,
                error,
                is_background,
            } => {
                self.handle_list_failed(side, path, error, is_background, cx);
            }

            SftpEvent::Error(msg) => {
                tracing::warn!(msg, "sftp error");
                cx.emit(BrowserEvent::Error(msg));
            }

            SftpEvent::Disconnected => {
                tracing::info!("sftp disconnected");
            }
        }
        cx.notify();
    }

    /// `side` is the server whose worker reported the end, which is the one
    /// that was written to unless the transfer was a download.
    fn reload_after_stopped_transfer(
        &mut self,
        side: Side,
        transfer: &LiveTransfer,
        cx: &mut Context<Self>,
    ) {
        if transfer.kind != TransferKind::Download {
            if let Some(remote) = self.side_mut(side) {
                remote.list_again(cx);
            }
            return;
        }
        self.reload_local_dir(cx);
    }

    fn on_local_event(&mut self, event: &FilePaneEvent, cx: &mut Context<Self>) {
        match event {
            FilePaneEvent::NavigateTo(path) => {
                self.load_local_dir(path, cx);
            }
            FilePaneEvent::PathChanged => {
                self.persist_paths(cx);
            }
            FilePaneEvent::FileSelected { .. } => {
                cx.notify();
            }
            FilePaneEvent::EntryDropped(entry) => {
                self.transfer_dropped(Endpoint::Local, entry, cx);
            }
            // Local entries are already real paths, so there is nothing to stage.
            FilePaneEvent::DragStarted(_) | FilePaneEvent::ExternalDragUnavailable { .. } => {}
            FilePaneEvent::ExternalDropped(_) => {}
            FilePaneEvent::HoverEntry(_) => {}
            FilePaneEvent::OpenFile(_) => {}
            FilePaneEvent::RequestOpenWith(_) => {}
            FilePaneEvent::PreviewRequested {
                path,
                limit,
                is_after_move,
            } => {
                let (path, limit) = (path.clone(), *limit);
                let executor = cx.background_executor().clone();
                let read = async move { executor.spawn(read_local_head(path, limit)).await };
                load_preview(&self.local_pane.clone(), *is_after_move, read, cx);
            }
            FilePaneEvent::TransferRequested { paths } => {
                // Local pane: upload into the remote pane's current folder.
                let sources = paths
                    .iter()
                    .map(|path| Source::Local(PathBuf::from(path)))
                    .collect();
                self.request_transfers(sources, Endpoint::Server(Side::Right), cx);
            }
            // Creating and renaming take milliseconds on a local disk, so they
            // run on the GPUI thread (as the cache-dir creation in
            // open_remote_file does).
            FilePaneEvent::CreateDir { path } => {
                self.local_fs_op(std::fs::create_dir(path), "Folder created", cx);
            }
            FilePaneEvent::CreateFile { path } => {
                let result = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map(drop);
                self.local_fs_op(result, "File created", cx);
            }
            FilePaneEvent::Duplicate { from, to } => {
                let item = EntryRef {
                    path: from.clone(),
                    is_dir: false,
                };
                self.copy_within(Endpoint::Local, vec![(item, to.clone())], cx);
            }
            FilePaneEvent::CopyRequested { items } => {
                self.copy_entries(self.local_pane.entity_id(), items, cx);
            }
            FilePaneEvent::PasteRequested => self.paste(Endpoint::Local, cx),
            FilePaneEvent::Rename { from, to } => {
                self.local_fs_op(std::fs::rename(from, to), "Renamed", cx);
            }
            FilePaneEvent::MoveInto { items, dir } => {
                let (items, dir) = (items.clone(), dir.clone());
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move { move_local_items(&items, &dir) })
                        .await;
                    this.update(cx, |browser, cx| {
                        let result = result.map_err(std::io::Error::other);
                        browser.local_fs_op(result, "Moved", cx);
                    })
                    .ok();
                })
                .detach();
            }
            FilePaneEvent::Delete { items } => {
                self.delete_local(items.clone(), cx);
            }
            FilePaneEvent::SourcePickerRequested => {
                cx.emit(BrowserEvent::LeftSourcePickerRequested);
            }
            // Remote-only items: the local menu never offers them.
            FilePaneEvent::Chmod { .. }
            | FilePaneEvent::CdTerminalHere(_)
            | FilePaneEvent::EditInTerminal(_) => {}
        }
    }

    /// Delete local entries off the GPUI thread: a selection of big folders
    /// takes long enough to freeze the window.
    fn delete_local(&mut self, items: Vec<EntryRef>, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { delete_local_items(&items) })
                .await;
            this.update(cx, |browser, cx| {
                let result = result.map_err(std::io::Error::other);
                browser.local_fs_op(result, "Deleted", cx);
            })
            .ok();
        })
        .detach();
    }

    /// Finish a local-pane file operation: report the outcome and reload the
    /// listing (also clears the pane's loading spinner on error).
    fn local_fs_op(
        &mut self,
        result: std::io::Result<()>,
        ok_status: &str,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => cx.emit(BrowserEvent::StatusChanged(ok_status.to_string())),
            Err(e) => cx.emit(BrowserEvent::Error(e.to_string())),
        }
        // A full load, not `reload_local_dir`: the pane shows its spinner
        // since the user confirmed the operation, and only this clears it.
        let dir = self.local_pane.read(cx).path.clone();
        self.load_local_dir(&dir, cx);
    }

    fn on_remote_event(&mut self, side: Side, event: &FilePaneEvent, cx: &mut Context<Self>) {
        let is_session = side == Side::Right;
        match event {
            FilePaneEvent::NavigateTo(path) => {
                let Some(remote) = self.side_mut(side) else {
                    return;
                };
                remote.prefetch_ancestors(path);
                let cached = remote.cache_get(path).cloned();
                if let Some(entries) = cached {
                    remote.pending_navigation = None;
                    let path = path.clone();
                    remote.pane.update(cx, |pane, cx| {
                        pane.set_entries(path.clone(), entries, cx);
                    });
                    if !remote.is_in_flight(&path) {
                        remote.mark_in_flight(&path);
                        remote.primary.list_dir(path);
                    }
                    return;
                }
                remote.pending_navigation = Some(PathBuf::from(path));
                if !remote.is_in_flight(path) {
                    remote.mark_in_flight(path);
                    remote.primary.list_dir(path.clone());
                }
            }
            FilePaneEvent::PathChanged => {
                self.persist_paths(cx);
            }
            FilePaneEvent::FileSelected { path } => {
                // Picking a file out of the list is as good a sign that it is
                // about to be dragged as hovering it.
                if is_session {
                    self.warm_dragout(path.clone(), cx);
                }
                cx.notify();
            }
            FilePaneEvent::EntryDropped(entry) => {
                self.transfer_dropped(Endpoint::Server(side), entry, cx);
            }
            FilePaneEvent::DragStarted(entry) if is_session => {
                // Only a single file can leave the window (see the pane's
                // external drag payload), so only that is worth staging.
                if let [item] = &*entry.items {
                    self.warm_dragout(item.path.clone(), cx);
                }
            }
            FilePaneEvent::ExternalDragUnavailable { name, reason } => {
                cx.emit(BrowserEvent::StatusChanged(match reason {
                    Some(reason) => format!("Can't drag out {name}: {reason}"),
                    None => format!("Still copying {name}. Drag it again in a moment."),
                }));
            }
            FilePaneEvent::ExternalDropped(paths) => {
                self.upload_paths(side, paths, cx);
            }
            FilePaneEvent::OpenFile(path) if is_session => {
                self.open_remote_file(path.clone(), cx);
            }
            FilePaneEvent::RequestOpenWith(path) if is_session => {
                self.open_remote_file_for_picker(path.clone(), cx);
            }
            FilePaneEvent::CdTerminalHere(path) if is_session => {
                cx.emit(BrowserEvent::CdTerminalHere(path.clone()));
            }
            FilePaneEvent::EditInTerminal(path) if is_session => {
                cx.emit(BrowserEvent::EditInTerminal(path.clone()));
            }
            FilePaneEvent::PreviewRequested {
                path,
                limit,
                is_after_move,
            } => {
                let Some(remote) = self.side(side) else {
                    return;
                };
                let sftp = remote.primary.clone();
                let (path, limit) = (path.clone(), *limit);
                let read = async move { sftp.read_head(path, limit).await };
                load_preview(&remote.pane.clone(), *is_after_move, read, cx);
            }
            FilePaneEvent::HoverEntry(maybe_path) => {
                let Some(remote) = self.side_mut(side) else {
                    return;
                };
                let maybe_path = maybe_path.clone();
                remote.hovering_path = maybe_path.clone();
                remote.pending_hover_task = None;

                let Some(path) = maybe_path else { return };

                let is_dir = remote
                    .pane
                    .read(cx)
                    .entry_at(&path)
                    .map(|entry| entry.opens_as_folder())
                    .unwrap_or(false);
                if is_dir && remote.cache_get(&path).is_some() {
                    return;
                }

                let task = cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    this.update(cx, |browser, cx| {
                        browser.hover_settled(side, path, is_dir, cx)
                    })
                    .ok();
                });
                if let Some(remote) = self.side_mut(side) {
                    remote.pending_hover_task = Some(task);
                }
            }
            FilePaneEvent::TransferRequested { paths } => {
                // Into the folder the other pane shows, so that pane must be
                // on screen for the user to see where the files went.
                self.reveal_second_pane(cx);
                let Some(to) = self.opposite(Endpoint::Server(side)) else {
                    cx.emit(BrowserEvent::StatusChanged(LEFT_NOT_CONNECTED.to_string()));
                    return;
                };
                let sources = paths
                    .iter()
                    .map(|path| Source::Server(side, path.clone()))
                    .collect();
                self.request_transfers(sources, to, cx);
            }
            FilePaneEvent::SourcePickerRequested if !is_session => {
                cx.emit(BrowserEvent::LeftSourcePickerRequested);
            }
            FilePaneEvent::CreateDir { path } => {
                let Some(sftp) = self.side(side).map(|remote| remote.primary.clone()) else {
                    return;
                };
                let path = path.clone();
                self.remote_fs_op(
                    side,
                    async move { sftp.make_dir(path).await },
                    "Folder created",
                    cx,
                );
            }
            FilePaneEvent::Rename { from, to } => {
                let Some(sftp) = self.side(side).map(|remote| remote.primary.clone()) else {
                    return;
                };
                let (from, to) = (from.clone(), to.clone());
                self.remote_fs_op(
                    side,
                    async move { sftp.rename(from, to).await },
                    "Renamed",
                    cx,
                );
            }
            FilePaneEvent::CreateFile { path } => {
                let Some(sftp) = self.primary(side) else {
                    return;
                };
                let path = path.clone();
                let op = async move {
                    let empty = scratch_file();
                    std::fs::write(&empty, b"")?;
                    let result = sftp.upload_file(empty.clone(), path).await;
                    let _ = std::fs::remove_file(&empty);
                    result
                };
                self.remote_fs_op(side, op, "File created", cx);
            }
            FilePaneEvent::Duplicate { from, to } => {
                let item = EntryRef {
                    path: from.clone(),
                    is_dir: false,
                };
                self.copy_within(Endpoint::Server(side), vec![(item, to.clone())], cx);
            }
            FilePaneEvent::CopyRequested { items } => {
                if let Some(pane) = self.side(side).map(|remote| remote.pane.entity_id()) {
                    self.copy_entries(pane, items, cx);
                }
            }
            FilePaneEvent::PasteRequested => self.paste(Endpoint::Server(side), cx),
            // These run as one operation over the whole list, so the folder
            // is listed again once, not once per entry.
            FilePaneEvent::MoveInto { items, dir } => {
                let Some(sftp) = self.side(side).map(|remote| remote.primary.clone()) else {
                    return;
                };
                let op = move_remote_items(sftp, items.clone(), dir.clone());
                self.remote_fs_op(side, op, "Moved", cx);
            }
            FilePaneEvent::Delete { items } => {
                let Some(sftp) = self.side(side).map(|remote| remote.primary.clone()) else {
                    return;
                };
                let items = items.clone();
                cx.emit(BrowserEvent::StatusChanged("Deleting…".to_string()));
                self.remote_fs_op(side, delete_remote_items(sftp, items), "Deleted", cx);
            }
            FilePaneEvent::Chmod { paths, mode } => {
                let Some(sftp) = self.side(side).map(|remote| remote.primary.clone()) else {
                    return;
                };
                let (paths, mode) = (paths.clone(), *mode);
                self.remote_fs_op(
                    side,
                    async move {
                        let total = paths.len();
                        let mut errors = Vec::new();
                        for path in paths {
                            if let Err(e) = sftp.set_permissions(path.clone(), mode).await {
                                tracing::warn!(path, ?e, "chmod failed");
                                errors.push(e.to_string());
                            }
                        }
                        batch_result("change permissions of", total, errors)
                            .map_err(anyhow::Error::msg)
                    },
                    "Permissions updated",
                    cx,
                );
            }
            // Only the session's own pane opens files, feeds drag-out and
            // talks to the terminal. Its pill opens no picker.
            FilePaneEvent::DragStarted(_)
            | FilePaneEvent::OpenFile(_)
            | FilePaneEvent::RequestOpenWith(_)
            | FilePaneEvent::CdTerminalHere(_)
            | FilePaneEvent::EditInTerminal(_)
            | FilePaneEvent::SourcePickerRequested => {}
        }
    }

    /// The pointer stayed on a row: fetch what the user is likely to need next.
    fn hover_settled(&mut self, side: Side, path: String, is_dir: bool, cx: &mut Context<Self>) {
        let Some(remote) = self.side(side) else {
            return;
        };
        if remote.hovering_path.as_deref() != Some(path.as_str()) {
            return;
        }
        if is_dir {
            if let Some(prefetch) = remote.prefetch.as_ref() {
                prefetch.prefetch_dirs(vec![path]);
            }
            return;
        }
        if side != Side::Right {
            return;
        }
        // A hover is a weak sign, so it only fetches small files. The rest
        // stage on click or on the drag itself.
        let is_small = self
            .right
            .pane
            .read(cx)
            .entry_at(&path)
            .is_some_and(|e| e.size <= DRAGOUT_PREFETCH_MAX_FILE);
        if is_small {
            self.warm_dragout(path, cx);
        }
    }

    /// Run a remote file operation, then report the outcome and re-list that
    /// pane's current folder (which also clears its loading spinner).
    fn remote_fs_op(
        &mut self,
        side: Side,
        op: impl std::future::Future<Output = anyhow::Result<()>> + 'static,
        ok_status: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(remote) = self.side(side) else {
            return;
        };
        let running_ops = remote.running_ops.clone();
        running_ops.set(running_ops.get() + 1);
        cx.spawn(async move |this, cx| {
            let result = op.await;
            running_ops.set(running_ops.get() - 1);
            this.update(cx, |browser, cx| {
                match result {
                    Ok(()) => cx.emit(BrowserEvent::StatusChanged(ok_status.to_string())),
                    Err(e) => cx.emit(BrowserEvent::Error(e.to_string())),
                }
                // The left pane may show another server by now.
                let remote = browser
                    .side_mut(side)
                    .filter(|remote| Rc::ptr_eq(&remote.running_ops, &running_ops));
                if let Some(remote) = remote {
                    remote.refresh_dir(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn handle_list_failed(
        &mut self,
        side: Side,
        path: String,
        error: String,
        is_background: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(remote) = self.side_mut(side) else {
            return;
        };
        remote.clear_in_flight(&path);
        // The folder may be gone or unreadable now; do not serve it from cache.
        remote.dir_cache.remove(&PathBuf::from(&path));

        let pane_path = remote.pane.read(cx).path.clone();
        let report = list_failure_report(
            remote.pending_navigation.as_deref(),
            &pane_path,
            &path,
            is_background,
        );
        match report {
            ListFailureReport::InPane => {
                tracing::warn!(path, error, "remote folder could not be listed");
                remote.pending_navigation = None;
                remote
                    .pane
                    .update(cx, |pane, cx| pane.set_list_error(path, error, cx));
            }
            ListFailureReport::StatusLine => {
                tracing::warn!(path, error, "sftp listing failed");
                cx.emit(BrowserEvent::Error(error));
            }
            ListFailureReport::Silent => {}
        }
    }

    /// List a local folder off the GPUI thread: a big folder or a slow
    /// network mount takes long enough to freeze the window.
    fn load_local_dir(&mut self, path: &str, cx: &mut Context<Self>) {
        let path = path.to_owned();
        let pane = self.local_pane.downgrade();
        self.local_load_task = Some(cx.spawn(async move |_this, cx| {
            let dir = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { load_local_entries(&dir) })
                .await;
            pane.update(cx, |pane, cx| match result {
                Ok(entries) => pane.set_entries(path, entries, cx),
                Err(e) => {
                    tracing::warn!(path, ?e, "local folder could not be listed");
                    pane.set_list_error(path, e.to_string(), cx);
                }
            })
            .ok();
        }));
    }

    /// List the folder on screen again after something changed in it. Unlike
    /// [`Self::load_local_dir`] this leaves a navigation the user is waiting
    /// on alone, and a failure keeps the old listing.
    fn reload_local_dir(&mut self, cx: &mut Context<Self>) {
        let path = self.local_pane.read(cx).path.clone();
        let pane = self.local_pane.downgrade();
        cx.spawn(async move |_this, cx| {
            let dir = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { load_local_entries(&dir) })
                .await;
            let Ok(entries) = result else { return };
            pane.update(cx, |pane, cx| {
                if is_same_dir(&pane.path, &path) {
                    pane.refresh_entries(entries, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Allocate a task id + cancel flag for a transfer and announce it, so the
    /// session lists it in the tasks popover with a progress ring and a stop
    /// button.
    #[allow(clippy::too_many_arguments)]
    fn begin_transfer(
        &mut self,
        target_dir: &str,
        name: &str,
        kind: TransferKind,
        (from, source_path): (Endpoint, &str),
        to: Endpoint,
        route: Option<String>,
        cx: &mut Context<Self>,
    ) -> (u64, CancelFlag) {
        let id = NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed);
        let cancel = CancelFlag::default();
        let pane_id = |endpoint| self.endpoint_pane(endpoint).map(Entity::entity_id);
        let transfer = LiveTransfer {
            kind,
            from,
            to,
            source_path: source_path.to_string(),
            panes: (pane_id(from), pane_id(to)),
            target_dir: target_dir.to_string(),
            name: name.to_string(),
            cancel: cancel.clone(),
        };
        let uses_session = transfer.uses(Side::Right);
        self.transfers.insert(id, transfer);
        cx.emit(BrowserEvent::TransferStarted {
            id,
            name: name.to_string(),
            kind,
            route,
            uses_session,
            cancel: cancel.clone(),
        });
        (id, cancel)
    }

    /// Transfer everything a drag carries to the pane it was dropped on. Only
    /// a drag from the pane across counts. Each entry is its own transfer, and
    /// each is checked for a name conflict.
    fn transfer_dropped(&mut self, to: Endpoint, entry: &DraggedEntry, cx: &mut Context<Self>) {
        let Some(from) = self.opposite(to) else {
            return;
        };
        if self.endpoint_pane(from).map(Entity::entity_id) != Some(entry.source) {
            return;
        }
        let sources = entry
            .items
            .iter()
            .map(|item| Source::at(from, &item.path))
            .collect();
        self.request_transfers(sources, to, cx);
    }

    /// The folder the pane of `to` shows, and the items its listing has there
    /// by name. The error says why nothing can be sent there. A local folder
    /// is checked on disk item by item instead, so its map stays empty. On a
    /// server a file created since the listing is not noticed.
    fn target_listing(
        &self,
        to: Endpoint,
        cx: &App,
    ) -> Result<(String, HashMap<String, FileFacts>), String> {
        let Endpoint::Server(side) = to else {
            return Ok((self.local_pane.read(cx).path.clone(), HashMap::new()));
        };
        let Some(remote) = self.side(side) else {
            return Err(LEFT_NOT_CONNECTED.to_string());
        };
        let pane = remote.pane.read(cx);
        // Before its first listing the pane has no folder and no names to
        // check: the transfer would land in "/" and replace without asking.
        if !pane.is_listed() {
            return Err(not_listed_message(remote.connection.display_name()));
        }
        let listed = pane
            .entries
            .iter()
            .map(|e| (e.name.clone(), entry_facts(e)))
            .collect();
        Ok((pane.path.clone(), listed))
    }

    fn source_facts(&self, source: &Source, cx: &App) -> FileFacts {
        let facts = match source {
            Source::Local(path) => local_facts(path),
            Source::Server(side, path) => self
                .side(*side)
                .and_then(|remote| remote.pane.read(cx).entry_at(path))
                .map(entry_facts),
        };
        facts.unwrap_or(FileFacts {
            is_folder: false,
            size: 0,
            mtime: 0,
        })
    }

    /// Send each source into the folder the pane of `to` shows, or ask first
    /// when its name is taken there. All sources come from one place.
    /// Whether the failed transfer `id` can start again: both ends still
    /// show the places it was between.
    pub(crate) fn can_retry(&self, id: u64) -> bool {
        self.failed_transfers.get(&id).is_some_and(|transfer| {
            let pane_id = |endpoint| self.endpoint_pane(endpoint).map(Entity::entity_id);
            transfer.panes == (pane_id(transfer.from), pane_id(transfer.to))
        })
    }

    /// Start a failed transfer again, to the same target name.
    pub(crate) fn retry_transfer(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.can_retry(id) {
            return;
        }
        let Some(transfer) = self.failed_transfers.remove(&id) else {
            return;
        };
        tracing::info!(id, name = transfer.name, "retrying a failed transfer");
        let source = Source::at(transfer.from, &transfer.source_path);
        self.start_transfer(
            source,
            transfer.to,
            &transfer.target_dir,
            &transfer.name,
            cx,
        );
    }

    fn copy_entries(&mut self, pane: gpui::EntityId, items: &[EntryRef], cx: &mut Context<Self>) {
        let status = match items {
            [item] => format!(
                "Copied {}",
                item.path.rsplit(['/', '\\']).next().unwrap_or("")
            ),
            _ => format!("Copied {} items", items.len()),
        };
        self.copied = Some((pane, items.to_vec()));
        cx.emit(BrowserEvent::StatusChanged(status));
    }

    /// Put what was copied into the folder the pane of `to` shows: a copy
    /// within the same pane, or a transfer from the other one.
    fn paste(&mut self, to: Endpoint, cx: &mut Context<Self>) {
        let Some((source_pane, items)) = self.copied.clone() else {
            cx.emit(BrowserEvent::StatusChanged(
                "Nothing to paste. Copy files first.".to_string(),
            ));
            return;
        };
        let Some(pane) = self.endpoint_pane(to).cloned() else {
            return;
        };
        if pane.entity_id() == source_pane {
            let pairs = paste_targets(&items, &pane.read(cx).path, |name| {
                pane.read(cx).entries.iter().any(|e| e.name == name)
            });
            self.copy_within(to, pairs, cx);
            return;
        }
        let from = self
            .opposite(to)
            .filter(|from| self.endpoint_pane(*from).map(Entity::entity_id) == Some(source_pane));
        let Some(from) = from else {
            cx.emit(BrowserEvent::StatusChanged(
                "The copied files are no longer shown here.".to_string(),
            ));
            return;
        };
        let sources = items
            .iter()
            .map(|item| Source::at(from, &item.path))
            .collect();
        self.request_transfers(sources, to, cx);
    }

    /// Copy each item to its path in the same place. On a server a file goes
    /// there and back through this computer: SFTP has no copy.
    fn copy_within(
        &mut self,
        at: Endpoint,
        pairs: Vec<(EntryRef, String)>,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(count = pairs.len(), ?at, "copying within one place");
        let Endpoint::Server(side) = at else {
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { copy_local_items(&pairs) })
                    .await;
                this.update(cx, |browser, cx| {
                    let result = result.map_err(std::io::Error::other);
                    browser.local_fs_op(result, "Copied", cx);
                })
                .ok();
            })
            .detach();
            return;
        };
        let Some(sftp) = self.primary(side) else {
            return;
        };
        let op = async move {
            let total = pairs.len();
            let mut errors = Vec::new();
            for (item, to) in pairs {
                if item.is_dir {
                    errors.push(format!(
                        "{}: folders can't be copied on the server",
                        item.path
                    ));
                    continue;
                }
                let local = scratch_file();
                let result = match sftp.download_file(item.path.clone(), local.clone()).await {
                    Ok(_) => sftp.upload_file(local.clone(), to).await,
                    Err(e) => Err(e),
                };
                let _ = std::fs::remove_file(&local);
                if let Err(e) = result {
                    tracing::warn!(path = %item.path, ?e, "copy on the server failed");
                    errors.push(format!("{}: {e}", item.path));
                }
            }
            batch_result("copy", total, errors).map_err(anyhow::Error::msg)
        };
        self.remote_fs_op(side, op, "Copied", cx);
    }

    fn request_transfers(&mut self, sources: Vec<Source>, to: Endpoint, cx: &mut Context<Self>) {
        let Some(first) = sources.first() else {
            return;
        };
        // This browser is still on screen, and takes keys and drops, while
        // the session connects again.
        if self.is_session_down && is_end_of_transfer(Side::Right, first.endpoint(), to) {
            cx.emit(BrowserEvent::StatusChanged(
                SESSION_NOT_CONNECTED.to_string(),
            ));
            return;
        }
        let (target_dir, listed) = match self.target_listing(to, cx) {
            Ok(listing) => listing,
            Err(refusal) => {
                cx.emit(BrowserEvent::StatusChanged(refusal));
                return;
            }
        };
        let mut running = running_target_names(self.transfers.values(), to, &target_dir);
        for source in sources {
            let Some(name) = source.name() else { continue };
            // A server may name a file `..\x` or `C:\x`. Joined to the target
            // folder on this computer, such a name would leave that folder.
            if to == Endpoint::Local && !crate::ssh::is_single_local_component(&name) {
                tracing::warn!(
                    name,
                    "download refused, the name is not a plain file name here"
                );
                cx.emit(BrowserEvent::Error(unsafe_name_message(&name)));
                continue;
            }
            let existing = match to {
                Endpoint::Local => local_facts(&PathBuf::from(&target_dir).join(&name)),
                Endpoint::Server(_) => listed.get(&name).copied(),
            };
            // A transfer still on its way there counts as an item of that name.
            let existing = existing.or_else(|| {
                running
                    .contains(&name)
                    .then(|| self.source_facts(&source, cx))
            });
            let Some(existing) = existing else {
                self.start_transfer(source, to, &target_dir, &name, cx);
                running.insert(name);
                continue;
            };
            let conflict = TransferConflict {
                id: 0,
                name,
                from: source.endpoint(),
                to,
                target_label: self.endpoint_label(to),
                source_path: source.path(),
                target_dir: target_dir.clone(),
                source: self.source_facts(&source, cx),
                existing,
            };
            self.queue_transfer_conflict(conflict, cx);
        }
    }

    /// The servers a task has to name. `None` when the session's own server
    /// says it all: a tab only has to tell apart what goes to the left one.
    fn transfer_route(&self, from: Endpoint, to: Endpoint) -> Option<String> {
        let left = Endpoint::Server(Side::Left);
        match (from, to) {
            (Endpoint::Server(_), Endpoint::Server(_)) => Some(copy_route(
                &self.endpoint_label(from),
                &self.endpoint_label(to),
            )),
            _ if from == left || to == left => Some(self.endpoint_label(left)),
            _ => None,
        }
    }

    fn primary(&self, side: Side) -> Option<SftpHandle> {
        self.side(side).map(|remote| remote.primary.clone())
    }

    fn start_transfer(
        &mut self,
        source: Source,
        to: Endpoint,
        target_dir: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        // The left server may have gone while a name conflict was being asked about.
        let not_connected = BrowserEvent::StatusChanged(LEFT_NOT_CONNECTED.to_string());
        let from = source.endpoint();
        let source_path = source.path();
        match (source, to) {
            (Source::Local(local), Endpoint::Server(side)) => {
                let Some(sftp) = self.primary(side) else {
                    return cx.emit(not_connected);
                };
                let remote = format!("{}/{}", target_dir.trim_end_matches('/'), name);
                tracing::info!(?local, remote, ?side, "queuing upload");
                let route = self.transfer_route(from, to);
                let (id, cancel) = self.begin_transfer(
                    target_dir,
                    name,
                    TransferKind::Upload,
                    (from, &source_path),
                    to,
                    route,
                    cx,
                );
                sftp.upload(local, remote, id, cancel);
            }
            (Source::Server(side, remote), Endpoint::Local) => {
                let Some(sftp) = self.primary(side) else {
                    return cx.emit(not_connected);
                };
                // A Unix name can be a whole path on Windows, and joining
                // it would write outside the chosen folder.
                if !crate::ssh::is_single_local_component(name) {
                    tracing::warn!(remote, name, "download refused: the name is a path here");
                    return cx.emit(BrowserEvent::Error(unsafe_name_message(name)));
                }
                let local = PathBuf::from(target_dir).join(name);
                tracing::info!(remote, ?local, ?side, "queuing download");
                let route = self.transfer_route(from, to);
                let (id, cancel) = self.begin_transfer(
                    target_dir,
                    name,
                    TransferKind::Download,
                    (from, &source_path),
                    to,
                    route,
                    cx,
                );
                sftp.download(remote, local, id, cancel);
            }
            (Source::Server(from_side, _), Endpoint::Server(to_side)) => {
                let (Some(source), Some(target)) = (self.primary(from_side), self.primary(to_side))
                else {
                    return cx.emit(not_connected);
                };
                let remote = format!("{}/{}", target_dir.trim_end_matches('/'), name);
                let route = self.transfer_route(from, to);
                tracing::info!(source_path, remote, ?route, "queuing copy between servers");
                let (id, cancel) = self.begin_transfer(
                    target_dir,
                    name,
                    TransferKind::Copy,
                    (from, &source_path),
                    to,
                    route,
                    cx,
                );
                // The target's worker pulls the files and reports the progress.
                target.relay(source, source_path.clone(), remote, id, cancel);
            }
            // Both panes never show this computer.
            (Source::Local(_), Endpoint::Local) => {}
        }
    }

    fn queue_transfer_conflict(&mut self, mut conflict: TransferConflict, cx: &mut Context<Self>) {
        self.next_conflict_id += 1;
        conflict.id = self.next_conflict_id;
        if self.transfer_conflicts.is_empty() {
            self.is_conflict_choice_for_all = false;
        }
        self.transfer_conflicts.push_back(conflict);
        // Announced every time: the app opens nothing while a dialog is up, and
        // a dialog that went away without an answer must not block the queue.
        cx.emit(BrowserEvent::TransferConflict);
    }

    /// The dialog was closed without an answer: skip that one conflict and ask
    /// about the others, whatever the "for all" box says.
    pub fn dismiss_transfer_conflict(&mut self, id: u64, cx: &mut Context<Self>) {
        self.is_conflict_choice_for_all = false;
        self.resolve_transfer_conflict(id, ConflictChoice::Skip, cx);
    }

    /// The conflict the user has to answer next.
    pub fn pending_transfer_conflict(&self) -> Option<&TransferConflict> {
        self.transfer_conflicts.front()
    }

    pub fn transfer_conflict_count(&self) -> usize {
        self.transfer_conflicts.len()
    }

    /// Apply the user's answer to conflict `id`, and to every other waiting
    /// conflict when the "for all" box is ticked. Does nothing when `id` is
    /// no longer the one being asked about.
    pub fn resolve_transfer_conflict(
        &mut self,
        id: u64,
        choice: ConflictChoice,
        cx: &mut Context<Self>,
    ) {
        if self.pending_transfer_conflict().map(|c| c.id) != Some(id) {
            return;
        }
        let count = if self.is_conflict_choice_for_all {
            self.transfer_conflicts.len()
        } else {
            1
        };
        let resolved: Vec<TransferConflict> = self.transfer_conflicts.drain(..count).collect();
        let mut taken = TakenNames::new();
        for conflict in resolved {
            self.apply_conflict_choice(conflict, choice, &mut taken, cx);
        }
        self.is_conflict_choice_for_all = false;
        if !self.transfer_conflicts.is_empty() {
            cx.emit(BrowserEvent::TransferConflict);
        }
    }

    fn apply_conflict_choice(
        &mut self,
        conflict: TransferConflict,
        choice: ConflictChoice,
        taken: &mut TakenNames,
        cx: &mut Context<Self>,
    ) {
        let name = match choice {
            ConflictChoice::Overwrite if conflict.can_overwrite() => conflict.name.clone(),
            ConflictChoice::KeepBoth => self.free_target_name(&conflict, taken, cx),
            // Also an "overwrite all" that reaches a file-versus-folder pair.
            ConflictChoice::Overwrite | ConflictChoice::Skip => {
                tracing::info!(name = %conflict.name, "transfer skipped, name is taken");
                cx.emit(BrowserEvent::StatusChanged(format!(
                    "Skipped: {}",
                    conflict.name
                )));
                return;
            }
        };
        let source = Source::at(conflict.from, &conflict.source_path);
        self.start_transfer(source, conflict.to, &conflict.target_dir, &name, cx);
    }

    /// A name for "keep both" that nothing in the target folder uses. The
    /// names taken per folder are built once per answer, not once per conflict.
    fn free_target_name(
        &self,
        conflict: &TransferConflict,
        taken: &mut TakenNames,
        cx: &App,
    ) -> String {
        let is_folder = conflict.source.is_folder;
        let key = (conflict.to, PathBuf::from(&conflict.target_dir));
        if !taken.contains_key(&key) {
            let Some(names) = self.target_names(conflict, cx) else {
                // The server is gone; the transfer then says so and nothing is sent.
                return conflict.name.clone();
            };
            taken.insert(key.clone(), names);
        }
        let names = taken.entry(key).or_default();
        let dir = PathBuf::from(&conflict.target_dir);
        let is_local = conflict.to == Endpoint::Local;
        let name = free_name(&conflict.name, is_folder, |name| {
            names.contains(name) || (is_local && dir.join(name).symlink_metadata().is_ok())
        });
        names.insert(name.clone());
        name
    }

    /// The names running transfers write into the conflict's folder, plus the
    /// folder's listing when it is on a server. None when that server is gone.
    fn target_names(&self, conflict: &TransferConflict, cx: &App) -> Option<HashSet<String>> {
        let mut names =
            running_target_names(self.transfers.values(), conflict.to, &conflict.target_dir);
        let Endpoint::Server(side) = conflict.to else {
            return Some(names);
        };
        let target = self.side(side)?;
        // The pane may have moved on since the question was asked.
        let pane = target.pane.read(cx);
        let entries: &[FileEntry] = if is_same_dir(&pane.path, &conflict.target_dir) {
            &pane.entries
        } else {
            target
                .cache_get(&conflict.target_dir)
                .map(Vec::as_slice)
                .unwrap_or_default()
        };
        names.extend(entries.iter().map(|e| e.name.clone()));
        Some(names)
    }

    /// Make sure a local copy of `remote` exists for an outbound drag.
    ///
    /// Called whenever the user shows interest in a file: hovering it, picking
    /// it, or starting to drag it. The pane owns the answer to "is there a copy
    /// already", so all this does is decide whether to fetch one.
    fn warm_dragout(&mut self, remote: String, cx: &mut Context<Self>) {
        let pane = self.right.pane.read(cx);
        if pane.has_fresh_staged(&remote) {
            return;
        }
        let is_staging = self
            .dragout_task
            .as_ref()
            .is_some_and(|task| !task.is_ready())
            && self.dragout_staging.as_ref().map(|(path, _)| path) == Some(&remote);
        if is_staging {
            return;
        }
        let Some(entry) = pane.entry_at(&remote) else {
            return;
        };
        if crate::ui::file_pane::dragout_refusal(entry).is_some() {
            return;
        }
        let (size, mtime) = (entry.size, entry.mtime);
        self.start_staging(remote, size, mtime, cx);
    }

    /// Where `remote` will be staged, with its parent directories in place.
    fn staging_target(&self, remote: &str) -> Option<PathBuf> {
        let staged = dragout_path(&self.right.connection.id, std::path::Path::new(remote))
            .inspect_err(
                |e| tracing::warn!(target: "flint::dragout", remote, "bad staging path: {e}"),
            )
            .ok()?;
        if let Some(parent) = staged.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(target: "flint::dragout", ?e, "failed to create staging dirs");
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        Some(staged)
    }

    /// Copy the small files of a freshly listed directory in one batch, so
    /// dragging one out lands instantly instead of waiting on a round trip.
    ///
    /// Smallest first and stopped at [`DRAGOUT_PREFETCH_BUDGET`]: these copies
    /// are made for files that may never be dragged anywhere.
    fn prefetch_dragout(&mut self, parent: &str, entries: &[FileEntry], cx: &mut Context<Self>) {
        let Some(prefetch) = self.right.prefetch.clone() else {
            return;
        };
        if self.right.pane.read(cx).path != parent {
            return;
        }

        let mut candidates: Vec<&FileEntry> = entries
            .iter()
            .filter(|e| !e.opens_as_folder() && e.size <= DRAGOUT_PREFETCH_MAX_FILE)
            .collect();
        candidates.sort_by_key(|e| e.size);

        let mut budget = DRAGOUT_PREFETCH_BUDGET;
        let mut planned = Vec::new();
        // Files staged before count too, so a revisit checks no more files
        // than a first visit.
        for entry in candidates.into_iter().take(DRAGOUT_PREFETCH_MAX_FILES) {
            let cost = entry.size.max(DRAGOUT_PREFETCH_MIN_COST);
            if cost > budget {
                break;
            }
            budget -= cost;
            let remote = format!("{}/{}", parent.trim_end_matches('/'), entry.name);
            if self.right.pane.read(cx).has_fresh_staged(&remote) {
                continue;
            }
            let Some(staged) = self.staging_target(&remote) else {
                continue;
            };
            planned.push((remote, staged, entry.size, entry.mtime));
        }
        if planned.is_empty() {
            return;
        }

        let items: Vec<StageItem> = planned
            .iter()
            .map(|(remote, staged, size, _)| StageItem {
                remote: remote.clone(),
                local: staging_part_path(staged),
                size: *size,
            })
            .collect();
        let remote_pane = self.right.pane.clone();
        let started = std::time::Instant::now();
        tracing::debug!(target: "flint::dragout", parent, files = planned.len(), "prefetching directory");

        self.dragout_batch_task = Some(cx.spawn(async move |_, cx| {
            let done: HashSet<String> = prefetch.stage_files(items).await.into_iter().collect();
            let staged: Vec<_> = planned
                .into_iter()
                .filter(|(remote, ..)| done.contains(remote))
                .filter(|(_, staged, ..)| {
                    std::fs::rename(staging_part_path(staged), staged).is_ok()
                })
                .collect();
            tracing::debug!(
                target: "flint::dragout",
                files = staged.len(),
                ms = started.elapsed().as_millis(),
                "directory prefetched",
            );
            remote_pane.update(cx, |pane, _| {
                for (remote, local, size, mtime) in staged {
                    pane.set_staged_dragout(remote, StagedFile { local, size, mtime });
                }
            });
        }));
    }

    /// Copy `remote` into the staging area. The copy goes over the prefetch
    /// channel so it never queues ahead of a transfer the user asked for, and
    /// lands under a temporary name first: a staging fetch writes in place, so
    /// a half-fetched file at the final path would look finished to a drag.
    fn start_staging(&mut self, remote: String, size: u64, mtime: u32, cx: &mut Context<Self>) {
        let Some(staged) = self.staging_target(&remote) else {
            return;
        };
        // Its own part name: a canceled copy of the same file removes only its own.
        let attempt = NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed);
        let part = staged.with_file_name(format!(
            "{}.flint-part-{attempt}",
            staged.file_name().unwrap_or_default().to_string_lossy()
        ));
        let sftp = self
            .right
            .prefetch
            .clone()
            .unwrap_or_else(|| self.right.primary.clone());
        let remote_pane = self.right.pane.clone();
        let started = std::time::Instant::now();
        tracing::debug!(target: "flint::dragout", remote, size, "staging for drag-out");

        let cancel = CancelFlag::default();
        if let Some((_, old)) = self
            .dragout_staging
            .replace((remote.clone(), cancel.clone()))
        {
            old.store(true, Ordering::Relaxed);
        }
        self.dragout_task = Some(cx.spawn(async move |_, cx| {
            // The worker removes the part file of a copy that failed or was canceled.
            if let Err(e) = sftp
                .stage_file(remote.clone(), part.clone(), size, cancel)
                .await
            {
                tracing::debug!(target: "flint::dragout", remote, "staging stopped: {e}");
                return;
            }
            if let Err(e) = std::fs::rename(&part, &staged) {
                tracing::warn!(target: "flint::dragout", ?e, "failed to publish staged file");
                let _ = std::fs::remove_file(&part);
                return;
            }
            tracing::debug!(
                target: "flint::dragout",
                remote,
                ms = started.elapsed().as_millis(),
                "staged",
            );
            remote_pane.update(cx, |pane, _| {
                pane.set_staged_dragout(
                    remote,
                    StagedFile {
                        local: staged,
                        size,
                        mtime,
                    },
                )
            });
        }));
    }

    /// Upload files dropped from the OS to the server that side shows.
    /// Files dropped on the session's terminal go to the folder its file
    /// pane shows. Returns that folder, or `None` when nothing was sent.
    pub(crate) fn upload_from_terminal(
        &mut self,
        paths: &[PathBuf],
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let pane = self.right.pane.read(cx);
        if self.is_session_down || !pane.is_listed() {
            cx.emit(BrowserEvent::StatusChanged(
                SESSION_NOT_CONNECTED.to_string(),
            ));
            return None;
        }
        let dir = pane.path.clone();
        self.upload_paths(Side::Right, paths, cx);
        Some(dir)
    }

    fn upload_paths(&mut self, side: Side, paths: &[PathBuf], cx: &mut Context<Self>) {
        let sources = paths
            .iter()
            .map(|path| Source::Local(path.clone()))
            .collect();
        self.request_transfers(sources, Endpoint::Server(side), cx);
    }

    fn is_too_large_to_open(&self, remote: &str, cx: &App) -> bool {
        self.right
            .pane
            .read(cx)
            .entry_at(remote)
            .is_some_and(|e| e.size > crate::ui::file_pane::DRAGOUT_MAX_BYTES)
    }

    fn open_remote_file(&mut self, remote: String, cx: &mut Context<Self>) {
        let filename = remote_file_name(&remote);
        let local = match sftp_cache_path(&self.right.connection.id, std::path::Path::new(&remote))
        {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(target: "flint::file_edit", remote, "bad cache path: {e}");
                cx.emit(BrowserEvent::Error(format!(
                    "Could not open {filename}: {e}"
                )));
                return;
            }
        };

        if self.is_too_large_to_open(&remote, cx) {
            cx.emit(BrowserEvent::StatusChanged(TOO_LARGE_TO_OPEN.to_string()));
            return;
        }

        // Creating the parent dirs takes milliseconds, so it runs on the GPUI thread.
        if let Some(parent) = local.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(target: "flint::file_edit", ?e, "failed to create cache dirs");
                cx.emit(BrowserEvent::Error(format!(
                    "Could not open {filename}: {e}"
                )));
                return;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }

        let remote_path = PathBuf::from(&remote);
        if self.in_flight_downloads.contains(&remote_path) {
            cx.emit(BrowserEvent::StatusChanged(
                "Already downloading…".to_string(),
            ));
            return;
        }

        // The file is already cached: open it without downloading again. Its
        // remote mtime is unknown, so conflict detection is conservative.
        if local.exists() {
            tracing::debug!(target: "flint::file_edit", ?local, "re-opening cached file");
            let open_result = {
                let associations = self.file_associations.borrow();
                crate::file_open::open_with_association_or_default(&local, &associations)
            };
            match open_result {
                Ok(()) => {
                    cx.emit(BrowserEvent::StatusChanged("Opened".to_string()));
                    self.start_watch_session(remote, local, None, cx);
                }
                Err(OpenError::NoHandler) => {
                    if let Some(key) = crate::file_open::association_key(&local) {
                        self.start_watch_session(remote, local.clone(), None, cx);
                        cx.emit(BrowserEvent::RequestPickerForFile {
                            extension: key,
                            local,
                        });
                    } else {
                        cx.emit(BrowserEvent::Error(NO_APP_FOR_FILE.to_string()));
                    }
                }
                Err(e) => {
                    cx.emit(BrowserEvent::Error(format!(
                        "Could not open {filename}: {e}"
                    )));
                }
            }
            return;
        }

        self.in_flight_downloads.insert(remote_path.clone());
        cx.emit(BrowserEvent::StatusChanged(format!(
            "Downloading {filename}…"
        )));

        let sftp = self.right.primary.clone();

        // detach: download must complete even if the user navigates away before it finishes
        cx.spawn(async move |this, cx| {
            let result = download_to_cache(&sftp, remote.clone(), &local).await;

            this.update(cx, |browser, cx| {
                browser.in_flight_downloads.remove(&remote_path);
                match result {
                    Ok(DownloadResult { mtime }) => {
                        tracing::debug!(target: "flint::file_edit", ?local, "download complete, opening");
                        let open_result = {
                            let associations = browser.file_associations.borrow();
                            crate::file_open::open_with_association_or_default(&local, &associations)
                        };
                        match open_result {
                            Ok(()) => {
                                cx.emit(BrowserEvent::StatusChanged("Opened".to_string()));
                                browser.start_watch_session(remote, local, mtime, cx);
                            }
                            Err(OpenError::NoHandler) => {
                                if let Some(key) = crate::file_open::association_key(&local) {
                                    browser.start_watch_session(remote, local.clone(), mtime, cx);
                                    cx.emit(BrowserEvent::RequestPickerForFile { extension: key, local });
                                } else {
                                    browser.discard_untracked_download(&local);
                                    cx.emit(BrowserEvent::Error(NO_APP_FOR_FILE.to_string()));
                                }
                            }
                            Err(e) => {
                                browser.discard_untracked_download(&local);
                                cx.emit(BrowserEvent::Error(format!("Could not open {filename}: {e}")));
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "flint::file_edit", ?e, "file download failed");
                        cx.emit(BrowserEvent::Error(format!("Could not download {filename}: {e}")));
                    }
                }
            }).ok();
        }).detach();
    }

    fn open_remote_file_for_picker(&mut self, remote: String, cx: &mut Context<Self>) {
        let filename = remote_file_name(&remote);
        let local = match sftp_cache_path(&self.right.connection.id, std::path::Path::new(&remote))
        {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(target: "flint::file_edit", remote, "bad cache path: {e}");
                cx.emit(BrowserEvent::Error(format!(
                    "Could not open {filename}: {e}"
                )));
                return;
            }
        };

        if self.is_too_large_to_open(&remote, cx) {
            cx.emit(BrowserEvent::StatusChanged(TOO_LARGE_TO_OPEN.to_string()));
            return;
        }

        if let Some(parent) = local.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(target: "flint::file_edit", ?e, "failed to create cache dirs");
                cx.emit(BrowserEvent::Error(format!(
                    "Could not open {filename}: {e}"
                )));
                return;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }

        let Some(extension) = crate::file_open::association_key(&local) else {
            cx.emit(BrowserEvent::Error(
                "Cannot determine file type".to_string(),
            ));
            return;
        };

        let remote_path = PathBuf::from(&remote);
        if self.in_flight_downloads.contains(&remote_path) {
            cx.emit(BrowserEvent::StatusChanged(
                "Already downloading…".to_string(),
            ));
            return;
        }

        if local.exists() {
            self.start_watch_session(remote, local.clone(), None, cx);
            cx.emit(BrowserEvent::RequestPickerForFile { extension, local });
            return;
        }

        self.in_flight_downloads.insert(remote_path.clone());
        cx.emit(BrowserEvent::StatusChanged(format!(
            "Downloading {filename}…"
        )));

        let sftp = self.right.primary.clone();

        cx.spawn(async move |this, cx| {
            let result = download_to_cache(&sftp, remote.clone(), &local).await;
            this.update(cx, |browser, cx| {
                browser.in_flight_downloads.remove(&remote_path);
                match result {
                    Ok(DownloadResult { mtime }) => {
                        browser.start_watch_session(remote, local.clone(), mtime, cx);
                        cx.emit(BrowserEvent::RequestPickerForFile { extension, local });
                    }
                    Err(e) => {
                        tracing::warn!(target: "flint::file_edit", ?e, "file download failed");
                        cx.emit(BrowserEvent::Error(format!(
                            "Could not download {filename}: {e}"
                        )));
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Remove a fresh download nothing tracks, or a later open would serve it stale.
    fn discard_untracked_download(&self, local: &std::path::Path) {
        if std::fs::remove_file(local).is_ok() {
            if let Some(root) = flint_cache_root("sftp", &self.right.connection.id) {
                remove_empty_parents(local, &root);
            }
        }
    }

    fn start_watch_session(
        &mut self,
        remote: String,
        local: PathBuf,
        mtime: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        if self.open_files.contains_key(&local) {
            return;
        }
        let filename = remote_file_name(&remote);
        match FileEditSession::new(remote, local.clone(), mtime, cx) {
            Ok(mut session) => {
                // A known remote mtime means the file was downloaded just
                // now, so it starts in sync. Without this, a file nobody
                // edited would count as unsaved on disconnect.
                if mtime.is_some() {
                    session.last_synced_local_mtime = std::fs::metadata(&local)
                        .and_then(|meta| meta.modified())
                        .ok();
                }
                self.open_files.insert(local, session);
            }
            Err(e) => {
                tracing::warn!(target: "flint::file_edit", ?e, "failed to start file watcher");
                cx.emit(BrowserEvent::Error(format!(
                    "Could not watch {filename} for changes: {e}"
                )));
            }
        }
    }

    pub(crate) fn on_file_changed(&mut self, local: PathBuf, cx: &mut Context<Self>) {
        let Some(session) = self.open_files.get_mut(&local) else {
            return;
        };

        match session.sync_state {
            // A reload that runs longer than the watcher's quiet period: the
            // change is the download writing the file.
            SyncState::Downloading => return,
            // Asked again: the dialog may have been lost to a second conflict.
            SyncState::Conflict => {
                cx.emit(BrowserEvent::ConflictDetected { local });
                return;
            }
            SyncState::Uploading => {
                session.pending_resync = true;
                return;
            }
            _ => {}
        }
        // A reload from the host leaves the file as it was last synced.
        // Read before the upload: a save while it runs must count as unsaved.
        let uploaded_mtime = local_mtime(&local);
        if !has_unsaved_changes(uploaded_mtime, session.last_synced_local_mtime) {
            return;
        }

        let remote = session.remote.clone();
        let saved_mtime = session.original_remote_mtime;
        session.sync_state = SyncState::Uploading;
        session.pending_resync = false;
        self.refresh_sync_states(cx);

        tracing::debug!(target: "flint::file_edit", local = %local.display(), remote, "queuing upload");

        let sftp = self.right.primary.clone();
        cx.spawn(async move |this, cx| {
            let current_mtime = match sftp.stat_file(remote.clone()).await {
                Ok(mtime) => mtime,
                Err(e) => {
                    tracing::warn!(target: "flint::file_edit", ?e, "stat failed before upload");
                    this.update(cx, |browser, cx| {
                        if let Some(session) = browser.open_files.get_mut(&local) {
                            session.sync_state = SyncState::Error;
                        }
                        browser.refresh_sync_states(cx);
                        cx.emit(BrowserEvent::Error(format!("Sync failed: {e}")));
                    })
                    .ok();
                    return;
                }
            };

            if is_remote_edited(saved_mtime, current_mtime) {
                tracing::warn!(
                    target: "flint::file_edit",
                    remote,
                    ?saved_mtime,
                    ?current_mtime,
                    "remote mtime changed, conflict",
                );
                this.update(cx, |browser, cx| {
                    if let Some(session) = browser.open_files.get_mut(&local) {
                        session.sync_state = SyncState::Conflict;
                    }
                    browser.refresh_sync_states(cx);
                    cx.emit(BrowserEvent::ConflictDetected {
                        local: local.clone(),
                    });
                })
                .ok();
                return;
            }

            match sftp.upload_file(local.clone(), remote.clone()).await {
                Ok(()) => {
                    tracing::info!(target: "flint::file_edit", remote, "upload complete");
                    let new_mtime = sftp.stat_file(remote).await.ok().flatten();
                    this.update(cx, |browser, cx| {
                        let is_pending = browser.mark_uploaded(&local, new_mtime, uploaded_mtime);
                        browser.refresh_sync_states(cx);
                        if is_pending {
                            browser.on_file_changed(local, cx);
                        }
                    })
                    .ok();
                }
                Err(e) => {
                    tracing::warn!(target: "flint::file_edit", ?e, remote, "upload failed");
                    this.update(cx, |browser, cx| {
                        let is_pending = browser.mark_upload_failed(&local);
                        browser.refresh_sync_states(cx);
                        cx.emit(BrowserEvent::Error(format!("Sync failed: {e}")));
                        if is_pending {
                            browser.on_file_changed(local, cx);
                        }
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    pub fn resolve_conflict_overwrite(&mut self, local: PathBuf, cx: &mut Context<Self>) {
        let Some(session) = self.open_files.get_mut(&local) else {
            return;
        };
        let remote = session.remote.clone();
        session.sync_state = SyncState::Uploading;
        self.refresh_sync_states(cx);
        let uploaded_mtime = local_mtime(&local);
        let sftp = self.right.primary.clone();
        cx.spawn(async move |this, cx| {
            match sftp.upload_file(local.clone(), remote.clone()).await {
                Ok(()) => {
                    let new_mtime = sftp.stat_file(remote).await.ok().flatten();
                    this.update(cx, |browser, cx| {
                        let is_pending = browser.mark_uploaded(&local, new_mtime, uploaded_mtime);
                        browser.refresh_sync_states(cx);
                        cx.emit(BrowserEvent::StatusChanged(
                            "Uploaded your version".to_string(),
                        ));
                        if is_pending {
                            browser.on_file_changed(local, cx);
                        }
                    })
                    .ok();
                }
                Err(e) => {
                    tracing::warn!(target: "flint::file_edit", ?e, "overwrite upload failed");
                    this.update(cx, |browser, cx| {
                        let is_pending = browser.mark_upload_failed(&local);
                        browser.refresh_sync_states(cx);
                        cx.emit(BrowserEvent::Error(format!("Upload failed: {e}")));
                        if is_pending {
                            browser.on_file_changed(local, cx);
                        }
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    pub fn resolve_conflict_reload(&mut self, local: PathBuf, cx: &mut Context<Self>) {
        let Some(session) = self.open_files.get_mut(&local) else {
            return;
        };
        let remote = session.remote.clone();
        session.sync_state = SyncState::Downloading;
        self.refresh_sync_states(cx);
        let sftp = self.right.primary.clone();
        cx.spawn(async move |this, cx| {
            match download_to_cache(&sftp, remote.clone(), &local).await {
                Ok(DownloadResult { mtime }) => {
                    this.update(cx, |browser, cx| {
                        if let Some(session) = browser.open_files.get_mut(&local) {
                            session.original_remote_mtime = mtime;
                            session.last_synced_local_mtime = std::fs::metadata(&local)
                                .and_then(|meta| meta.modified())
                                .ok();
                            session.sync_state = SyncState::Synced {
                                at: std::time::SystemTime::now(),
                            };
                        }
                        browser.refresh_sync_states(cx);
                        cx.emit(BrowserEvent::StatusChanged(
                            "Reloaded from host".to_string(),
                        ));
                        match crate::file_open::open_with_association_or_default(
                            &local,
                            &browser.file_associations.borrow(),
                        ) {
                            Ok(()) => {}
                            Err(OpenError::NoHandler) => {
                                if let Some(key) = crate::file_open::association_key(&local) {
                                    cx.emit(BrowserEvent::RequestPickerForFile {
                                        extension: key,
                                        local: local.clone(),
                                    });
                                } else {
                                    cx.emit(BrowserEvent::Error(NO_APP_FOR_FILE.to_string()));
                                }
                            }
                            Err(e) => cx.emit(BrowserEvent::Error(format!(
                                "Reloaded, but could not open the file: {e}"
                            ))),
                        }
                    })
                    .ok();
                }
                Err(e) => {
                    tracing::warn!(target: "flint::file_edit", ?e, remote, "reload failed");
                    this.update(cx, |browser, cx| {
                        if let Some(session) = browser.open_files.get_mut(&local) {
                            session.sync_state = SyncState::Error;
                        }
                        browser.refresh_sync_states(cx);
                        cx.emit(BrowserEvent::Error(format!("Reload failed: {e}")));
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    pub fn resolve_conflict_cancel(&mut self, local: &std::path::Path, cx: &mut Context<Self>) {
        if let Some(session) = self.open_files.get_mut(local) {
            session.sync_state = SyncState::Idle;
        }
        self.refresh_sync_states(cx);
    }

    /// An upload of `local` ended well: record what is on the server now.
    /// `local_mtime` is the file's mtime read before the upload started.
    /// Returns whether the file was saved again while the upload ran, which
    /// the caller answers with another upload.
    fn mark_uploaded(
        &mut self,
        local: &std::path::Path,
        remote_mtime: Option<u32>,
        local_mtime: Option<std::time::SystemTime>,
    ) -> bool {
        let Some(session) = self.open_files.get_mut(local) else {
            return false;
        };
        session.original_remote_mtime = remote_mtime;
        session.last_synced_local_mtime = local_mtime;
        session.sync_state = SyncState::Synced {
            at: std::time::SystemTime::now(),
        };
        std::mem::take(&mut session.pending_resync)
    }

    /// The failed counterpart of [`Self::mark_uploaded`].
    fn mark_upload_failed(&mut self, local: &std::path::Path) -> bool {
        let Some(session) = self.open_files.get_mut(local) else {
            return false;
        };
        session.sync_state = SyncState::Error;
        std::mem::take(&mut session.pending_resync)
    }

    /// Give up the files open for editing to the browser that replaces this one.
    pub fn take_open_files(&mut self) -> HashMap<PathBuf, FileEditSession> {
        std::mem::take(&mut self.open_files)
    }

    /// Watch the files the replaced browser had open, and upload what was
    /// saved while the connection was down.
    pub fn adopt_open_files(
        &mut self,
        files: HashMap<PathBuf, FileEditSession>,
        cx: &mut Context<Self>,
    ) {
        for (local, old) in files {
            // A file reopened from the cache was never synced, so None alone is no edit.
            let is_unsent = matches!(old.sync_state, SyncState::Error | SyncState::Uploading)
                || old.pending_resync
                || (old.last_synced_local_mtime.is_some()
                    && has_unsaved_changes(local_mtime(&local), old.last_synced_local_mtime));
            match FileEditSession::new(old.remote, local.clone(), old.original_remote_mtime, cx) {
                Ok(mut session) => {
                    session.last_synced_local_mtime = old.last_synced_local_mtime;
                    // An upload or reload in flight ended with the old connection.
                    if matches!(old.sync_state, SyncState::Conflict) {
                        session.sync_state = SyncState::Conflict;
                    }
                    self.open_files.insert(local.clone(), session);
                    if is_unsent {
                        self.on_file_changed(local, cx);
                    }
                }
                // The file stays in the cache: only tracked files are ever removed.
                Err(e) => {
                    tracing::warn!(target: "flint::file_edit", ?e, local = %local.display(), "failed to watch file again after reconnect");
                }
            }
        }
        self.refresh_sync_states(cx);
    }

    pub fn cleanup_on_disconnect(&mut self) -> PreservedFiles {
        // Without a cache folder nothing was ever written, see `flint_cache_path`.
        let cache_root = flint_cache_root("sftp", &self.right.connection.id);

        let mut preserved_count = 0usize;

        // Only files known to be synced go. The cache may hold edits this
        // browser does not track, such as those kept from an earlier session.
        for (local, session) in std::mem::take(&mut self.open_files) {
            if has_unsaved_changes(local_mtime(&local), session.last_synced_local_mtime) {
                preserved_count += 1;
            } else if std::fs::remove_file(&local).is_ok() {
                if let Some(root) = &cache_root {
                    remove_empty_parents(&local, root);
                }
            }
            // The session drops here, and its watcher and bridge task with it.
        }

        // Staged drag-out copies are throwaway, so they go regardless.
        if let Some(staged) = flint_cache_root("dragout", &self.right.connection.id) {
            let _ = std::fs::remove_dir_all(staged);
        }

        PreservedFiles {
            count: preserved_count,
            cache_root: cache_root.unwrap_or_default(),
        }
    }

    fn refresh_sync_states(&self, cx: &mut Context<Self>) {
        let states: HashMap<String, SyncIndicator> = self
            .open_files
            .values()
            .filter_map(|session| {
                let indicator = match &session.sync_state {
                    SyncState::Idle => return None,
                    SyncState::Uploading => SyncIndicator::Uploading,
                    SyncState::Downloading => SyncIndicator::Downloading,
                    SyncState::Synced { at } => SyncIndicator::Synced { at: *at },
                    SyncState::Conflict => SyncIndicator::Conflict,
                    SyncState::Error => SyncIndicator::Error,
                };
                Some((session.remote.clone(), indicator))
            })
            .collect();
        self.right.pane.update(cx, |pane, cx| {
            pane.set_sync_states(states, cx);
        });
    }
}

impl SftpBrowser {
    /// Whether the keys currently go to one of the file panes.
    pub fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.left_focus_handle(cx).contains_focused(window, cx)
            || self
                .right
                .pane
                .focus_handle(cx)
                .contains_focused(window, cx)
    }

    /// Keep the keys in the left half when what it shows was just replaced.
    fn move_left_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((old, _old_input)) = self.left_focus_before_change.take() else {
            return;
        };
        if !old.contains_focused(window, cx) {
            return;
        }
        match &self.peer_password_input {
            Some(input) => input.update(cx, |input, cx| input.focus(window, cx)),
            None => self.left_focus_handle(cx).focus(window, cx),
        }
    }

    fn ensure_peer_password_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let is_wanted = matches!(
            &self.peer,
            Some(PeerSlot {
                state: PeerSlotState::Failed {
                    is_password_rejected: true,
                    ..
                },
                ..
            })
        );
        if !is_wanted || self.peer_password_input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("New password")
        });
        cx.subscribe(&input, |this, _input, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.request_peer_retry(cx);
            }
        })
        .detach();
        self.peer_password_input = Some(input);
    }

    /// Ask for another try at the left server, with the password from the
    /// field if one was typed.
    fn request_peer_retry(&mut self, cx: &mut Context<Self>) {
        let password = self
            .peer_password_input
            .as_ref()
            .map(|input| Zeroizing::new(input.read(cx).value().to_string()))
            .filter(|password| !password.is_empty());
        cx.emit(BrowserEvent::PeerRetryRequested { password });
    }

    /// Bring the keys to the remote file list, the pane a session is about.
    pub fn focus_files(&self, window: &mut Window, cx: &mut App) {
        self.right.pane.focus_handle(cx).focus(window, cx);
    }

    /// Bring the keys to the left half, whatever it shows. With the keys
    /// there, they follow a change of its source (see `move_left_focus`).
    pub fn focus_left(&self, window: &mut Window, cx: &mut App) {
        self.left_focus_handle(cx).focus(window, cx);
    }

    /// Tab in a file list or on the placeholder: move the keys to the other
    /// half.
    fn on_switch_pane(&mut self, _: &SwitchPane, window: &mut Window, cx: &mut Context<Self>) {
        // A hidden half cannot take the keys.
        if !Preferences::is_second_pane_shown(cx) {
            return;
        }
        let left = self.left_focus_handle(cx);
        let target = if left.contains_focused(window, cx) {
            self.right.pane.focus_handle(cx)
        } else {
            left
        };
        target.focus(window, cx);
    }

    /// Keys of the placeholder while it holds them itself: Enter is its main
    /// button, Escape goes back to this computer.
    fn on_placeholder_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The password field reads its own keys.
        if event.is_held || !self.focus_handle.is_focused(window) {
            return;
        }
        let is_failed = matches!(
            &self.peer,
            Some(PeerSlot {
                state: PeerSlotState::Failed { .. },
                ..
            })
        );
        match pane_command(&event.keystroke.key, &event.keystroke.modifiers) {
            Some(PaneCommand::Open) if is_failed => self.request_peer_retry(cx),
            Some(PaneCommand::Cancel) => cx.emit(BrowserEvent::UseLocalRequested),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Render for SftpBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_peer_password_input(window, cx);
        self.move_left_focus(window, cx);
        let root = div()
            .size_full()
            .flex()
            .overflow_hidden()
            .on_action(cx.listener(Self::on_switch_pane));
        if !Preferences::is_second_pane_shown(cx) {
            return root.child(self.right.pane.clone());
        }

        let left: AnyElement = match self.left_pane() {
            Some(pane) => pane.clone().into_any_element(),
            None => self.render_peer_placeholder(cx),
        };
        // Next to the terminal the panes are narrow, so they go one above
        // the other.
        let is_stacked = Preferences::terminal_position(cx) == TerminalPosition::Beside;
        let divider = div().flex_none().bg(theme::border());
        let (root, divider) = if is_stacked {
            (root.flex_col(), divider.h(px(1.)).w_full())
        } else {
            (root.flex_row(), divider.w(px(1.)).h_full())
        };
        root.child(left)
            .child(divider)
            .child(self.right.pane.clone())
    }
}

impl SftpBrowser {
    /// What the left half shows while its server is not ready: the pane
    /// header with the source pill, then the state and what to do about it.
    fn render_peer_placeholder(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(slot) = &self.peer else {
            return div().flex_1().into_any_element();
        };
        let theme = cx.theme();
        let name = slot.connection.display_name().to_string();
        let muted = theme.muted_foreground;

        let pick_source: SourcePillClick = Box::new(cx.listener(|_, _, _, cx| {
            cx.emit(BrowserEvent::LeftSourcePickerRequested);
        }));
        let header = h_flex()
            .h(rems_from_px(38.))
            .flex_none()
            .px(rems_from_px(6.))
            .items_center()
            .child(source_pill(
                source_icon(PaneKind::Peer),
                name.clone().into(),
                false,
                Some(pick_source),
                cx,
            ));

        let use_local = Button::new("peer-use-local")
            .ghost()
            .small()
            .child("Use local")
            .on_click(cx.listener(|_, _, _, cx| cx.emit(BrowserEvent::UseLocalRequested)));

        let body = v_flex()
            .flex_1()
            .px(rems_from_px(16.))
            .gap_2()
            .items_center()
            .justify_center();
        let body = match &slot.state {
            PeerSlotState::Failed {
                was_connected,
                message,
                ..
            } => body
                .child(
                    div()
                        .w_full()
                        .text_center()
                        .text_color(theme.danger)
                        .child(peer_failure_title(*was_connected, &name)),
                )
                .child(
                    div()
                        .w_full()
                        .text_center()
                        .text_sm()
                        .text_color(muted)
                        .child(message.clone()),
                )
                .when_some(self.peer_password_input.as_ref(), |body, input| {
                    body.child(
                        div()
                            .w_full()
                            .max_w(rems_from_px(260.))
                            .child(Input::new(input).small()),
                    )
                })
                .child(
                    h_flex().gap_2().child(use_local).child(
                        Button::new("peer-retry")
                            .primary()
                            .small()
                            .child(if *was_connected { "Reconnect" } else { "Retry" })
                            .on_click(cx.listener(|this, _, _, cx| this.request_peer_retry(cx))),
                    ),
                ),
            // `Ready` has a pane and never gets here.
            PeerSlotState::Connecting | PeerSlotState::Ready(_) => body
                .child(Spinner::new().color(muted))
                .child(
                    div()
                        .w_full()
                        .text_center()
                        .text_sm()
                        .text_color(muted)
                        .child(format!("Connecting to {name}\u{2026}")),
                )
                .child(use_local),
        };

        v_flex()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            // Tab leaves for the right pane, as it does in a file list.
            .key_context(KEY_CONTEXT)
            .on_key_down(cx.listener(Self::on_placeholder_key))
            .child(header)
            .child(body)
            .into_any_element()
    }
}

/// Read for the pane's preview and show the result there. The task is kept
/// by the preview, so closing it or moving on cancels the read.
fn load_preview(
    pane: &Entity<FilePane>,
    is_after_move: bool,
    read: impl std::future::Future<Output = anyhow::Result<FileHead>> + 'static,
    cx: &mut App,
) {
    let Some(preview) = pane.read(cx).preview.clone() else {
        return;
    };
    let weak_preview = preview.downgrade();
    let task = cx.spawn(async move |cx| {
        if is_after_move {
            cx.background_executor().timer(PREVIEW_MOVE_DELAY).await;
        }
        let result = read.await;
        weak_preview
            .update(cx, |preview, cx| preview.show(result, cx))
            .ok();
    });
    preview.update(cx, |preview, _| preview.set_load(task));
}

async fn read_local_head(path: String, limit: usize) -> anyhow::Result<FileHead> {
    use std::io::Read as _;

    let metadata = std::fs::metadata(&path)?;
    if !metadata.is_file() {
        anyhow::bail!("not a regular file");
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path)?
        .take(limit as u64)
        .read_to_end(&mut bytes)?;
    Ok(FileHead {
        bytes,
        size: metadata.len(),
    })
}

const LEFT_NOT_CONNECTED: &str = "The left pane is not connected.";
const SESSION_NOT_CONNECTED: &str = "This session is not connected.";
const NO_APP_FOR_FILE: &str = "No app is set for this file type";
/// Opening a file downloads it with no task row and no way to stop it.
const TOO_LARGE_TO_OPEN: &str = "Too large to open here. Download it instead.";

fn unsafe_name_message(name: &str) -> String {
    format!("Cannot download {name}: this name is not a valid file name on this computer.")
}

/// The last part of a remote path, as messages name the file.
fn remote_file_name(remote: &str) -> String {
    std::path::Path::new(remote)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| remote.to_string())
}

/// Download `remote` into the edit cache at `local`. The file is written
/// under a temporary name and renamed when it is complete, so a download
/// that is still running or was cut short is never opened as the file.
async fn download_to_cache(
    sftp: &SftpHandle,
    remote: String,
    local: &std::path::Path,
) -> anyhow::Result<DownloadResult> {
    let part = staging_part_path(local);
    let result = sftp
        .download_file(remote, part.clone())
        .await
        .and_then(|done| {
            // A file from the server is from the internet: macOS and Windows then ask
            // before running it, as they would for a browser download.
            crate::update::mark_as_downloaded(&part);
            std::fs::rename(&part, local)?;
            Ok(done)
        });
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

fn not_listed_message(server: &str) -> String {
    format!("{server} has not listed a folder yet. Try again when its pane shows one.")
}

/// True when the server on `side` is where a transfer reads or writes.
fn is_end_of_transfer(side: Side, from: Endpoint, to: Endpoint) -> bool {
    [from, to].contains(&Endpoint::Server(side))
}

/// Why the transfers of a left server that left the pane failed.
fn left_server_gone_reason(is_lost: bool, name: &str) -> String {
    if is_lost {
        return format!("Connection to {name} lost");
    }
    format!("{name} was closed in the left pane")
}

/// The verb of a pane's transfer toward the other pane, and the server it
/// goes to. `other_server` is the server in the other pane when both panes
/// show one.
fn transfer_wording(kind: PaneKind, other_server: Option<&str>) -> (&'static str, Option<String>) {
    match (kind, other_server) {
        (PaneKind::Local, _) => ("Upload", None),
        (PaneKind::Session, None) => ("Download", None),
        (PaneKind::Session | PaneKind::Peer, other) => ("Copy", other.map(str::to_string)),
    }
}

/// Both ends of a copy between two servers, as the tasks list names it.
fn copy_route(from: &str, to: &str) -> String {
    format!("from {from} to {to}")
}

fn entry_facts(entry: &FileEntry) -> FileFacts {
    FileFacts {
        is_folder: entry.opens_as_folder(),
        size: entry.size,
        mtime: entry.mtime,
    }
}

fn peer_failure_title(was_connected: bool, name: &str) -> String {
    if was_connected {
        format!("Connection to {name} lost")
    } else {
        format!("Could not connect to {name}")
    }
}

/// Why the left pane cannot change its source while transfers use its server.
fn busy_message(running: usize, name: &str) -> String {
    if running == 1 {
        return format!(
            "1 transfer with {name} is still running. Stop it in the tasks list first."
        );
    }
    format!("{running} transfers with {name} are still running. Stop them in the tasks list first.")
}

fn sftp_cache_path(connection_id: &str, remote: &std::path::Path) -> Result<PathBuf, &'static str> {
    flint_cache_path("sftp", connection_id, remote)
}

/// Where a remote file is staged for an outbound drag. Deliberately separate
/// from the edit cache: that one is written in place when a file is reloaded
/// from the server, and the drag resolver must never hand the OS a
/// half-written file.
fn dragout_path(connection_id: &str, remote: &std::path::Path) -> Result<PathBuf, &'static str> {
    flint_cache_path("dragout", connection_id, remote)
}

/// The temporary name a staging copy is written under before it is renamed
/// into place.
fn staging_part_path(staged: &std::path::Path) -> PathBuf {
    staged.with_file_name(format!(
        "{}.flint-part",
        staged.file_name().unwrap_or_default().to_string_lossy()
    ))
}

fn flint_cache_path(
    kind: &str,
    connection_id: &str,
    remote: &std::path::Path,
) -> Result<PathBuf, &'static str> {
    for component in remote.components() {
        match component {
            std::path::Component::ParentDir => return Err("path contains '..' segment"),
            // On Windows a part such as `C:` would replace the path built below.
            std::path::Component::Normal(part)
                if !crate::ssh::is_single_local_component(&part.to_string_lossy()) =>
            {
                return Err("path has a part this system reads as a path");
            }
            _ => {}
        }
    }
    let base = flint_cache_root(kind, connection_id).ok_or("no cache directory")?;
    // Pushed part by part, not joined whole: the `\\?\` prefix below turns
    // off separator normalization on Windows, so a '/' left in the path
    // would be read as part of a file name (os error 123).
    let mut local = base;
    if let Some(parent) = remote.parent() {
        for component in parent.components() {
            if let std::path::Component::Normal(part) = component {
                local.push(part);
            }
        }
    }
    // A folder per remote path: on a case-insensitive disk `README` and
    // `readme` would otherwise share one local file.
    local.push(remote_path_tag(remote));
    if let Some(name) = remote.file_name() {
        local.push(name);
    }
    // Only a drive path takes the prefix as it is: a network share would
    // need `\\?\UNC\`, and is left alone.
    #[cfg(windows)]
    let is_drive_path = matches!(
        local.components().next(),
        Some(std::path::Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::Disk(_))
    );
    #[cfg(windows)]
    let local = if is_drive_path {
        PathBuf::from(format!("\\\\?\\{}", local.display()))
    } else {
        local
    };
    Ok(local)
}

/// A short hex tag that differs for remote paths that differ only in case.
fn remote_path_tag(remote: &std::path::Path) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(remote.as_os_str().as_encoded_bytes());
    digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The folder that holds one kind of cached files of one connection.
fn flint_cache_root(kind: &str, connection_id: &str) -> Option<PathBuf> {
    Some(
        dirs::cache_dir()?
            .join("Flint")
            .join(kind)
            .join(connection_id),
    )
}

/// Compares folders as paths, so a trailing slash does not matter.
/// The folders above `path`, nearest first, as far up as prefetching goes.
fn ancestors_to_prefetch(path: &str) -> Vec<&str> {
    std::path::Path::new(path)
        .ancestors()
        .skip(1)
        .take(PREFETCHED_ANCESTORS)
        .filter_map(|ancestor| ancestor.to_str())
        .filter(|ancestor| !ancestor.is_empty())
        .collect()
}

fn is_same_dir(a: &str, b: &str) -> bool {
    std::path::Path::new(a) == std::path::Path::new(b)
}

fn local_mtime(local: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(local)
        .and_then(|meta| meta.modified())
        .ok()
}

/// True unless a file with mtime `current` is unchanged since it was last
/// synced with the host. A file that cannot be read counts as changed.
fn has_unsaved_changes(
    current: Option<std::time::SystemTime>,
    last_synced: Option<std::time::SystemTime>,
) -> bool {
    current.is_none() || current != last_synced
}

/// True when the server's copy changed since it was downloaded, so the user
/// must pick reload or overwrite. A time that appears or goes away counts too.
fn is_remote_edited(saved_mtime: Option<u32>, current_mtime: Option<u32>) -> bool {
    current_mtime != saved_mtime
}

/// Remove the folders between `file` and `root` that are now empty, `root` too.
fn remove_empty_parents(file: &std::path::Path, root: &std::path::Path) {
    for dir in file
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(root))
    {
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
    }
}

fn prune_dir_cache(cache: &mut HashMap<PathBuf, Vec<FileEntry>>, visited: &VecDeque<PathBuf>) {
    if cache.len() <= DIR_CACHE_MAX {
        return;
    }
    let kept: HashSet<&std::path::Path> = visited.iter().map(PathBuf::as_path).collect();
    cache.retain(|path, _| {
        path.parent().is_some_and(|parent| kept.contains(parent))
            || visited.iter().any(|dir| dir.starts_with(path))
    });
}

/// Names already used per target folder while one conflict answer is applied.
type TakenNames = HashMap<(Endpoint, PathBuf), HashSet<String>>;

/// The names that transfers still running write into folder `dir` at `to`.
fn running_target_names<'a>(
    transfers: impl Iterator<Item = &'a LiveTransfer>,
    to: Endpoint,
    dir: &str,
) -> HashSet<String> {
    transfers
        .filter(|t| t.to == to && is_same_dir(&t.target_dir, dir))
        .map(|t| t.name.clone())
        .collect()
}

/// Where a failed listing is reported.
#[derive(Debug, PartialEq)]
enum ListFailureReport {
    /// In the pane, with Retry: the user is waiting on this folder or is in it.
    InPane,
    StatusLine,
    Silent,
}

fn list_failure_report(
    pending: Option<&std::path::Path>,
    pane_path: &str,
    failed: &str,
    is_background: bool,
) -> ListFailureReport {
    if pending == Some(std::path::Path::new(failed)) {
        return ListFailureReport::InPane;
    }
    // Prefetches fail all the time on folders the user cannot read.
    if is_background {
        return ListFailureReport::Silent;
    }
    // While the user waits on another folder, the pane must keep loading.
    if pending.is_none() && is_same_dir(pane_path, failed) {
        return ListFailureReport::InPane;
    }
    ListFailureReport::StatusLine
}

/// Outcome of an operation over several entries. Every entry is tried, so the
/// error says how many were left and why the first one failed.
fn batch_result(action: &str, total: usize, mut errors: Vec<String>) -> Result<(), String> {
    if errors.is_empty() {
        return Ok(());
    }
    let failed = errors.len();
    let first = errors.swap_remove(0);
    if total == 1 {
        return Err(first);
    }
    Err(format!(
        "Could not {action} {failed} of {total} items: {first}"
    ))
}

/// Where each copied item lands in `dir`. A name that is taken there, by
/// the item itself or another, gets " copy" added.
fn paste_targets(
    items: &[EntryRef],
    dir: &str,
    is_taken: impl Fn(&str) -> bool,
) -> Vec<(EntryRef, String)> {
    let mut used: Vec<String> = Vec::new();
    items
        .iter()
        .filter_map(|item| {
            let name = item.path.rsplit(['/', '\\']).next()?;
            let is_free =
                |candidate: &str| !is_taken(candidate) && !used.iter().any(|u| u == candidate);
            let name = if is_free(name) {
                name.to_string()
            } else {
                duplicate_name(name, item.is_dir, |candidate| !is_free(candidate))
            };
            used.push(name.clone());
            Some((
                item.clone(),
                format!("{}/{name}", dir.trim_end_matches(['/', '\\'])),
            ))
        })
        .collect()
}

/// A file in the temp folder for a copy that passes through this computer.
fn scratch_file() -> PathBuf {
    std::env::temp_dir().join(format!("flint-copy-{}", uuid::Uuid::new_v4()))
}

fn copy_local_items(pairs: &[(EntryRef, String)]) -> Result<(), String> {
    let mut errors = Vec::new();
    for (item, to) in pairs {
        let to = std::path::Path::new(to);
        // Never over something that is there: the names were picked free.
        let result = if std::fs::symlink_metadata(to).is_ok() {
            Err(format!("{} already exists", to.display()))
        } else {
            copy_local_tree(std::path::Path::new(&item.path), to)
                .map_err(|e| format!("{}: {e}", item.path))
        };
        if let Err(e) = result {
            tracing::warn!(path = %item.path, e, "local copy failed");
            errors.push(e);
        }
    }
    batch_result("copy", pairs.len(), errors)
}

fn copy_local_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    if !std::fs::metadata(from)?.is_dir() {
        return std::fs::copy(from, to).map(drop);
    }
    std::fs::create_dir(to)?;
    for child in std::fs::read_dir(from)? {
        let child = child?;
        copy_local_tree(&child.path(), &to.join(child.file_name()))?;
    }
    Ok(())
}

/// A move never replaces: `rename` would silently overwrite a file there.
fn move_local_items(items: &[EntryRef], dir: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    for item in items {
        let Some(to) = move_destination(&item.path, dir) else {
            continue;
        };
        let result = if std::fs::symlink_metadata(&to).is_ok() {
            Err(format!("{to} already exists"))
        } else {
            std::fs::rename(&item.path, &to).map_err(|e| format!("{}: {e}", item.path))
        };
        if let Err(e) = result {
            tracing::warn!(path = %item.path, dir, e, "local move failed");
            errors.push(e);
        }
    }
    batch_result("move", items.len(), errors)
}

/// Some servers' rename replaces an existing target, so a taken name is
/// refused first.
async fn move_remote_items(
    sftp: SftpHandle,
    items: Vec<EntryRef>,
    dir: String,
) -> anyhow::Result<()> {
    let total = items.len();
    let mut errors = Vec::new();
    for item in items {
        let Some(to) = move_destination(&item.path, &dir) else {
            continue;
        };
        let result = if sftp.stat_file(to.clone()).await.is_ok() {
            Err(format!("{to} already exists"))
        } else {
            sftp.rename(item.path.clone(), to)
                .await
                .map_err(|e| e.to_string())
        };
        if let Err(e) = result {
            tracing::warn!(path = %item.path, dir, e, "remote move failed");
            errors.push(e);
        }
    }
    batch_result("move", total, errors).map_err(anyhow::Error::msg)
}

fn delete_local_items(items: &[EntryRef]) -> Result<(), String> {
    let mut errors = Vec::new();
    for item in items {
        let path = &item.path;
        let result = if item.is_dir {
            std::fs::remove_dir_all(path)
        } else {
            // On Windows a link to a folder is removed with remove_dir,
            // not remove_file. remove_dir never deletes contents, so
            // trying it second cannot touch the folder behind the link.
            std::fs::remove_file(path)
                .or_else(|file_error| std::fs::remove_dir(path).map_err(|_| file_error))
        };
        if let Err(e) = result {
            tracing::warn!(path, ?e, "local delete failed");
            errors.push(format!("{path}: {e}"));
        }
    }
    batch_result("delete", items.len(), errors)
}

async fn delete_remote_items(sftp: SftpHandle, items: Vec<EntryRef>) -> anyhow::Result<()> {
    let total = items.len();
    let mut errors = Vec::new();
    for item in items {
        let result = if item.is_dir {
            sftp.remove_dir_recursive(item.path.clone()).await
        } else {
            sftp.remove_file(item.path.clone()).await
        };
        if let Err(e) = result {
            tracing::warn!(path = %item.path, ?e, "remote delete failed");
            errors.push(e.to_string());
        }
    }
    batch_result("delete", total, errors).map_err(anyhow::Error::msg)
}

/// Kind, size and date of a local path, following a link; `None` if nothing
/// is there. A link that points nowhere still takes the name, so it counts
/// as a file.
fn local_facts(path: &std::path::Path) -> Option<FileFacts> {
    let meta = std::fs::metadata(path)
        .or_else(|_| std::fs::symlink_metadata(path))
        .ok()?;
    Some(FileFacts {
        is_folder: meta.is_dir(),
        size: meta.len(),
        mtime: modified_secs(&meta),
    })
}

fn modified_secs(meta: &std::fs::Metadata) -> u32 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

fn load_local_entries(path: &str) -> std::io::Result<Vec<crate::ssh::FileEntry>> {
    let mut entries: Vec<_> = std::fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok()?;
            // DirEntry::metadata does not follow symlinks, so a symlink-to-dir
            // classifies as Symlink (is_dir false), same as the remote side.
            // Where it points is recorded apart, for navigation and display.
            let ft = meta.file_type();
            let kind = if ft.is_dir() {
                crate::ssh::EntryKind::Folder
            } else if ft.is_symlink() {
                crate::ssh::EntryKind::Symlink
            } else if ft.is_file() {
                crate::ssh::EntryKind::File
            } else {
                crate::ssh::EntryKind::Other
            };
            let mut entry = crate::ssh::FileEntry {
                name,
                is_dir: meta.is_dir(),
                size: meta.len(),
                mtime: modified_secs(&meta),
                permissions: None,
                kind,
                link_target: None,
                is_link_to_dir: false,
                is_link_broken: false,
            };
            if ft.is_symlink() {
                let target = std::fs::read_link(e.path())
                    .ok()
                    .map(|target| target.to_string_lossy().into_owned());
                // fs::metadata follows the link; a broken one or a loop is an error.
                let stat = std::fs::metadata(e.path())
                    .ok()
                    .map(|m| crate::ssh::LinkTargetStat {
                        is_dir: m.is_dir(),
                        size: Some(m.len()),
                        mtime: Some(modified_secs(&m)),
                    });
                entry.set_link_target(target, stat);
            }
            Some(entry)
        })
        .collect();

    crate::ssh::sort_listing(&mut entries);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_transfer(from: Endpoint, to: Endpoint) -> LiveTransfer {
        LiveTransfer {
            kind: TransferKind::Upload,
            from,
            to,
            source_path: "/home/a.txt".into(),
            panes: (None, None),
            target_dir: "/srv".into(),
            name: "a.txt".into(),
            cancel: CancelFlag::default(),
        }
    }

    #[test]
    fn keep_both_skips_names_of_running_transfers() {
        let right = Endpoint::Server(Side::Right);
        let mut first = live_transfer(Endpoint::Local, right);
        first.name = "config (1).yml".into();
        let mut elsewhere = live_transfer(Endpoint::Local, right);
        elsewhere.target_dir = "/tmp".into();
        elsewhere.name = "config (2).yml".into();
        let running = running_target_names([first, elsewhere].iter(), right, "/srv/");
        assert_eq!(running, HashSet::from(["config (1).yml".to_string()]));
        let name = free_name("config.yml", false, |name| running.contains(name));
        assert_eq!(name, "config (2).yml");
    }

    #[test]
    fn a_full_dir_cache_keeps_visited_folders_their_children_and_parents() {
        let mut cache: HashMap<PathBuf, Vec<FileEntry>> = (0..DIR_CACHE_MAX)
            .map(|i| (PathBuf::from(format!("/old/{i}")), Vec::new()))
            .collect();
        let visited = VecDeque::from([PathBuf::from("/srv/app")]);
        prune_dir_cache(&mut cache, &visited);
        assert_eq!(cache.len(), DIR_CACHE_MAX, "under the bound nothing goes");

        for path in ["/srv/app", "/srv/app/logs", "/srv", "/"] {
            cache.insert(PathBuf::from(path), Vec::new());
        }
        prune_dir_cache(&mut cache, &visited);
        let mut kept: Vec<_> = cache
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        kept.sort();
        assert_eq!(kept, ["/", "/srv", "/srv/app", "/srv/app/logs"]);
    }

    #[test]
    fn a_file_counts_as_unsaved_unless_its_mtime_is_the_synced_one() {
        let synced = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let later = synced + Duration::from_secs(1);
        assert!(!has_unsaved_changes(Some(synced), Some(synced)));
        assert!(has_unsaved_changes(Some(later), Some(synced)));
        assert!(has_unsaved_changes(Some(synced), None));
        assert!(has_unsaved_changes(None, Some(synced)));
    }

    #[test]
    fn removing_a_cached_file_keeps_folders_that_hold_other_files() {
        let root = tempfile::tempdir().unwrap();
        let kept = root.path().join("srv/app/kept.txt");
        let gone = root.path().join("srv/app/sub/gone.txt");
        std::fs::create_dir_all(gone.parent().unwrap()).unwrap();
        std::fs::write(&kept, "edit").unwrap();
        std::fs::write(&gone, "").unwrap();

        std::fs::remove_file(&gone).unwrap();
        remove_empty_parents(&gone, root.path());
        assert!(!root.path().join("srv/app/sub").exists());
        assert!(kept.exists());

        std::fs::remove_file(&kept).unwrap();
        remove_empty_parents(&kept, root.path());
        assert!(!root.path().exists());
    }

    #[test]
    fn a_transfer_uses_the_servers_at_its_two_ends() {
        let (left, right) = (Endpoint::Server(Side::Left), Endpoint::Server(Side::Right));

        // An upload to the left server does not touch the session's own.
        let upload = live_transfer(Endpoint::Local, left);
        assert!(upload.uses(Side::Left));
        assert!(!upload.uses(Side::Right));

        let download = live_transfer(right, Endpoint::Local);
        assert!(download.uses(Side::Right));
        assert!(!download.uses(Side::Left));

        let copy = live_transfer(left, right);
        assert!(copy.uses(Side::Left) && copy.uses(Side::Right));
    }

    #[test]
    fn a_pane_without_a_listing_names_its_server() {
        assert_eq!(
            not_listed_message("web-02"),
            "web-02 has not listed a folder yet. Try again when its pane shows one."
        );
    }

    #[test]
    fn a_closed_left_server_is_not_called_lost() {
        assert_eq!(
            left_server_gone_reason(true, "web-02"),
            "Connection to web-02 lost"
        );
        assert_eq!(
            left_server_gone_reason(false, "web-02"),
            "web-02 was closed in the left pane"
        );
    }

    #[test]
    fn failed_listing_the_user_waits_on_shows_in_pane() {
        let pending = std::path::Path::new("/root");
        for is_background in [false, true] {
            assert_eq!(
                list_failure_report(Some(pending), "/", "/root", is_background),
                ListFailureReport::InPane
            );
        }
    }

    #[test]
    fn the_folders_above_are_prefetched_nearest_first() {
        assert_eq!(
            ancestors_to_prefetch("/srv/app/releases"),
            ["/srv/app", "/srv", "/"]
        );
        assert!(ancestors_to_prefetch("/").is_empty());
        assert_eq!(
            ancestors_to_prefetch("/a/b/c/d/e/f/g/h/i/j").len(),
            PREFETCHED_ANCESTORS
        );
    }

    #[test]
    fn failed_prefetch_nobody_waits_on_is_silent() {
        assert_eq!(
            list_failure_report(None, "/", "/root", true),
            ListFailureReport::Silent
        );
        assert_eq!(
            list_failure_report(None, "/root", "/root", true),
            ListFailureReport::Silent
        );
        let pending = std::path::Path::new("/etc");
        assert_eq!(
            list_failure_report(Some(pending), "/", "/root", true),
            ListFailureReport::Silent
        );
    }

    #[test]
    fn failed_refresh_of_current_folder_shows_in_pane() {
        assert_eq!(
            list_failure_report(None, "/srv/", "/srv", false),
            ListFailureReport::InPane
        );
    }

    #[test]
    fn failed_listing_of_another_folder_goes_to_status_line() {
        assert_eq!(
            list_failure_report(None, "/", "/srv", false),
            ListFailureReport::StatusLine
        );
        // The user is waiting on /etc, so the pane must not stop loading.
        let pending = std::path::Path::new("/etc");
        assert_eq!(
            list_failure_report(Some(pending), "/srv", "/srv", false),
            ListFailureReport::StatusLine
        );
    }

    #[test]
    fn batch_result_counts_what_was_left() {
        assert_eq!(batch_result("delete", 3, Vec::new()), Ok(()));
        // One entry: the plain error, with no count.
        assert_eq!(
            batch_result("delete", 1, vec!["denied".into()]),
            Err("denied".into())
        );
        assert_eq!(
            batch_result("delete", 20, vec!["denied".into(), "busy".into()]),
            Err("Could not delete 2 of 20 items: denied".into())
        );
    }

    #[test]
    fn local_delete_goes_on_past_a_failure() {
        let dir = std::env::temp_dir().join(format!("flint-batch-delete-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("folder/inner")).unwrap();
        std::fs::write(dir.join("folder/inner/file"), b"x").unwrap();
        std::fs::write(dir.join("kept-until-deleted"), b"x").unwrap();
        let item = |name: &str, is_dir: bool| EntryRef {
            path: dir.join(name).to_string_lossy().into_owned(),
            is_dir,
        };

        let result = delete_local_items(&[
            item("missing", false),
            item("folder", true),
            item("kept-until-deleted", false),
        ]);

        assert!(result
            .unwrap_err()
            .starts_with("Could not delete 1 of 3 items: "));
        assert!(!dir.join("folder").exists());
        assert!(!dir.join("kept-until-deleted").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn transfer_wording_follows_what_the_other_pane_shows() {
        assert_eq!(transfer_wording(PaneKind::Local, None), ("Upload", None));
        assert_eq!(
            transfer_wording(PaneKind::Session, None),
            ("Download", None)
        );
        assert_eq!(
            transfer_wording(PaneKind::Session, Some("web-02")),
            ("Copy", Some("web-02".to_string()))
        );
        assert_eq!(
            transfer_wording(PaneKind::Peer, Some("web-01")),
            ("Copy", Some("web-01".to_string()))
        );
    }

    #[test]
    fn copy_route_names_the_source_first() {
        assert_eq!(copy_route("web-02", "web-01"), "from web-02 to web-01");
    }

    #[test]
    fn pasted_items_keep_free_names_and_never_collide() {
        let item = |path: &str| EntryRef {
            path: path.to_string(),
            is_dir: false,
        };
        let items = [
            item("/src/a.txt"),
            item("/here/b.txt"),
            item("/other/b.txt"),
        ];
        let taken = ["b.txt"];
        let targets: Vec<String> = paste_targets(&items, "/here/", |name| taken.contains(&name))
            .into_iter()
            .map(|(_, to)| to)
            .collect();
        assert_eq!(
            targets,
            ["/here/a.txt", "/here/b copy.txt", "/here/b copy 2.txt"]
        );
    }

    #[test]
    fn a_local_copy_takes_the_whole_folder_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("site");
        std::fs::create_dir_all(from.join("css")).unwrap();
        std::fs::write(from.join("css/main.css"), "body {}").unwrap();
        let to = dir.path().join("site copy");
        let pair = |to: &std::path::Path| {
            (
                EntryRef {
                    path: from.to_string_lossy().into_owned(),
                    is_dir: true,
                },
                to.to_string_lossy().into_owned(),
            )
        };
        copy_local_items(&[pair(&to)]).unwrap();
        assert_eq!(
            std::fs::read_to_string(to.join("css/main.css")).unwrap(),
            "body {}"
        );
        assert!(copy_local_items(&[pair(&to)]).is_err());
    }

    #[test]
    fn copy_has_its_own_status() {
        assert_eq!(TransferKind::Copy.done_status(), "Copy complete");
    }

    #[test]
    fn transfer_source_is_named_after_its_last_part() {
        assert_eq!(
            Source::at(Endpoint::Local, "/home/me/a.txt")
                .name()
                .as_deref(),
            Some("a.txt")
        );
        let remote = Source::at(Endpoint::Server(Side::Left), "/srv/site");
        assert_eq!(remote.name().as_deref(), Some("site"));
        assert_eq!(remote.endpoint(), Endpoint::Server(Side::Left));
        assert!(Source::at(Endpoint::Server(Side::Right), "/srv/")
            .name()
            .is_none());
    }

    #[test]
    fn a_remote_file_is_named_after_its_last_part() {
        assert_eq!(remote_file_name("/srv/site/index.html"), "index.html");
        assert_eq!(remote_file_name("/"), "/");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_points_nowhere_still_takes_its_name() {
        let dir = std::env::temp_dir().join(format!("flint-dangling-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(dir.join("missing"), &link).unwrap();

        assert!(local_facts(&link).is_some_and(|facts| !facts.is_folder));
        assert!(local_facts(&dir.join("missing")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_local_folder_is_an_error_not_an_empty_list() {
        let missing = std::env::temp_dir().join(format!("flint-no-such-{}", std::process::id()));
        assert!(load_local_entries(&missing.to_string_lossy()).is_err());
    }

    #[test]
    fn peer_failure_title_tells_a_drop_from_a_failed_connect() {
        assert_eq!(
            peer_failure_title(false, "web-02"),
            "Could not connect to web-02"
        );
        assert_eq!(
            peer_failure_title(true, "web-02"),
            "Connection to web-02 lost"
        );
    }

    #[test]
    fn busy_message_counts_the_transfers() {
        assert_eq!(
            busy_message(1, "web-02"),
            "1 transfer with web-02 is still running. Stop it in the tasks list first."
        );
        assert_eq!(
            busy_message(2, "web-02"),
            "2 transfers with web-02 are still running. Stop them in the tasks list first."
        );
    }

    #[test]
    fn same_dir_ignores_trailing_slash() {
        assert!(is_same_dir("/srv/www/", "/srv/www"));
        assert!(!is_same_dir("/srv/www", "/srv"));
    }

    #[test]
    fn sftp_cache_path_rejects_dotdot() {
        assert!(sftp_cache_path("c", std::path::Path::new("/foo/../etc/passwd")).is_err());
        assert!(sftp_cache_path("c", std::path::Path::new("/../etc")).is_err());
        assert!(sftp_cache_path("c", std::path::Path::new("/a/b/../c")).is_err());
    }

    #[test]
    fn sftp_cache_path_accepts_normal_paths() {
        let result = sftp_cache_path("my-conn", std::path::Path::new("/home/user/file.txt"));
        assert!(result.is_ok());
        let p = result.unwrap();
        assert!(p.starts_with(flint_cache_root("sftp", "my-conn").unwrap()));
        assert!(p.ends_with("file.txt"));
        assert!(p
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .ends_with("my-conn/home/user"));
    }

    #[test]
    fn cache_paths_differ_by_case() {
        let upper = sftp_cache_path("c", std::path::Path::new("/srv/README")).unwrap();
        let lower = sftp_cache_path("c", std::path::Path::new("/srv/readme")).unwrap();
        let fold = |p: &std::path::Path| p.to_string_lossy().to_lowercase();
        assert_ne!(fold(&upper), fold(&lower));
        assert!(upper.ends_with("README"));
    }

    #[test]
    fn remote_edit_is_a_conflict() {
        assert!(!is_remote_edited(Some(100), Some(100)));
        assert!(is_remote_edited(Some(100), Some(200)));
        assert!(is_remote_edited(Some(100), None));
        assert!(is_remote_edited(None, Some(100)));
    }

    // Regression: a nested remote path must not leave literal '/' separators in
    // the local path. On Windows the `\\?\` verbatim prefix disables separator
    // normalization, so a surviving '/' produces os error 123 on open.
    #[test]
    fn sftp_cache_path_normalizes_separators() {
        let p = sftp_cache_path("conn", std::path::Path::new("/root/docker-compose.yml")).unwrap();
        #[cfg(windows)]
        {
            let s = p.to_string_lossy();
            assert!(!s.contains('/'), "path still contains '/': {s}");
            assert!(s.starts_with("\\\\?\\"), "missing verbatim prefix: {s}");
        }
        assert!(p.ends_with("docker-compose.yml"));
        assert!(p.parent().unwrap().parent().unwrap().ends_with("conn/root"));
    }
}
