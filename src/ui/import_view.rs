//! Dialog content for importing hosts and snippets from `~/.ssh/config`,
//! PuTTY, and `termius-export` JSON files (flint's own format). Scans the
//! config and PuTTY on open and loads a termius-export JSON on demand. Rows
//! that are already in the vault start unchecked. The selected ones are
//! written into the vault.

use std::path::Path;

use gpui::prelude::*;
use gpui::{div, px, Context, Entity, EventEmitter, Window};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    v_flex, ActiveTheme as _, Disableable as _, Sizable as _,
};
use uuid::Uuid;

use crate::import::{
    parse_termius_export, scan_all, ImportSource, ImportedHost, ImportedJump, ImportedSnippet,
    JumpTarget,
};
use crate::ui::{dialog_max_height, VaultState};
use crate::vault::{AuthMethod, ServerEntry, ServerEntrySummary, SnippetBody, VaultManager};

pub enum ImportViewEvent {
    /// Import finished. `keys_failed` counts hosts whose key could not be
    /// read/decoded (they were imported with fallback auth); `jumps_skipped`
    /// counts `ProxyJump`s that could not be turned into a link, so those
    /// hosts were imported as direct connections. `save_error` is set when the
    /// hosts reached the running vault but the file write failed: they work
    /// now and are gone on restart.
    Done {
        added: usize,
        snippets_added: usize,
        keys_failed: usize,
        jumps_skipped: usize,
        save_error: Option<String>,
    },
    Cancelled,
}

struct Candidate {
    host: ImportedHost,
    is_selected: bool,
    /// An entry with the same host:port already exists in the vault.
    is_in_vault: bool,
    /// Key material: inline (Termius) or the `IdentityFile` content, read
    /// once when the candidate is built. `None` = no key or unreadable file.
    key_pem: Option<String>,
    /// Masked passphrase input, present only when the key is encrypted and
    /// the source didn't carry the passphrase. Never asked for otherwise.
    passphrase_input: Option<Entity<InputState>>,
}

/// One snippet offered by a loaded export file.
struct SnippetCandidate {
    snippet: ImportedSnippet,
    is_selected: bool,
    /// A snippet with the same name already exists in the vault.
    is_in_vault: bool,
}

/// A found host with its key read and checked. Made on a background thread:
/// reading the key touches the disk, and checking it decodes it.
struct LoadedHost {
    host: ImportedHost,
    key_pem: Option<String>,
    /// The key is encrypted and the source did not carry the passphrase.
    needs_passphrase: bool,
}

fn load_host(mut host: ImportedHost) -> LoadedHost {
    let key_pem = take_key_pem(&mut host);
    let needs_passphrase = host.key_passphrase.is_none()
        && key_pem.as_deref().is_some_and(crate::ssh::key_is_encrypted);
    LoadedHost {
        host,
        key_pem,
        needs_passphrase,
    }
}

/// One termius-export file, read and parsed.
struct LoadedFile {
    hosts: Vec<LoadedHost>,
    snippets: Vec<ImportedSnippet>,
    skipped: Vec<String>,
}

/// The error text is user-facing.
fn load_termius_file(path: &Path) -> Result<LoadedFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let import = parse_termius_export(&text)?;
    Ok(LoadedFile {
        hosts: import.hosts.into_iter().map(load_host).collect(),
        snippets: import.snippets,
        skipped: import.skipped,
    })
}

pub struct ImportView {
    vault_state: Entity<VaultState>,
    candidates: Vec<Candidate>,
    snippets: Vec<SnippetCandidate>,
    /// The scan of the ssh config and PuTTY has not come back yet.
    is_scanning: bool,
    /// User-facing problem with the last "load from file" or import attempt.
    file_error: Option<String>,
    /// Records of the loaded export file that could not be read.
    file_skipped: Vec<String>,
}

impl EventEmitter<ImportViewEvent> for ImportView {}

