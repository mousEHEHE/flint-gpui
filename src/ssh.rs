use russh::client;
use russh::keys;
use russh::keys::key::PublicKey;
use russh::{Channel, ChannelMsg};
use russh_sftp::client::SftpSession;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use zeroize::Zeroizing;

use crate::connection::{is_loopback_bind, ForwardKind, ForwardSpec, ResolvedProxy};
use crate::forward::{ForwardMsg, RunningForward};
use crate::shell::{handle_shell_commands, ShellHandle};

/// Max directory listings kept in flight at once for [`SftpHandle::read_dir_batch`].
/// The SFTP channel multiplexes by request id, so a handful in flight hides
/// per-request latency without flooding the server.
const READDIR_BATCH_CONCURRENCY: usize = 32;

/// Max symlinks resolved at once for one listing. Each link costs a readlink
/// and a stat, so a link-heavy folder (/etc/alternatives) must overlap them.
const LINK_RESOLVE_CONCURRENCY: usize = 32;

/// Limit on the TCP connect to the server. A dead or filtered host fails in
/// this window instead of hanging the SSH thread indefinitely.
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Limit on the SSH handshake + auth after the TCP connect. The clock pauses
/// while a host-key or keyboard-interactive prompt is waiting on the user (see
/// [`with_handshake_timeout`]), so a slow human can't time out an otherwise
/// healthy handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Limit on a `tcpip-forward` / `cancel-tcpip-forward` global request. Both are
/// awaited inside [`run_connection`]'s select loop, which is the only servicer
/// of new channel opens and of the disconnect signal, so a server that accepts
/// the request and never answers must not park the whole connection on it.
const FORWARD_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Limit on opening one `direct-tcpip` channel for a -L/-D forward. The open
/// is awaited inside [`run_connection`]'s select loop too, and a SOCKS forward
/// makes many of them, so an unreachable target must give up quickly: until
/// it does, no shell, SFTP channel or disconnect is served.
const FORWARD_OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// Read/write chunk for streaming SFTP transfers. russh-sftp caps a single
/// READ/WRITE request at ~255 KiB and splits anything larger, so a 256 KiB
/// buffer means each loop iteration is one near-maximal request.
const TRANSFER_CHUNK: usize = 256 * 1024;

/// Minimum interval between [`SftpEvent::TransferProgress`] events for one
/// transfer, so a fast link doesn't flood the UI channel.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Files in flight at once when a folder is mirrored, in either direction.
/// SFTP round-trips dominate for many small files, so overlapping them is the
/// main speed lever.
const FOLDER_FILE_CONCURRENCY: usize = 16;

/// Files of one folder upload that send a chunk at the same moment. Every
/// chunk must be answered within the SFTP request timeout (10 s), and the
/// clock starts when the chunk is queued, not when it leaves. Sixteen chunks
/// at once are 4 MB, which a slow uplink cannot send in time.
const FOLDER_WRITE_CONCURRENCY: usize = 4;

/// Tracked transfers (drag/drop, menu) that run at once on one SFTP channel;
/// the rest wait their turn. They share one link, so more would not be faster.
const TRANSFER_CONCURRENCY: usize = 2;

/// How often a waiting transfer looks at its cancel flag.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Chunks of one file that may wait between the two servers of a copy. With
/// [`TRANSFER_CHUNK`] pieces that is about 1 MiB per file in flight.
const RELAY_PIPE_CHUNKS: usize = 4;

/// Files of a folder copied between two servers at once. Each one holds a
/// pipe of [`RELAY_PIPE_CHUNKS`] in memory, and more files than write slots
/// would only fill pipes.
const RELAY_FILE_CONCURRENCY: usize = FOLDER_WRITE_CONCURRENCY;

/// What the user is told when the source side of a copy stops answering.
const SOURCE_LOST: &str = "the connection to the source server was lost";

/// Classification of a directory entry, richer than `is_dir`. Derived from
/// Unix mode bits (remote) or [`std::fs::FileType`] (local). `Folder` always
/// agrees with `FileEntry::is_dir`; the extra variants let the UI label and
/// sort symlinks and special files (devices, fifos, sockets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Folder,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u32,
    /// Unix mode bits (e.g. 0o755), when the server reports them.
    /// `None` for local listings and servers that omit permissions.
    pub permissions: Option<u32>,
    pub kind: EntryKind,
    /// Where a symlink points, as the server stores it (may be relative).
    /// Set only by listings made for the panes; `None` if it cannot be read.
    pub link_target: Option<String>,
    /// The entry is a symlink whose target is a directory. Kept apart from
    /// `is_dir` on purpose: delete and transfers read `is_dir`, and they must
    /// act on the link itself, never on the folder behind it.
    pub is_link_to_dir: bool,
    /// The entry is a symlink whose target cannot be reached: missing, a
    /// loop, or not ours to stat. Its `size` is then the length of the
    /// target path, which means nothing to the user.
    pub is_link_broken: bool,
}

impl FileEntry {
    /// Whether opening the entry navigates into it: a real folder, or a
    /// symlink to one. For navigation and display only, see `is_link_to_dir`.
    pub fn opens_as_folder(&self) -> bool {
        self.is_dir || self.is_link_to_dir
    }
}

/// What a stat that follows a symlink said about its target.
pub struct LinkTargetStat {
    pub is_dir: bool,
    pub size: Option<u64>,
    pub mtime: Option<u32>,
}

impl FileEntry {
    /// Record where a symlink points. `stat` is `None` when the target cannot
    /// be reached (broken link, loop, no permission).
    pub fn set_link_target(&mut self, target: Option<String>, stat: Option<LinkTargetStat>) {
        self.link_target = target;
        let Some(stat) = stat else {
            self.is_link_broken = true;
            return;
        };
        self.is_link_to_dir = stat.is_dir;
        if stat.is_dir {
            return;
        }
        // Reading a link reads the file behind it, so the row must carry that
        // file's size: a drag-out copy stops after `size` bytes, and the link's
        // own size (the length of the target path) would cut it short.
        if let Some(size) = stat.size {
            self.size = size;
        }
        if let Some(mtime) = stat.mtime {
            self.mtime = mtime;
        }
    }
}

/// Default order of a listing: folders (and links to folders) first, then by name.
pub fn sort_listing(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        b.opens_as_folder()
            .cmp(&a.opens_as_folder())
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// Split one listed directory of a delete walk into the folders to descend
/// into and the paths to remove as files. A symlink always lands in the
/// second group, so the walk removes the link and never enters its target.
fn split_for_delete(dir: &str, entries: Vec<FileEntry>) -> (Vec<String>, Vec<String>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries {
        let child = format!("{}/{}", dir.trim_end_matches('/'), entry.name);
        if entry.is_dir {
            dirs.push(child);
        } else {
            files.push(child);
        }
    }
    (dirs, files)
}

pub struct DownloadResult {
    pub mtime: Option<u32>,
}

/// One file to copy locally for an outbound drag (see [`SftpHandle::stage_files`]).
pub struct StageItem {
    pub remote: String,
    /// Where to write it. The caller renames the result into place, so this is
    /// a temporary path: a half-written file must never sit where a drag will
    /// look for it.
    pub local: std::path::PathBuf,
    pub size: u64,
}

#[derive(Clone)]
pub enum AuthCredential {
    Password(Zeroizing<String>),
    PrivateKey {
        pem: Zeroizing<String>,
        passphrase: Option<Zeroizing<String>>,
    },
    /// Authenticate with whatever keys the system SSH agent holds
    /// (OpenSSH agent service on Windows, `SSH_AUTH_SOCK` elsewhere).
    Agent,
}

/// One authenticated machine on the path to a host. Built by the UI from a
/// vault entry, so it carries the id the events flow back with.
#[derive(Clone)]
pub struct SshHop {
    /// Vault entry id as a String, matching `Connection::id`. The UI resolves
    /// display names from it, so prompts name the machine that raised them.
    pub entry_id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub credential: AuthCredential,
    pub expected_host_key: Option<String>,
}

/// The full path to a host: zero or more bastions, then the target itself.
/// Only the first hop is reached over TCP, so the proxy belongs to the route
/// rather than to any one hop.
#[derive(Clone)]
pub struct SshRoute {
    pub jumps: Vec<SshHop>,
    pub target: SshHop,
    pub proxy: Option<ResolvedProxy>,
}

impl SshHop {
    fn failed(&self, error: AuthError) -> HopFailure {
        HopFailure {
            entry_id: self.entry_id.clone(),
            error,
        }
    }

    /// Name for messages about this hop. An imported host can carry an empty
    /// label, and "jump host ''" tells the user nothing.
    pub fn display_name(&self) -> &str {
        if self.label.is_empty() {
            &self.host
        } else {
            &self.label
        }
    }
}

impl SshRoute {
    /// Every hop in connect order, target last.
    pub fn hops(&self) -> impl Iterator<Item = &SshHop> {
        self.jumps.iter().chain(std::iter::once(&self.target))
    }

    pub fn hops_mut(&mut self) -> impl Iterator<Item = &mut SshHop> {
        self.jumps
            .iter_mut()
            .chain(std::iter::once(&mut self.target))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("wrong password")]
    WrongPassword,
    #[error("the server rejected this key for user '{0}'")]
    KeyRejected(String),
    #[error("incorrect passphrase for the private key")]
    WrongPassphrase,
    #[error("invalid private key format: {0}")]
    InvalidPemFormat(String),
    #[error("the private key is encrypted, enter its passphrase")]
    PassphraseRequired,
    #[error("could not connect: {0}")]
    ConnectionFailed(String),
    #[error("host key verification failed")]
    HostKeyRejected,
    #[error("authentication canceled")]
    Canceled,
    #[error("the server rejected the authentication answers")]
    KbdInteractiveRejected,
    #[error("SSH agent unavailable: {0}")]
    AgentUnavailable(String),
    #[error("the SSH agent holds no keys")]
    AgentNoKeys,
    #[error("the server accepted none of the {0} key(s) offered by the SSH agent")]
    AgentKeysRejected(usize),
    #[error("SSH error: {0}")]
    SshProtocol(String),
    /// The bastion is configured never to forward (`AllowTcpForwarding no`).
    /// Kept apart from `ConnectionFailed` on purpose: retrying cannot help,
    /// so the reconnect ladder must not swallow the reason. The failure is
    /// tagged with the bastion's entry, which the UI names, so the text does
    /// not repeat it.
    #[error("refused to open a tunnel: TCP forwarding is not allowed there")]
    TunnelRefused,
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("failed to open channel: {0}")]
    ChannelOpen(String),
    #[error("failed to initialize SFTP: {0}")]
    SftpInit(String),
    #[error("failed to open shell: {0}")]
    ShellOpen(String),
    #[error("connection closed")]
    ConnectionClosed,
}

pub enum SshCmd {
    OpenSftpChannel {
        reply: tokio::sync::oneshot::Sender<Result<(SftpHandle, String), SshError>>,
    },
    OpenShellChannel {
        rows: u16,
        cols: u16,
        reply: tokio::sync::oneshot::Sender<Result<ShellHandle, SshError>>,
    },
    OpenExec {
        cmd: String,
        reply: tokio::sync::oneshot::Sender<Result<ExecHandle, SshError>>,
    },
    /// Bring one port forward up. No reply channel: what happened comes back
    /// on the event stream as [`SshConnectionEvent::ForwardChanged`], which is
    /// also where every later change to it arrives.
    StartForward {
        spec: ForwardSpec,
    },
    StopForward {
        id: String,
    },
}

/// A one-shot remote command. Callers only need its stdout, so the handle hands
/// out raw stdout chunks and closes the channel when the command exits; stderr
/// and the exit status are logged.
pub struct ExecHandle {
    stdout_rx: async_channel::Receiver<Vec<u8>>,
}

impl ExecHandle {
    pub fn subscribe(&self) -> async_channel::Receiver<Vec<u8>> {
        self.stdout_rx.clone()
    }
}

/// Events from the connection thread. Everything a hop can raise carries the
/// hop's `entry_id`: on a jumped host the machine that prompts or refuses is
/// often a bastion, and attributing it to the target would name the wrong
/// server and write the wrong vault entry.
pub enum SshConnectionEvent {
    Connected,
    AuthFailed {
        entry_id: String,
        error: AuthError,
    },
    Disconnected,
    /// The server presented a key that needs a user decision (first contact,
    /// or, much worse, a key that doesn't match the stored fingerprint).
    /// The handshake is parked until `reply` is answered.
    HostKeyPrompt(HostKeyPrompt),
    /// The server's key was verified silently against `~/.ssh/known_hosts`
    /// on first contact. Reported so the fingerprint can be persisted on the
    /// vault entry (later connects then verify against the vault directly).
    HostKeyTrusted {
        entry_id: String,
        fingerprint: String,
    },
    /// The server sent a keyboard-interactive challenge that can't be answered
    /// silently (2FA codes, PAM prompts). Auth is parked until `reply` is
    /// answered.
    AuthPrompt(AuthPrompt),
    /// A port forward went up, changed its connection count, or stopped. `id`
    /// is the vault row's uuid as a String.
    ForwardChanged {
        id: String,
        state: ForwardState,
    },
}

/// Live state of one port forward. Always the complete state, never a delta,
/// so a listener that missed an event is correct again after the next one.
#[derive(Debug, Clone)]
pub enum ForwardState {
    /// `bound_port` differs from the requested port only when the server was
    /// asked to pick one (`-R` on port 0).
    Bound { bound_port: u16, active: u32 },
    /// `reason: None` = the user stopped it; `Some` = it failed or died.
    Stopped { reason: Option<String> },
}

/// Sent mid-handshake when the server's host key can't be verified silently.
/// `reply.send(true)` trusts the key and resumes the handshake; `false` (or
/// dropping the sender) aborts the connection with [`AuthError::HostKeyRejected`].
pub struct HostKeyPrompt {
    /// Vault entry of the hop presenting the key, not always the target.
    pub entry_id: String,
    /// Key algorithm, e.g. "ssh-ed25519".
    pub key_type: String,
    /// "SHA256:..." fingerprint of the key the server just presented.
    pub fingerprint: String,
    /// The previously trusted fingerprint, if any: the one stored in the
    /// vault, or the one `~/.ssh/known_hosts` holds for this host. `Some`
    /// means MISMATCH: the server's key changed and this could be a
    /// man-in-the-middle.
    pub stored_fingerprint: Option<String>,
    pub reply: tokio::sync::oneshot::Sender<bool>,
}

/// One field of a keyboard-interactive challenge.
pub struct AuthPromptField {
    /// Prompt text from the server, e.g. "Verification code: ".
    pub text: String,
    /// `false` = the answer must not be shown while typed (passwords, OTP).
    pub echo: bool,
}

/// Sent mid-auth when the server's keyboard-interactive challenge needs the
/// user. `reply.send(Some(answers))` (one per field, same order) continues
/// auth; `None` (or dropping the sender) cancels with [`AuthError::Canceled`].
pub struct AuthPrompt {
    /// Vault entry of the hop asking, not always the target.
    pub entry_id: String,
    /// Challenge name from the server; often empty.
    pub name: String,
    /// Free-form instructions from the server; often empty.
    pub instructions: String,
    pub fields: Vec<AuthPromptField>,
    pub reply: tokio::sync::oneshot::Sender<Option<Vec<Zeroizing<String>>>>,
}

/// Shared cancellation flag for one tracked transfer. Cancel is not a command:
/// the streaming loops poll this flag between chunks, and a transfer that
/// still waits for a free slot polls it too.
pub type CancelFlag = Arc<std::sync::atomic::AtomicBool>;

/// Terminal state of a tracked transfer.
#[derive(Debug, Clone)]
pub enum TransferOutcome {
    /// `skipped_links` counts symlinks a folder transfer left out (links are
    /// never followed, so a loop cannot hang the walk).
    Done {
        skipped_links: usize,
    },
    Canceled,
    Failed(String),
}

/// Marker error a canceled streaming loop bails with, so the worker can tell
/// "user canceled" apart from a real failure.
#[derive(Debug, thiserror::Error)]
#[error("transfer canceled")]
struct TransferCanceled;

/// One piece of a file on its way from the source server of a copy to the
/// worker that writes it. A channel that closes before `End` means the source
/// is gone, so a cut-off file is never taken for a whole one.
#[derive(Debug)]
pub enum RelayChunk {
    Data(Vec<u8>),
    End,
    Failed(String),
}

pub enum SftpCmd {
    ListDir(String),
    Download {
        remote: String,
        local: std::path::PathBuf,
        id: u64,
        cancel: CancelFlag,
    },
    Upload {
        local: std::path::PathBuf,
        remote: String,
        id: u64,
        cancel: CancelFlag,
    },
    /// Copy `source_path` on the server behind `source` to `remote` here. This
    /// worker pulls the bytes; see [`SftpHandle::relay`].
    Relay {
        source: SftpHandle,
        source_path: String,
        remote: String,
        id: u64,
        cancel: CancelFlag,
    },
    /// Walk `source` here and work out what its copy to `target` on another
    /// server consists of.
    PlanCopy {
        source: String,
        target: String,
        cancel: CancelFlag,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<UploadPlan>>,
    },
    /// Read up to `limit` bytes from the start of `remote`, for a preview.
    ReadHead {
        remote: String,
        limit: usize,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<FileHead>>,
    },
    /// Read `remote` into `chunks`, for the worker of another server to write.
    ReadFile {
        remote: String,
        chunks: async_channel::Sender<RelayChunk>,
        cancel: CancelFlag,
    },
    PrefetchDirs {
        paths: Vec<String>,
    },
    DownloadFile {
        remote: String,
        local: std::path::PathBuf,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<DownloadResult>>,
    },
    /// Fetch a file whose size the caller already knows. Skips the metadata
    /// probe and the trailing empty read that prove it on the wire, see
    /// [`SftpHandle::stage_file`].
    StageFile {
        remote: String,
        local: std::path::PathBuf,
        size: u64,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    /// Fetch many known-size files at once, see [`SftpHandle::stage_files`].
    StageFiles {
        items: Vec<StageItem>,
        reply: tokio::sync::oneshot::Sender<Vec<String>>,
    },
    UploadFile {
        local: std::path::PathBuf,
        remote: String,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    StatFile {
        remote: String,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<Option<u32>>>,
    },
    MakeDir {
        path: String,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    /// List many directories concurrently over the single SFTP channel. SFTP
    /// multiplexes requests by id, so this pays ~one round-trip for the whole
    /// batch instead of one per directory. Each result is paired with the path
    /// it was requested for so callers can match them back up.
    ReadDirBatch {
        paths: Vec<String>,
        reply: tokio::sync::oneshot::Sender<Vec<(String, anyhow::Result<Vec<FileEntry>>)>>,
    },
    RemoveFile {
        path: String,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    RemoveDir {
        path: String,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    Rename {
        from: String,
        to: String,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    /// chmod: set only the permission bits (mode), leaving everything else
    /// untouched.
    SetPermissions {
        path: String,
        mode: u32,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
}

#[derive(Debug)]
pub enum SftpEvent {
    DirListed {
        path: String,
        entries: Vec<FileEntry>,
    },
    DownloadComplete {
        local: std::path::PathBuf,
    },
    UploadComplete {
        remote: String,
    },
    PrefetchComplete {
        path: String,
        entries: Vec<FileEntry>,
    },
    /// A listing failed. `is_background` is set for prefetches, which nobody
    /// asked for directly, so the UI stays quiet unless the user is waiting
    /// on that path.
    ListFailed {
        path: String,
        error: String,
        is_background: bool,
    },
    /// Periodic (throttled) progress for a streaming transfer. `total` is 0
    /// when the size is unknown. `id` is set for transfers the UI tracks as
    /// tasks (drag/drop and menu transfers); `None` for incidental ones
    /// (opening a file). Informational only: completion is still signalled by
    /// `DownloadComplete` / `UploadComplete` / a reply channel.
    TransferProgress {
        id: Option<u64>,
        name: String,
        done: u64,
        total: u64,
    },
    /// A tracked transfer has to wait for a free slot (`is_waiting`), or has
    /// left the queue and runs now. A transfer that starts at once sends
    /// neither.
    TransferWaiting {
        id: u64,
        is_waiting: bool,
    },
    /// Terminal event for a tracked (`id`-carrying) transfer.
    TransferFinished {
        id: u64,
        name: String,
        outcome: TransferOutcome,
    },
    Error(String),
    Disconnected,
}

/// Throttled progress emitter for one transfer. A folder transfer shares one
/// between all its files, so the bytes add up to one figure for the whole
/// tree. Uses `try_send` so a full event channel drops a progress tick instead
/// of stalling the transfer.
struct TransferReporter<'a> {
    event_tx: &'a async_channel::Sender<SftpEvent>,
    id: Option<u64>,
    name: String,
    total: u64,
    done: std::sync::atomic::AtomicU64,
    created: std::time::Instant,
    /// When the last tick went out, on the clock of [`Self::clock_ms`].
    last_emit_ms: std::sync::atomic::AtomicU64,
}

impl<'a> TransferReporter<'a> {
    fn new(
        event_tx: &'a async_channel::Sender<SftpEvent>,
        id: Option<u64>,
        name: &str,
        total: u64,
    ) -> Self {
        Self {
            event_tx,
            id,
            name: name.to_string(),
            total,
            done: std::sync::atomic::AtomicU64::new(0),
            created: std::time::Instant::now(),
            last_emit_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Milliseconds since the reporter was made, started one interval ahead
    /// so the first chunk emits immediately: small files get at least one
    /// progress tick instead of none.
    fn clock_ms(&self) -> u64 {
        (self.created.elapsed() + PROGRESS_INTERVAL).as_millis() as u64
    }

    fn add(&self, bytes: u64) {
        use std::sync::atomic::Ordering;

        self.done.fetch_add(bytes, Ordering::Relaxed);
        let now_ms = self.clock_ms();
        let interval_ms = PROGRESS_INTERVAL.as_millis() as u64;
        if now_ms.saturating_sub(self.last_emit_ms.load(Ordering::Relaxed)) < interval_ms {
            return;
        }
        self.last_emit_ms.store(now_ms, Ordering::Relaxed);
        self.emit();
    }

    /// Send the current figures now, whatever the throttle says.
    fn emit(&self) {
        let _ = self.event_tx.try_send(SftpEvent::TransferProgress {
            id: self.id,
            name: self.name.clone(),
            done: self.done.load(std::sync::atomic::Ordering::Relaxed),
            total: self.total,
        });
    }
}

/// Identity + cancel flag for a transfer the UI tracks as a task.
#[derive(Clone)]
struct TransferTrack {
    id: u64,
    cancel: CancelFlag,
    /// Set by a folder upload when one of its files failed, so the files
    /// still in flight stop and clean up after themselves.
    has_failed_sibling: CancelFlag,
    /// Limits how many files of a folder upload write a chunk at once, see
    /// [`FOLDER_WRITE_CONCURRENCY`]. `None` for a transfer of one file.
    write_slots: Option<Arc<tokio::sync::Semaphore>>,
}

impl TransferTrack {
    fn new(id: u64, cancel: CancelFlag) -> Self {
        Self {
            id,
            cancel,
            has_failed_sibling: CancelFlag::default(),
            write_slots: None,
        }
    }

    /// The track the files of one folder upload share. It follows the cancel
    /// flag of `track` (when the transfer has one) and adds its own flag for a
    /// failed sibling.
    fn for_folder_files(track: Option<&TransferTrack>) -> Self {
        let cancel = track.map(|t| t.cancel.clone()).unwrap_or_default();
        Self {
            write_slots: Some(Arc::new(tokio::sync::Semaphore::new(
                FOLDER_WRITE_CONCURRENCY,
            ))),
            ..Self::new(track.map(|t| t.id).unwrap_or(0), cancel)
        }
    }

    fn canceled(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.cancel.load(Ordering::Relaxed) || self.has_failed_sibling.load(Ordering::Relaxed)
    }
}

/// Last path component, for progress labels ("file.zip", not "/srv/file.zip").
fn base_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

#[derive(Clone)]
pub struct SshConnection {
    cmd_tx: async_channel::Sender<SshCmd>,
    event_rx: async_channel::Receiver<SshConnectionEvent>,
}

impl SshConnection {
    pub fn spawn(route: SshRoute) -> Self {
        let (cmd_tx, cmd_rx) = async_channel::bounded::<SshCmd>(8);
        let (event_tx, event_rx) = async_channel::bounded::<SshConnectionEvent>(8);

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, run_connection(route, cmd_rx, event_tx));
        });

        Self { cmd_tx, event_rx }
    }

    pub fn subscribe_events(&self) -> async_channel::Receiver<SshConnectionEvent> {
        self.event_rx.clone()
    }

    pub async fn open_sftp(&self) -> Result<(SftpHandle, String), SshError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SshCmd::OpenSftpChannel { reply: reply_tx })
            .await
            .map_err(|_| SshError::ConnectionClosed)?;
        reply_rx.await.map_err(|_| SshError::ConnectionClosed)?
    }

    pub async fn open_shell(&self, rows: u16, cols: u16) -> Result<ShellHandle, SshError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SshCmd::OpenShellChannel {
                rows,
                cols,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SshError::ConnectionClosed)?;
        reply_rx.await.map_err(|_| SshError::ConnectionClosed)?
    }

    pub async fn open_exec(&self, cmd: String) -> Result<ExecHandle, SshError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SshCmd::OpenExec {
                cmd,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SshError::ConnectionClosed)?;
        reply_rx.await.map_err(|_| SshError::ConnectionClosed)?
    }

    // The two forward commands have no reply channel (the outcome arrives as
    // a `ForwardChanged` event like every later change), so a dead connection
    // can only be logged here.

    pub async fn start_forward(&self, spec: ForwardSpec) {
        let id = spec.id.clone();
        if self
            .cmd_tx
            .send(SshCmd::StartForward { spec })
            .await
            .is_err()
        {
            tracing::warn!(id, "cannot start a forward: the connection is closed");
        }
    }

    pub async fn stop_forward(&self, id: String) {
        if self
            .cmd_tx
            .send(SshCmd::StopForward { id: id.clone() })
            .await
            .is_err()
        {
            tracing::warn!(id, "cannot stop a forward: the connection is closed");
        }
    }
}

#[derive(Clone)]
pub struct SftpHandle {
    cmd_tx: async_channel::Sender<SftpCmd>,
    event_rx: async_channel::Receiver<SftpEvent>,
}

impl SftpHandle {
    pub fn subscribe(&self) -> async_channel::Receiver<SftpEvent> {
        self.event_rx.clone()
    }