impl ImportView {
    pub fn new(
        vault_state: Entity<VaultState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // The scan reads the config, the files it includes and every key
        // file it names, so it runs off the main thread.
        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async { scan_all().into_iter().map(load_host).collect::<Vec<_>>() })
                .await;
            this.update_in(cx, |this, window, cx| this.add_scanned(loaded, window, cx))
                .ok();
        })
        .detach();

        Self {
            vault_state,
            candidates: Vec::new(),
            snippets: Vec::new(),
            is_scanning: true,
            file_error: None,
            file_skipped: Vec::new(),
        }
    }

    fn add_scanned(
        &mut self,
        loaded: Vec<LoadedHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = Self::existing_addresses(&self.vault_state, cx);
        let mut candidates: Vec<Candidate> = loaded
            .into_iter()
            .map(|host| Self::make_candidate(host, &existing, window, cx))
            .collect();
        // A file loaded during the scan already has rows: they go last.
        candidates.append(&mut self.candidates);
        self.candidates = candidates;
        self.is_scanning = false;
        cx.notify();
    }

    /// host:port pairs already in the vault. Their candidates start
    /// unchecked so re-imports are ignored by default.
    fn existing_addresses(
        vault_state: &Entity<VaultState>,
        cx: &Context<Self>,
    ) -> Vec<(String, u16)> {
        vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_entries().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.host.to_lowercase(), e.port))
            .collect()
    }

    fn make_candidate(
        loaded: LoadedHost,
        existing: &[(String, u16)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Candidate {
        let LoadedHost {
            host,
            key_pem,
            needs_passphrase,
        } = loaded;
        let is_in_vault = existing
            .iter()
            .any(|(h, p)| *h == host.host.to_lowercase() && *p == host.port);
        let passphrase_input = needs_passphrase.then(|| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("key passphrase")
            })
        });
        Candidate {
            is_selected: !is_in_vault,
            host,
            is_in_vault,
            key_pem,
            passphrase_input,
        }
    }

    /// Snippet names already in the vault. Matched with `eq_ignore_ascii_case`,
    /// the rule [`VaultManager::find_snippet_by_name`] uses on import.
    fn existing_snippet_names(vault_state: &Entity<VaultState>, cx: &Context<Self>) -> Vec<String> {
        vault_state
            .read(cx)
            .manager()
            .and_then(|vm| vm.list_snippets().ok())
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    fn selected_count(&self) -> usize {
        self.candidates.iter().filter(|c| c.is_selected).count()
            + self.snippets.iter().filter(|s| s.is_selected).count()
    }

    fn toggle(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(c) = self.candidates.get_mut(ix) {
            c.is_selected = !c.is_selected;
            cx.notify();
        }
    }

    fn toggle_snippet(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(s) = self.snippets.get_mut(ix) {
            s.is_selected = !s.is_selected;
            cx.notify();
        }
    }

    fn set_all(&mut self, is_selected: bool, cx: &mut Context<Self>) {
        for c in &mut self.candidates {
            c.is_selected = is_selected;
        }
        for s in &mut self.snippets {
            s.is_selected = is_selected;
        }
        cx.notify();
    }

    /// Pick a termius-export JSON and add its hosts and snippets as
    /// candidates. Picking again replaces the previous file's rows.
    fn pick_termius_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .add_filter("termius-export JSON", &["json"])
                .pick_file()
                .await;
            let Some(handle) = handle else { return };
            let path = handle.path().to_path_buf();
            let loaded = cx
                .background_executor()
                .spawn(async move { load_termius_file(&path) })
                .await;
            this.update_in(cx, |this, window, cx| {
                match loaded {
                    Ok(file) => this.add_file_candidates(file, window, cx),
                    Err(e) => {
                        // The rows of an earlier file stay, and so does their
                        // skipped line.
                        this.file_error = Some(format!("Could not load export: {e}"));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn add_file_candidates(
        &mut self,
        file: LoadedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_error = if !file.hosts.is_empty() || !file.snippets.is_empty() {
            None
        } else if file.skipped.is_empty() {
            Some("The export file contains no hosts or snippets.".to_string())
        } else {
            Some("The export file contains no hosts or snippets that could be read.".to_string())
        };
        if !file.skipped.is_empty() {
            tracing::warn!(skipped = ?file.skipped, "termius export: some records were skipped");
        }
        self.file_skipped = file.skipped;
        self.candidates
            .retain(|c| c.host.source != ImportSource::Termius);
        let existing = Self::existing_addresses(&self.vault_state, cx);
        let new: Vec<Candidate> = file
            .hosts
            .into_iter()
            .map(|h| Self::make_candidate(h, &existing, window, cx))
            .collect();
        self.candidates.extend(new);

        let existing_names = Self::existing_snippet_names(&self.vault_state, cx);
        self.snippets = file
            .snippets
            .into_iter()
            .map(|snippet| {
                let is_in_vault = existing_names
                    .iter()
                    .any(|n| n.eq_ignore_ascii_case(&snippet.name));
                SnippetCandidate {
                    snippet,
                    is_selected: !is_in_vault,
                    is_in_vault,
                }
            })
            .collect();
        cx.notify();
    }

    fn import_selected(&mut self, cx: &mut Context<Self>) {
        // (host, pem, passphrase) per selected row. The passphrase inputs are
        // read here, outside the vault borrow.
        let selected: Vec<(ImportedHost, Option<String>, Option<String>)> = self
            .candidates
            .iter()
            .filter(|c| c.is_selected)
            .map(|c| {
                let passphrase = c.host.key_passphrase.clone().or_else(|| {
                    c.passphrase_input
                        .as_ref()
                        .map(|i| i.read(cx).value().to_string())
                        .filter(|v| !v.is_empty())
                });
                (c.host.clone(), c.key_pem.clone(), passphrase)
            })
            .collect();

        let snippets: Vec<ImportedSnippet> = self
            .snippets
            .iter()
            .filter(|s| s.is_selected)
            .map(|s| s.snippet.clone())
            .collect();

        let counts = self.vault_state.update(cx, |vs, _| {
            let Some(vm) = vs.manager_mut() else {
                tracing::error!("import attempted with a locked vault");
                return None;
            };
            Some(write_selected(vm, &selected, &snippets))
        });

        // Closing with "Imported 0 hosts." would read as a success.
        let Some(counts) = counts else {
            self.file_error = Some("The vault is locked. Unlock it and try again.".to_string());
            cx.notify();
            return;
        };
        cx.emit(ImportViewEvent::Done {
            added: counts.added,
            snippets_added: counts.snippets_added,
            keys_failed: counts.keys_failed,
            jumps_skipped: counts.jumps_skipped,
            save_error: counts.save_error,
        });
    }
}

/// Counts for the status line after an import.
#[derive(Default)]
struct ImportCounts {
    added: usize,
    /// Snippets created. One whose name was already taken is left alone and
    /// not counted.
    snippets_added: usize,
    /// Hosts whose key could not be read or decoded. Those were imported
    /// with fallback auth.
    keys_failed: usize,
    /// `ProxyJump`s that could not be turned into a link.
    jumps_skipped: usize,
    /// Why the vault file could not be written. The entries are in the
    /// running vault either way; this says they were not persisted.
    save_error: Option<String>,
}

/// Write every selected row into the vault in one batch, then link the
/// `ProxyJump`s. Snippets go first so a host's startup snippet can resolve to
/// one the same file brought in. The batch only defers the file write: on a
/// save failure the records are still in the running vault, so the counts
/// stand and `save_error` carries the reason they will not survive a restart.
fn write_selected(
    vm: &mut VaultManager,
    selected: &[(ImportedHost, Option<String>, Option<String>)],
    snippets: &[ImportedSnippet],
) -> ImportCounts {
    let now = unix_now();
    // Local user as the fallback login when the source names none.
    let default_user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "root".to_string());

    let mut counts = ImportCounts::default();
    // Vault id of each row's new entry, in the same order, so the jump-host
    // pass can find the row a `ProxyJump` names.
    let mut created: Vec<Option<Uuid>> = Vec::with_capacity(selected.len());

    // One encrypt + atomic write for the whole import: every record method
    // saves on its own, and an import touches dozens of them.
    let write = vm.batch(|vm| {
        for snippet in snippets {
            if vm.find_snippet_by_name(&snippet.name).is_some() {
                continue;
            }
            let body = SnippetBody::Script {
                content: snippet.script.clone(),
            };
            match vm.add_snippet(snippet.name.clone(), None, body) {
                Ok(_) => counts.snippets_added += 1,
                Err(e) => tracing::error!(?e, name = snippet.name, "failed to add snippet"),
            }
        }

        for (host, pem, passphrase) in selected {
            let auth = resolve_auth(
                vm,
                host,
                pem.as_deref(),
                passphrase.clone(),
                &mut counts.keys_failed,
            );
            let (group, tags) = resolve_group_and_tags(vm, host);
            let entry = ServerEntry {
                id: Uuid::new_v4(),
                label: host.label.clone(),
                host: host.host.clone(),
                port: host.port,
                username: host
                    .username
                    .clone()
                    .unwrap_or_else(|| default_user.clone()),
                auth,
                notes: Some(format!("Imported from {}", host.source.display_label())),
                os: host.os.clone(),
                last_opened_at: None,
                host_key_fingerprint: None,
                last_remote_dir: None,
                last_local_dir: None,
                group,
                tags,
                proxy: crate::vault::ProxyChoice::Default,
                // Linked in a second pass: a `ProxyJump` may name a host
                // defined further down the config, which has no vault id
                // before every entry exists.
                jump_host: None,
                forwards: Vec::new(),
                startup_snippet: host
                    .startup_snippet
                    .as_deref()
                    .and_then(|name| vm.find_snippet_by_name(name)),
                created_at: now,
                updated_at: now,
            };
            let entry_id = entry.id;
            match vm.add_entry(entry) {
                Ok(()) => {
                    counts.added += 1;
                    created.push(Some(entry_id));
                }
                Err(e) => {
                    tracing::error!(?e, host = host.host, "failed to add imported entry");
                    created.push(None);
                }
            }
        }
        counts.jumps_skipped = link_jump_hosts(vm, selected, &created);
    });

    if let Err(e) = write {
        tracing::error!(
            ?e,
            added = counts.added,
            "failed to write the vault after import"
        );
        counts.save_error = Some(e.to_string());
    }
    counts
}

/// How many skipped records the dialog names before it only counts them.
const SKIPPED_NAMED_MAX: usize = 5;

/// "1 host", "3 hosts". Only for nouns that take a plain "s".
pub(crate) fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        return format!("1 {noun}");
    }
    format!("{count} {noun}s")
}

/// The "Skipped N records: ..." line under the header, or `None` when the
/// file was read in full.
fn skipped_summary(skipped: &[String]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let named = skipped
        .iter()
        .take(SKIPPED_NAMED_MAX)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let rest = skipped.len().saturating_sub(SKIPPED_NAMED_MAX);
    let more = if rest == 0 {
        String::new()
    } else {
        format!(", and {rest} more")
    };
    Some(format!(
        "Skipped {} in the export file: {named}{more}.",
        plural(skipped.len(), "record")
    ))
}

/// Key material for a row: inline (Termius) or read from its identity file.
///
/// Every host without an `IdentityFile` shares the default key. An encrypted
/// one would put the same passphrase field on each of those rows, and such a
/// key is normally unlocked by the agent. A key in a format we cannot decode
/// would count as a lost key on each row. So the default key is kept only
/// when it is usable as it is; otherwise the host is left to the agent.
fn take_key_pem(host: &mut ImportedHost) -> Option<String> {
    let key_pem = host.key_pem.clone().or_else(|| {
        host.identity_file
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
    });
    let is_usable = key_pem
        .as_deref()
        .is_some_and(crate::ssh::key_decodes_without_passphrase);
    if host.is_default_identity && !is_usable {
        host.identity_file = None;
        host.is_default_identity = false;
        return None;
    }
    key_pem
}

/// Does a row with no usable key and no stored password go to the SSH agent?
/// That is what `ssh` does for an ssh config host, and a Termius host with no
/// credentials says the same. PuTTY sessions keep the empty password, which
/// ends in a password prompt.
fn falls_back_to_agent(host: &ImportedHost) -> bool {
    host.password.is_none() && host.source != ImportSource::Putty
}

/// Name for the vault copy of a row's key: the name the source gave it, the
/// file name of a default key (every row shares that one), or the row's label.
fn imported_key_name(host: &ImportedHost) -> String {
    if let Some(name) = &host.key_name {
        return name.clone();
    }
    let default_key_name = host
        .identity_file
        .as_ref()
        .filter(|_| host.is_default_identity)
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned());
    default_key_name.unwrap_or_else(|| format!("{} (imported)", host.label))
}

/// Auth for one imported row: the key, then the stored password, then the
/// agent, then an empty password (PuTTY only, see `falls_back_to_agent`).
/// `keys_failed` counts rows that carried key material we could not use.
fn resolve_auth(
    vm: &mut VaultManager,
    host: &ImportedHost,
    pem: Option<&str>,
    passphrase: Option<String>,
    keys_failed: &mut usize,
) -> AuthMethod {
    let fallback = || match &host.password {
        Some(password) => AuthMethod::Password(password.clone()),
        None if falls_back_to_agent(host) => AuthMethod::Agent,
        None => AuthMethod::Password(String::new()),
    };
    let Some(pem) = pem else {
        // An identity file that could not be read still counts as a lost key.
        if host.identity_file.is_some() {
            *keys_failed += 1;
        }
        return fallback();
    };
    match vm.add_key_dedup(imported_key_name(host), pem.to_string(), passphrase) {
        Ok(key_id) => AuthMethod::PrivateKey { key_id },
        Err(e) => {
            // Wrong or missing passphrase, or an unparseable key: import the
            // host anyway with fallback auth.
            tracing::warn!(host = host.host, error = %e, "could not import key");
            *keys_failed += 1;
            fallback()
        }
    }
}

/// Resolve the folder path / tag names of an imported host to vault ids,
/// creating missing groups/tags on the way.
fn resolve_group_and_tags(vm: &mut VaultManager, host: &ImportedHost) -> (Option<Uuid>, Vec<Uuid>) {
    let mut group = None;
    for name in &host.group_path {
        match vm.find_or_create_group(name, group) {
            Ok(id) => group = Some(id),
            Err(e) => {
                tracing::warn!(group = name, error = %e, "could not create group");
                break;
            }
        }
    }
    let mut tags = Vec::new();
    for name in &host.tags {
        match vm.find_or_create_tag(name) {
            Ok(id) => tags.push(id),
            Err(e) => tracing::warn!(tag = name, error = %e, "could not create tag"),
        }
    }
    (group, tags)
}