    // The fire-and-forget senders below use try_send, which on the unbounded
    // command queue can only fail when the connection is gone, never under
    // load.

    pub fn list_dir(&self, path: String) {
        if self.cmd_tx.try_send(SftpCmd::ListDir(path)).is_err() {
            tracing::warn!("list_dir dropped: sftp channel closed");
        }
    }

    /// Queue a tracked download. `id` correlates progress/finish events with
    /// the UI task; setting `cancel` aborts between chunks.
    pub fn download(&self, remote: String, local: std::path::PathBuf, id: u64, cancel: CancelFlag) {
        if self
            .cmd_tx
            .try_send(SftpCmd::Download {
                remote,
                local,
                id,
                cancel,
            })
            .is_err()
        {
            tracing::warn!("download dropped: sftp channel closed");
        }
    }

    /// Queue a tracked upload, see [`Self::download`].
    pub fn upload(&self, local: std::path::PathBuf, remote: String, id: u64, cancel: CancelFlag) {
        if self
            .cmd_tx
            .try_send(SftpCmd::Upload {
                local,
                remote,
                id,
                cancel,
            })
            .is_err()
        {
            tracing::warn!("upload dropped: sftp channel closed");
        }
    }

    /// Queue a tracked copy of `source_path` on the server behind `source` to
    /// `remote` on this one. The bytes pass through this machine: this worker
    /// asks the other one for each file and writes it the way an upload is
    /// written. Progress and the end are reported here, as for an upload.
    pub fn relay(
        &self,
        source: SftpHandle,
        source_path: String,
        remote: String,
        id: u64,
        cancel: CancelFlag,
    ) {
        let cmd = SftpCmd::Relay {
            source,
            source_path,
            remote,
            id,
            cancel,
        };
        if self.cmd_tx.try_send(cmd).is_err() {
            tracing::warn!("copy dropped: sftp channel closed");
        }
    }

    async fn plan_copy(
        &self,
        source: String,
        target: String,
        cancel: CancelFlag,
    ) -> anyhow::Result<UploadPlan> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::PlanCopy {
                source,
                target,
                cancel,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!(SOURCE_LOST))?;
        reply_rx.await.map_err(|_| anyhow::anyhow!(SOURCE_LOST))?
    }

    fn read_file(
        &self,
        remote: String,
        chunks: async_channel::Sender<RelayChunk>,
        cancel: CancelFlag,
    ) -> anyhow::Result<()> {
        self.cmd_tx
            .try_send(SftpCmd::ReadFile {
                remote,
                chunks,
                cancel,
            })
            .map_err(|_| anyhow::anyhow!(SOURCE_LOST))
    }

    pub fn prefetch_dirs(&self, paths: Vec<String>) {
        if self
            .cmd_tx
            .try_send(SftpCmd::PrefetchDirs { paths })
            .is_err()
        {
            tracing::debug!("prefetch dropped: sftp channel closed");
        }
    }

    pub async fn download_file(
        &self,
        remote: String,
        local: std::path::PathBuf,
    ) -> anyhow::Result<DownloadResult> {
        tracing::debug!(target: "flint::file_edit", remote, "queuing file download to cache");
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::DownloadFile {
                remote,
                local,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    /// Fetch `remote` into `local`, given its size from a directory listing.
    ///
    /// [`Self::download_file`] costs five round trips: two metadata probes (one
    /// in each layer), the open, the read, and an extra read to see the end of
    /// the file. Knowing the size up front removes four of them, which is the
    /// difference between a drag-out copy landing in time and not.
    pub async fn stage_file(
        &self,
        remote: String,
        local: std::path::PathBuf,
        size: u64,
    ) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::StageFile {
                remote,
                local,
                size,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    /// Fetch a batch of known-size files, returning the remote paths that
    /// arrived. SFTP multiplexes requests by id, so the whole batch costs about
    /// as much as the slowest single file rather than the sum of all of them.
    pub async fn stage_files(&self, items: Vec<StageItem>) -> Vec<String> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self
            .cmd_tx
            .send(SftpCmd::StageFiles {
                items,
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return Vec::new();
        }
        reply_rx.await.unwrap_or_default()
    }

    pub async fn upload_file(
        &self,
        local: std::path::PathBuf,
        remote: String,
    ) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::UploadFile {
                local,
                remote,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    pub async fn stat_file(&self, remote: String) -> anyhow::Result<Option<u32>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::StatFile {
                remote,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    pub async fn read_head(&self, remote: String, limit: usize) -> anyhow::Result<FileHead> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::ReadHead {
                remote,
                limit,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    pub async fn make_dir(&self, path: String) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::MakeDir {
                path,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    /// List many directories at once, run concurrently inside the worker. Returns
    /// one `(path, result)` pair per input path (order is not preserved). Used by
    /// the recursive remote walk so a deep tree costs ~one round-trip per level
    /// rather than one per directory.
    pub async fn read_dir_batch(
        &self,
        paths: Vec<String>,
    ) -> anyhow::Result<Vec<(String, anyhow::Result<Vec<FileEntry>>)>> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::ReadDirBatch {
                paths,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))
    }

    pub async fn remove_file(&self, path: String) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::RemoveFile {
                path,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    pub async fn remove_dir(&self, path: String) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::RemoveDir {
                path,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    pub async fn rename(&self, from: String, to: String) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::Rename {
                from,
                to,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    /// chmod: set the Unix mode bits on a remote path.
    pub async fn set_permissions(&self, path: String, mode: u32) -> anyhow::Result<()> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(SftpCmd::SetPermissions {
                path,
                mode,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("sftp channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("sftp reply dropped"))?
    }

    /// Delete a remote directory tree. SFTP's rmdir only removes empty
    /// directories, so this walks the tree (one listing round-trip per level)
    /// and deletes files first, then directories bottom-up.
    pub async fn remove_dir_recursive(&self, root: String) -> anyhow::Result<()> {
        // BFS to collect the tree: files at any depth, dirs per level.
        let mut dirs_by_depth: Vec<Vec<String>> = vec![vec![root]];
        let mut files: Vec<String> = Vec::new();
        loop {
            let frontier = dirs_by_depth
                .last()
                .expect("popped only right before break")
                .clone();
            if frontier.is_empty() {
                dirs_by_depth.pop();
                break;
            }
            let results = self.read_dir_batch(frontier).await?;
            let mut next_level: Vec<String> = Vec::new();
            for (dir, listing) in results {
                let entries = listing.map_err(|e| anyhow::anyhow!("list {dir}: {e}"))?;
                let (child_dirs, child_files) = split_for_delete(&dir, entries);
                next_level.extend(child_dirs);
                files.extend(child_files);
            }
            dirs_by_depth.push(next_level);
        }

        for file in files {
            self.remove_file(file).await?;
        }
        // Deepest directories first so every rmdir sees an empty dir.
        for level in dirs_by_depth.into_iter().rev() {
            for dir in level {
                self.remove_dir(dir).await?;
            }
        }
        Ok(())
    }
}

async fn run_connection(
    route: SshRoute,
    cmd_rx: async_channel::Receiver<SshCmd>,
    event_tx: async_channel::Sender<SshConnectionEvent>,
) {
    let host = route.target.host.clone();
    let username = route.target.username.clone();

    // Created before the handshake: the russh handler needs the sender to hand
    // over the channels a `-R` forward's inbound connections arrive on.
    let (forward_tx, forward_rx) = async_channel::unbounded::<ForwardMsg>();

    let chain = match connect_chain(&route, &event_tx, &forward_tx).await {
        Ok(chain) => chain,
        Err(failure) => {
            tracing::error!(error = ?failure.error, host, "ssh connection failed");
            let _ = event_tx
                .send(SshConnectionEvent::AuthFailed {
                    entry_id: failure.entry_id,
                    error: failure.error,
                })
                .await;
            return;
        }
    };
    // Credentials must not outlive authentication.
    drop(route);
    // `tcpip_forward` needs `&mut self`, so the handle is bound mutably.
    let ConnectedChain {
        session: mut handle,
        disconnect_rx: disc_rx,
        jumps: _jumps,
    } = chain;
    tracing::info!(host, username, "ssh session authenticated");
    let _ = event_tx.send(SshConnectionEvent::Connected).await;

    // Forward events must never go straight onto `event_tx`: it is bounded, and
    // this loop is the only servicer of `cmd_rx`, so awaiting a full channel
    // here would wedge the whole connection. A drain task does the blocking
    // send instead, and the loop only ever `try_send`s onto this queue.
    let (forward_event_tx, forward_event_rx) = async_channel::unbounded::<SshConnectionEvent>();
    tokio::task::spawn_local({
        let event_tx = event_tx.clone();
        async move {
            while let Ok(event) = forward_event_rx.recv().await {
                if event_tx.send(event).await.is_err() {
                    break;
                }
            }
        }
    });

    let mut forwards: HashMap<String, RunningForward> = HashMap::new();

    tokio::pin!(disc_rx);
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Ok(cmd) = cmd else { break };
                match cmd {
                    SshCmd::OpenSftpChannel { reply } => {
                        let _ = reply.send(open_sftp_channel(&handle).await);
                    }
                    SshCmd::OpenShellChannel { rows, cols, reply } => {
                        let _ = reply.send(open_shell_channel(&handle, rows, cols).await);
                    }
                    SshCmd::OpenExec { cmd, reply } => {
                        let _ = reply.send(open_exec_channel(&handle, cmd).await);
                    }
                    SshCmd::StartForward { spec } => {
                        start_forward(
                            &mut handle, spec, &mut forwards, &forward_tx, &forward_event_tx,
                        )
                        .await;
                    }
                    SshCmd::StopForward { id } => {
                        stop_forward(&handle, &id, &mut forwards, &forward_event_tx).await;
                    }
                }
            }
            msg = forward_rx.recv() => {
                if let Ok(msg) = msg {
                    handle_forward_msg(
                        &handle, msg, &mut forwards, &forward_tx, &forward_event_tx,
                    )
                    .await;
                }
            }
            _ = &mut disc_rx => {
                tracing::info!(host, "ssh session disconnected by remote");
                break;
            }
        }
    }

    let _ = event_tx.send(SshConnectionEvent::Disconnected).await;
}

// Port forwarding: only `run_connection`'s select loop touches the russh
// handle. Listeners and pumps live in `crate::forward` and reach it through
// `ForwardMsg`.

/// Push a forward's current state to the UI. Unbounded, so this never blocks
/// the loop and never drops.
fn emit_forward(
    event_tx: &async_channel::Sender<SshConnectionEvent>,
    id: &str,
    state: ForwardState,
) {
    if event_tx
        .try_send(SshConnectionEvent::ForwardChanged {
            id: id.to_string(),
            state,
        })
        .is_err()
    {
        tracing::warn!(id, "forward event dropped: the connection is shutting down");
    }
}

/// Bring one forward up and record it, or report why it could not start.
async fn start_forward(
    handle: &mut client::Handle<ClientHandler>,
    spec: ForwardSpec,
    forwards: &mut HashMap<String, RunningForward>,
    forward_tx: &async_channel::Sender<ForwardMsg>,
    event_tx: &async_channel::Sender<SshConnectionEvent>,
) {
    if let Some(running) = forwards.get(&spec.id) {
        // Re-answer so a UI that missed the first event catches up.
        emit_forward(
            event_tx,
            &spec.id,
            ForwardState::Bound {
                bound_port: running.bound_port,
                active: running.active,
            },
        );
        return;
    }

    if !is_loopback_bind(&spec.bind_address) {
        tracing::warn!(
            id = %spec.id, address = %spec.bind_address,
            "forward binds a public address: anyone on the network can use it",
        );
    }

    let bound = match spec.kind {
        ForwardKind::Remote => request_remote_forward(handle, &spec)
            .await
            .map(|p| (p, None)),
        _ => bind_local_listener(&spec).await.map(|(p, l)| (p, Some(l))),
    };

    let (bound_port, listener) = match bound {
        Ok(bound) => bound,
        Err(reason) => {
            tracing::warn!(id = %spec.id, route = %spec.route_label(), reason, "forward failed to start");
            emit_forward(
                event_tx,
                &spec.id,
                ForwardState::Stopped {
                    reason: Some(reason),
                },
            );
            return;
        }
    };

    let running = RunningForward::new(spec.clone(), bound_port);
    if let Some(listener) = listener {
        tokio::task::spawn_local(crate::forward::run_listener(
            spec.clone(),
            listener,
            running.stop_signal(),
            forward_tx.clone(),
        ));
    }
    tracing::info!(
        id = %spec.id, kind = spec.kind.display_label(), route = %spec.route_label(),
        bound_port, "forward started",
    );
    forwards.insert(spec.id.clone(), running);
    emit_forward(
        event_tx,
        &spec.id,
        ForwardState::Bound {
            bound_port,
            active: 0,
        },
    );
}

/// Listen locally for a `-L` / `-D` forward.
async fn bind_local_listener(spec: &ForwardSpec) -> Result<(u16, tokio::net::TcpListener), String> {
    let listener = tokio::net::TcpListener::bind((spec.bind_address.as_str(), spec.bind_port))
        .await
        .map_err(|e| {
            format!(
                "could not listen on {}:{}: {e}",
                spec.bind_address, spec.bind_port
            )
        })?;
    let port = listener
        .local_addr()
        .map(|addr| addr.port())
        .unwrap_or(spec.bind_port);
    Ok((port, listener))
}

/// Ask the server to listen on our behalf for a `-R` forward.
async fn request_remote_forward(
    handle: &mut client::Handle<ClientHandler>,
    spec: &ForwardSpec,
) -> Result<u16, String> {
    let answered = tokio::time::timeout(
        FORWARD_REQUEST_TIMEOUT,
        handle.tcpip_forward(spec.bind_address.clone(), spec.bind_port as u32),
    )
    .await
    .map_err(|_| {
        format!(
            "the server did not answer the listen request within {}s",
            FORWARD_REQUEST_TIMEOUT.as_secs()
        )
    })?;
    let allocated = answered.map_err(|e| match e {
        russh::Error::RequestDenied => {
            "the server refused to listen: check AllowTcpForwarding, and GatewayPorts \
             for a non-loopback address, in its sshd_config"
                .to_string()
        }
        other => other.to_string(),
    })?;
    // russh reports the allocated port only when 0 was requested; otherwise it
    // answers 0 and the port we asked for is the one that is listening.
    Ok(if spec.bind_port == 0 {
        allocated as u16
    } else {
        spec.bind_port
    })
}

/// Take a forward down. Dropping the [`RunningForward`] closes its stop
/// channel, which unwinds the accept loop and every live pump.
async fn stop_forward(
    handle: &client::Handle<ClientHandler>,
    id: &str,
    forwards: &mut HashMap<String, RunningForward>,
    event_tx: &async_channel::Sender<SshConnectionEvent>,
) {
    let Some(running) = forwards.remove(id) else {
        return;
    };
    if running.spec.kind == ForwardKind::Remote {
        let address = running.spec.bind_address.clone();
        let canceled = tokio::time::timeout(
            FORWARD_REQUEST_TIMEOUT,
            handle.cancel_tcpip_forward(address, running.bound_port as u32),
        )
        .await;
        match canceled {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(id, %e, "the server would not cancel the remote forward");
            }
            Err(_) => tracing::warn!(id, "the server never answered the cancel request"),
        }
    }
    tracing::info!(id, route = %running.spec.route_label(), "forward stopped");
    emit_forward(event_tx, id, ForwardState::Stopped { reason: None });
}

/// Everything the forwarding tasks need the russh handle for.
async fn handle_forward_msg(
    handle: &client::Handle<ClientHandler>,
    msg: ForwardMsg,
    forwards: &mut HashMap<String, RunningForward>,
    forward_tx: &async_channel::Sender<ForwardMsg>,
    event_tx: &async_channel::Sender<SshConnectionEvent>,
) {
    match msg {
        ForwardMsg::OpenChannel {
            host,
            port,
            originator,
            reply,
        } => {
            // The open parks until the server answers, and this loop is the
            // whole connection: a filtered target the server waits out would
            // freeze the terminal with it.
            let opened = tokio::time::timeout(
                FORWARD_OPEN_TIMEOUT,
                handle.channel_open_direct_tcpip(
                    host,
                    port as u32,
                    originator.ip().to_string(),
                    originator.port() as u32,
                ),
            )
            .await
            .unwrap_or(Err(russh::Error::ConnectionTimeout));
            let _ = reply.send(opened);
        }
        ForwardMsg::Inbound {
            channel,
            connected_address,
            connected_port,
        } => {
            let Some(running) = match_inbound(forwards, &connected_address, connected_port) else {
                tracing::warn!(
                    connected_address,
                    connected_port,
                    "the server opened a forwarded channel for no forward of ours",
                );
                // `close()` waits for a slot on the session's inbound queue,
                // which the loop must not do: under load that queue is drained
                // by the session task, and this loop is what feeds it.
                tokio::task::spawn_local(async move {
                    let _ = channel.close().await;
                });
                return;
            };
            tokio::task::spawn_local(crate::forward::pump_inbound(
                running.spec.clone(),
                channel,
                running.stop_signal(),
                forward_tx.clone(),
            ));
        }
        ForwardMsg::ConnectionDelta { forward_id, delta } => {
            let Some(running) = forwards.get_mut(&forward_id) else {
                return;
            };
            running.active = running.active.saturating_add_signed(delta);
            emit_forward(
                event_tx,
                &forward_id,
                ForwardState::Bound {
                    bound_port: running.bound_port,
                    active: running.active,
                },
            );
        }
        ForwardMsg::ListenerFailed { forward_id, reason } => {
            if forwards.remove(&forward_id).is_some() {
                emit_forward(
                    event_tx,
                    &forward_id,
                    ForwardState::Stopped {
                        reason: Some(reason),
                    },
                );
            }
        }
    }
}

/// Which `-R` forward an inbound `forwarded-tcpip` channel belongs to. The
/// server echoes the address we asked it to bind, but some normalize it
/// ("localhost" becomes "127.0.0.1"), so the port is the reliable key and the
/// address only breaks a tie between two forwards sharing it.
fn match_inbound<'a>(
    forwards: &'a HashMap<String, RunningForward>,
    address: &str,
    port: u16,
) -> Option<&'a RunningForward> {
    let on_port = || {
        forwards
            .values()
            .filter(|f| f.spec.kind == ForwardKind::Remote && f.bound_port == port)
    };
    on_port()
        .find(|f| f.spec.bind_address == address)
        .or_else(|| on_port().next())
}

async fn open_sftp_channel(
    handle: &client::Handle<ClientHandler>,
) -> Result<(SftpHandle, String), SshError> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| SshError::ChannelOpen(e.to_string()))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| SshError::ChannelOpen(e.to_string()))?;
    let sftp = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| SshError::SftpInit(e.to_string()))?;