/// Apply the `ProxyJump` links the ssh config declared, now that every
/// imported host has a vault id. Returns how many hops were dropped. Those
/// hosts stay direct, so the user has to be told.
fn link_jump_hosts(
    vm: &mut VaultManager,
    selected: &[(ImportedHost, Option<String>, Option<String>)],
    created: &[Option<Uuid>],
) -> usize {
    let entries = vm.list_entries().unwrap_or_default();
    let mut skipped = 0usize;

    for ((host, _, _), entry_id) in selected.iter().zip(created) {
        let Some(entry_id) = *entry_id else { continue };
        let target = match &host.jump {
            None => continue,
            Some(ImportedJump::MultiHop(raw)) | Some(ImportedJump::Malformed(raw)) => {
                tracing::warn!(host = host.label, jump = raw, "ProxyJump cannot be linked");
                skipped += 1;
                continue;
            }
            Some(ImportedJump::Single(target)) => target,
        };
        let Some(jump_id) = find_jump_entry(&entries, target) else {
            tracing::warn!(
                host = host.label,
                jump = target.name,
                "ProxyJump names a host that is not in the vault",
            );
            skipped += 1;
            continue;
        };
        if let Err(e) = vm.set_entry_jump_host(entry_id, Some(jump_id)) {
            tracing::warn!(host = host.label, error = %e, "could not link jump host");
            skipped += 1;
        }
    }
    skipped
}