    let home_dir = sftp
        .canonicalize(".")
        .await
        .unwrap_or_else(|_| "/".to_string());
    tracing::info!(home_dir, "sftp session opened");

    // Commands must never be silently dropped (a lost Upload/Download is data
    // loss from the user's perspective), so the queue is unbounded. Depth is
    // bounded in practice by what the user can click. Events stay bounded;
    // progress ticks use try_send and are droppable by design.
    let (sftp_cmd_tx, sftp_cmd_rx) = async_channel::unbounded::<SftpCmd>();
    let (sftp_event_tx, sftp_event_rx) = async_channel::bounded::<SftpEvent>(64);

    // spawn_local: SftpSession may not be Send, but we're on a single-threaded runtime
    tokio::task::spawn_local(handle_sftp_commands(sftp, sftp_cmd_rx, sftp_event_tx));

    Ok((
        SftpHandle {
            cmd_tx: sftp_cmd_tx,
            event_rx: sftp_event_rx,
        },
        home_dir,
    ))
}

async fn open_shell_channel(
    handle: &client::Handle<ClientHandler>,
    rows: u16,
    cols: u16,
) -> Result<ShellHandle, SshError> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| SshError::ShellOpen(e.to_string()))?;

    channel
        .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await
        .map_err(|e| SshError::ShellOpen(e.to_string()))?;

    channel
        .request_shell(true)
        .await
        .map_err(|e| SshError::ShellOpen(e.to_string()))?;

    tracing::info!(rows, cols, "shell channel opened");

    let (cmd_tx, cmd_rx) = async_channel::bounded(64);
    let (event_tx, event_rx) = async_channel::bounded(64);

    tokio::task::spawn_local(handle_shell_commands(channel, rows, cols, cmd_rx, event_tx));

    Ok(ShellHandle::new(cmd_tx, event_rx))
}

async fn open_exec_channel(
    handle: &client::Handle<ClientHandler>,
    cmd: String,
) -> Result<ExecHandle, SshError> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| SshError::ChannelOpen(e.to_string()))?;

    channel
        .exec(true, cmd.as_bytes())
        .await
        .map_err(|e| SshError::ChannelOpen(e.to_string()))?;

    tracing::info!("exec channel opened");

    let (stdout_tx, stdout_rx) = async_channel::unbounded::<Vec<u8>>();

    tokio::task::spawn_local(drive_exec_channel(channel, stdout_tx));

    Ok(ExecHandle { stdout_rx })
}

/// Pump one exec channel until the command exits. Dropping the handle closes
/// `stdout_tx`'s receiver, which stops the pump at the next piece of output.
async fn drive_exec_channel(
    mut channel: Channel<client::Msg>,
    stdout_tx: async_channel::Sender<Vec<u8>>,
) {
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => {
                if stdout_tx.send(data.to_vec()).await.is_err() {
                    break;
                }
            }
            ChannelMsg::ExtendedData { data, ext: 1 } => {
                tracing::debug!(stderr = %String::from_utf8_lossy(&data), "exec stderr");
            }
            ChannelMsg::ExitStatus { exit_status } => {
                tracing::debug!(exit_status, "exec exited");
            }
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
}

async fn handle_sftp_commands(
    sftp: SftpSession,
    cmd_rx: async_channel::Receiver<SftpCmd>,
    event_tx: async_channel::Sender<SftpEvent>,
) {
    let sftp = std::rc::Rc::new(sftp);
    let lanes = TransferLanes::new(sftp.clone(), event_tx.clone());
    // Transfers run as their own tasks, so a listing or a rename is answered
    // while a big file is still on its way. SFTP matches replies to requests
    // by id, so both can share the channel.
    let mut transfers = tokio::task::JoinSet::new();

    while let Ok(cmd) = cmd_rx.recv().await {
        reap_finished_transfers(&mut transfers);
        match cmd {
            SftpCmd::ListDir(path) => {
                let event = match list_dir_resolved(&sftp, &path).await {
                    Ok(entries) => SftpEvent::DirListed { path, entries },
                    Err(e) => {
                        tracing::warn!(?e, path, "list_dir failed");
                        SftpEvent::ListFailed {
                            path,
                            error: e.to_string(),
                            is_background: false,
                        }
                    }
                };
                let _ = event_tx.send(event).await;
            }

            SftpCmd::Download {
                remote,
                local,
                id,
                cancel,
            } => {
                let claim = lanes.claim(TransferTarget::Local(local.clone()));
                let track = TransferTrack::new(id, cancel);
                transfers.spawn_local(run_tracked_download(
                    lanes.clone(),
                    claim,
                    remote,
                    local,
                    track,
                ));
            }

            SftpCmd::Upload {
                local,
                remote,
                id,
                cancel,
            } => {
                let claim = lanes.claim(TransferTarget::Remote(remote.clone()));
                let track = TransferTrack::new(id, cancel);
                transfers.spawn_local(run_tracked_upload(
                    lanes.clone(),
                    claim,
                    local,
                    remote,
                    track,
                ));
            }

            SftpCmd::Relay {
                source,
                source_path,
                remote,
                id,
                cancel,
            } => {
                let claim = lanes.claim(TransferTarget::Remote(remote.clone()));
                let track = TransferTrack::new(id, cancel);
                transfers.spawn_local(run_tracked_relay(
                    lanes.clone(),
                    claim,
                    source,
                    source_path,
                    remote,
                    track,
                ));
            }

            // The two below serve a copy that another worker runs. They take
            // no slot and no claim: only the writing side queues, so two
            // copies in opposite directions cannot wait on each other.
            SftpCmd::PlanCopy {
                source,
                target,
                cancel,
                reply,
            } => {
                let sftp = sftp.clone();
                transfers.spawn_local(async move {
                    let _ = reply.send(plan_remote_copy(&sftp, &source, &target, &cancel).await);
                });
            }

            SftpCmd::ReadHead {
                remote,
                limit,
                reply,
            } => {
                let sftp = sftp.clone();
                transfers.spawn_local(async move {
                    let result = read_head(&sftp, &remote, limit, &reply).await;
                    let _ = reply.send(result);
                });
            }

            SftpCmd::ReadFile {
                remote,
                chunks,
                cancel,
            } => {
                let sftp = sftp.clone();
                transfers.spawn_local(async move {
                    read_file_into(&sftp, &remote, &chunks, &cancel).await;
                });
            }

            SftpCmd::DownloadFile {
                remote,
                local,
                reply,
            } => {
                let claim = lanes.claim(TransferTarget::Local(local.clone()));
                let lanes = lanes.clone();
                transfers.spawn_local(async move {
                    claim.wait_for_turn().await;
                    let result =
                        download_with_mtime(&lanes.sftp, &remote, &local, Some(&lanes.event_tx))
                            .await;
                    drop(claim);
                    let _ = reply.send(result);
                });
            }

            SftpCmd::StageFile {
                remote,
                local,
                size,
                reply,
            } => {
                let result = stage_file(&sftp, &remote, &local, size).await;
                let _ = reply.send(result);
            }

            SftpCmd::StageFiles { items, reply } => {
                let _ = reply.send(stage_files_concurrent(&sftp, items).await);
            }

            SftpCmd::UploadFile {
                local,
                remote,
                reply,
            } => {
                let claim = lanes.claim(TransferTarget::Remote(remote.clone()));
                let lanes = lanes.clone();
                transfers.spawn_local(async move {
                    claim.wait_for_turn().await;
                    let result =
                        upload(&lanes.sftp, &local, &remote, Some(&lanes.event_tx), None).await;
                    drop(claim);
                    let _ = reply.send(result.map(|_skipped_links| ()));
                });
            }

            SftpCmd::StatFile { remote, reply } => {
                let result = sftp
                    .metadata(remote)
                    .await
                    .map(|a| a.mtime)
                    .map_err(|e| anyhow::anyhow!("{e}"));
                let _ = reply.send(result);
            }

            SftpCmd::MakeDir { path, reply } => {
                let result = sftp
                    .create_dir(path)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"));
                let _ = reply.send(result);
            }

            SftpCmd::ReadDirBatch { paths, reply } => {
                use futures::stream::StreamExt;

                // A shared reference is Copy, so each future gets its own.
                let sftp_ref = &sftp;
                let results: Vec<(String, anyhow::Result<Vec<FileEntry>>)> =
                    futures::stream::iter(paths.into_iter().map(|path| async move {
                        let result = list_dir(sftp_ref, &path).await;
                        (path, result)
                    }))
                    .buffer_unordered(READDIR_BATCH_CONCURRENCY)
                    .collect()
                    .await;
                let _ = reply.send(results);
            }

            SftpCmd::RemoveFile { path, reply } => {
                let result = sftp
                    .remove_file(path)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"));
                let _ = reply.send(result);
            }

            SftpCmd::RemoveDir { path, reply } => {
                let result = sftp
                    .remove_dir(path)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"));
                let _ = reply.send(result);
            }

            SftpCmd::Rename { from, to, reply } => {
                let result = sftp
                    .rename(from, to)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"));
                let _ = reply.send(result);
            }

            SftpCmd::SetPermissions { path, mode, reply } => {
                // Only the permission bits: any other `Some` field would be
                // written too (size=Some(0) truncates the file).
                let mut attrs = russh_sftp::protocol::FileAttributes::empty();
                attrs.permissions = Some(mode);
                let result = sftp
                    .set_metadata(path, attrs)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"));
                let _ = reply.send(result);
            }

            SftpCmd::PrefetchDirs { paths } => {
                use futures::stream::StreamExt;

                // A shared reference is Copy, so each future gets its own.
                let sftp_ref = &sftp;
                let results: Vec<(String, anyhow::Result<Vec<FileEntry>>)> =
                    futures::stream::iter(paths.into_iter().map(|path| async move {
                        let result = list_dir_resolved(sftp_ref, &path).await;
                        (path, result)
                    }))
                    .buffer_unordered(READDIR_BATCH_CONCURRENCY)
                    .collect()
                    .await;

                for (path, result) in results {
                    match result {
                        Ok(entries) => {
                            tracing::debug!(
                                target: "flint::prefetch",
                                "prefetched {}: {} entries",
                                path, entries.len()
                            );
                            let _ = event_tx
                                .send(SftpEvent::PrefetchComplete {
                                    path: path.clone(),
                                    entries,
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::debug!(
                                target: "flint::prefetch",
                                "prefetch failed for {}: {}",
                                path, e
                            );
                            let _ = event_tx
                                .send(SftpEvent::ListFailed {
                                    path,
                                    error: e.to_string(),
                                    is_background: true,
                                })
                                .await;
                        }
                    }
                }
            }
        }
    }

    // Every handle is gone, but a transfer that was already asked for still
    // runs to its end.
    while let Some(joined) = transfers.join_next().await {
        log_transfer_task_end(joined);
    }
    let _ = event_tx.send(SftpEvent::Disconnected).await;
}

fn reap_finished_transfers(transfers: &mut tokio::task::JoinSet<()>) {
    while let Some(joined) = transfers.try_join_next() {
        log_transfer_task_end(joined);
    }
}

fn log_transfer_task_end(joined: Result<(), tokio::task::JoinError>) {
    if let Err(e) = joined {
        tracing::error!(%e, "a transfer task stopped without finishing");
    }
}

/// The place a transfer writes to.
#[derive(Debug, Clone, PartialEq)]
enum TransferTarget {
    Local(std::path::PathBuf),
    Remote(String),
}

impl TransferTarget {
    /// True when the two write the same path, or one writes inside a folder
    /// the other writes.
    fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(a), Self::Local(b)) => {
                let (a, b) = (fold_local_case(a), fold_local_case(b));
                a.starts_with(&b) || b.starts_with(&a)
            }
            (Self::Remote(a), Self::Remote(b)) => {
                is_remote_path_inside(a, b) || is_remote_path_inside(b, a)
            }
            _ => false,
        }
    }
}

/// macOS and Windows keep `A.txt` and `a.txt` as one file, so two downloads
/// that differ only in case would write the same temporary file. Treating
/// them as one target everywhere only costs a wait.
fn fold_local_case(path: &std::path::Path) -> std::path::PathBuf {
    std::path::PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// True when `path` is `folder` or lies under it.
fn is_remote_path_inside(path: &str, folder: &str) -> bool {
    let path = path.trim_end_matches('/');
    let folder = folder.trim_end_matches('/');
    match path.strip_prefix(folder) {
        Some(rest) => rest.is_empty() || rest.starts_with('/'),
        None => false,
    }
}

/// Targets of the transfers that run or wait, oldest first. Two transfers to
/// the same place would write the same temporary file, so the later one waits
/// for the earlier one, in the order the user asked for them.
#[derive(Default)]
struct TargetQueue {
    next_ticket: u64,
    claims: Vec<(u64, TransferTarget)>,
}

impl TargetQueue {
    fn push(&mut self, target: TransferTarget) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        self.claims.push((ticket, target));
        ticket
    }

    /// True while an older transfer writes to the place `ticket` writes to.
    fn is_blocked(&self, ticket: u64) -> bool {
        let Some((_, target)) = self.claims.iter().find(|(t, _)| *t == ticket) else {
            return false;
        };
        self.claims
            .iter()
            .any(|(other, other_target)| *other < ticket && other_target.overlaps(target))
    }

    fn remove(&mut self, ticket: u64) {
        self.claims.retain(|(t, _)| *t != ticket);
    }
}

/// What the transfer tasks of one SFTP worker share.
#[derive(Clone)]
struct TransferLanes {
    sftp: std::rc::Rc<SftpSession>,
    event_tx: async_channel::Sender<SftpEvent>,
    /// One permit per tracked transfer that runs.
    permits: std::rc::Rc<tokio::sync::Semaphore>,
    targets: std::rc::Rc<std::cell::RefCell<TargetQueue>>,
    target_freed: std::rc::Rc<tokio::sync::Notify>,
}

impl TransferLanes {
    fn new(sftp: std::rc::Rc<SftpSession>, event_tx: async_channel::Sender<SftpEvent>) -> Self {
        Self {
            sftp,
            event_tx,
            permits: std::rc::Rc::new(tokio::sync::Semaphore::new(TRANSFER_CONCURRENCY)),
            targets: Default::default(),
            target_freed: Default::default(),
        }
    }

    /// Take a place in the queue for `target`. Call it when the command
    /// arrives, not inside the task, so the order is the order of the commands.
    fn claim(&self, target: TransferTarget) -> TargetClaim {
        let ticket = self.targets.borrow_mut().push(target);
        TargetClaim {
            ticket,
            targets: self.targets.clone(),
            target_freed: self.target_freed.clone(),
        }
    }
}

/// Wait until a tracked transfer may run: no older transfer writes to the
/// same place and a slot is free. Returns `None` when the user canceled it
/// while it waited.
async fn wait_for_transfer_slot<'a>(
    permits: &'a tokio::sync::Semaphore,
    event_tx: &async_channel::Sender<SftpEvent>,
    claim: &TargetClaim,
    track: &TransferTrack,
) -> Option<tokio::sync::SemaphorePermit<'a>> {
    if !claim.is_blocked() {
        if let Ok(permit) = permits.try_acquire() {
            return Some(permit);
        }
    }
    let _ = event_tx
        .send(SftpEvent::TransferWaiting {
            id: track.id,
            is_waiting: true,
        })
        .await;

    let turn = async {
        claim.wait_for_turn().await;
        permits.acquire().await
    };
    tokio::pin!(turn);
    // Cancel is a plain flag that wakes nobody, so look at it now and then.
    let permit = loop {
        tokio::select! {
            permit = &mut turn => break permit,
            _ = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {
                if track.canceled() {
                    return None;
                }
            }
        }
    };
    if track.canceled() {
        return None;
    }
    let _ = event_tx
        .send(SftpEvent::TransferWaiting {
            id: track.id,
            is_waiting: false,
        })
        .await;
    Some(permit.expect("the transfer semaphore is never closed"))
}

/// A place in the [`TargetQueue`]. Dropping it lets the next transfer to that
/// target go.
struct TargetClaim {
    ticket: u64,
    targets: std::rc::Rc<std::cell::RefCell<TargetQueue>>,
    target_freed: std::rc::Rc<tokio::sync::Notify>,
}

impl TargetClaim {
    fn is_blocked(&self) -> bool {
        self.targets.borrow().is_blocked(self.ticket)
    }

    async fn wait_for_turn(&self) {
        // Nothing else runs between the check and the first poll of
        // `notified` (one thread, no await in between), so no wakeup is lost.
        while self.is_blocked() {
            self.target_freed.notified().await;
        }
    }
}

impl Drop for TargetClaim {
    fn drop(&mut self) {
        self.targets.borrow_mut().remove(self.ticket);
        self.target_freed.notify_waiters();
    }
}

/// Turn the result of a tracked transfer into what the UI is told. `what` and
/// `path` only name the transfer in the log.
fn transfer_outcome(result: anyhow::Result<usize>, what: &str, path: &str) -> TransferOutcome {
    match result {
        Ok(skipped_links) => TransferOutcome::Done { skipped_links },
        // Nothing to clean up after a cancel or a failure: a file is written
        // under a temporary name and only renamed into place when whole, so
        // the target is either untouched or complete. A folder transfer leaves
        // the files that did finish: deleting data here is riskier than
        // leaving it.
        Err(e) if e.is::<TransferCanceled>() => {
            tracing::info!(path, "{what} canceled");
            TransferOutcome::Canceled
        }
        Err(e) => {
            tracing::warn!(?e, path, "{what} failed");
            TransferOutcome::Failed(e.to_string())
        }
    }
}

/// Tell the UI how a tracked transfer ended. `on_done` is the event that makes
/// the pane reload.
async fn report_tracked_transfer(
    event_tx: &async_channel::Sender<SftpEvent>,
    id: u64,
    name: String,
    outcome: TransferOutcome,
    on_done: SftpEvent,
) {
    let follow_up = match &outcome {
        TransferOutcome::Done { .. } => Some(on_done),
        TransferOutcome::Canceled => None,
        TransferOutcome::Failed(error) => Some(SftpEvent::Error(error.clone())),
    };
    let _ = event_tx
        .send(SftpEvent::TransferFinished { id, name, outcome })
        .await;
    if let Some(event) = follow_up {
        let _ = event_tx.send(event).await;
    }
}

async fn run_tracked_download(
    lanes: TransferLanes,
    claim: TargetClaim,
    remote: String,
    local: std::path::PathBuf,
    track: TransferTrack,
) {
    let result = match wait_for_transfer_slot(&lanes.permits, &lanes.event_tx, &claim, &track).await
    {
        Some(_permit) => {
            download(
                &lanes.sftp,
                &remote,
                &local,
                Some(&lanes.event_tx),
                Some(&track),
            )
            .await
        }
        None => Err(anyhow::Error::new(TransferCanceled)),
    };
    // Free the target before the events: a full event queue must not hold up
    // the next transfer.
    drop(claim);
    let outcome = transfer_outcome(result, "download", &remote);
    let name = base_name(&remote).to_string();
    let on_done = SftpEvent::DownloadComplete { local };
    report_tracked_transfer(&lanes.event_tx, track.id, name, outcome, on_done).await;
}

async fn run_tracked_upload(
    lanes: TransferLanes,
    claim: TargetClaim,
    local: std::path::PathBuf,
    remote: String,
    track: TransferTrack,
) {
    let result = match wait_for_transfer_slot(&lanes.permits, &lanes.event_tx, &claim, &track).await
    {
        Some(_permit) => {
            upload(
                &lanes.sftp,
                &local,
                &remote,
                Some(&lanes.event_tx),
                Some(&track),
            )
            .await
        }
        None => Err(anyhow::Error::new(TransferCanceled)),
    };
    drop(claim);
    let outcome = transfer_outcome(result, "upload", &remote);
    let name = base_name(&remote).to_string();
    let on_done = SftpEvent::UploadComplete { remote };
    report_tracked_transfer(&lanes.event_tx, track.id, name, outcome, on_done).await;
}