/// Match a `ProxyJump` hop to a vault entry: by the alias first (it becomes
/// the imported host's label), then by the address it resolves to.
fn find_jump_entry(entries: &[ServerEntrySummary], target: &JumpTarget) -> Option<Uuid> {
    entries
        .iter()
        .find(|e| e.label == target.name)
        .or_else(|| {
            entries
                .iter()
                .find(|e| e.host.eq_ignore_ascii_case(&target.host) && e.port == target.port)
        })
        .map(|e| e.id)
}

impl Render for ImportView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let host_count = self.candidates.len();
        let snippet_count = self.snippets.len();
        let total = host_count + snippet_count;
        let selected = self.selected_count();

        let header = v_flex()
            .gap_1()
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(theme.foreground)
                    .child("Import"),
            )
            .child(div().text_sm().text_color(theme.muted_foreground).child(
                if total == 0 && self.is_scanning {
                    "Looking for hosts in ~/.ssh/config and PuTTY sessions…".to_string()
                } else if total == 0 {
                    "No hosts found in ~/.ssh/config or PuTTY sessions. \
                         You can still load a termius-export JSON file."
                        .to_string()
                } else if snippet_count == 0 {
                    format!(
                        "Found {}. Hosts already in the vault are unchecked.",
                        plural(host_count, "host")
                    )
                } else {
                    format!(
                        "Found {} and {}. The ones already in the vault are unchecked.",
                        plural(host_count, "host"),
                        plural(snippet_count, "snippet")
                    )
                },
            ));

        let mut card = v_flex()
            .max_h(dialog_max_height(window))
            .p(px(12.))
            .gap(px(8.))
            .child(header);

        if let Some(error) = &self.file_error {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }

        if let Some(summary) = skipped_summary(&self.file_skipped) {
            card = card.child(div().text_sm().text_color(theme.warning).child(summary));
        }

        let mut toolbar = h_flex().px_1().items_center().justify_between();
        toolbar = if total > 0 {
            toolbar.child(
                Checkbox::new("import-all")
                    .checked(selected == total)
                    .label("Select all")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_all(*checked, cx);
                    })),
            )
        } else {
            toolbar.child(div())
        };
        toolbar = toolbar.child(
            Button::new("import-termius-file")
                .ghost()
                .small()
                .child("Load termius-export JSON…")
                .tooltip("flint's own JSON format. Termius itself has no export.")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.pick_termius_file(window, cx);
                })),
        );
        card = card.child(toolbar);

        if total > 0 {
            let rows = self.candidates.iter().enumerate().map(|(ix, c)| {
                let addr = format!(
                    "{}{}:{}",
                    c.host
                        .username
                        .as_deref()
                        .map(|u| format!("{u}@"))
                        .unwrap_or_default(),
                    c.host.host,
                    c.host.port,
                );
                let name = if c.host.label.is_empty() {
                    c.host.host.clone()
                } else {
                    c.host.label.clone()
                };
                let jump_badge = match &c.host.jump {
                    None => None,
                    Some(ImportedJump::Single(target)) => {
                        Some((format!("via {}", target.name), theme.muted_foreground))
                    }
                    Some(ImportedJump::MultiHop(_)) => {
                        Some(("multi-hop jump skipped".to_string(), theme.warning))
                    }
                    Some(ImportedJump::Malformed(_)) => {
                        Some(("jump not understood".to_string(), theme.warning))
                    }
                };

                let row = h_flex()
                    .id(("import-row", ix))
                    .w_full()
                    .h(px(24.))
                    .px(px(6.))
                    .gap(px(8.))
                    .items_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.muted))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(ix, cx)))
                    .child(Checkbox::new(("import-check", ix)).checked(c.is_selected))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(160.))
                            .text_size(px(12.5))
                            .text_color(theme.foreground)
                            .truncate()
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(addr),
                    )
                    .when(!c.host.group_path.is_empty(), |el| {
                        el.child(badge(
                            &c.host.group_path.join(" / "),
                            theme.muted_foreground,
                            cx,
                        ))
                    })
                    .children(
                        c.host
                            .tags
                            .iter()
                            .map(|t| badge(&format!("#{t}"), theme.muted_foreground, cx)),
                    )
                    .when(
                        c.key_pem.is_some() || c.host.identity_file.is_some(),
                        |el| {
                            // A default key shows its file name, so it is clear
                            // which key is copied into the vault.
                            let text = if c.host.is_default_identity {
                                imported_key_name(&c.host)
                            } else {
                                "key".to_string()
                            };
                            el.child(badge(&text, theme.muted_foreground, cx))
                        },
                    )
                    .when(c.key_pem.is_none() && falls_back_to_agent(&c.host), |el| {
                        el.child(badge("agent", theme.muted_foreground, cx))
                    })
                    .when_some(c.host.startup_snippet.as_deref(), |el, name| {
                        el.child(badge(
                            &format!("startup: {name}"),
                            theme.muted_foreground,
                            cx,
                        ))
                    })
                    .when_some(jump_badge, |el, (text, color)| {
                        el.child(badge(&text, color, cx))
                    })
                    .when(c.is_in_vault, |el| {
                        el.child(badge("exists", theme.warning, cx))
                    })
                    .child(badge(
                        c.host.source.display_label(),
                        theme.muted_foreground,
                        cx,
                    ));

                // The only place this dialog asks for a passphrase: under
                // the row of an encrypted key whose passphrase is not known.
                let passphrase_row =
                    c.passphrase_input
                        .as_ref()
                        .filter(|_| c.is_selected)
                        .map(|input| {
                            h_flex()
                                .pl(px(34.))
                                .pr_2()
                                .pb_1()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("encrypted key:"),
                                )
                                .child(div().flex_1().child(Input::new(input).small()))
                        });

                v_flex().child(row).children(passphrase_row)
            });

            let snippet_rows = self.snippets.iter().enumerate().map(|(ix, c)| {
                let preview = c.snippet.script.lines().next().unwrap_or_default().trim();

                h_flex()
                    .id(("import-snippet-row", ix))
                    .w_full()
                    .h(px(24.))
                    .px(px(6.))
                    .gap(px(8.))
                    .items_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.muted))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_snippet(ix, cx)))
                    .child(Checkbox::new(("import-snippet-check", ix)).checked(c.is_selected))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(160.))
                            .text_size(px(12.5))
                            .text_color(theme.foreground)
                            .truncate()
                            .child(c.snippet.name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(preview.to_string()),
                    )
                    .when(c.is_in_vault, |el| {
                        el.child(badge("exists", theme.warning, cx))
                    })
                    .child(badge("snippet", theme.muted_foreground, cx))
            });

            card = card.child(
                v_flex()
                    .id("import-list")
                    // What the dialog can spare once the heading, the toolbar,
                    // the footer buttons and the card's own padding have taken
                    // their share. A fixed cap overflows a short window.
                    .max_h((dialog_max_height(window) - px(140.)).max(px(120.)))
                    .overflow_y_scroll()
                    .gap_0p5()
                    .children(rows)
                    .children(snippet_rows),
            );
        }

        let mut footer = h_flex().gap_2().justify_end().child(
            Button::new("import-cancel")
                .ghost()
                .child(if total == 0 { "Close" } else { "Cancel" })
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.emit(ImportViewEvent::Cancelled);
                })),
        );
        if total > 0 {
            footer = footer.child(
                Button::new("import-confirm")
                    .primary()
                    .child(format!("Import {selected}"))
                    .disabled(selected == 0)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.import_selected(cx);
                    })),
            );
        }

        card.child(footer)
    }
}