async fn run_tracked_relay(
    lanes: TransferLanes,
    claim: TargetClaim,
    source: SftpHandle,
    source_path: String,
    remote: String,
    track: TransferTrack,
) {
    let result = match wait_for_transfer_slot(&lanes.permits, &lanes.event_tx, &claim, &track).await
    {
        Some(_permit) => {
            relay(
                &lanes.sftp,
                &source,
                source_path,
                &remote,
                &lanes.event_tx,
                &track,
            )
            .await
        }
        None => Err(anyhow::Error::new(TransferCanceled)),
    };
    drop(claim);
    let outcome = transfer_outcome(result, "copy", &remote);
    let name = base_name(&remote).to_string();
    let on_done = SftpEvent::UploadComplete { remote };
    report_tracked_transfer(&lanes.event_tx, track.id, name, outcome, on_done).await;
}

/// Copy `source_path` on the server behind `source` to `remote` on this one.
/// The other worker walks the tree, this one writes it.
async fn relay(
    sftp: &SftpSession,
    source: &SftpHandle,
    source_path: String,
    remote: &str,
    events: &async_channel::Sender<SftpEvent>,
    track: &TransferTrack,
) -> anyhow::Result<usize> {
    let plan = source
        .plan_copy(source_path, remote.to_string(), track.cancel.clone())
        .await?;
    send_plan(
        sftp,
        plan,
        Some(source),
        RELAY_FILE_CONCURRENCY,
        base_name(remote),
        Some(events),
        Some(track),
    )
    .await
}

/// Run the handshake future under [`HANDSHAKE_TIMEOUT`], but stop the clock
/// while `prompt_pending` is set. That's the host-key dialog waiting on the
/// user, which can legitimately take minutes.
async fn with_handshake_timeout<T>(
    fut: impl std::future::Future<Output = T>,
    prompt_pending: &std::sync::atomic::AtomicBool,
) -> Option<T> {
    use std::sync::atomic::Ordering;
    tokio::pin!(fut);
    let tick = Duration::from_millis(250);
    let mut elapsed = Duration::ZERO;
    loop {
        tokio::select! {
            out = &mut fut => return Some(out),
            _ = tokio::time::sleep(tick) => {
                if !prompt_pending.load(Ordering::Relaxed) {
                    elapsed += tick;
                    if elapsed >= HANDSHAKE_TIMEOUT {
                        return None;
                    }
                }
            }
        }
    }
}

/// A failure on one hop, tagged with the hop that produced it so the UI can
/// name the machine that actually refused.
struct HopFailure {
    entry_id: String,
    error: AuthError,
}

/// A live chain: the target's session plus every jump session it rides on.
struct ConnectedChain {
    session: client::Handle<ClientHandler>,
    disconnect_rx: tokio::sync::oneshot::Receiver<()>,
    /// Held for the session's lifetime. Each jump session runs the transport
    /// its successor's tunnel is carried over; dropping one kills the chain.
    jumps: Vec<client::Handle<ClientHandler>>,
}

/// Authenticate every hop in order. Hop 0 is reached over TCP (optionally
/// through the configured proxy); each later hop over a `direct-tcpip` tunnel
/// opened on the hop before it.
async fn connect_chain(
    route: &SshRoute,
    event_tx: &async_channel::Sender<SshConnectionEvent>,
    forward_tx: &async_channel::Sender<ForwardMsg>,
) -> Result<ConnectedChain, HopFailure> {
    let mut jumps: Vec<client::Handle<ClientHandler>> = Vec::new();

    for (index, hop) in route.jumps.iter().enumerate() {
        let previous = if index == 0 {
            None
        } else {
            Some((&route.jumps[index - 1], &jumps[index - 1]))
        };
        // A jump's own disconnect signal is dropped on purpose: when a bastion
        // goes away the tunnel above it ends, and the hop behind it reports the
        // loss: one signal for the whole chain.
        let (session, _disconnect_rx) =
            connect_hop(hop, previous, route.proxy.as_ref(), event_tx, None).await?;
        tracing::info!(hop = index, host = %hop.host, user = %hop.username, "jump hop authenticated");
        jumps.push(session);
    }

    let previous = jumps
        .last()
        .map(|session| (&route.jumps[jumps.len() - 1], session));
    let (session, disconnect_rx) = connect_hop(
        &route.target,
        previous,
        route.proxy.as_ref(),
        event_tx,
        Some(forward_tx),
    )
    .await?;

    Ok(ConnectedChain {
        session,
        disconnect_rx,
        jumps,
    })
}

/// Reach one hop: over TCP when it is the first, otherwise through a tunnel on
/// the hop before it.
async fn connect_hop(
    hop: &SshHop,
    previous: Option<(&SshHop, &client::Handle<ClientHandler>)>,
    proxy: Option<&ResolvedProxy>,
    event_tx: &async_channel::Sender<SshConnectionEvent>,
    forward_tx: Option<&async_channel::Sender<ForwardMsg>>,
) -> Result<
    (
        client::Handle<ClientHandler>,
        tokio::sync::oneshot::Receiver<()>,
    ),
    HopFailure,
> {
    match previous {
        None => {
            let stream = connect_tcp(&hop.host, hop.port, proxy)
                .await
                .map_err(|error| hop.failed(error))?;
            connect_and_auth(hop, stream, event_tx.clone(), forward_tx.cloned())
                .await
                .map_err(|error| hop.failed(error))
        }
        Some((jump_hop, jump_session)) => {
            // A tunnel that never opens is the bastion refusing, not the hop
            // behind it. Tag it so the UI names the machine that said no.
            let stream = open_tunnel(jump_hop, jump_session, hop)
                .await
                .map_err(|error| jump_hop.failed(error))?;
            connect_and_auth(hop, stream, event_tx.clone(), forward_tx.cloned())
                .await
                .map_err(|error| hop.failed(error))
        }
    }
}

/// TCP connect (direct, or via a proxy) with its own (shorter) timeout,
/// separate from the handshake: an unreachable host/proxy fails here instead
/// of hanging the SSH thread. The whole proxy handshake is inside the timeout
/// so a dead proxy can't stall it either.
async fn connect_tcp(
    host: &str,
    port: u16,
    proxy: Option<&ResolvedProxy>,
) -> Result<tokio::net::TcpStream, AuthError> {
    tokio::time::timeout(TCP_CONNECT_TIMEOUT, async {
        match proxy {
            None => tokio::net::TcpStream::connect((host, port))
                .await
                .map_err(|e| AuthError::ConnectionFailed(e.to_string())),
            Some(p) => connect_via_proxy(p, host, port).await,
        }
    })
    .await
    .map_err(|_| {
        AuthError::ConnectionFailed(format!(
            "timed out after {}s",
            TCP_CONNECT_TIMEOUT.as_secs()
        ))
    })?
}

/// Ask the jump host to open a `direct-tcpip` channel to the next hop and hand
/// back the resulting byte stream. Only an explicit "forwarding is not allowed"
/// is permanent. Everything else (target still booting, port filtered, the
/// bastion's own link dying) is the transient case the reconnect ladder exists
/// for, so it maps onto `ConnectionFailed`.
async fn open_tunnel(
    jump_hop: &SshHop,
    jump_session: &client::Handle<ClientHandler>,
    next: &SshHop,
) -> Result<russh::ChannelStream<client::Msg>, AuthError> {
    // channel_open_direct_tcpip parks until the server confirms the channel, so
    // a bastion that never answers must not hang the SSH thread.
    let opened = tokio::time::timeout(
        TCP_CONNECT_TIMEOUT,
        jump_session.channel_open_direct_tcpip(next.host.clone(), next.port as u32, "127.0.0.1", 0),
    )
    .await;

    let channel = match opened {
        Err(_) => {
            return Err(AuthError::ConnectionFailed(format!(
                "jump host '{}' did not open a tunnel within {}s",
                jump_hop.display_name(),
                TCP_CONNECT_TIMEOUT.as_secs()
            )));
        }
        Ok(Err(russh::Error::ChannelOpenFailure(
            russh::ChannelOpenFailure::AdministrativelyProhibited,
        ))) => return Err(AuthError::TunnelRefused),
        Ok(Err(e)) => {
            return Err(AuthError::ConnectionFailed(format!(
                "jump host '{}' could not reach {}:{}: {e}",
                jump_hop.display_name(),
                next.host,
                next.port
            )));
        }
        Ok(Ok(channel)) => channel,
    };
    Ok(channel.into_stream())
}

/// Run the SSH handshake and authentication over an already-open stream. The
/// stream is a `TcpStream` for the first hop and a tunnel for every later one,
/// so this stays generic: two monomorphizations, no boxing.
async fn connect_and_auth<S>(
    hop: &SshHop,
    stream: S,
    event_tx: async_channel::Sender<SshConnectionEvent>,
    forward_tx: Option<async_channel::Sender<ForwardMsg>>,
) -> Result<
    (
        client::Handle<ClientHandler>,
        tokio::sync::oneshot::Receiver<()>,
    ),
    AuthError,
>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (disconnect_tx, disconnect_rx) = tokio::sync::oneshot::channel();
    // A dead link is found by keepalive: a probe every 20s, given up after 6
    // unanswered ones (about 2 min), so a short network stall does not kill
    // the session. The inactivity timeout stays off, or it would close a
    // healthy connection that is only idle.
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(20)),
        keepalive_max: 6,
        inactivity_timeout: None,
        ..Default::default()
    });

    let prompt_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler = ClientHandler {
        disconnect_tx: Some(disconnect_tx),
        expected_host_key: hop.expected_host_key.clone(),
        entry_id: hop.entry_id.clone(),
        host: hop.host.clone(),
        port: hop.port,
        event_tx: event_tx.clone(),
        forward_tx,
        prompt_pending: prompt_pending.clone(),
    };

    // The handshake includes check_server_key, which may park on a user prompt;
    // with_handshake_timeout suspends its clock for that.
    let connect_result = with_handshake_timeout(
        client::connect_stream(config, stream, handler),
        &prompt_pending,
    )
    .await
    .ok_or_else(|| {
        AuthError::ConnectionFailed(format!(
            "SSH handshake timed out after {}s",
            HANDSHAKE_TIMEOUT.as_secs()
        ))
    })?;

    let mut session = connect_result.map_err(|e| {
        // check_server_key returning false surfaces as russh's UnknownKey:
        // the user rejected the host key, which must not be retried.
        match e.downcast_ref::<russh::Error>() {
            Some(russh::Error::UnknownKey) => AuthError::HostKeyRejected,
            _ => AuthError::ConnectionFailed(e.to_string()),
        }
    })?;

    // Auth can park on a keyboard-interactive prompt (2FA codes), so it shares
    // the prompt-aware timeout with the handshake instead of a plain one.
    let auth_result = with_handshake_timeout(
        authenticate(&mut session, hop, &event_tx, &prompt_pending),
        &prompt_pending,
    )
    .await
    .ok_or_else(|| {
        AuthError::ConnectionFailed(format!(
            "authentication timed out after {}s",
            HANDSHAKE_TIMEOUT.as_secs()
        ))
    })?;
    auth_result?;

    Ok((session, disconnect_rx))
}

/// Open a TCP stream to `host:port` tunnelled through the given proxy. Both
/// branches return a plain `TcpStream` (raw byte tunnel after the proxy
/// handshake), so the SSH transport above is identical to a direct connect.
/// The target hostname is sent to the proxy for *remote* DNS, never resolved
/// locally, so the host's name doesn't leak onto the local network.
async fn connect_via_proxy(
    proxy: &ResolvedProxy,
    host: &str,
    port: u16,
) -> Result<tokio::net::TcpStream, AuthError> {
    match proxy {
        ResolvedProxy::Socks5 { addr, auth } => {
            use tokio_socks::tcp::Socks5Stream;
            let stream = match auth {
                Some((user, pass)) => {
                    Socks5Stream::connect_with_password(addr.as_str(), (host, port), user, pass)
                        .await
                }
                None => Socks5Stream::connect(addr.as_str(), (host, port)).await,
            }
            .map_err(|e| AuthError::ConnectionFailed(format!("SOCKS5 proxy: {e}")))?;
            Ok(stream.into_inner())
        }
        ResolvedProxy::Http { addr, auth } => {
            connect_http_proxy(addr, auth.as_ref(), host, port).await
        }
    }
}

/// Manual HTTP CONNECT tunnel. Reads the response one byte at a time up to the
/// `\r\n\r\n` header terminator so no SSH bytes are accidentally consumed past
/// it (a bulk read could swallow the start of the server's SSH banner and
/// corrupt the handshake).
async fn connect_http_proxy(
    proxy_addr: &str,
    auth: Option<&(String, String)>,
    host: &str,
    port: u16,
) -> Result<tokio::net::TcpStream, AuthError> {
    use base64::Engine;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut stream = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .map_err(|e| AuthError::ConnectionFailed(format!("HTTP proxy: {e}")))?;

    let authority = connect_authority(host, port);
    let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some((user, pass)) = auth {
        let creds = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        req.push_str(&format!("Proxy-Authorization: Basic {creds}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| AuthError::ConnectionFailed(format!("HTTP proxy: {e}")))?;

    let mut header: Vec<u8> = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        let n = stream
            .read(&mut byte)
            .await
            .map_err(|e| AuthError::ConnectionFailed(format!("HTTP proxy: {e}")))?;
        if n == 0 {
            return Err(AuthError::ConnectionFailed(
                "HTTP proxy closed the connection".to_string(),
            ));
        }
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() > 8192 {
            return Err(AuthError::ConnectionFailed(
                "HTTP proxy response header too large".to_string(),
            ));
        }
    }

    let text = String::from_utf8_lossy(&header);
    let status_line = text.lines().next().unwrap_or("");
    let code = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok());
    match code {
        Some(200) => Ok(stream),
        Some(407) => Err(AuthError::ConnectionFailed(
            "HTTP proxy authentication failed (407)".to_string(),
        )),
        Some(_) => Err(AuthError::ConnectionFailed(format!(
            "HTTP proxy refused tunnel: {status_line}"
        ))),
        None => Err(AuthError::ConnectionFailed(
            "malformed HTTP proxy response".to_string(),
        )),
    }
}

/// `host:port` for a CONNECT request line. An IPv6 address needs brackets,
/// or its own colons run into the port.
fn connect_authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        return format!("[{host}]:{port}");
    }
    format!("{host}:{port}")
}

/// Run the credential's primary auth method; when the server turns it down,
/// fall back to keyboard-interactive. That fallback is what makes both
/// PAM-only servers (`PasswordAuthentication no` + kbd-interactive) and 2FA
/// setups (`AuthenticationMethods publickey,keyboard-interactive`) work: in
/// both cases the primary attempt reports "failure" while the server keeps the
/// keyboard-interactive method open on the same session.
async fn authenticate(
    session: &mut client::Handle<ClientHandler>,
    hop: &SshHop,
    event_tx: &async_channel::Sender<SshConnectionEvent>,
    prompt_pending: &std::sync::atomic::AtomicBool,
) -> Result<(), AuthError> {
    let username = hop.username.as_str();
    let primary_err = match &hop.credential {
        AuthCredential::Password(password) => {
            let ok = session
                .authenticate_password(username, password.as_str())
                .await
                .map_err(|e| AuthError::SshProtocol(e.to_string()))?;
            if ok {
                return Ok(());
            }
            AuthError::WrongPassword
        }
        AuthCredential::PrivateKey { pem, passphrase } => {
            let had_passphrase = passphrase.as_ref().map(|p| !p.is_empty()).unwrap_or(false);
            let pass_str = passphrase
                .as_ref()
                .filter(|p| !p.is_empty())
                .map(|p| p.as_str());

            let key_pair = keys::decode_secret_key(pem.as_str(), pass_str)
                .map_err(|e| classify_key_error(e, had_passphrase))?;

            let ok = session
                .authenticate_publickey(username, Arc::new(key_pair))
                .await
                .map_err(|e| AuthError::SshProtocol(e.to_string()))?;
            if ok {
                return Ok(());
            }
            AuthError::KeyRejected(username.to_string())
        }
        AuthCredential::Agent => match agent_auth(session, username).await {
            Ok(()) => return Ok(()),
            // No agent, an empty agent, or keys the server turned down. All of
            // them continue with keyboard-interactive, like `ssh` does: an
            // imported ssh config host uses the agent by default, and it may
            // be a password-only server. If the server offers no prompt, this
            // error is still the one the user sees.
            Err(e) => {
                tracing::debug!(error = %e, "agent auth failed, trying keyboard-interactive");
                e
            }
        },
    };
    keyboard_interactive(session, hop, event_tx, prompt_pending, primary_err).await
}

/// Offer every identity the system SSH agent holds, first accepted wins.
/// Signing happens inside the agent; no key material enters this process.
async fn agent_auth(
    session: &mut client::Handle<ClientHandler>,
    username: &str,
) -> Result<(), AuthError> {
    let mut agent = connect_agent().await?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| AuthError::AgentUnavailable(format!("agent did not respond: {e}")))?;
    if identities.is_empty() {
        return Err(AuthError::AgentNoKeys);
    }

    let total = identities.len();
    for key in identities {
        let fingerprint = format!("SHA256:{}", key.fingerprint());
        let (returned_agent, result) = session.authenticate_future(username, key, agent).await;
        agent = returned_agent;
        match result {
            Ok(true) => {
                tracing::info!(fingerprint, "agent key accepted");
                return Ok(());
            }
            Ok(false) => tracing::debug!(fingerprint, "agent key rejected by server"),
            // Signing failed (key removed mid-flight, agent prompt declined):
            // move on to the next identity rather than aborting.
            Err(e) => tracing::warn!(fingerprint, %e, "agent signing failed"),
        }
    }
    Err(AuthError::AgentKeysRejected(total))
}

#[cfg(windows)]
async fn connect_agent() -> Result<
    keys::agent::client::AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>,
    AuthError,
> {
    use tokio::net::windows::named_pipe::ClientOptions;
    // The pipe the Win32-OpenSSH "OpenSSH Authentication Agent" service
    // listens on. Modern Pageant (PuTTY 0.78 and later) can expose it too.
    const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";
    let pipe = ClientOptions::new().open(OPENSSH_AGENT_PIPE).map_err(|e| {
        AuthError::AgentUnavailable(format!(
            "{e} (is the 'OpenSSH Authentication Agent' service running?)"
        ))
    })?;
    Ok(keys::agent::client::AgentClient::connect(pipe))
}

#[cfg(unix)]
async fn connect_agent(
) -> Result<keys::agent::client::AgentClient<tokio::net::UnixStream>, AuthError> {
    keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|e| AuthError::AgentUnavailable(format!("{e} (is SSH_AUTH_SOCK set?)")))
}

/// Drive a keyboard-interactive exchange. A lone hidden "Password:" challenge
/// is PAM asking for the password we already hold, so it's answered silently,
/// but only once, so a wrong stored password fails instead of looping. Every
/// other challenge is surfaced to the user via [`SshConnectionEvent::AuthPrompt`].
async fn keyboard_interactive(
    session: &mut client::Handle<ClientHandler>,
    hop: &SshHop,
    event_tx: &async_channel::Sender<SshConnectionEvent>,
    prompt_pending: &std::sync::atomic::AtomicBool,
    primary_err: AuthError,
) -> Result<(), AuthError> {
    use client::KeyboardInteractiveAuthResponse as Kbd;
    use std::sync::atomic::Ordering;

    let mut response = match session
        .authenticate_keyboard_interactive_start(hop.username.as_str(), None)
        .await
    {
        Ok(r) => r,
        // The session is already gone (the server offered no method besides
        // the one that just failed), so the primary failure is the one to report.
        Err(_) => return Err(primary_err),
    };

    let mut stored_password = match &hop.credential {
        AuthCredential::Password(p) => Some(p.clone()),
        AuthCredential::PrivateKey { .. } | AuthCredential::Agent => None,
    };
    let mut user_was_prompted = false;

    loop {
        let (name, instructions, prompts) = match response {
            Kbd::Success => return Ok(()),
            Kbd::Failure => {
                return Err(if user_was_prompted {
                    AuthError::KbdInteractiveRejected
                } else {
                    primary_err
                });
            }
            Kbd::InfoRequest {
                name,
                instructions,
                prompts,
            } => (name, instructions, prompts),
        };

        let answers: Vec<Zeroizing<String>> = if prompts.is_empty() {
            // Servers may send empty info requests; acknowledge and move on.
            Vec::new()
        } else if prompts.len() == 1
            && !prompts[0].echo
            && asks_for_stored_password(&prompts[0].prompt)
            && stored_password.is_some()
        {
            vec![stored_password.take().expect("guarded by is_some above")]
        } else {
            user_was_prompted = true;
            let n = prompts.len();
            let fields = prompts
                .into_iter()
                .map(|p| AuthPromptField {
                    text: p.prompt,
                    echo: p.echo,
                })
                .collect();
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

            prompt_pending.store(true, Ordering::Relaxed);
            let sent = event_tx
                .send(SshConnectionEvent::AuthPrompt(AuthPrompt {
                    entry_id: hop.entry_id.clone(),
                    name,
                    instructions,
                    fields,
                    reply: reply_tx,
                }))
                .await;
            let answer = if sent.is_ok() {
                reply_rx.await.ok().flatten()
            } else {
                None
            };
            prompt_pending.store(false, Ordering::Relaxed);

            match answer {
                Some(mut a) => {
                    // The protocol requires exactly one answer per prompt.
                    a.resize(n, Zeroizing::new(String::new()));
                    a
                }
                None => {
                    // The russh session task is parked waiting for a response:
                    // unblock it with empty answers before bailing out.
                    let _ = session
                        .authenticate_keyboard_interactive_respond(vec![String::new(); n])
                        .await;
                    return Err(AuthError::Canceled);
                }
            }
        };

        response = session
            // russh takes plain strings, so this last copy is not wiped.
            .authenticate_keyboard_interactive_respond(
                answers.iter().map(|answer| answer.to_string()).collect(),
            )
            .await
            .map_err(|e| AuthError::SshProtocol(e.to_string()))?;
    }
}