/// Small dim chip ("key", "exists", "#tag", group path, source name).
fn badge(text: &str, color: gpui::Hsla, cx: &Context<ImportView>) -> gpui::AnyElement {
    let theme = cx.theme();
    div()
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(3.))
        .bg(theme.muted)
        .text_size(px(10.))
        .text_color(color)
        .child(text.to_string())
        .into_any_element()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::ProxyChoice;

    fn summary(label: &str, host: &str, port: u16) -> ServerEntrySummary {
        ServerEntrySummary {
            id: Uuid::new_v4(),
            label: label.to_string(),
            host: host.to_string(),
            port,
            username: "deploy".to_string(),
            os: None,
            last_opened_at: None,
            group_name: None,
            tag_names: Vec::new(),
            proxy: ProxyChoice::Default,
            team_name: None,
        }
    }

    fn target(name: &str, host: &str, port: u16) -> JumpTarget {
        JumpTarget {
            name: name.to_string(),
            host: host.to_string(),
            port,
            username: None,
        }
    }

    fn test_vault(dir: &tempfile::TempDir) -> VaultManager {
        VaultManager::create_with_params(
            &dir.path().join("vault.bin"),
            &secrecy::SecretString::new("pw".to_string()),
            crate::vault::format::KdfParams {
                m_cost: 8,
                t_cost: 1,
                p_cost: 1,
            },
        )
        .unwrap()
        .0
    }

    #[test]
    fn plural_adds_an_s_except_for_one() {
        assert_eq!(plural(0, "host"), "0 hosts");
        assert_eq!(plural(1, "host"), "1 host");
        assert_eq!(plural(2, "snippet"), "2 snippets");
    }

    #[test]
    fn skipped_summary_names_records_and_counts_the_overflow() {
        assert_eq!(skipped_summary(&[]), None);

        let two = vec![
            "host \"a\" (no address)".to_string(),
            "key #2 (missing field `pem`)".to_string(),
        ];
        assert_eq!(
            skipped_summary(&two).unwrap(),
            "Skipped 2 records in the export file: host \"a\" (no address), \
             key #2 (missing field `pem`)."
        );
        assert_eq!(
            skipped_summary(&two[..1]).unwrap(),
            "Skipped 1 record in the export file: host \"a\" (no address)."
        );

        let many: Vec<String> = (1..=8).map(|n| format!("host #{n} (bad)")).collect();
        let summary = skipped_summary(&many).unwrap();
        assert!(summary.starts_with("Skipped 8 records in"), "{summary}");
        assert!(summary.contains("host #5 (bad), and 3 more."), "{summary}");
        assert!(!summary.contains("host #6"), "{summary}");
    }

    fn termius_host(label: &str, startup_snippet: Option<&str>) -> ImportedHost {
        ImportedHost {
            label: label.to_string(),
            host: format!("{label}.example.com"),
            port: 22,
            username: Some("deploy".to_string()),
            identity_file: None,
            is_default_identity: false,
            key_pem: None,
            key_name: None,
            key_passphrase: None,
            password: Some("pw".to_string()),
            os: None,
            group_path: Vec::new(),
            tags: Vec::new(),
            startup_snippet: startup_snippet.map(str::to_string),
            jump: None,
            source: ImportSource::Termius,
        }
    }

    fn ssh_config_host(
        identity_file: Option<std::path::PathBuf>,
        is_default: bool,
    ) -> ImportedHost {
        ImportedHost {
            identity_file,
            is_default_identity: is_default,
            password: None,
            source: ImportSource::SshConfig,
            ..termius_host("web", None)
        }
    }

    #[test]
    fn an_ssh_config_host_without_a_key_uses_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = test_vault(&dir);
        let mut keys_failed = 0;

        let auth = resolve_auth(
            &mut vm,
            &ssh_config_host(None, false),
            None,
            None,
            &mut keys_failed,
        );

        assert!(matches!(auth, AuthMethod::Agent));
        assert_eq!(keys_failed, 0);
    }

    #[test]
    fn a_putty_session_without_a_key_keeps_the_empty_password() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = test_vault(&dir);
        let host = ImportedHost {
            source: ImportSource::Putty,
            ..ssh_config_host(None, false)
        };

        let auth = resolve_auth(&mut vm, &host, None, None, &mut 0);

        assert!(matches!(auth, AuthMethod::Password(password) if password.is_empty()));
    }

    #[test]
    fn a_stored_password_beats_the_agent() {
        assert!(!falls_back_to_agent(&termius_host("web", None)));
    }

    // Throwaway keys made for these tests; the second has the passphrase "secret".
    const PLAIN_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDNCBAqbPr5W4GmLORUKjfNKAm4w1exA7cV631Qp+ehzgAAAIgdqxg9HasY