/// True for a prompt the stored password answers. A "New password:" prompt
/// (an expired password) also has the word in it, and answering that one would
/// set the password instead of sending it.
fn asks_for_stored_password(prompt: &str) -> bool {
    let prompt = prompt.to_ascii_lowercase();
    prompt.contains("password") && !prompt.contains("new")
}

/// Why a stored private key can't be decoded as-is. Both cases mean the UI
/// should ask the user for a passphrase before attempting to connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyIssue {
    /// The key is encrypted and no passphrase is available.
    MissingPassphrase,
    /// A passphrase was supplied but doesn't decrypt the key.
    WrongPassphrase,
}

/// `true` when the PEM only decodes with a passphrase.
pub fn key_is_encrypted(pem: &str) -> bool {
    matches!(
        check_key_passphrase(pem, None),
        Some(KeyIssue::MissingPassphrase)
    )
}

/// `true` when the PEM decodes as it is, with no passphrase.
pub fn key_decodes_without_passphrase(pem: &str) -> bool {
    keys::decode_secret_key(pem, None).is_ok()
}

/// Try to decode a private key with the given (possibly empty) passphrase.
/// `None` = decodes fine, or fails for a non-passphrase reason. In the latter
/// case the save/auth path reports the format error with full context.
pub fn check_key_passphrase(pem: &str, passphrase: Option<&str>) -> Option<KeyIssue> {
    let pass = passphrase.filter(|p| !p.is_empty());
    match keys::decode_secret_key(pem, pass) {
        Ok(_) => None,
        Err(keys::Error::KeyIsEncrypted) => Some(KeyIssue::MissingPassphrase),
        Err(_) if pass.is_some() => Some(KeyIssue::WrongPassphrase),
        Err(_) => None,
    }
}

fn classify_key_error(e: keys::Error, had_passphrase: bool) -> AuthError {
    match e {
        keys::Error::KeyIsEncrypted => AuthError::PassphraseRequired,
        keys::Error::CouldNotReadKey => {
            AuthError::InvalidPemFormat("unrecognized key format (is this a PEM file?)".to_string())
        }
        _ if had_passphrase => AuthError::WrongPassphrase,
        e => AuthError::InvalidPemFormat(e.to_string()),
    }
}

/// List `path` for a pane: [`list_dir`] plus, for every symlink, where it
/// points and whether that is a directory. The recursive walks (delete,
/// folder download) keep using the plain [`list_dir`]: they never follow
/// links, so they should not pay for resolving them.
async fn list_dir_resolved(sftp: &SftpSession, path: &str) -> anyhow::Result<Vec<FileEntry>> {
    let mut entries = list_dir(sftp, path).await?;
    resolve_links(sftp, path, &mut entries).await;
    sort_listing(&mut entries);
    Ok(entries)
}

/// Fill the link fields for the symlinks in `entries`.
/// A broken link, a loop or a target we may not stat stays a plain
/// link; none of them fails the listing.
async fn resolve_links(sftp: &SftpSession, dir: &str, entries: &mut [FileEntry]) {
    use futures::stream::{self, StreamExt as _};

    let links: Vec<(usize, String)> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.kind == EntryKind::Symlink)
        .map(|(ix, entry)| (ix, format!("{}/{}", dir.trim_end_matches('/'), entry.name)))
        .collect();
    if links.is_empty() {
        return;
    }

    let resolved: Vec<(usize, Option<String>, Option<LinkTargetStat>)> =
        stream::iter(links.into_iter().map(|(ix, path)| async move {
            let (target, meta) = futures::join!(sftp.read_link(path.clone()), sftp.metadata(path));
            let stat = meta.ok().map(|m| LinkTargetStat {
                is_dir: matches!(m.file_type(), russh_sftp::protocol::FileType::Dir),
                size: m.size,
                mtime: m.mtime,
            });
            (ix, target.ok(), stat)
        }))
        .buffer_unordered(LINK_RESOLVE_CONCURRENCY)
        .collect()
        .await;

    for (ix, target, stat) in resolved {
        entries[ix].set_link_target(target, stat);
    }
}

/// True when a name from a server's listing is one path component. The
/// server picks these names, and a hostile one can send `../x` or an absolute
/// path to make a download land outside the folder the user chose.
fn is_plain_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// [`is_plain_entry_name`], plus what only this machine reads as a path: on
/// Windows a backslash or a drive prefix. Such a name is fine to show, since
/// a Unix server may really have it, but not to join onto a local folder.
pub(crate) fn is_single_local_component(name: &str) -> bool {
    use std::path::{Component, Path};

    if !is_plain_entry_name(name) || (cfg!(windows) && name.contains('\\')) {
        return false;
    }
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

async fn list_dir(sftp: &SftpSession, path: &str) -> anyhow::Result<Vec<FileEntry>> {
    let read_dir = sftp.read_dir(path).await?;
    let mut entries: Vec<FileEntry> = read_dir
        .filter(|e| {
            let is_plain = is_plain_entry_name(&e.file_name());
            if !is_plain {
                tracing::warn!(path, name = %e.file_name(), "dropped a listing entry: its name is a path");
            }
            is_plain
        })
        .map(|e| {
            let meta = e.metadata();
            // readdir reports lstat-style attrs, so a symlink classifies as
            // Symlink (and `is_dir` stays false) even when it points at a dir.
            let kind = match meta.file_type() {
                russh_sftp::protocol::FileType::Dir => EntryKind::Folder,
                russh_sftp::protocol::FileType::Symlink => EntryKind::Symlink,
                russh_sftp::protocol::FileType::File => EntryKind::File,
                russh_sftp::protocol::FileType::Other => EntryKind::Other,
            };
            FileEntry {
                name: e.file_name(),
                is_dir: meta.is_dir(),
                size: meta.size.unwrap_or(0),
                mtime: meta.mtime.unwrap_or(0),
                permissions: meta.permissions,
                kind,
                link_target: None,
                is_link_to_dir: false,
                is_link_broken: false,
            }
        })
        .collect();

    sort_listing(&mut entries);
    Ok(entries)
}

async fn download_with_mtime(
    sftp: &SftpSession,
    remote: &str,
    local: &std::path::Path,
    events: Option<&async_channel::Sender<SftpEvent>>,
) -> anyhow::Result<DownloadResult> {
    let mtime = sftp.metadata(remote).await.ok().and_then(|a| a.mtime);
    download(sftp, remote, local, events, None).await?;
    Ok(DownloadResult { mtime })
}

/// Fetch a file of known size, stopping as soon as that many bytes have
/// arrived rather than asking the server to confirm the end. A file that grew
/// since the listing is copied short, which is why the caller re-checks the
/// size before trusting the copy.
async fn stage_file(
    sftp: &SftpSession,
    remote: &str,
    local: &std::path::Path,
    size: u64,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut remote_file = sftp.open(remote).await?;
    let mut local_file = tokio::fs::File::create(local)
        .await
        .map_err(|e| anyhow::anyhow!("create {}: {e}", local.display()))?;
    let mut buf = vec![0u8; TRANSFER_CHUNK];
    let mut written = 0u64;
    while written < size {
        let n = remote_file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        local_file
            .write_all(&buf[..n])
            .await
            .map_err(|e| anyhow::anyhow!("write {}: {e}", local.display()))?;
        written += n as u64;
    }
    local_file
        .flush()
        .await
        .map_err(|e| anyhow::anyhow!("flush {}: {e}", local.display()))?;
    Ok(())
}

/// How many staging fetches are in flight at once. These are speculative, so
/// the point is to finish the batch in about one file's worth of latency
/// without monopolising the channel they share with directory prefetching.
const STAGE_CONCURRENCY: usize = 8;

/// Fetch a batch of known-size files, returning the remote paths that arrived.
/// A file that fails is dropped from the result rather than failing the batch:
/// this is speculative work, and one unreadable file should not cost the rest
/// their head start.
async fn stage_files_concurrent(sftp: &SftpSession, items: Vec<StageItem>) -> Vec<String> {
    use futures::stream::{self, StreamExt as _};

    stream::iter(items)
        .map(|item| async move {
            match stage_file(sftp, &item.remote, &item.local, item.size).await {
                Ok(()) => Some(item.remote),
                Err(e) => {
                    tracing::debug!(target: "flint::dragout", remote = item.remote, "staging skipped: {e}");
                    let _ = tokio::fs::remove_file(&item.local).await;
                    None
                }
            }
        })
        .buffer_unordered(STAGE_CONCURRENCY)
        .filter_map(|done| async move { done })
        .collect()
        .await
}

/// Download `remote` to `local`, the mirror of [`upload`]. If `remote` is a
/// directory it is mirrored recursively (drag/drop hands us whole folders);
/// plain files take the fast path. Without the directory branch, opening a
/// folder as a file fails with a bare SSH_FX_FAILURE ("Failure").
/// Returns how many symlinks a folder download skipped (0 for a single file).
async fn download(
    sftp: &SftpSession,
    remote: &str,
    local: &std::path::Path,
    events: Option<&async_channel::Sender<SftpEvent>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<usize> {
    let meta = sftp.metadata(remote).await.ok();
    if meta.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
        return download_dir(sftp, remote, local, events, track).await;
    }
    let total = meta.and_then(|m| m.size).unwrap_or(0);
    let reporter =
        events.map(|tx| TransferReporter::new(tx, track.map(|t| t.id), base_name(remote), total));
    download_file_streaming(sftp, remote, local, reporter.as_ref(), track).await?;
    Ok(0)
}

/// One remote file to fetch as part of a folder download.
struct DownloadItem {
    remote: String,
    local: std::path::PathBuf,
    size: u64,
}

/// Mirror a remote directory tree into `local`. Two phases: first walk the tree
/// (listing each level concurrently) to learn every file and the total byte
/// count, then fetch the files with [`FOLDER_FILE_CONCURRENCY`] in flight at once.
/// Progress is aggregate (the bytes of all files, counted chunk by chunk,
/// against the whole-tree total), so the bar reflects the folder and keeps
/// moving through a large file.
/// Symlinks are not followed; the count of skipped ones is returned.
async fn download_dir(
    sftp: &SftpSession,
    remote: &str,
    local: &std::path::Path,
    events: Option<&async_channel::Sender<SftpEvent>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<usize> {
    use futures::stream::{self, StreamExt as _, TryStreamExt as _};

    // Phase 1: enumerate. Breadth-first; each level's directories are listed
    // concurrently (capped at [`READDIR_BATCH_CONCURRENCY`] in flight) so a deep
    // tree costs ~one round-trip per level instead of one per directory, without
    // flooding the single SFTP channel on a very wide level.
    let mut files: Vec<DownloadItem> = Vec::new();
    let mut skipped_links = 0usize;
    let mut dirs: Vec<std::path::PathBuf> = vec![local.to_path_buf()];
    let mut frontier: Vec<(String, std::path::PathBuf)> = vec![(
        remote.trim_end_matches('/').to_string(),
        local.to_path_buf(),
    )];
    while !frontier.is_empty() {
        if track.map(|t| t.canceled()).unwrap_or(false) {
            return Err(anyhow::Error::new(TransferCanceled));
        }
        let listings: Vec<(String, std::path::PathBuf, anyhow::Result<Vec<FileEntry>>)> =
            stream::iter(frontier)
                .map(|(rdir, ldir)| async move {
                    let entries = list_dir(sftp, &rdir).await;
                    (rdir, ldir, entries)
                })
                .buffer_unordered(READDIR_BATCH_CONCURRENCY)
                .collect()
                .await;
        let mut next: Vec<(String, std::path::PathBuf)> = Vec::new();
        for (rdir, ldir, listing) in listings {
            for entry in listing? {
                let child_remote = format!("{}/{}", rdir, entry.name);
                if !is_single_local_component(&entry.name) {
                    tracing::warn!(
                        remote = %child_remote,
                        "skipped during download: the name would leave the local folder"
                    );
                    continue;
                }
                let child_local = ldir.join(&entry.name);
                // Classify off `kind` (exact S_IFMT), not `is_dir`: the latter is
                // a bit-containment test that is also true for sockets/block
                // devices, so listing one as a directory would abort the walk.
                if matches!(entry.kind, EntryKind::Folder) {
                    dirs.push(child_local.clone());
                    next.push((child_remote, child_local));
                } else if matches!(entry.kind, EntryKind::File) {
                    files.push(DownloadItem {
                        remote: child_remote,
                        local: child_local,
                        size: entry.size,
                    });
                } else {
                    if entry.kind == EntryKind::Symlink {
                        skipped_links += 1;
                    }
                    tracing::warn!(
                        remote = %child_remote,
                        "skipped during download: not a regular file or directory (symlink/special)"
                    );
                }
            }
        }
        frontier = next;
    }

    // Create every directory up front (parents included via create_dir_all), so
    // the concurrent phase never races two files creating the same parent, and
    // empty remote dirs still land locally.
    for dir in &dirs {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| anyhow::anyhow!("create dir {}: {e}", dir.display()))?;
    }

    // Phase 2: fetch files concurrently. They all feed one reporter, so the
    // UI sees one figure for the folder.
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let reporter = events
        .map(|tx| TransferReporter::new(tx, track.map(|t| t.id), base_name(remote), total_bytes));
    let reporter = reporter.as_ref();

    // Publish the total up front so the bar shows the real size while the
    // first files start, instead of sitting at 0/0.
    if let Some(reporter) = reporter {
        reporter.emit();
    }

    stream::iter(files.into_iter().map(Ok::<_, anyhow::Error>))
        .try_for_each_concurrent(FOLDER_FILE_CONCURRENCY, |item| async move {
            if track.map(|t| t.canceled()).unwrap_or(false) {
                return Err(anyhow::Error::new(TransferCanceled));
            }
            download_file_streaming(sftp, &item.remote, &item.local, reporter, track)
                .await
                .map_err(|e| {
                    if e.is::<TransferCanceled>() {
                        e
                    } else {
                        anyhow::anyhow!("download {}: {e}", item.remote)
                    }
                })
        })
        .await?;
    Ok(skipped_links)
}

/// Suffix of the temporary file a transfer writes before it replaces the
/// real one.
const PART_SUFFIX: &str = ".flint-part";
/// Suffix the replaced remote file carries for the moment of the swap.
const OLD_SUFFIX: &str = ".flint-old";

fn local_part_path(target: &std::path::Path) -> std::path::PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(PART_SUFFIX);
    std::path::PathBuf::from(name)
}

/// Deletes a file this transfer created unless the transfer finished. It runs
/// on drop, so it also covers a transfer whose future is dropped midway (a
/// sibling file of a folder download failed).
struct RemoveUnlessFinished {
    path: std::path::PathBuf,
    is_finished: bool,
}

impl Drop for RemoveUnlessFinished {
    fn drop(&mut self) {
        if !self.is_finished {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The local file a download really writes.
struct LocalTarget {
    path: std::path::PathBuf,
    /// Permissions of the file being replaced; `None` when there is none.
    permissions: Option<std::fs::Permissions>,
}

/// Work out which file a download to `local` replaces, and refuse the ones it
/// must not replace. A link is followed, so the download lands in the file the
/// link points at and the link stays.
async fn local_write_target(local: &std::path::Path) -> anyhow::Result<LocalTarget> {
    let is_link = tokio::fs::symlink_metadata(local)
        .await
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false);
    let path = if is_link {
        tokio::fs::canonicalize(local)
            .await
            .unwrap_or_else(|_| local.to_path_buf())
    } else {
        local.to_path_buf()
    };
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return Ok(LocalTarget {
            path,
            permissions: None,
        });
    };
    if !meta.is_file() {
        anyhow::bail!("{} exists and is not a regular file", path.display());
    }
    // A rename would replace a file that an in-place write could not open.
    if meta.permissions().readonly() {
        anyhow::bail!("{} is read-only", path.display());
    }
    Ok(LocalTarget {
        path,
        permissions: Some(meta.permissions()),
    })
}

/// Stream `remote` to `local` in [`TRANSFER_CHUNK`] pieces, so a multi-GB file
/// costs a fixed buffer instead of its full size in RAM. Progress goes to
/// `reporter`, which throttles it. A tracked transfer (`track`) polls its
/// cancel flag between chunks and bails with [`TransferCanceled`].
///
/// The bytes go to a `.flint-part` file next to the target, which is renamed
/// over the target only when the download is whole. A failed or canceled
/// download therefore leaves a file that was already there untouched.
async fn download_file_streaming(
    sftp: &SftpSession,
    remote: &str,
    local: &std::path::Path,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<()> {
    let target = local_write_target(local).await?;
    let mut remote_file = sftp.open(remote).await?;
    write_local_target(&mut remote_file, &target, reporter, track).await
}

/// Write everything `source` yields to `target` through a part file.
async fn write_local_target<R: tokio::io::AsyncRead + Unpin>(
    source: &mut R,
    target: &LocalTarget,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<()> {
    let part = local_part_path(&target.path);
    let part_file = match tokio::fs::File::create(&part).await {
        Ok(file) => file,
        // The folder takes no new files (or the name got too long), but the
        // target itself may still be writable. Writing it in place is the
        // only way left.
        Err(e) if target.permissions.is_none() || is_local_name_refused(&e) => {
            tracing::warn!(?e, part = %part.display(), "no temporary file, downloading in place");
            return download_in_place(source, target, reporter, track).await;
        }
        // Any other reason (disk full, quota) would also break an in-place
        // write, but only after it had emptied the existing file.
        Err(e) => anyhow::bail!("create {}: {e}", part.display()),
    };
    let mut part_guard = RemoveUnlessFinished {
        path: part.clone(),
        is_finished: false,
    };
    copy_remote_to_local(source, part_file, &part, reporter, track).await?;

    if let Some(permissions) = target.permissions.clone() {
        if let Err(e) = tokio::fs::set_permissions(&part, permissions).await {
            tracing::warn!(?e, part = %part.display(), "could not keep the old file's permissions");
        }
    }
    tokio::fs::rename(&part, &target.path)
        .await
        .map_err(|e| anyhow::anyhow!("replace {}: {e}", target.path.display()))?;
    part_guard.is_finished = true;
    Ok(())
}

/// The folder itself refuses a file of this name, which says nothing about
/// whether the file next to it can be written.
fn is_local_name_refused(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        error.kind(),
        ErrorKind::PermissionDenied | ErrorKind::InvalidFilename
    )
}

/// Download straight into the target file. Only for the case where no
/// temporary file can be made: a failure here does damage a file that was
/// already there, but it never deletes it.
async fn download_in_place<R: tokio::io::AsyncRead + Unpin>(
    remote_file: &mut R,
    target: &LocalTarget,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<()> {
    let is_new = target.permissions.is_none();
    let local_file = tokio::fs::File::create(&target.path)
        .await
        .map_err(|e| anyhow::anyhow!("create {}: {e}", target.path.display()))?;
    // A half-written new file would look like a complete download.
    let mut new_file_guard = RemoveUnlessFinished {
        path: target.path.clone(),
        is_finished: !is_new,
    };
    copy_remote_to_local(remote_file, local_file, &target.path, reporter, track).await?;
    new_file_guard.is_finished = true;
    Ok(())
}

/// Copy every byte of `remote_file` into `local_file` (at `path`, named only
/// for error messages) and close it.
async fn copy_remote_to_local<R: tokio::io::AsyncRead + Unpin>(
    remote_file: &mut R,
    mut local_file: tokio::fs::File,
    path: &std::path::Path,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut buf = vec![0u8; TRANSFER_CHUNK];
    loop {
        if track.map(|t| t.canceled()).unwrap_or(false) {
            return Err(anyhow::Error::new(TransferCanceled));
        }
        let n = remote_file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        local_file
            .write_all(&buf[..n])
            .await
            .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
        if let Some(reporter) = reporter {
            reporter.add(n as u64);
        }
    }
    local_file
        .flush()
        .await
        .map_err(|e| anyhow::anyhow!("flush {}: {e}", path.display()))?;
    Ok(())
}

/// Upload a single local path to `remote`. If `local` is a directory it is
/// mirrored recursively (drag/drop hands us whole folders); plain files take the
/// fast path. Without the directory branch, opening a folder as a file fails with
/// "access denied" (os error 5 on Windows).
/// Returns how many symlinks a folder upload skipped (0 for a single file).
async fn upload(
    sftp: &SftpSession,
    local: &std::path::Path,
    remote: &str,
    events: Option<&async_channel::Sender<SftpEvent>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<usize> {
    let meta = tokio::fs::metadata(local)
        .await
        .map_err(|e| anyhow::anyhow!("stat {}: {e}", local.display()))?;
    if meta.is_file() {
        let reporter = events.map(|tx| {
            TransferReporter::new(tx, track.map(|t| t.id), base_name(remote), meta.len())
        });
        upload_file_streaming(sftp, local, remote, reporter.as_ref(), track).await?;
        return Ok(0);
    }
    if !meta.is_dir() {
        anyhow::bail!("{} is neither a file nor a directory", local.display());
    }
    upload_dir(sftp, local, remote, events, track).await
}

/// Where the bytes of one file of a plan come from.
#[derive(Debug, PartialEq)]
pub enum UploadFrom {
    /// A file on this computer.
    Local(std::path::PathBuf),
    /// A path on the source server of a copy between two servers.
    Peer(String),
}

/// One file to send as part of an upload or a copy.
#[derive(Debug, PartialEq)]
pub struct UploadItem {
    from: UploadFrom,
    remote: String,
}

/// Everything a folder upload or a copy between servers has to do, found by
/// walking the source tree.
#[derive(Debug, Default)]
pub struct UploadPlan {
    /// Remote directories to create, one list per depth, parents first.
    dir_levels: Vec<Vec<String>>,
    files: Vec<UploadItem>,
    total_bytes: u64,
    skipped_links: usize,
}

/// Walk the local tree under `local` and work out what its upload to `remote`
/// consists of. Symlinks are not followed, only counted.
async fn plan_folder_upload(
    local: &std::path::Path,
    remote: &str,
    track: Option<&TransferTrack>,
) -> anyhow::Result<UploadPlan> {
    let mut plan = UploadPlan::default();
    let mut frontier: Vec<(std::path::PathBuf, String)> = vec![(
        local.to_path_buf(),
        remote.trim_end_matches('/').to_string(),
    )];
    while !frontier.is_empty() {
        plan.dir_levels
            .push(frontier.iter().map(|(_, rdir)| rdir.clone()).collect());
        let mut next = Vec::new();
        for (ldir, rdir) in frontier {
            // A tree like node_modules takes a while to walk.
            if track.map(|t| t.canceled()).unwrap_or(false) {
                return Err(anyhow::Error::new(TransferCanceled));
            }
            plan_upload_of_dir(&ldir, &rdir, &mut plan, &mut next).await?;
        }
        frontier = next;
    }
    Ok(plan)
}

/// Add the entries of one local directory to `plan`; its subdirectories go to
/// `subdirs` for the next round of the walk.
async fn plan_upload_of_dir(
    ldir: &std::path::Path,
    rdir: &str,
    plan: &mut UploadPlan,
    subdirs: &mut Vec<(std::path::PathBuf, String)>,
) -> anyhow::Result<()> {
    let mut rd = tokio::fs::read_dir(ldir)
        .await
        .map_err(|e| anyhow::anyhow!("read_dir {}: {e}", ldir.display()))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| anyhow::anyhow!("read_dir {}: {e}", ldir.display()))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_remote = format!("{rdir}/{name}");
        let ft = entry
            .file_type()
            .await
            .map_err(|e| anyhow::anyhow!("file_type {}: {e}", entry.path().display()))?;
        if ft.is_dir() {
            subdirs.push((entry.path(), child_remote));
        } else if ft.is_file() {
            let meta = entry
                .metadata()
                .await
                .map_err(|e| anyhow::anyhow!("stat {}: {e}", entry.path().display()))?;
            plan.total_bytes += meta.len();
            plan.files.push(UploadItem {
                from: UploadFrom::Local(entry.path()),
                remote: child_remote,
            });
        } else {
            if ft.is_symlink() {
                plan.skipped_links += 1;
            }
            tracing::warn!(
                path = %entry.path().display(),
                "skipped during upload: not a regular file or directory (symlink/special)"
            );
        }
    }
    Ok(())
}

/// Walk `source` on this server and work out what its copy to `target` on
/// another server consists of. A single file gives a plan of one file.
/// Symlinks inside a folder are not followed, only counted.
async fn plan_remote_copy(
    sftp: &SftpSession,
    source: &str,
    target: &str,
    cancel: &CancelFlag,
) -> anyhow::Result<UploadPlan> {
    use futures::stream::{self, StreamExt as _};

    let meta = sftp
        .metadata(source)
        .await
        .map_err(|e| anyhow::anyhow!("stat {source}: {e}"))?;
    let mut plan = UploadPlan::default();
    if !meta.is_dir() {
        plan.total_bytes = meta.size.unwrap_or(0);
        plan.files.push(UploadItem {
            from: UploadFrom::Peer(source.to_string()),
            remote: target.to_string(),
        });
        return Ok(plan);
    }

    // Breadth-first, each level listed concurrently, as in [`download_dir`].
    let mut frontier: Vec<(String, String)> = vec![(
        source.trim_end_matches('/').to_string(),
        target.trim_end_matches('/').to_string(),
    )];
    while !frontier.is_empty() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(anyhow::Error::new(TransferCanceled));
        }
        plan.dir_levels.push(
            frontier
                .iter()
                .map(|(_, target_dir)| target_dir.clone())
                .collect(),
        );
        let listings: Vec<(String, String, anyhow::Result<Vec<FileEntry>>)> =
            stream::iter(frontier)
                .map(|(source_dir, target_dir)| async move {
                    let entries = list_dir(sftp, &source_dir).await;
                    (source_dir, target_dir, entries)
                })
                .buffer_unordered(READDIR_BATCH_CONCURRENCY)
                .collect()
                .await;
        let mut next = Vec::new();
        for (source_dir, target_dir, listing) in listings {
            let entries = listing.map_err(|e| anyhow::anyhow!("list {source_dir}: {e}"))?;
            add_listing_to_plan(&source_dir, &target_dir, entries, &mut plan, &mut next);
        }
        frontier = next;
    }
    Ok(plan)
}

/// Add the entries of one source directory to `plan`; its subdirectories go
/// to `subdirs` as (source, target) pairs for the next round of the walk.
fn add_listing_to_plan(
    source_dir: &str,
    target_dir: &str,
    entries: Vec<FileEntry>,
    plan: &mut UploadPlan,
    subdirs: &mut Vec<(String, String)>,
) {
    for entry in entries {
        let child_source = format!("{source_dir}/{}", entry.name);
        let child_target = format!("{target_dir}/{}", entry.name);
        // `kind` is the exact file type; `is_dir` is also true for a socket.
        match entry.kind {
            EntryKind::Folder => subdirs.push((child_source, child_target)),
            EntryKind::File => {
                plan.total_bytes += entry.size;
                plan.files.push(UploadItem {
                    from: UploadFrom::Peer(child_source),
                    remote: child_target,
                });
            }
            EntryKind::Symlink => {
                plan.skipped_links += 1;
                tracing::warn!(source = %child_source, "skipped during copy: a symlink");
            }
            EntryKind::Other => {
                tracing::warn!(source = %child_source, "skipped during copy: not a regular file");
            }
        }
    }
}

/// Read `remote` on this server into `chunks`, for the worker of another
/// server to write. A failure travels down the same channel. When the reader
/// of the channel goes away this stops without a word: the other side has
/// already reported why.
/// The start of a file and its full size.
pub struct FileHead {
    pub bytes: Vec<u8>,
    pub size: u64,
}