PQAAAAtzc2gtZWQyNTUxOQAAACDNCBAqbPr5W4GmLORUKjfNKAm4w1exA7cV631Qp+ehzg
AAAEBYVd5eVr/WVKNvr18oR7cHE3FAe9v0ttFY2vO9tSenEc0IECps+vlbgaYs5FQqN80o
CbjDV7EDtxXrfVCn56HOAAAABHRlc3QB
-----END OPENSSH PRIVATE KEY-----
";
    const ENCRYPTED_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABBL3cSlJg
vJy0a6uTp0QD/LAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIPLZ02mBooX667a0
R7Yd+8IWSk68/8i+TIBpZMqhD+zyAAAAkGsRmkPB84qAADq2yKH/eQ5yx0OWfQ3h7Cps9p
yB8CCfl+Ub2bIhIB7SGDauShbf162tkp2c4wRvgqBPd4r6dAEXsvb+3MBsCVkL6erX0ywT
rPsm5Jy1UfbG8+m0J7gky/ZrNv/D6t4V1tbNHx7uAoAbKdhFjg08MADGiNbx1C/5P+INJ3
2IdanIe8ji+RNniA==
-----END OPENSSH PRIVATE KEY-----
";

    /// Run a default-key row through the same two steps the dialog does.
    fn import_default_key(
        vm: &mut VaultManager,
        dir: &tempfile::TempDir,
        label: &str,
        key_text: &str,
    ) -> (ImportedHost, AuthMethod, usize) {
        let path = dir.path().join("id_ed25519");
        std::fs::write(&path, key_text).unwrap();
        let mut host = ImportedHost {
            label: label.to_string(),
            ..ssh_config_host(Some(path), true)
        };
        let mut keys_failed = 0;
        let pem = take_key_pem(&mut host);
        let auth = resolve_auth(vm, &host, pem.as_deref(), None, &mut keys_failed);
        (host, auth, keys_failed)
    }

    #[test]
    fn an_unencrypted_default_key_is_shared_and_named_after_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = test_vault(&dir);

        let (host, first, _) = import_default_key(&mut vm, &dir, "web", PLAIN_KEY);
        let (_, second, keys_failed) = import_default_key(&mut vm, &dir, "db", PLAIN_KEY);

        assert!(host.is_default_identity);
        let (AuthMethod::PrivateKey { key_id }, AuthMethod::PrivateKey { key_id: second_id }) =
            (first, second)
        else {
            panic!("both hosts should use the default key");
        };
        assert_eq!(key_id, second_id);
        assert_eq!(keys_failed, 0);
        let keys = vm.list_keys().unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "id_ed25519");
    }

    #[test]
    fn an_encrypted_default_key_leaves_the_host_to_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = test_vault(&dir);

        let (host, auth, keys_failed) = import_default_key(&mut vm, &dir, "web", ENCRYPTED_KEY);

        assert_eq!(host.identity_file, None);
        assert!(matches!(auth, AuthMethod::Agent));
        assert_eq!(keys_failed, 0);
        assert!(vm.list_keys().unwrap().is_empty());
    }

    #[test]
    fn a_default_key_that_does_not_decode_is_not_a_lost_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = test_vault(&dir);

        let (host, auth, keys_failed) = import_default_key(&mut vm, &dir, "web", "not a key");

        assert_eq!(host.identity_file, None);
        assert!(matches!(auth, AuthMethod::Agent));
        assert_eq!(keys_failed, 0);
    }

    #[test]
    fn a_default_key_that_cannot_be_read_leaves_the_host_to_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = ssh_config_host(Some(dir.path().join("id_ed25519")), true);
        let mut vm = test_vault(&dir);
        let mut keys_failed = 0;

        let pem = take_key_pem(&mut host);
        let auth = resolve_auth(&mut vm, &host, pem.as_deref(), None, &mut keys_failed);

        assert_eq!(host.identity_file, None);
        assert!(matches!(auth, AuthMethod::Agent));
        // Not a lost key: the config never named it.
        assert_eq!(keys_failed, 0);
    }

    #[test]
    fn a_named_identity_file_that_cannot_be_read_counts_as_a_lost_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = ssh_config_host(Some(dir.path().join("work")), false);
        let mut vm = test_vault(&dir);
        let mut keys_failed = 0;

        let pem = take_key_pem(&mut host);
        resolve_auth(&mut vm, &host, pem.as_deref(), None, &mut keys_failed);

        assert_eq!(keys_failed, 1);
        assert_eq!(imported_key_name(&host), "web (imported)");
    }

    fn snippet(name: &str, script: &str) -> ImportedSnippet {
        ImportedSnippet {
            name: name.to_string(),
            script: script.to_string(),
        }
    }

    #[test]
    fn imported_hosts_link_to_the_snippets_of_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = test_vault(&dir);

        let hosts = vec![
            (termius_host("web", Some("Tail syslog")), None, None),
            (termius_host("db", Some("Never imported")), None, None),
        ];
        let counts = write_selected(
            &mut vm,
            &hosts,
            &[snippet("Tail syslog", "tail -f /var/log/syslog")],
        );

        assert_eq!(counts.added, 2);
        assert_eq!(counts.snippets_added, 1);
        assert!(counts.save_error.is_none());

        let snippet_id = vm.find_snippet_by_name("Tail syslog").unwrap();
        let entries = vm.list_entries().unwrap();
        let web = entries.iter().find(|e| e.label == "web").unwrap();
        let db = entries.iter().find(|e| e.label == "db").unwrap();
        assert_eq!(
            vm.get_entry(web.id).unwrap().startup_snippet,
            Some(snippet_id)
        );
        // The snippet that file never carried stays unlinked, not invented.
        assert_eq!(vm.get_entry(db.id).unwrap().startup_snippet, None);
    }

    /// A re-import must reuse the snippet already stored, not stack up copies.
    #[test]
    fn a_snippet_that_is_already_stored_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let mut vm = test_vault(&dir);
        let existing = vm
            .add_snippet(
                "Deploy".into(),
                None,
                SnippetBody::Script {
                    content: "make".into(),
                },
            )
            .unwrap();

        let hosts = vec![(termius_host("web", Some("deploy")), None, None)];
        let counts = write_selected(&mut vm, &hosts, &[snippet("deploy", "make deploy")]);

        assert_eq!(counts.snippets_added, 0);
        assert_eq!(vm.list_snippets().unwrap().len(), 1);
        let web = vm.list_entries().unwrap()[0].id;
        assert_eq!(vm.get_entry(web).unwrap().startup_snippet, Some(existing));
    }

    #[test]
    fn a_jump_hop_matches_the_alias_before_the_address() {
        // Two hosts share an address; the alias is what the config named.
        let entries = vec![
            summary("other", "gate.internal", 22),
            summary("gate", "gate.internal", 22),
        ];
        let found = find_jump_entry(&entries, &target("gate", "gate.internal", 22)).unwrap();
        assert_eq!(found, entries[1].id);
    }

    /// A hop covered only by a wildcard block (`Host dev-*`) is no host's
    /// label, so it can only be matched by the address it resolved to.
    #[test]
    fn a_jump_hop_falls_back_to_the_resolved_address() {
        let entries = vec![summary("Dev gateway", "dev-gate.example.net", 2022)];
        let found =
            find_jump_entry(&entries, &target("dev-gate", "DEV-GATE.example.net", 2022)).unwrap();
        assert_eq!(found, entries[0].id);
    }

    #[test]
    fn a_jump_hop_on_another_port_is_not_a_match() {
        let entries = vec![summary("gateway", "gate.internal", 22)];
        assert!(find_jump_entry(&entries, &target("gate", "gate.internal", 2222)).is_none());
    }
}