async fn read_head(
    sftp: &SftpSession,
    remote: &str,
    limit: usize,
    reply: &tokio::sync::oneshot::Sender<anyhow::Result<FileHead>>,
) -> anyhow::Result<FileHead> {
    use tokio::io::AsyncReadExt as _;

    // Follows a symlink. A FIFO or a device would block the server's SFTP
    // process on open, and with it every other request on this channel.
    let attrs = sftp.metadata(remote).await?;
    if !attrs.file_type().is_file() {
        anyhow::bail!("not a regular file");
    }
    let mut file = sftp.open(remote).await?;
    let mut bytes = Vec::new();
    let mut buf = vec![0u8; TRANSFER_CHUNK];
    while bytes.len() < limit {
        // Nobody waits for the preview any more.
        if reply.is_closed() {
            anyhow::bail!("preview closed");
        }
        let wanted = buf.len().min(limit - bytes.len());
        let n = file.read(&mut buf[..wanted]).await?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    Ok(FileHead {
        bytes,
        size: attrs.len(),
    })
}

async fn read_file_into(
    sftp: &SftpSession,
    remote: &str,
    chunks: &async_channel::Sender<RelayChunk>,
    cancel: &CancelFlag,
) {
    use tokio::io::AsyncReadExt as _;

    let mut file = match sftp.open(remote).await {
        Ok(file) => file,
        Err(e) => {
            let _ = chunks
                .send(RelayChunk::Failed(format!("open {remote}: {e}")))
                .await;
            return;
        }
    };
    let mut buf = vec![0u8; TRANSFER_CHUNK];
    loop {
        // The writing side sees the same flag and reports the cancel.
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let chunk = match file.read(&mut buf).await {
            Ok(0) => RelayChunk::End,
            Ok(n) => RelayChunk::Data(buf[..n].to_vec()),
            Err(e) => RelayChunk::Failed(format!("read {remote}: {e}")),
        };
        let is_last = !matches!(chunk, RelayChunk::Data(_));
        if chunks.send(chunk).await.is_err() || is_last {
            return;
        }
    }
}

/// Mirror a local directory tree into `remote`, the twin of [`download_dir`].
/// Two phases: first walk the local tree to learn every file and the total
/// byte count, then send the files with [`FOLDER_FILE_CONCURRENCY`] in flight
/// at once. Progress is aggregate, as it is for a folder download.
/// Symlinks are not followed; the count of skipped ones is returned.
async fn upload_dir(
    sftp: &SftpSession,
    local: &std::path::Path,
    remote: &str,
    events: Option<&async_channel::Sender<SftpEvent>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<usize> {
    let plan = plan_folder_upload(local, remote, track).await?;
    send_plan(
        sftp,
        plan,
        None,
        FOLDER_FILE_CONCURRENCY,
        base_name(remote),
        events,
        track,
    )
    .await
}

/// Carry out `plan`: make its directories, then send its files, `concurrency`
/// of them in flight at once. `peer` is the source server of a copy between
/// two servers; a plan of local files needs none. `name` labels the progress.
/// Returns how many symlinks the plan left out.
async fn send_plan(
    sftp: &SftpSession,
    plan: UploadPlan,
    peer: Option<&SftpHandle>,
    concurrency: usize,
    name: &str,
    events: Option<&async_channel::Sender<SftpEvent>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<usize> {
    use futures::stream::{self, StreamExt as _};

    // Create every directory before any file goes up, level by level so a
    // parent always exists first, and so empty local dirs still land remotely.
    let is_canceled = || track.map(|t| t.canceled()).unwrap_or(false);
    for level in plan.dir_levels {
        let results: Vec<anyhow::Result<()>> = stream::iter(level)
            .map(|rdir| async move {
                if is_canceled() {
                    return Ok(());
                }
                ensure_remote_dir(sftp, &rdir).await
            })
            .buffer_unordered(READDIR_BATCH_CONCURRENCY)
            .collect()
            .await;
        // Nothing below a missing folder can be written. A plan without
        // files would even end as done with nothing on the server.
        if let Some(e) = results.into_iter().find_map(Result::err) {
            return Err(e);
        }
    }
    if is_canceled() {
        return Err(anyhow::Error::new(TransferCanceled));
    }

    let reporter =
        events.map(|tx| TransferReporter::new(tx, track.map(|t| t.id), name, plan.total_bytes));
    let reporter = reporter.as_ref();
    // Publish the total up front so the bar shows the real size while the
    // first files start, instead of sitting at 0/0.
    if let Some(reporter) = reporter {
        reporter.emit();
    }

    let verb = if peer.is_some() { "copy" } else { "upload" };
    let files_track = TransferTrack::for_folder_files(track);
    let files_track = &files_track;
    let mut results = stream::iter(plan.files)
        .map(|item| async move {
            if files_track.canceled() {
                return Err(anyhow::Error::new(TransferCanceled));
            }
            send_plan_file(sftp, &item, peer, reporter, files_track)
                .await
                .map_err(|e| {
                    if e.is::<TransferCanceled>() {
                        e
                    } else {
                        anyhow::anyhow!("{verb} {}: {e}", item.remote)
                    }
                })
        })
        .buffer_unordered(concurrency);

    // Unlike a download, an upload cleans up its part file with a request to
    // the server, which a dropped future never sends. So after the first
    // failure the files in flight are told to stop and are awaited, not
    // dropped.
    let mut first_error = None;
    while let Some(result) = results.next().await {
        let Err(e) = result else { continue };
        if first_error.is_none() {
            files_track
                .has_failed_sibling
                .store(true, std::sync::atomic::Ordering::Relaxed);
            first_error = Some(e);
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(plan.skipped_links),
    }
}

/// Make `path` a folder on the server. One that is already there is fine: a
/// merge into an existing tree meets one at every level.
async fn ensure_remote_dir(sftp: &SftpSession, path: &str) -> anyhow::Result<()> {
    let Err(create_error) = sftp.create_dir(path).await else {
        return Ok(());
    };
    // SFTP v3 has no "already exists" error, so look at what is there.
    let is_folder = sftp.metadata(path).await.is_ok_and(|meta| meta.is_dir());
    if is_folder {
        return Ok(());
    }
    tracing::warn!(remote = path, %create_error, "could not create a remote folder");
    anyhow::bail!("create folder {path}: {create_error}")
}

async fn send_plan_file(
    sftp: &SftpSession,
    item: &UploadItem,
    peer: Option<&SftpHandle>,
    reporter: Option<&TransferReporter<'_>>,
    track: &TransferTrack,
) -> anyhow::Result<()> {
    match (&item.from, peer) {
        (UploadFrom::Local(local), _) => {
            upload_file_streaming(sftp, local, &item.remote, reporter, Some(track)).await
        }
        (UploadFrom::Peer(source_path), Some(peer)) => {
            relay_file(sftp, peer, source_path, &item.remote, reporter, track).await
        }
        (UploadFrom::Peer(source_path), None) => {
            anyhow::bail!("no source server to read {source_path} from")
        }
    }
}

/// Copy one file from the server behind `peer` to `remote` on this one. The
/// pipe between the two is bounded, so a slow target slows the reading down
/// instead of filling memory.
async fn relay_file(
    sftp: &SftpSession,
    peer: &SftpHandle,
    source_path: &str,
    remote: &str,
    reporter: Option<&TransferReporter<'_>>,
    track: &TransferTrack,
) -> anyhow::Result<()> {
    let (chunk_tx, chunk_rx) = async_channel::bounded(RELAY_PIPE_CHUNKS);
    peer.read_file(source_path.to_string(), chunk_tx, track.cancel.clone())?;
    // Dropping `chunk_rx` on the way out is what stops the reader.
    upload_from(
        sftp,
        &mut UploadSource::Relay(&chunk_rx),
        remote,
        reporter,
        Some(track),
    )
    .await
}

/// Where an upload reads its bytes.
enum UploadSource<'a> {
    /// An open local file, and its path for error messages.
    File(&'a mut tokio::fs::File, &'a std::path::Path),
    /// The pieces the source server of a copy sends.
    Relay(&'a async_channel::Receiver<RelayChunk>),
}

impl UploadSource<'_> {
    fn is_relay(&self) -> bool {
        matches!(self, Self::Relay(_))
    }

    /// Put the next bytes into `buf` and return how many; 0 at the end.
    async fn fill(
        &mut self,
        buf: &mut [u8],
        track: Option<&TransferTrack>,
    ) -> anyhow::Result<usize> {
        use tokio::io::AsyncReadExt as _;

        let chunks = match self {
            Self::File(file, path) => {
                return file
                    .read(buf)
                    .await
                    .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()));
            }
            Self::Relay(chunks) => chunks,
        };
        let is_canceled = || track.map(|t| t.canceled()).unwrap_or(false);
        loop {
            // The source may wait on a slow disk; cancel is a plain flag that
            // wakes nobody, so look at it now and then.
            let chunk = tokio::select! {
                chunk = chunks.recv() => chunk,
                _ = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {
                    if is_canceled() {
                        return Err(anyhow::Error::new(TransferCanceled));
                    }
                    continue;
                }
            };
            match chunk {
                // An empty piece is not the end; only `End` is.
                Ok(RelayChunk::Data(data)) if data.is_empty() => {}
                Ok(RelayChunk::Data(data)) => {
                    anyhow::ensure!(
                        data.len() <= buf.len(),
                        "the source sent an oversized piece"
                    );
                    buf[..data.len()].copy_from_slice(&data);
                    return Ok(data.len());
                }
                Ok(RelayChunk::End) => return Ok(0),
                // The source stops on a cancel too, and may be first to see it.
                Ok(RelayChunk::Failed(_)) | Err(_) if is_canceled() => {
                    return Err(anyhow::Error::new(TransferCanceled));
                }
                Ok(RelayChunk::Failed(error)) => anyhow::bail!(error),
                Err(_) => anyhow::bail!(SOURCE_LOST),
            }
        }
    }
}

/// The remote file an upload really writes.
struct RemoteTarget {
    path: String,
    /// A regular file is already there and gets replaced.
    is_existing: bool,
    /// Size of the file being replaced, when the server reports it.
    size: Option<u64>,
    /// Mode of the file being replaced, when the server reports it.
    permissions: Option<u32>,
    /// Owner and group of the file being replaced, when the server reports
    /// them.
    owner: Option<(u32, u32)>,
}

/// How an upload reaches its target.
enum RemoteWrite {
    /// Write a `.flint-part` file, then rename it over the target.
    ViaPart(RemoteTarget),
    /// Write the target itself: it is something a rename must not replace.
    InPlace(String),
}

/// Work out which file an upload to `remote` replaces, and refuse the ones it
/// must not replace. A link is followed, so the upload lands in the file the
/// link points at and the link stays.
async fn remote_write_plan(sftp: &SftpSession, remote: &str) -> anyhow::Result<RemoteWrite> {
    use russh_sftp::protocol::{FileType, OpenFlags};

    let new_file = |path: String| {
        RemoteWrite::ViaPart(RemoteTarget {
            path,
            is_existing: false,
            size: None,
            permissions: None,
            owner: None,
        })
    };
    let Ok(link_meta) = sftp.symlink_metadata(remote).await else {
        return Ok(new_file(remote.to_string()));
    };
    let (path, meta) = if link_meta.file_type() == FileType::Symlink {
        let Ok(path) = sftp.canonicalize(remote).await else {
            return Ok(RemoteWrite::InPlace(remote.to_string()));
        };
        let Ok(meta) = sftp.metadata(path.as_str()).await else {
            return Ok(new_file(path));
        };
        (path, meta)
    } else {
        (remote.to_string(), link_meta)
    };

    match meta.file_type() {
        FileType::File => {}
        FileType::Dir => anyhow::bail!("{path} is a folder"),
        // A device, pipe or socket: replacing it with a plain file would
        // destroy it.
        FileType::Symlink | FileType::Other => return Ok(RemoteWrite::InPlace(path)),
    }

    // A rename needs only the folder to be writable, so it would replace a
    // file this user has no right to write. Ask for write access first
    // (this open does not truncate).
    let probe = sftp
        .open_with_flags(path.as_str(), OpenFlags::WRITE)
        .await
        .map_err(|e| anyhow::anyhow!("cannot write {path}: {e}"))?;
    drop(probe);

    Ok(RemoteWrite::ViaPart(RemoteTarget {
        path,
        is_existing: true,
        size: meta.size,
        permissions: meta.permissions,
        owner: meta.uid.zip(meta.gid),
    }))
}

/// Stream a local file to `remote` in [`TRANSFER_CHUNK`] pieces (fixed memory,
/// see [`download`]).
///
/// The bytes go to a `.flint-part` file next to the target, which replaces the
/// target only when the upload is whole. A failed or canceled upload therefore
/// leaves a file that was already there untouched. The exception is a file
/// that belongs to someone else and cannot be given back to them: it is
/// written in place, so that it keeps its owner.
async fn upload_file_streaming(
    sftp: &SftpSession,
    local: &std::path::Path,
    remote: &str,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<()> {
    let mut local_file = tokio::fs::File::open(local)
        .await
        .map_err(|e| anyhow::anyhow!("open {}: {e}", local.display()))?;
    let mut source = UploadSource::File(&mut local_file, local);
    upload_from(sftp, &mut source, remote, reporter, track).await
}

/// Write everything `source` yields to `remote`, through a part file when
/// the target allows it.
async fn upload_from(
    sftp: &SftpSession,
    source: &mut UploadSource<'_>,
    remote: &str,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<()> {
    match remote_write_plan(sftp, remote).await? {
        RemoteWrite::InPlace(path) => {
            let remote_file = sftp.create(path.as_str()).await?;
            copy_to_remote(source, remote_file, reporter, track)
                .await
                .map(|_written| ())
        }
        RemoteWrite::ViaPart(target) => {
            upload_via_part(sftp, source, &target, reporter, track).await
        }
    }
}

async fn upload_via_part(
    sftp: &SftpSession,
    source: &mut UploadSource<'_>,
    target: &RemoteTarget,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt as _;

    let part = format!("{}{PART_SUFFIX}", target.path);
    let mut part_file = match sftp.create(part.as_str()).await {
        Ok(file) => file,
        // The folder takes no new files (or the name got too long), but the
        // target itself may still be writable. Writing it in place is the
        // only way left.
        Err(e) if !target.is_existing || is_remote_name_refused(&e) => {
            tracing::warn!(%e, part, "no temporary file, uploading in place");
            return upload_in_place(sftp, source, target, reporter, track).await;
        }
        // Any other reason (disk full, quota) would also break an in-place
        // write, but only after it had emptied the existing file.
        Err(e) => anyhow::bail!("create {part}: {e}"),
    };
    if !keep_remote_owner(&part_file, target.owner).await {
        // Replacing the file would hand it to another user or group, and a
        // service that reads it (a web server, sshd) could lose access.
        tracing::info!(remote = %target.path, "owner cannot be kept, uploading in place");
        let _ = part_file.shutdown().await;
        let _ = sftp.remove_file(part.as_str()).await;
        return upload_in_place(sftp, source, target, reporter, track).await;
    }
    if let Err(e) = copy_to_remote(source, part_file, reporter, track).await {
        let _ = sftp.remove_file(part.as_str()).await;
        return Err(e);
    }
    publish_remote_part(sftp, &part, target).await
}

/// The folder itself refuses a file of this name, which says nothing about
/// whether the file next to it can be written. OpenSSH reports a name that is
/// too long as a bad message.
fn is_remote_name_refused(error: &russh_sftp::client::error::Error) -> bool {
    use russh_sftp::protocol::StatusCode;
    let russh_sftp::client::error::Error::Status(status) = error else {
        return false;
    };
    matches!(
        status.status_code,
        StatusCode::PermissionDenied | StatusCode::BadMessage
    )
}

/// True when the new file `part_file` has, or was given, the owner and group
/// of the file it will replace. Also true when either side is unknown.
async fn keep_remote_owner(
    part_file: &russh_sftp::client::fs::File,
    owner: Option<(u32, u32)>,
) -> bool {
    let Some((uid, gid)) = owner else { return true };
    let Ok(meta) = part_file.metadata().await else {
        return true;
    };
    if !is_owner_change(meta.uid.zip(meta.gid), (uid, gid)) {
        return true;
    }
    // Works for root, and for a change of group to one the user is in.
    let mut attrs = russh_sftp::protocol::FileAttributes::empty();
    attrs.uid = Some(uid);
    attrs.gid = Some(gid);
    part_file.set_metadata(attrs).await.is_ok()
}

fn is_owner_change(new_owner: Option<(u32, u32)>, old_owner: (u32, u32)) -> bool {
    new_owner.is_some_and(|new_owner| new_owner != old_owner)
}

/// Upload straight into the target file. Only for the cases where a temporary
/// file cannot be made, or cannot take the place of the old file without
/// changing its owner: a failure here does damage a file that was already
/// there, but it never deletes it.
async fn upload_in_place(
    sftp: &SftpSession,
    source: &mut UploadSource<'_>,
    target: &RemoteTarget,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<()> {
    if source.is_relay() && target.is_existing {
        return overwrite_in_place(sftp, source, target, reporter, track).await;
    }
    let remote_file = sftp.create(target.path.as_str()).await?;
    let result = copy_to_remote(source, remote_file, reporter, track).await;
    // A half-written new file would look like a complete upload.
    if result.is_err() && !target.is_existing {
        let _ = sftp.remove_file(target.path.as_str()).await;
    }
    result.map(|_written| ())
}

/// Write `source` over an existing file from its start, without emptying it
/// first, then cut off what is left of the old content.
///
/// This is how a copy between two servers writes in place. With one machine
/// saved as two servers the target can be the very file being read, and
/// emptying it first would destroy it. Written this way such a file only gets
/// its own bytes back, each at the place it was read from.
async fn overwrite_in_place(
    sftp: &SftpSession,
    source: &mut UploadSource<'_>,
    target: &RemoteTarget,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<()> {
    use russh_sftp::protocol::OpenFlags;

    let path = target.path.as_str();
    let remote_file = sftp.open_with_flags(path, OpenFlags::WRITE).await?;
    let written = copy_to_remote(source, remote_file, reporter, track).await?;
    if !has_old_tail(target.size, written) {
        return Ok(());
    }
    let mut attrs = russh_sftp::protocol::FileAttributes::empty();
    attrs.size = Some(written);
    sftp.set_metadata(path, attrs).await.map_err(|e| {
        anyhow::anyhow!("{path} is written, but the rest of the old file could not be cut off: {e}")
    })
}

/// True when bytes of the old file may be left after `written` bytes went
/// over it from the start: it was longer, or its size is not known.
fn has_old_tail(old_size: Option<u64>, written: u64) -> bool {
    old_size.is_none_or(|old_size| old_size > written)
}

/// Copy the rest of `source` into `remote_file` and close it. Returns how
/// many bytes were written.
async fn copy_to_remote(
    source: &mut UploadSource<'_>,
    mut remote_file: russh_sftp::client::fs::File,
    reporter: Option<&TransferReporter<'_>>,
    track: Option<&TransferTrack>,
) -> anyhow::Result<u64> {
    use tokio::io::AsyncWriteExt as _;

    let write_slots = track.and_then(|t| t.write_slots.as_deref());
    let mut buf = vec![0u8; TRANSFER_CHUNK];
    let mut written = 0u64;
    loop {
        if track.map(|t| t.canceled()).unwrap_or(false) {
            return Err(anyhow::Error::new(TransferCanceled));
        }
        let n = source.fill(&mut buf, track).await?;
        if n == 0 {
            break;
        }
        let _write_slot = match write_slots {
            Some(slots) => Some(
                slots
                    .acquire()
                    .await
                    .expect("the write semaphore is never closed"),
            ),
            None => None,
        };
        remote_file.write_all(&buf[..n]).await?;
        written += n as u64;
        if let Some(reporter) = reporter {
            reporter.add(n as u64);
        }
    }
    remote_file.shutdown().await?;
    Ok(written)
}

/// Move a finished part file to its real name.
async fn publish_remote_part(
    sftp: &SftpSession,
    part: &str,
    target: &RemoteTarget,
) -> anyhow::Result<()> {
    if let Some(mode) = target.permissions {
        keep_remote_mode(sftp, part, mode).await;
    }
    let Err(rename_error) = sftp.rename(part, target.path.as_str()).await else {
        return Ok(());
    };
    // SFTP v3 rename refuses a name that is taken. Swap by hand, but only for
    // a regular file: moving a folder aside here would replace it with a file.
    let is_regular_file = sftp
        .symlink_metadata(target.path.as_str())
        .await
        .map(|meta| meta.file_type() == russh_sftp::protocol::FileType::File)
        .unwrap_or(false);
    if !is_regular_file {
        let _ = sftp.remove_file(part).await;
        anyhow::bail!("rename {part} to {}: {rename_error}", target.path);
    }
    swap_remote_file(sftp, part, &target.path).await
}

/// Replace the file at `path` with `part` on a server whose rename will not
/// overwrite: move the old file aside, move the new one in, drop the old one.
/// If the second step fails the old file is moved back, so `path` is never
/// left without its content unless the connection dies between the two renames,
/// and then the old content is still on the server under `.flint-old`.
async fn swap_remote_file(sftp: &SftpSession, part: &str, path: &str) -> anyhow::Result<()> {
    let old = format!("{path}{OLD_SUFFIX}");
    // A leftover from an interrupted swap would block the rename below. `path`
    // exists right now, so that leftover is no longer the only copy.
    let _ = sftp.remove_file(old.as_str()).await;

    if let Err(e) = sftp.rename(path, old.as_str()).await {
        let _ = sftp.remove_file(part).await;
        anyhow::bail!("replace {path}: {e}");
    }
    if let Err(e) = sftp.rename(part, path).await {
        if let Err(restore_error) = sftp.rename(old.as_str(), path).await {
            tracing::error!(path, old, %restore_error, "could not put the old file back");
            anyhow::bail!("replace {path}: {e}; the previous version is kept as {old}");
        }
        let _ = sftp.remove_file(part).await;
        anyhow::bail!("replace {path}: {e}");
    }
    if let Err(e) = sftp.remove_file(old.as_str()).await {
        tracing::warn!(old, %e, "could not remove the replaced file");
    }
    Ok(())
}

/// Best-effort: give `path` the permission bits of the file it replaces.
async fn keep_remote_mode(sftp: &SftpSession, path: &str, mode: u32) {
    let mut attrs = russh_sftp::protocol::FileAttributes::empty();
    attrs.permissions = Some(mode & 0o7777);
    if let Err(e) = sftp.set_metadata(path, attrs).await {
        tracing::warn!(path, %e, "could not keep the old file's permissions");
    }
}

struct ClientHandler {
    disconnect_tx: Option<tokio::sync::oneshot::Sender<()>>,
    /// Fingerprint the user previously trusted for this host, if any.
    expected_host_key: Option<String>,
    /// Vault entry of the hop this handler speaks for, so a key decision is
    /// attributed to, and saved on, the right host.
    entry_id: String,
    /// Host/port being connected to, used to consult `~/.ssh/known_hosts`.
    host: String,
    port: u16,
    event_tx: async_channel::Sender<SshConnectionEvent>,
    /// Where inbound `forwarded-tcpip` channels are handed off. `None` on a
    /// jump hop: only the target is ever asked to listen (`tcpip_forward`), so
    /// a bastion offering one of these is claiming a connection it was never
    /// told about, and the pump would dial the forward's target on this
    /// machine for it.
    forward_tx: Option<async_channel::Sender<ForwardMsg>>,
    /// Set while a host-key prompt is waiting on the user, so the handshake
    /// timeout stops counting (see [`with_handshake_timeout`]).
    prompt_pending: Arc<std::sync::atomic::AtomicBool>,
}

/// What `~/.ssh/known_hosts` says about a key a server presented.
#[derive(Debug, PartialEq, Eq)]
enum KnownHosts {
    Trusted,
    /// The file holds another key of the same type for this host. Carries the
    /// "SHA256:..." fingerprint of that key.
    Changed(String),
    Unknown,
}

/// Compare `presented` with the keys known_hosts holds for the host. A match
/// on any line wins, as in OpenSSH: a host may have an old and a new key
/// listed while it rotates them. russh's own `check_known_hosts` reports a
/// changed key in that case.
fn known_hosts_verdict(recorded: &[PublicKey], presented: &PublicKey) -> KnownHosts {
    if recorded.contains(presented) {
        return KnownHosts::Trusted;
    }
    match recorded.iter().find(|key| key.name() == presented.name()) {
        Some(key) => KnownHosts::Changed(format!("SHA256:{}", key.fingerprint())),
        None => KnownHosts::Unknown,
    }
}

impl ClientHandler {
    fn check_known_hosts(&self, presented: &PublicKey) -> KnownHosts {
        let recorded: Vec<PublicKey> = match keys::known_host_keys(&self.host, self.port) {
            Ok(lines) => lines.into_iter().map(|(_, key)| key).collect(),
            Err(e) => {
                tracing::warn!(host = %self.host, %e, "could not read ~/.ssh/known_hosts");
                Vec::new()
            }
        };
        known_hosts_verdict(&recorded, presented)
    }
}

#[async_trait::async_trait]
impl client::Handler for ClientHandler {
    type Error = anyhow::Error;

    /// Trust-on-first-use host key verification. A silently-accepted key is
    /// only possible when its fingerprint matches the stored one; anything
    /// else (first contact, or a changed key) parks the handshake on a user
    /// prompt. Returning `Ok(false)` makes russh abort with `UnknownKey`.
    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        use std::sync::atomic::Ordering;

        let fingerprint = format!("SHA256:{}", server_public_key.fingerprint());
        let key_type = server_public_key.name().to_string();

        let stored_fingerprint = match self.expected_host_key.as_deref() {
            Some(stored) if stored == fingerprint => {
                tracing::debug!(fingerprint, "host key matches stored fingerprint");
                return Ok(true);
            }
            Some(stored) => {
                tracing::warn!(
                    stored,
                    presented = fingerprint,
                    "HOST KEY MISMATCH: server key changed"
                );
                Some(stored.to_string())
            }
            None => None,
        };

        // First contact for the vault, but the user may already trust this
        // host in OpenSSH. A ~/.ssh/known_hosts match accepts silently and is
        // reported so the fingerprint gets persisted on the vault entry. A
        // different key there is a changed key, the same as a vault mismatch.
        // A file that cannot be read counts as no entry.
        let stored_fingerprint = match stored_fingerprint {
            Some(stored) => Some(stored),
            None => match self.check_known_hosts(server_public_key) {
                KnownHosts::Trusted => {
                    tracing::info!(fingerprint, "host key found in ~/.ssh/known_hosts");
                    let _ = self
                        .event_tx
                        .send(SshConnectionEvent::HostKeyTrusted {
                            entry_id: self.entry_id.clone(),
                            fingerprint,
                        })
                        .await;
                    return Ok(true);
                }
                KnownHosts::Changed(recorded) => {
                    tracing::warn!(
                        recorded,
                        presented = fingerprint,
                        "HOST KEY MISMATCH: ~/.ssh/known_hosts holds a different key"
                    );
                    Some(recorded)
                }
                KnownHosts::Unknown => None,
            },
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.prompt_pending.store(true, Ordering::Relaxed);
        let sent = self
            .event_tx
            .send(SshConnectionEvent::HostKeyPrompt(HostKeyPrompt {
                entry_id: self.entry_id.clone(),
                key_type,
                fingerprint,
                stored_fingerprint,
                reply: reply_tx,
            }))
            .await;
        // Channel gone (UI dropped the session) or prompt dismissed: reject.
        let accepted = sent.is_ok() && reply_rx.await.unwrap_or(false);
        self.prompt_pending.store(false, Ordering::Relaxed);
        Ok(accepted)
    }

    /// A connection arrived on a port the server is listening on for one of
    /// our `-R` forwards. This callback runs in a `tokio::spawn`ed task, where
    /// `spawn_local` panics, so the channel is only handed over; the pump is
    /// started by `run_connection`.
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        originator_port: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        tracing::debug!(
            connected_address,
            connected_port,
            originator_address,
            originator_port,
            "inbound forwarded connection",
        );
        // Dropping the channel is the whole refusal. `close()` would await on
        // the session's inbound queue, and this callback runs *in* the task
        // that drains it: a peer that opens these faster than the queue is
        // read would wedge the hop for good.
        let Some(forward_tx) = &self.forward_tx else {
            tracing::warn!(
                host = %self.host, connected_port,
                "a jump host offered a forwarded connection we never asked for, refused",
            );
            return Ok(());
        };
        let handed_over = forward_tx.try_send(ForwardMsg::Inbound {
            channel,
            connected_address: connected_address.to_string(),
            connected_port: connected_port as u16,
        });
        // Unbounded: this can only fail once the connection is gone.
        if handed_over.is_err() {
            tracing::warn!(connected_port, "dropped an inbound forwarded connection");
        }
        Ok(())
    }

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        if let Some(tx) = self.disconnect_tx.take() {
            let _ = tx.send(());
        }
        match reason {
            client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            client::DisconnectReason::Error(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_public_key() -> PublicKey {
        keys::key::KeyPair::generate_ed25519()
            .unwrap()
            .clone_public_key()
            .unwrap()
    }

    #[test]
    fn a_key_listed_in_known_hosts_is_trusted_even_next_to_an_old_one() {
        let (old, current) = (new_public_key(), new_public_key());
        assert_eq!(
            known_hosts_verdict(&[old, current.clone()], &current),
            KnownHosts::Trusted
        );
    }

    #[test]
    fn a_different_key_in_known_hosts_is_a_changed_key() {
        let (recorded, presented) = (new_public_key(), new_public_key());
        let expected = KnownHosts::Changed(format!("SHA256:{}", recorded.fingerprint()));
        assert_eq!(known_hosts_verdict(&[recorded], &presented), expected);
    }

    #[test]
    fn a_host_missing_from_known_hosts_is_unknown() {
        assert_eq!(
            known_hosts_verdict(&[], &new_public_key()),
            KnownHosts::Unknown
        );
    }

    #[test]
    fn a_listing_name_that_is_a_path_is_not_plain() {
        for name in ["", ".", "..", "../x", "/etc/passwd", "a/b", "a/", "a\0b"] {
            assert!(!is_plain_entry_name(name), "{name:?}");
            assert!(!is_single_local_component(name), "{name:?}");
        }
        for name in ["notes.txt", ".ssh", "..hidden", "a b"] {
            assert!(is_plain_entry_name(name), "{name:?}");
            assert!(is_single_local_component(name), "{name:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_path_in_a_name_is_not_a_local_component() {
        for name in ["..\\x", "a\\b", "C:x", "C:\\x"] {
            assert!(!is_single_local_component(name), "{name:?}");
        }
    }

    #[test]
    fn an_ipv6_proxy_target_gets_brackets() {
        assert_eq!(connect_authority("example.com", 22), "example.com:22");
        assert_eq!(connect_authority("::1", 2222), "[::1]:2222");
        assert_eq!(connect_authority("[::1]", 22), "[::1]:22");
    }

    #[test]
    fn a_new_password_prompt_is_not_answered_with_the_stored_one() {
        assert!(asks_for_stored_password("Password: "));
        assert!(asks_for_stored_password("(current) UNIX password: "));
        assert!(!asks_for_stored_password("New password: "));
        assert!(!asks_for_stored_password("Retype new UNIX password: "));
        assert!(!asks_for_stored_password("Verification code: "));
    }

    #[test]
    fn an_encrypted_key_without_a_passphrase_asks_for_one() {
        let error = classify_key_error(keys::Error::KeyIsEncrypted, false);
        assert!(matches!(error, AuthError::PassphraseRequired));
    }

    /// Yields `data`, then fails the way a dropped connection does.
    struct BrokenSource {
        data: Option<Vec<u8>>,
    }

    impl tokio::io::AsyncRead for BrokenSource {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(match self.data.take() {
                Some(data) => {
                    buf.put_slice(&data);
                    Ok(())
                }
                None => Err(std::io::Error::other("connection lost")),
            })
        }
    }

    fn broken_source() -> BrokenSource {
        BrokenSource {
            data: Some(b"half of the new".to_vec()),
        }
    }

    fn names_in(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    async fn write_local(
        source: &mut (impl tokio::io::AsyncRead + Unpin),
        local: &std::path::Path,
        track: Option<&TransferTrack>,
    ) -> anyhow::Result<()> {
        let target = local_write_target(local).await?;
        write_local_target(source, &target, None, track).await
    }

    #[tokio::test]
    async fn download_replaces_an_existing_file_when_whole() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.yml");
        std::fs::write(&local, "old").unwrap();

        write_local(&mut &b"new content"[..], &local, None)
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&local).unwrap(), "new content");
        assert_eq!(names_in(dir.path()), ["config.yml"]);
    }

    fn remote_target(path: &str) -> TransferTarget {
        TransferTarget::Remote(path.to_string())
    }

    #[test]
    fn transfer_targets_overlap_on_the_same_path_or_a_parent_folder() {
        assert!(remote_target("/srv/a.zip").overlaps(&remote_target("/srv/a.zip")));
        assert!(remote_target("/srv/site").overlaps(&remote_target("/srv/site/index.html")));
        assert!(remote_target("/srv/site/index.html").overlaps(&remote_target("/srv/site/")));
        assert!(!remote_target("/srv/site").overlaps(&remote_target("/srv/site2")));
        assert!(!remote_target("/srv/a.zip").overlaps(&remote_target("/srv/b.zip")));

        let local = |path: &str| TransferTarget::Local(std::path::PathBuf::from(path));
        assert!(local("/home/me/site").overlaps(&local("/home/me/site/index.html")));
        assert!(!local("/home/me/site").overlaps(&local("/home/me/site2")));
        assert!(local("/home/me/Readme.md").overlaps(&local("/home/me/README.md")));
        // A local path and a remote path are different places, whatever they spell.
        assert!(!local("/srv/a.zip").overlaps(&remote_target("/srv/a.zip")));
    }

    #[test]
    fn target_queue_holds_back_only_the_later_transfer_to_the_same_place() {
        let mut queue = TargetQueue::default();
        let first = queue.push(remote_target("/srv/a.zip"));
        let other = queue.push(remote_target("/srv/b.zip"));
        let second = queue.push(remote_target("/srv/a.zip"));
        let third = queue.push(remote_target("/srv/a.zip"));

        assert!(!queue.is_blocked(first));
        assert!(!queue.is_blocked(other));
        assert!(queue.is_blocked(second));
        assert!(queue.is_blocked(third));

        queue.remove(first);
        assert!(!queue.is_blocked(second));
        assert!(queue.is_blocked(third));
    }

    #[test]
    fn transfer_outcome_tells_a_cancel_from_a_failure() {
        let done = transfer_outcome(Ok(2), "upload", "/srv/a");
        assert!(matches!(done, TransferOutcome::Done { skipped_links: 2 }));
        let canceled = transfer_outcome(
            Err(anyhow::Error::new(TransferCanceled)),
            "upload",
            "/srv/a",
        );
        assert!(matches!(canceled, TransferOutcome::Canceled));
        let failed = transfer_outcome(Err(anyhow::anyhow!("disk full")), "upload", "/srv/a");
        assert!(matches!(failed, TransferOutcome::Failed(text) if text == "disk full"));
    }

    struct SlotFixture {
        permits: tokio::sync::Semaphore,
        targets: std::rc::Rc<std::cell::RefCell<TargetQueue>>,
        target_freed: std::rc::Rc<tokio::sync::Notify>,
        event_tx: async_channel::Sender<SftpEvent>,
        event_rx: async_channel::Receiver<SftpEvent>,
    }

    impl SlotFixture {
        fn new(free_slots: usize) -> Self {
            let (event_tx, event_rx) = async_channel::bounded(64);
            Self {
                permits: tokio::sync::Semaphore::new(free_slots),
                targets: Default::default(),
                target_freed: Default::default(),
                event_tx,
                event_rx,
            }
        }

        fn claim(&self, path: &str) -> TargetClaim {
            TargetClaim {
                ticket: self.targets.borrow_mut().push(remote_target(path)),
                targets: self.targets.clone(),
                target_freed: self.target_freed.clone(),
            }
        }

        fn waiting_events(&self) -> Vec<bool> {
            let mut seen = Vec::new();
            while let Ok(event) = self.event_rx.try_recv() {
                if let SftpEvent::TransferWaiting { is_waiting, .. } = event {
                    seen.push(is_waiting);
                }
            }
            seen
        }
    }

    #[tokio::test]
    async fn transfer_with_a_free_slot_starts_without_a_queued_notice() {
        let fixture = SlotFixture::new(1);
        let claim = fixture.claim("/srv/a.zip");
        let track = TransferTrack::new(1, CancelFlag::default());

        let permit =
            wait_for_transfer_slot(&fixture.permits, &fixture.event_tx, &claim, &track).await;

        assert!(permit.is_some());
        assert!(fixture.waiting_events().is_empty());
    }

    #[tokio::test]
    async fn queued_transfer_can_be_canceled_before_it_starts() {
        let fixture = SlotFixture::new(0);
        let claim = fixture.claim("/srv/a.zip");
        let track = TransferTrack::new(1, CancelFlag::default());
        track
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let permit =
            wait_for_transfer_slot(&fixture.permits, &fixture.event_tx, &claim, &track).await;

        assert!(permit.is_none());
        assert_eq!(fixture.waiting_events(), [true]);
    }

    #[tokio::test]
    async fn queued_transfer_starts_when_a_slot_is_freed() {
        let fixture = SlotFixture::new(0);
        let claim = fixture.claim("/srv/a.zip");
        let track = TransferTrack::new(1, CancelFlag::default());

        let (permit, ()) = tokio::join!(
            wait_for_transfer_slot(&fixture.permits, &fixture.event_tx, &claim, &track),
            async { fixture.permits.add_permits(1) },
        );

        assert!(permit.is_some());
        assert_eq!(fixture.waiting_events(), [true, false]);
    }

    #[tokio::test]
    async fn second_transfer_to_the_same_target_waits_for_the_first() {
        let fixture = SlotFixture::new(2);
        let first = fixture.claim("/srv/a.zip");
        let second = fixture.claim("/srv/a.zip");
        let track = TransferTrack::new(2, CancelFlag::default());

        let (permit, ()) = tokio::join!(
            wait_for_transfer_slot(&fixture.permits, &fixture.event_tx, &second, &track),
            async {
                tokio::task::yield_now().await;
                assert_eq!(fixture.permits.available_permits(), 2);
                drop(first);
            },
        );

        assert!(permit.is_some());
        assert_eq!(fixture.waiting_events(), [true, false]);
    }

    #[tokio::test]
    async fn failed_download_keeps_the_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.yml");
        std::fs::write(&local, "old").unwrap();

        let result = write_local(&mut broken_source(), &local, None).await;

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&local).unwrap(), "old");
        assert_eq!(names_in(dir.path()), ["config.yml"]);
    }

    #[tokio::test]
    async fn canceled_download_keeps_the_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.yml");
        std::fs::write(&local, "old").unwrap();
        let track = TransferTrack::new(1, CancelFlag::default());
        track
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let error = write_local(&mut &b"new content"[..], &local, Some(&track))
            .await
            .unwrap_err();

        assert!(error.is::<TransferCanceled>());
        assert_eq!(std::fs::read_to_string(&local).unwrap(), "old");
        assert_eq!(names_in(dir.path()), ["config.yml"]);
    }

    #[tokio::test]
    async fn failed_download_of_a_new_file_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.yml");

        let result = write_local(&mut broken_source(), &local, None).await;

        assert!(result.is_err());
        assert!(names_in(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn download_never_replaces_a_folder() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.yml");
        std::fs::create_dir(&local).unwrap();
        std::fs::write(local.join("inside"), "kept").unwrap();

        let result = write_local(&mut &b"new content"[..], &local, None).await;

        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(local.join("inside")).unwrap(),
            "kept"
        );
        assert_eq!(names_in(dir.path()), ["config.yml"]);
    }

    #[tokio::test]
    async fn download_refuses_a_read_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.yml");
        std::fs::write(&local, "old").unwrap();
        let mut permissions = std::fs::metadata(&local).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&local, permissions).unwrap();

        let result = write_local(&mut &b"new content"[..], &local, None).await;

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&local).unwrap(), "old");
        assert_eq!(names_in(dir.path()), ["config.yml"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_keeps_the_mode_of_the_replaced_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("deploy.sh");
        std::fs::write(&local, "old").unwrap();
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o750)).unwrap();

        write_local(&mut &b"new content"[..], &local, None)
            .await
            .unwrap();

        let mode = std::fs::metadata(&local).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_writes_through_a_link_and_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.yml");
        let link = dir.path().join("link.yml");
        std::fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_local(&mut &b"new content"[..], &link, None)
            .await
            .unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new content");
        assert_eq!(names_in(dir.path()), ["link.yml", "real.yml"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_in_place_download_never_deletes_the_file() {
        use std::os::unix::fs::PermissionsExt as _;

        // A folder that takes no new files: the part file cannot be made, so
        // the download falls back to writing the target itself.
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.yml");
        std::fs::write(&local, "old").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = write_local(&mut broken_source(), &local, None).await;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err());
        assert_eq!(names_in(dir.path()), ["config.yml"]);
    }

    // Unix only: Windows reports a folder in the way as "access denied", the
    // same error as a folder that takes no new files.
    #[cfg(unix)]
    #[tokio::test]
    async fn download_stops_when_the_part_file_fails_for_another_reason() {
        // Not a refusal by the folder, so an in-place write (which empties the
        // file first) must not be tried.
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("config.yml");
        std::fs::write(&local, "old").unwrap();
        std::fs::create_dir(local_part_path(&local)).unwrap();

        let result = write_local(&mut &b"new content"[..], &local, None).await;

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&local).unwrap(), "old");
        assert_eq!(
            names_in(dir.path()),
            ["config.yml", "config.yml.flint-part"]
        );
    }

    #[test]
    fn only_a_refused_name_allows_the_in_place_fallback() {
        use std::io::{Error, ErrorKind};

        assert!(is_local_name_refused(&Error::from(
            ErrorKind::PermissionDenied
        )));
        assert!(is_local_name_refused(&Error::from(
            ErrorKind::InvalidFilename
        )));
        assert!(!is_local_name_refused(&Error::from(ErrorKind::StorageFull)));
        assert!(!is_local_name_refused(&Error::from(
            ErrorKind::QuotaExceeded
        )));
    }

    #[test]
    fn remote_fallback_needs_a_refused_name() {
        use russh_sftp::client::error::Error;
        use russh_sftp::protocol::{Status, StatusCode};

        let status = |status_code| {
            Error::Status(Status {
                id: 0,
                status_code,
                error_message: String::new(),
                language_tag: String::new(),
            })
        };
        assert!(is_remote_name_refused(&status(
            StatusCode::PermissionDenied
        )));
        assert!(is_remote_name_refused(&status(StatusCode::BadMessage)));
        // Disk full and quota arrive as a plain failure.
        assert!(!is_remote_name_refused(&status(StatusCode::Failure)));
        assert!(!is_remote_name_refused(&Error::Timeout));
    }

    #[test]
    fn owner_change_needs_both_sides_known_and_different() {
        assert!(!is_owner_change(Some((1000, 1000)), (1000, 1000)));
        assert!(is_owner_change(Some((1000, 1000)), (33, 33)));
        assert!(is_owner_change(Some((1000, 1000)), (1000, 33)));
        assert!(!is_owner_change(None, (33, 33)));
    }

    #[test]
    fn part_path_is_a_sibling_with_the_part_suffix() {
        assert_eq!(
            local_part_path(std::path::Path::new("/tmp/a/config.yml")),
            std::path::PathBuf::from("/tmp/a/config.yml.flint-part")
        );
    }

    fn remote(id: &str, bind_address: &str, bound_port: u16) -> RunningForward {
        let spec = ForwardSpec {
            id: id.to_string(),
            kind: ForwardKind::Remote,
            bind_address: bind_address.to_string(),
            bind_port: bound_port,
            target_host: "127.0.0.1".into(),
            target_port: 3000,
        };
        RunningForward::new(spec, bound_port)
    }

    fn map(forwards: Vec<RunningForward>) -> HashMap<String, RunningForward> {
        forwards
            .into_iter()
            .map(|f| (f.spec.id.clone(), f))
            .collect()
    }

    #[test]
    fn two_remote_forwards_on_different_ports_do_not_cross() {
        let forwards = map(vec![
            remote("web", "localhost", 8080),
            remote("db", "localhost", 5432),
        ]);
        // The server normalizes "localhost", so neither address matches and the
        // port has to be what keeps the two apart.
        let web = match_inbound(&forwards, "127.0.0.1", 8080).unwrap();
        let db = match_inbound(&forwards, "127.0.0.1", 5432).unwrap();
        assert_eq!(web.spec.id, "web");
        assert_eq!(db.spec.id, "db");
    }

    #[test]
    fn the_bound_address_breaks_a_tie_between_two_forwards_on_one_port() {
        let forwards = map(vec![
            remote("loopback", "localhost", 8080),
            remote("public", "0.0.0.0", 8080),
        ]);
        assert_eq!(
            match_inbound(&forwards, "0.0.0.0", 8080).unwrap().spec.id,
            "public"
        );
        assert_eq!(
            match_inbound(&forwards, "localhost", 8080).unwrap().spec.id,
            "loopback"
        );
    }

    fn file_entry(name: &str, kind: EntryKind) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir: kind == EntryKind::Folder,
            size: 0,
            mtime: 0,
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
            ..file_entry(name, EntryKind::Symlink)
        }
    }

    #[test]
    fn a_link_to_a_folder_opens_as_a_folder_but_is_not_one() {
        let link = link_to_dir("current", "releases/2026-10-01");
        assert!(link.opens_as_folder());
        assert!(!link.is_dir);
        assert!(!file_entry("broken", EntryKind::Symlink).opens_as_folder());
        assert!(file_entry("releases", EntryKind::Folder).opens_as_folder());
    }

    #[test]
    fn a_link_to_a_file_reports_the_size_of_the_file() {
        let mut link = FileEntry {
            size: 11,
            mtime: 5,
            ..file_entry("log", EntryKind::Symlink)
        };
        link.set_link_target(
            Some("app.log.big".into()),
            Some(LinkTargetStat {
                is_dir: false,
                size: Some(4096),
                mtime: Some(9),
            }),
        );
        assert_eq!((link.size, link.mtime), (4096, 9));
        assert!(!link.opens_as_folder());
        assert!(!link.is_dir);
    }

    #[test]
    fn a_link_to_a_folder_keeps_its_own_size_and_is_never_a_real_folder() {
        let mut link = FileEntry {
            size: 11,
            mtime: 5,
            ..file_entry("current", EntryKind::Symlink)
        };
        link.set_link_target(
            Some("releases/1".into()),
            Some(LinkTargetStat {
                is_dir: true,
                size: Some(4096),
                mtime: Some(9),
            }),
        );
        assert!(link.opens_as_folder());
        assert!(!link.is_dir);
        assert_eq!((link.size, link.mtime), (11, 5));
    }

    #[test]
    fn a_broken_link_keeps_its_target_text_and_stays_a_plain_link() {
        let mut link = FileEntry {
            size: 11,
            ..file_entry("gone", EntryKind::Symlink)
        };
        link.set_link_target(Some("nowhere".into()), None);
        assert_eq!(link.link_target.as_deref(), Some("nowhere"));
        assert!(!link.opens_as_folder());
        assert!(link.is_link_broken);
        assert_eq!(link.size, 11);
    }

    #[test]
    fn listing_puts_links_to_folders_among_the_folders() {
        let mut entries = vec![
            file_entry("notes.txt", EntryKind::File),
            file_entry("releases", EntryKind::Folder),
            file_entry("broken", EntryKind::Symlink),
            link_to_dir("current", "releases/2026-10-01"),
        ];
        sort_listing(&mut entries);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["current", "releases", "broken", "notes.txt"]);
    }

    #[test]
    fn delete_walk_removes_a_link_and_never_enters_its_target() {
        let (dirs, files) = split_for_delete(
            "/var/www/",
            vec![
                file_entry("releases", EntryKind::Folder),
                link_to_dir("current", "releases/2026-10-01"),
                file_entry("index.html", EntryKind::File),
            ],
        );
        assert_eq!(dirs, ["/var/www/releases"]);
        assert_eq!(files, ["/var/www/current", "/var/www/index.html"]);
    }

    #[test]
    fn a_local_forward_never_answers_an_inbound_channel() {
        // A -L listening on 8080 here has nothing to do with the server
        // listening on 8080 there.
        let mut local = remote("local", "127.0.0.1", 8080);
        local.spec.kind = ForwardKind::Local;
        assert!(match_inbound(&map(vec![local]), "127.0.0.1", 8080).is_none());
    }

    #[test]
    fn a_port_no_forward_asked_for_matches_nothing() {
        let forwards = map(vec![remote("web", "localhost", 8080)]);
        assert!(match_inbound(&forwards, "localhost", 9090).is_none());
    }

    fn progress_ticks(event_rx: &async_channel::Receiver<SftpEvent>) -> Vec<(u64, u64)> {
        let mut ticks = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let SftpEvent::TransferProgress { done, total, .. } = event {
                ticks.push((done, total));
            }
        }
        ticks
    }

    #[test]
    fn reporter_sends_the_first_chunk_and_holds_back_the_next() {
        let (event_tx, event_rx) = async_channel::bounded(8);
        let reporter = TransferReporter::new(&event_tx, Some(1), "site", 100);

        reporter.add(10);
        reporter.add(20);

        assert_eq!(progress_ticks(&event_rx), [(10, 100)]);
    }

    #[test]
    fn reporter_adds_up_the_bytes_of_every_file_that_shares_it() {
        let (event_tx, event_rx) = async_channel::bounded(8);
        let reporter = TransferReporter::new(&event_tx, Some(1), "site", 100);
        let (first_file, second_file) = (&reporter, &reporter);

        first_file.add(10);
        second_file.add(20);
        first_file.add(30);
        reporter.emit();

        assert_eq!(progress_ticks(&event_rx).last(), Some(&(60, 100)));
    }

    #[test]
    fn folder_files_stop_on_a_cancel_and_on_a_failed_sibling() {
        use std::sync::atomic::Ordering;

        let track = TransferTrack::new(7, CancelFlag::default());
        let files_track = TransferTrack::for_folder_files(Some(&track));
        assert!(!files_track.canceled());
        // Only the files of a folder take turns to write.
        assert!(track.write_slots.is_none());
        let slots = files_track.write_slots.as_ref().unwrap();
        assert_eq!(slots.available_permits(), FOLDER_WRITE_CONCURRENCY);

        files_track
            .has_failed_sibling
            .store(true, Ordering::Relaxed);
        assert!(files_track.canceled());
        // The failure of a file is not a cancel of the transfer.
        assert!(!track.canceled());

        let files_track = TransferTrack::for_folder_files(Some(&track));
        track.cancel.store(true, Ordering::Relaxed);
        assert!(files_track.canceled());
    }

    #[tokio::test]
    async fn folder_upload_plan_lists_parents_first_and_sums_the_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("site");
        std::fs::create_dir_all(root.join("css/fonts")).unwrap();
        std::fs::create_dir(root.join("empty")).unwrap();
        std::fs::write(root.join("index.html"), "12345").unwrap();
        std::fs::write(root.join("css/main.css"), "123").unwrap();
        std::fs::write(root.join("css/fonts/a.woff"), "1").unwrap();

        let mut plan = plan_folder_upload(&root, "/srv/site/", None).await.unwrap();

        for level in &mut plan.dir_levels {
            level.sort();
        }
        assert_eq!(
            plan.dir_levels,
            [
                vec!["/srv/site".to_string()],
                vec!["/srv/site/css".to_string(), "/srv/site/empty".to_string()],
                vec!["/srv/site/css/fonts".to_string()],
            ]
        );
        plan.files.sort_by(|a, b| a.remote.cmp(&b.remote));
        let item = |remote: &str, local: &str| UploadItem {
            from: UploadFrom::Local(root.join(local)),
            remote: remote.to_string(),
        };
        assert_eq!(
            plan.files,
            [
                item("/srv/site/css/fonts/a.woff", "css/fonts/a.woff"),
                item("/srv/site/css/main.css", "css/main.css"),
                item("/srv/site/index.html", "index.html"),
            ]
        );
        assert_eq!(plan.total_bytes, 9);
        assert_eq!(plan.skipped_links, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn folder_upload_plan_counts_links_and_does_not_follow_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("site");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("index.html"), "12345").unwrap();
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
        std::os::unix::fs::symlink(root.join("index.html"), root.join("home.html")).unwrap();

        let plan = plan_folder_upload(&root, "/srv/site", None).await.unwrap();

        assert_eq!(plan.dir_levels, [vec!["/srv/site".to_string()]]);
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.total_bytes, 5);
        assert_eq!(plan.skipped_links, 2);
    }

    #[tokio::test]
    async fn folder_upload_plan_stops_when_canceled() {
        let dir = tempfile::tempdir().unwrap();
        let track = TransferTrack::new(1, CancelFlag::default());
        track
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let Err(error) = plan_folder_upload(dir.path(), "/srv/site", Some(&track)).await else {
            panic!("a canceled walk must not produce a plan");
        };

        assert!(error.is::<TransferCanceled>());
    }

    fn sized_entry(name: &str, kind: EntryKind, size: u64) -> FileEntry {
        FileEntry {
            size,
            ..file_entry(name, kind)
        }
    }

    #[test]
    fn copy_plan_takes_files_and_folders_and_leaves_the_rest() {
        let mut plan = UploadPlan::default();
        let mut subdirs = Vec::new();
        let entries = vec![
            sized_entry("css", EntryKind::Folder, 4096),
            sized_entry("index.html", EntryKind::File, 5),
            sized_entry("app.js", EntryKind::File, 7),
            sized_entry("current", EntryKind::Symlink, 11),
            sized_entry("app.sock", EntryKind::Other, 13),
        ];

        add_listing_to_plan("/var/www", "/srv/site", entries, &mut plan, &mut subdirs);

        assert_eq!(
            subdirs,
            [("/var/www/css".to_string(), "/srv/site/css".to_string())]
        );
        let item = |source: &str, remote: &str| UploadItem {
            from: UploadFrom::Peer(source.to_string()),
            remote: remote.to_string(),
        };
        assert_eq!(
            plan.files,
            [
                item("/var/www/index.html", "/srv/site/index.html"),
                item("/var/www/app.js", "/srv/site/app.js"),
            ]
        );
        assert_eq!(plan.total_bytes, 12);
        assert_eq!(plan.skipped_links, 1);
        // The walk adds the directory levels, not the listing.
        assert!(plan.dir_levels.is_empty());
    }

    #[test]
    fn an_overwrite_in_place_cuts_only_a_longer_old_file() {
        assert!(has_old_tail(Some(100), 40));
        assert!(!has_old_tail(Some(40), 40));
        assert!(!has_old_tail(Some(40), 100));
        // A server that reports no size: cut to be sure.
        assert!(has_old_tail(None, 40));
    }

    async fn fill_all(
        chunks: &async_channel::Receiver<RelayChunk>,
        track: Option<&TransferTrack>,
    ) -> anyhow::Result<Vec<u8>> {
        let mut source = UploadSource::Relay(chunks);
        let mut buf = vec![0u8; 8];
        let mut all = Vec::new();
        loop {
            let n = source.fill(&mut buf, track).await?;
            if n == 0 {
                return Ok(all);
            }
            all.extend_from_slice(&buf[..n]);
        }
    }

    #[tokio::test]
    async fn relay_source_yields_the_data_and_stops_at_the_end_mark() {
        let (tx, rx) = async_channel::bounded(RELAY_PIPE_CHUNKS);
        tx.send(RelayChunk::Data(b"abc".to_vec())).await.unwrap();
        tx.send(RelayChunk::Data(Vec::new())).await.unwrap();
        tx.send(RelayChunk::Data(b"de".to_vec())).await.unwrap();
        tx.send(RelayChunk::End).await.unwrap();

        assert_eq!(fill_all(&rx, None).await.unwrap(), b"abcde");
    }

    #[tokio::test]
    async fn relay_source_passes_on_a_read_failure() {
        let (tx, rx) = async_channel::bounded(RELAY_PIPE_CHUNKS);
        tx.send(RelayChunk::Data(b"abc".to_vec())).await.unwrap();
        tx.send(RelayChunk::Failed("read /srv/a: denied".to_string()))
            .await
            .unwrap();

        let error = fill_all(&rx, None).await.unwrap_err();

        assert_eq!(error.to_string(), "read /srv/a: denied");
    }

    #[tokio::test]
    async fn relay_source_closed_before_the_end_mark_is_an_error() {
        let (tx, rx) = async_channel::bounded(RELAY_PIPE_CHUNKS);
        tx.send(RelayChunk::Data(b"abc".to_vec())).await.unwrap();
        drop(tx);

        let error = fill_all(&rx, None).await.unwrap_err();

        assert_eq!(error.to_string(), SOURCE_LOST);
        assert!(!error.is::<TransferCanceled>());
    }

    #[tokio::test]
    async fn relay_source_stops_waiting_when_canceled() {
        let (_tx, rx) = async_channel::bounded::<RelayChunk>(RELAY_PIPE_CHUNKS);
        let track = TransferTrack::new(1, CancelFlag::default());
        track
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let error = fill_all(&rx, Some(&track)).await.unwrap_err();

        assert!(error.is::<TransferCanceled>());
    }

    #[tokio::test]
    async fn relay_source_closed_by_a_cancel_reports_the_cancel() {
        let (tx, rx) = async_channel::bounded::<RelayChunk>(RELAY_PIPE_CHUNKS);
        let track = TransferTrack::new(1, CancelFlag::default());
        track
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        drop(tx);

        let error = fill_all(&rx, Some(&track)).await.unwrap_err();

        assert!(error.is::<TransferCanceled>());
    }
}
