//! Host import from external sources: `~/.ssh/config` (OpenSSH), PuTTY's
//! registry sessions (Windows), and `termius-export` JSON files. Discovery
//! only: turning [`ImportedHost`]s into vault entries is the import dialog's
//! job.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::ssh_target::split_hop_address;

/// How deep `Include` directives may nest before we stop following them.
/// Real configs use one level; the cap only guards against include cycles.
const MAX_INCLUDE_DEPTH: u32 = 8;

/// Key files `ssh` tries on its own when a host names no `IdentityFile`, in
/// the order we prefer them.
const DEFAULT_IDENTITY_NAMES: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    SshConfig,
    /// Only produced on Windows, where PuTTY keeps its sessions in the registry.
    #[cfg_attr(not(windows), allow(dead_code))]
    Putty,
    Termius,
}

impl ImportSource {
    pub fn display_label(&self) -> &'static str {
        match self {
            ImportSource::SshConfig => "ssh config",
            ImportSource::Putty => "PuTTY",
            ImportSource::Termius => "Termius",
        }
    }
}

/// One bastion named by a `ProxyJump`, resolved against the same config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpTarget {
    /// The name as written: an alias from this config, or a literal address.
    pub name: String,
    /// What the name resolves to: the alias's `HostName` when the config
    /// gives one, otherwise the name itself.
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
}

/// A host's `ProxyJump`, as far as the vault can represent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportedJump {
    Single(JumpTarget),
    /// `ProxyJump a,b`. A vault entry holds one link, and chaining it would
    /// rewrite the bastions' own routes (`b` is only reached through `a` for
    /// *this* host). Carries the value as written so the dialog can say what
    /// it skipped instead of importing a different route.
    MultiHop(String),
    /// The hop is not `[user@]host[:port]`: a port that is not a number, an
    /// unclosed bracket. Kept as written for the same reason: dropping it
    /// would import the host as a direct connection with nothing said.
    Malformed(String),
}

#[derive(Clone)]
pub struct ImportedHost {
    /// Host alias (ssh config), session name (PuTTY), or label (Termius).
    /// Becomes the label.
    pub label: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    /// Resolved `IdentityFile` path, if the config names one.
    pub identity_file: Option<PathBuf>,
    /// `identity_file` was not named by the config: it is a default key
    /// (`~/.ssh/id_ed25519` and friends) that `ssh` would try by itself.
    pub is_default_identity: bool,
    /// Private key material carried inline (Termius export).
    pub key_pem: Option<String>,
    /// User-given name of the inline key, if any.
    pub key_name: Option<String>,
    /// Passphrase of the inline key, when the export carries one.
    pub key_passphrase: Option<String>,
    /// Stored login password (Termius export).
    pub password: Option<String>,
    /// OS hint recorded by the source app (Termius export).
    pub os: Option<String>,
    /// Folder path, outermost first (Termius export).
    pub group_path: Vec<String>,
    /// Tag names (Termius export).
    pub tags: Vec<String>,
    /// Name of the snippet the source runs on connect (Termius export).
    pub startup_snippet: Option<String>,
    /// `ProxyJump` from the ssh config; `None` = connect directly.
    pub jump: Option<ImportedJump>,
    pub source: ImportSource,
}

impl ImportedHost {
    /// A host with only the universally available fields filled in; the
    /// Termius-specific extras default to empty.
    fn new(
        label: String,
        host: String,
        port: u16,
        username: Option<String>,
        source: ImportSource,
    ) -> Self {
        Self {
            label,
            host,
            port,
            username,
            identity_file: None,
            is_default_identity: false,
            key_pem: None,
            key_name: None,
            key_passphrase: None,
            password: None,
            os: None,
            group_path: Vec::new(),
            tags: Vec::new(),
            startup_snippet: None,
            jump: None,
            source,
        }
    }
}

/// Written by hand so that a log line or a panic message never carries a
/// password, a private key or its passphrase.
impl std::fmt::Debug for ImportedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted = |secret: &Option<String>| secret.as_ref().map(|_| "<redacted>");
        f.debug_struct("ImportedHost")
            .field("label", &self.label)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("identity_file", &self.identity_file)
            .field("is_default_identity", &self.is_default_identity)
            .field("key_pem", &redacted(&self.key_pem))
            .field("key_name", &self.key_name)
            .field("key_passphrase", &redacted(&self.key_passphrase))
            .field("password", &redacted(&self.password))
            .field("os", &self.os)
            .field("group_path", &self.group_path)
            .field("tags", &self.tags)
            .field("startup_snippet", &self.startup_snippet)
            .field("jump", &self.jump)
            .field("source", &self.source)
            .finish()
    }
}

/// A saved command a source carries alongside its hosts (Termius export).
#[derive(Debug, Clone)]
pub struct ImportedSnippet {
    pub name: String,
    pub script: String,
}

/// Everything one `termius-export` file holds.
#[derive(Debug)]
pub struct TermiusImport {
    pub hosts: Vec<ImportedHost>,
    pub snippets: Vec<ImportedSnippet>,
    /// Records that were left out, one user-facing line each: what the record
    /// is and why it could not be read.
    pub skipped: Vec<String>,
}

/// Scan every supported source. Order: ssh config first, then PuTTY.
pub fn scan_all() -> Vec<ImportedHost> {
    let mut hosts = scan_ssh_config();
    hosts.extend(scan_putty_sessions());
    hosts
}

pub fn scan_ssh_config() -> Vec<ImportedHost> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let dir = home.join(".ssh");
    let Ok(text) = std::fs::read_to_string(dir.join("config")) else {
        return Vec::new();
    };
    parse_ssh_config(&text, &dir)
}

/// One `Host` block: its patterns and the options seen inside it, in order.
#[derive(Clone)]
struct Block {
    patterns: Vec<String>,
    options: Vec<(String, String)>,
}

/// Parse ssh config text into importable hosts. Every concrete (non-wildcard)
/// `Host` alias becomes a candidate; its options are resolved with OpenSSH's
/// "first obtained value wins" rule across all blocks whose patterns match the
/// alias, so `Host *` defaults and `Host dev-*` group settings apply.
///
/// A host that names no `IdentityFile` gets the first default key found in
/// `base_dir`, because that is what `ssh` would offer for it.
fn parse_ssh_config(text: &str, base_dir: &Path) -> Vec<ImportedHost> {
    // Options above the first `Host` line apply to every host. This block has
    // to stay first: the first value obtained wins, and those come first.
    let mut blocks = vec![Block {
        patterns: vec!["*".to_string()],
        options: Vec::new(),
    }];
    collect_blocks(text, base_dir, 0, &mut blocks);
    let default_identity = find_default_identity(base_dir);

    let mut seen = HashSet::new();
    let mut hosts = Vec::new();
    for block in &blocks {
        for alias in &block.patterns {
            if alias.contains('*') || alias.contains('?') || alias.starts_with('!') {
                continue;
            }
            if !seen.insert(alias.clone()) {
                continue; // first definition wins, like OpenSSH
            }

            let options = resolve_options(&blocks, alias);
            let jump = options
                .proxy_jump
                .as_deref()
                .and_then(|value| parse_proxy_jump(value, &blocks));

            let host = resolved_hostname(&options, alias);
            let is_default_identity = options.identity.is_none() && default_identity.is_some();
            let identity_file = match options.identity {
                Some(path) => Some(expand_path(&path)),
                None => default_identity.clone(),
            };
            hosts.push(ImportedHost {
                identity_file,
                is_default_identity,
                jump,
                ..ImportedHost::new(
                    alias.clone(),
                    host,
                    options.port.unwrap_or(22),
                    options.user,
                    ImportSource::SshConfig,
                )
            });
        }
    }
    hosts
}

/// The first default key file that exists in `ssh_dir`.
fn find_default_identity(ssh_dir: &Path) -> Option<PathBuf> {
    DEFAULT_IDENTITY_NAMES
        .iter()
        .map(|name| ssh_dir.join(name))
        .find(|path| path.is_file())
}

/// The options that survive for one alias, applying OpenSSH's "first obtained
/// value wins" rule across every block whose patterns match it.
struct HostOptions {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity: Option<String>,
    proxy_jump: Option<String>,
}

fn resolve_options(blocks: &[Block], alias: &str) -> HostOptions {
    let mut out = HostOptions {
        hostname: None,
        user: None,
        port: None,
        identity: None,
        proxy_jump: None,
    };
    for block in blocks {
        if !block_matches(block, alias) {
            continue;
        }
        for (key, value) in &block.options {
            match key.as_str() {
                "hostname" if out.hostname.is_none() => out.hostname = Some(value.clone()),
                "user" if out.user.is_none() => out.user = Some(value.clone()),
                "port" if out.port.is_none() => out.port = value.parse::<u16>().ok(),
                "identityfile" if out.identity.is_none() => out.identity = Some(value.clone()),
                "proxyjump" if out.proxy_jump.is_none() => out.proxy_jump = Some(value.clone()),
                _ => {}
            }
        }
    }
    out
}

/// `%h` in HostName expands to the alias as typed.
fn resolved_hostname(options: &HostOptions, alias: &str) -> String {
    match &options.hostname {
        Some(hostname) => hostname.replace("%h", alias),
        None => alias.to_string(),
    }
}

/// Read a `ProxyJump` value. The named hop is resolved through the same
/// config, so an alias covered only by a wildcard block (`Host dev-*`) still
/// yields its real address, and a hop defined further down the file resolves
/// too, because every block is collected before any of this runs. Only an
/// explicit `none` returns `None`: anything else the vault cannot represent is
/// kept so the import dialog can report it.
fn parse_proxy_jump(value: &str, blocks: &[Block]) -> Option<ImportedJump> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") {
        return None;
    }
    let hops: Vec<&str> = value
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();
    if hops.len() > 1 {
        return Some(ImportedJump::MultiHop(value.to_string()));
    }

    let Some((user, name, port)) = hops.first().and_then(|hop| split_hop_address(hop)) else {
        return Some(ImportedJump::Malformed(value.to_string()));
    };
    let options = resolve_options(blocks, &name);
    Some(ImportedJump::Single(JumpTarget {
        host: resolved_hostname(&options, &name),
        port: port.or(options.port).unwrap_or(22),
        username: user.or(options.user),
        name,
    }))
}

/// Walk config lines, following `Include` directives, appending `Host` blocks.
fn collect_blocks(text: &str, base_dir: &Path, depth: u32, blocks: &mut Vec<Block>) {
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = split_directive(line) else {
            continue;
        };

        match key.as_str() {
            "host" => {
                blocks.push(Block {
                    patterns: value.split_whitespace().map(unquote).collect(),
                    options: Vec::new(),
                });
            }
            // `Match` conditions can't be evaluated offline; an empty-pattern
            // block soaks up its options so they don't leak into neighbors.
            "match" => {
                blocks.push(Block {
                    patterns: Vec::new(),
                    options: Vec::new(),
                });
            }
            "include" if depth < MAX_INCLUDE_DEPTH => {
                let patterns_before = blocks.last().map(|block| block.patterns.clone());
                let count_before = blocks.len();
                for part in value.split_whitespace() {
                    for path in resolve_include(&unquote(part), base_dir) {
                        if let Ok(included) = std::fs::read_to_string(&path) {
                            collect_blocks(&included, base_dir, depth + 1, blocks);
                        }
                    }
                }
                // A `Host` line inside the included file ends with that file.
                // Reopen the block the `Include` sat in, so the options after
                // it do not land in the included file's last block.
                if let Some(patterns) = patterns_before.filter(|_| blocks.len() > count_before) {
                    blocks.push(Block {
                        patterns,
                        options: Vec::new(),
                    });
                }
            }
            _ => {
                // Never empty in practice: `parse_ssh_config` seeds the list
                // with the global block.
                if let Some(block) = blocks.last_mut() {
                    block.options.push((key, unquote(&value)));
                }
            }
        }
    }
}

/// Split `Key Value`, `Key=Value`, or `Key = Value` into a lowercase key and
/// the raw value.
fn split_directive(line: &str) -> Option<(String, String)> {
    let pos = line.find(|c: char| c == '=' || c.is_whitespace())?;
    let key = line[..pos].to_ascii_lowercase();
    let value = line[pos..].trim_start_matches(|c: char| c == '=' || c.is_whitespace());
    if key.is_empty() || value.is_empty() {
        return None;
    }
    Some((key, value.trim().to_string()))
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').to_string()
}

/// Does this block apply to `alias`? Any negated pattern that matches vetoes
/// the block; otherwise any positive match accepts it.
fn block_matches(block: &Block, alias: &str) -> bool {
    let mut positive = false;
    for pattern in &block.patterns {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob_match(negated, alias) {
                return false;
            }
        } else if glob_match(pattern, alias) {
            positive = true;
        }
    }
    positive
}

/// Minimal glob: `*` = any run (including empty), `?` = any single char.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Classic two-pointer with backtracking on the last `*`.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut star_ti) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Resolve an `Include` operand: `~` and `%d` expand to home, bare relative
/// paths are relative to `~/.ssh`, and a `*` in the filename globs within its
/// directory.
fn resolve_include(operand: &str, base_dir: &Path) -> Vec<PathBuf> {
    let expanded = expand_path(operand);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    };

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !name.contains('*') && !name.contains('?') {
        return vec![path];
    }
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut matches: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| glob_match(&name, &n.to_string_lossy()))
                .unwrap_or(false)
        })
        .collect();
    matches.sort();
    matches
}

/// Expand a leading `~/` or `%d/` to the home directory. `~alice/key` is
/// another user's home, which we cannot resolve, so it is left as written.
fn expand_path(p: &str) -> PathBuf {
    let p = p.trim();
    for prefix in ["~", "%d"] {
        let Some(rest) = p.strip_prefix(prefix) else {
            continue;
        };
        if !rest.is_empty() && !rest.starts_with(['/', '\\']) {
            continue;
        }
        if let Some(home) = dirs::home_dir() {
            return home.join(rest.trim_start_matches(['/', '\\']));
        }
    }
    PathBuf::from(p)
}

/// The `termius-export` format, version 1. It is flint's own format:
/// Termius has no export, so no file made by Termius is read here. A file
/// looks like this:
///
/// ```json
/// {
///   "format": "termius-export",
///   "version": 1,
///   "groups":   [{"id": "g1", "name": "infra", "parent_id": null}],
///   "tags":     [{"id": "t1", "name": "prod"}],
///   "keys":     [{"id": "k1", "name": "id_ed25519", "pem": "-----BEGIN ...", "passphrase": null}],
///   "snippets": [{"id": "s1", "name": "tail syslog", "script": "tail -f /var/log/syslog"}],
///   "hosts": [{
///     "label": "web-1", "host": "203.0.113.10", "port": 22, "username": "deploy",
///     "os": "ubuntu", "group": "g1", "tags": ["t1"], "startup_snippet": "s1",
///     "auth": {"method": "key", "key_id": "k1", "password": null}
///   }]
/// }
/// ```
///
/// `format` and `version` are required. Every list may be missing or `null`. Hosts point
/// at groups, tags, keys and snippets by `id`; a group points at its parent
/// folder with `parent_id`. `auth.method` is `"key"`, `"password"` or
/// `"none"`, and a missing `auth` means `"none"`. In a record only `id` (and
/// `pem` for a key, `host` for a host) is required. Unknown fields are ignored.
///
/// The lists are kept as raw values so each record is read on its own: one
/// broken record is skipped and the rest of the file still imports.
#[derive(Deserialize)]
struct TermiusExport {
    format: String,
    version: u32,
    #[serde(default, deserialize_with = "termius_list")]
    groups: Vec<serde_json::Value>,
    #[serde(default, deserialize_with = "termius_list")]
    tags: Vec<serde_json::Value>,
    #[serde(default, deserialize_with = "termius_list")]
    keys: Vec<serde_json::Value>,
    #[serde(default, deserialize_with = "termius_list")]
    snippets: Vec<serde_json::Value>,
    #[serde(default, deserialize_with = "termius_list")]
    hosts: Vec<serde_json::Value>,
}

/// A list that is `null` in the file reads as an empty one, like a missing list.
fn termius_list<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<serde_json::Value>, D::Error> {
    Ok(Option::<Vec<serde_json::Value>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct TermiusGroup {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    parent_id: Option<String>,
}

#[derive(Deserialize)]
struct TermiusTag {
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct TermiusKey {
    id: String,
    #[serde(default)]
    name: String,
    pem: String,
    #[serde(default)]
    passphrase: Option<String>,
}

#[derive(Deserialize)]
struct TermiusSnippet {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    script: String,
}

#[derive(Deserialize)]
struct TermiusHost {
    #[serde(default)]
    label: String,
    host: String,
    #[serde(default = "default_ssh_port")]
    port: u16,
    #[serde(default)]
    username: String,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    startup_snippet: Option<String>,
    /// Missing or `null` means no stored credentials.
    #[serde(default)]
    auth: Option<TermiusAuth>,
}

fn default_ssh_port() -> u16 {
    22
}

#[derive(Deserialize)]
struct TermiusAuth {
    #[serde(default)]
    method: String, // "key" | "password" | "none"
    #[serde(default)]
    key_id: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

/// Parse a `termius-export` JSON file into importable hosts and snippets. The
/// returned error string is user-facing (shown in the import dialog).
///
/// Only a file that is not a `termius-export` at all is an error. A record
/// that cannot be read is left out and named in [`TermiusImport::skipped`].
pub fn parse_termius_export(text: &str) -> Result<TermiusImport, String> {
    // Windows tools often write a byte order mark, which serde_json rejects.
    let text = text.trim_start_matches('\u{feff}');
    let export: TermiusExport = serde_json::from_str(text).map_err(termius_file_error)?;
    if export.format != "termius-export" {
        return Err(format!(
            "unexpected format \"{}\", expected a termius-export file",
            export.format
        ));
    }
    if export.version != 1 {
        return Err(format!(
            "unsupported termius-export version {}",
            export.version
        ));
    }

    let mut skipped = Vec::new();
    let group_records: Vec<TermiusGroup> =
        termius_records("group", export.groups, "", &mut skipped);
    let tag_records: Vec<TermiusTag> = termius_records("tag", export.tags, "", &mut skipped);
    let key_records: Vec<TermiusKey> = termius_records(
        "key",
        export.keys,
        "; hosts that use it are imported without a key",
        &mut skipped,
    );
    let snippet_records: Vec<TermiusSnippet> =
        termius_records("snippet", export.snippets, "", &mut skipped);
    let host_records: Vec<TermiusHost> = termius_records("host", export.hosts, "", &mut skipped);

    // Hosts point at snippets by id, but a host carries the name, as it does
    // for groups and tags, so the dialog resolves everything against the vault
    // by name.
    let snippet_names: HashMap<&str, &str> = snippet_records
        .iter()
        .filter(|s| !s.name.trim().is_empty() && !s.script.trim().is_empty())
        .map(|s| (s.id.as_str(), s.name.trim()))
        .collect();
    for record in &snippet_records {
        if !snippet_names.contains_key(record.id.as_str()) {
            let name = Some(record.name.trim())
                .filter(|n| !n.is_empty())
                .unwrap_or(&record.id);
            skipped.push(format!("snippet \"{name}\" (no name or no script)"));
        }
    }
    let snippets: Vec<ImportedSnippet> = snippet_records
        .iter()
        .filter(|s| snippet_names.contains_key(s.id.as_str()))
        .map(|s| ImportedSnippet {
            name: s.name.trim().to_string(),
            script: s.script.clone(),
        })
        .collect();

    let lookup = TermiusLookup {
        groups: group_records.iter().map(|g| (g.id.as_str(), g)).collect(),
        tags: tag_records
            .iter()
            .map(|t| (t.id.as_str(), t.name.as_str()))
            .collect(),
        keys: key_records.iter().map(|k| (k.id.as_str(), k)).collect(),
        snippet_names,
    };

    let mut hosts = Vec::new();
    for record in &host_records {
        if record.host.trim().is_empty() {
            let label = record.label.trim();
            skipped.push(if label.is_empty() {
                "host without a label (no address)".to_string()
            } else {
                format!("host \"{label}\" (no address)")
            });
            continue;
        }
        hosts.push(termius_host(record, &lookup));
    }
    Ok(TermiusImport {
        hosts,
        snippets,
        skipped,
    })
}

/// Why the file as a whole was refused: broken JSON, or JSON of another shape.
fn termius_file_error(error: serde_json::Error) -> String {
    use serde_json::error::Category;
    match error.classify() {
        Category::Syntax | Category::Eof | Category::Io => format!("not valid JSON ({error})"),
        Category::Data => format!("not a termius-export file ({error})"),
    }
}

/// Read each record of one list on its own. A record that does not fit is
/// named in `skipped` (with `note` added to the reason) and left out.
fn termius_records<T: serde::de::DeserializeOwned>(
    kind: &str,
    records: Vec<serde_json::Value>,
    note: &str,
    skipped: &mut Vec<String>,
) -> Vec<T> {
    let mut parsed = Vec::new();
    for (index, record) in records.into_iter().enumerate() {
        let name = termius_record_name(&record, index);
        match serde_json::from_value::<T>(record) {
            Ok(value) => parsed.push(value),
            Err(error) => skipped.push(format!("{kind} {name} ({error}{note})")),
        }
    }
    parsed
}

/// How to call a record that failed to parse: the first of its label, name,
/// address or id that is there, in quotes, or its position in the list.
fn termius_record_name(record: &serde_json::Value, index: usize) -> String {
    ["label", "name", "host", "id"]
        .iter()
        .filter_map(|field| record.get(field)?.as_str())
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(|text| format!("\"{text}\""))
        .unwrap_or_else(|| format!("#{}", index + 1))
}

/// The tables, keyed by id, that a host is resolved against.
struct TermiusLookup<'a> {
    groups: HashMap<&'a str, &'a TermiusGroup>,
    tags: HashMap<&'a str, &'a str>,
    keys: HashMap<&'a str, &'a TermiusKey>,
    snippet_names: HashMap<&'a str, &'a str>,
}

fn termius_host(record: &TermiusHost, lookup: &TermiusLookup) -> ImportedHost {
    // `method` is the recommended auth; password is reported independently
    // and kept as a fallback either way.
    let auth = record.auth.as_ref();
    let key = auth
        .filter(|auth| auth.method == "key")
        .and_then(|auth| auth.key_id.as_deref())
        .and_then(|id| lookup.keys.get(id));
    let password = auth
        .and_then(|auth| auth.password.clone())
        .filter(|p| !p.is_empty());

    let group_path = record
        .group
        .as_deref()
        .map(|id| termius_group_path(id, &lookup.groups))
        .unwrap_or_default();
    let tags: Vec<String> = record
        .tags
        .iter()
        .filter_map(|id| lookup.tags.get(id.as_str()))
        .filter(|name| !name.trim().is_empty())
        .map(|name| name.trim().to_string())
        .collect();
    let startup_snippet = record
        .startup_snippet
        .as_deref()
        .and_then(|id| lookup.snippet_names.get(id))
        .map(|name| name.to_string());

    ImportedHost {
        key_pem: key.map(|k| k.pem.clone()),
        key_name: key.map(|k| k.name.clone()).filter(|n| !n.trim().is_empty()),
        key_passphrase: key.and_then(|k| k.passphrase.clone()),
        password,
        os: record.os.clone().filter(|os| !os.trim().is_empty()),
        group_path,
        tags,
        startup_snippet,
        ..ImportedHost::new(
            record.label.trim().to_string(),
            record.host.trim().to_string(),
            record.port,
            Some(record.username.trim().to_string()).filter(|u| !u.is_empty()),
            ImportSource::Termius,
        )
    }
}

/// Folder path for a group id, outermost first. Skips blank names; stops on
/// dangling references or cycles.
fn termius_group_path(id: &str, groups: &HashMap<&str, &TermiusGroup>) -> Vec<String> {
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = Some(id);
    while let Some(gid) = cursor {
        // A group met twice is a cycle in the file.
        if !seen.insert(gid) {
            break;
        }
        let Some(group) = groups.get(gid) else { break };
        if !group.name.trim().is_empty() {
            path.push(group.name.trim().to_string());
        }
        cursor = group.parent_id.as_deref();
    }
    path.reverse();
    path
}

#[cfg(windows)]
pub fn scan_putty_sessions() -> Vec<ImportedHost> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(sessions) = hkcu.open_subkey("Software\\SimonTatham\\PuTTY\\Sessions") else {
        return Vec::new();
    };

    let mut hosts = Vec::new();
    for name in sessions.enum_keys().flatten() {
        let Ok(session) = sessions.open_subkey(&name) else {
            continue;
        };

        let protocol: String = session.get_value("Protocol").unwrap_or_default();
        if !protocol.is_empty() && protocol != "ssh" {
            continue;
        }
        let mut host: String = session.get_value("HostName").unwrap_or_default();
        if host.trim().is_empty() {
            continue; // "Default Settings" and other host-less sessions
        }
        let mut username: String = session.get_value("UserName").unwrap_or_default();
        // PuTTY accepts user@host in the HostName box.
        if let Some((u, h)) = host.split_once('@') {
            if username.is_empty() {
                username = u.to_string();
            }
            host = h.to_string();
        }
        let port: u32 = session.get_value("PortNumber").unwrap_or(22);

        // PuTTY keys are .ppk, which cannot be imported as PEM, so no identity.
        hosts.push(ImportedHost::new(
            percent_decode(&name),
            host.trim().to_string(),
            u16::try_from(port).unwrap_or(22),
            (!username.trim().is_empty()).then(|| username.trim().to_string()),
            ImportSource::Putty,
        ));
    }
    hosts
}

#[cfg(not(windows))]
pub fn scan_putty_sessions() -> Vec<ImportedHost> {
    Vec::new()
}

/// PuTTY percent-encodes session names in the registry ("my%20server").
#[cfg(any(windows, test))]
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Taken from the bytes: a `&str` slice panics when the two bytes
        // after `%` end inside a multi-byte character.
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok());
        let escaped = hex.filter(|_| bytes[i] == b'%');
        if let Some(byte) = escaped.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Vec<ImportedHost> {
        parse_ssh_config(text, Path::new("."))
    }

    /// The single jump target of the first parsed host.
    fn jump_of(text: &str) -> JumpTarget {
        match parse(text).remove(0).jump {
            Some(ImportedJump::Single(target)) => target,
            other => panic!("expected a single jump host, got {other:?}"),
        }
    }

    #[test]
    fn proxy_jump_user_host_port() {
        let target = jump_of("Host web\n  ProxyJump ops@gate.example.com:2222\n");
        assert_eq!(target.name, "gate.example.com");
        assert_eq!(target.host, "gate.example.com");
        assert_eq!(target.port, 2222);
        assert_eq!(target.username.as_deref(), Some("ops"));
    }

    #[test]
    fn proxy_jump_bare_host_defaults_to_port_22() {
        let target = jump_of("Host web\n  ProxyJump gate.example.com\n");
        assert_eq!(target.host, "gate.example.com");
        assert_eq!(target.port, 22);
        assert_eq!(target.username, None);
    }

    #[test]
    fn proxy_jump_bracketed_ipv6_with_port() {
        let target = jump_of("Host web\n  ProxyJump ops@[2001:db8::1]:2222\n");
        assert_eq!(target.host, "2001:db8::1");
        assert_eq!(target.port, 2222);
        assert_eq!(target.username.as_deref(), Some("ops"));
    }

    /// The trap: the closing bracket ends the address, so there is no port to
    /// split off. Splitting on the last colon would leave "2001:db8:".
    #[test]
    fn proxy_jump_bracketed_ipv6_without_port() {
        let target = jump_of("Host web\n  ProxyJump [2001:db8::1]\n");
        assert_eq!(target.host, "2001:db8::1");
        assert_eq!(target.port, 22);
    }

    #[test]
    fn proxy_jump_unbracketed_ipv6_has_no_port() {
        let target = jump_of("Host web\n  ProxyJump 2001:db8::1\n");
        assert_eq!(target.host, "2001:db8::1");
        assert_eq!(target.port, 22);
    }

    /// No `Host` line names the hop literally. Only a wildcard block covers
    /// it, so its address comes from that block.
    #[test]
    fn proxy_jump_alias_resolved_through_a_wildcard_block() {
        let target = jump_of(
            "Host web\n  ProxyJump dev-gate\n\nHost dev-*\n  HostName %h.example.net\n  \
             User admin\n  Port 2022\n",
        );
        assert_eq!(target.name, "dev-gate");
        assert_eq!(target.host, "dev-gate.example.net");
        assert_eq!(target.port, 2022);
        assert_eq!(target.username.as_deref(), Some("admin"));
    }

    /// The hop is defined after the host that references it.
    #[test]
    fn proxy_jump_forward_reference() {
        let target = jump_of(
            "Host web\n  ProxyJump gate\n\nHost gate\n  HostName gate.internal\n  Port 2200\n",
        );
        assert_eq!(target.host, "gate.internal");
        assert_eq!(target.port, 2200);
    }

    /// An explicit port on the ProxyJump beats the alias's own `Port`.
    #[test]
    fn proxy_jump_explicit_port_beats_the_alias_port() {
        let target = jump_of(
            "Host web\n  ProxyJump gate:2020\n\nHost gate\n  HostName gate.internal\n  \
             Port 2200\n",
        );
        assert_eq!(target.port, 2020);
    }

    #[test]
    fn proxy_jump_multi_hop_is_reported_not_guessed() {
        let host = parse("Host web\n  ProxyJump first,second\n").remove(0);
        assert_eq!(
            host.jump,
            Some(ImportedJump::MultiHop("first,second".to_string()))
        );
    }

    /// A hop we cannot split is not the same as "no ProxyJump": dropping it
    /// would import the host as a direct connection and say nothing.
    #[test]
    fn proxy_jump_that_cannot_be_split_is_reported_not_dropped() {
        for value in ["gate:", "gate:70000", "gate:22x", "[2001:db8::1", "user@"] {
            let host = parse(&format!("Host web\n  ProxyJump {value}\n")).remove(0);
            assert_eq!(
                host.jump,
                Some(ImportedJump::Malformed(value.to_string())),
                "for {value:?}",
            );
        }
    }

    #[test]
    fn proxy_jump_none_means_direct() {
        assert_eq!(parse("Host web\n  ProxyJump none\n").remove(0).jump, None);
        assert_eq!(
            parse("Host web\n  HostName web.internal\n").remove(0).jump,
            None
        );
    }

    /// A `Host *` block supplies ProxyJump for every host that has none.
    #[test]
    fn proxy_jump_inherited_from_a_wildcard_block() {
        let target = jump_of("Host web\n\nHost *\n  ProxyJump gate.example.com\n");
        assert_eq!(target.host, "gate.example.com");
    }

    #[test]
    fn basic_host_block() {
        let hosts = parse("Host web\n  HostName web.example.com\n  User deploy\n  Port 2222\n");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].label, "web");
        assert_eq!(hosts[0].host, "web.example.com");
        assert_eq!(hosts[0].username.as_deref(), Some("deploy"));
        assert_eq!(hosts[0].port, 2222);
    }

    #[test]
    fn wildcard_blocks_supply_defaults_but_are_not_imported() {
        let hosts = parse("Host *\n  User shared\n\nHost db\n  HostName db.internal\n");
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].label, "db");
        assert_eq!(hosts[0].username.as_deref(), Some("shared"));
    }

    #[test]
    fn first_obtained_value_wins() {
        let hosts = parse("Host app\n  User app-user\n\nHost *\n  User fallback\n  Port 2200\n");
        assert_eq!(hosts[0].username.as_deref(), Some("app-user"));
        assert_eq!(hosts[0].port, 2200);
    }

    #[test]
    fn alias_without_hostname_uses_alias_and_percent_h_expands() {
        let hosts = parse("Host bare\n\nHost jump\n  HostName %h.example.net\n");
        assert_eq!(hosts[0].host, "bare");
        assert_eq!(hosts[1].host, "jump.example.net");
    }

    #[test]
    fn equals_syntax_multiple_patterns_and_negation() {
        let hosts = parse(
            "Host a b !a-prod\nHostName=multi.example.com\n\nHost dev-* !dev-secret\n  User dev\n\nHost dev-box\n",
        );
        let a = hosts.iter().find(|h| h.label == "a").unwrap();
        assert_eq!(a.host, "multi.example.com");
        assert!(hosts.iter().any(|h| h.label == "b"));
        let devbox = hosts.iter().find(|h| h.label == "dev-box").unwrap();
        assert_eq!(devbox.username.as_deref(), Some("dev"));
    }

    #[test]
    fn match_blocks_do_not_leak_options() {
        let hosts =
            parse("Match user root\n  Port 666\n\nHost clean\n  HostName clean.example.com\n");
        assert_eq!(hosts[0].port, 22);
    }

    #[test]
    fn options_above_the_first_host_line_apply_to_every_host() {
        let hosts = parse(
            "User shared\nPort 2200\n\nHost web\n  HostName web.example.com\n\nHost db\n  User dba\n",
        );
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].username.as_deref(), Some("shared"));
        assert_eq!(hosts[0].port, 2200);
        // A global value is obtained first, so it beats the host's own block,
        // the same as in OpenSSH.
        assert_eq!(hosts[1].username.as_deref(), Some("shared"));
        assert_eq!(hosts[1].port, 2200);
    }

    #[test]
    fn a_global_identity_file_is_not_a_default_key() {
        let hosts = parse("IdentityFile /keys/work\n\nHost web\n");
        assert_eq!(
            hosts[0].identity_file.as_deref(),
            Some(Path::new("/keys/work"))
        );
        assert!(!hosts[0].is_default_identity);
    }

    #[test]
    fn options_after_a_match_block_stay_out_of_the_global_block() {
        let hosts = parse("User shared\n\nMatch user root\n  Port 666\n\nHost clean\n");
        assert_eq!(hosts[0].username.as_deref(), Some("shared"));
        assert_eq!(hosts[0].port, 22);
    }

    #[test]
    fn options_after_an_include_stay_in_the_block_that_included_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("extra"), "Host inner\n  Port 2222\n").unwrap();
        let hosts = parse_ssh_config("Include extra\nUser shared\n\nHost outer\n", dir.path());

        let inner = hosts.iter().find(|h| h.label == "inner").unwrap();
        assert_eq!(inner.port, 2222);
        assert_eq!(inner.username.as_deref(), Some("shared"));
        let outer = hosts.iter().find(|h| h.label == "outer").unwrap();
        assert_eq!(outer.username.as_deref(), Some("shared"));
        assert_eq!(outer.port, 22);
    }

    #[test]
    fn a_host_without_identity_file_gets_the_default_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("id_rsa"), "rsa").unwrap();
        std::fs::write(dir.path().join("id_ed25519"), "ed25519").unwrap();
        let hosts = parse_ssh_config(
            "Host plain\n\nHost keyed\n  IdentityFile /keys/work\n",
            dir.path(),
        );

        assert_eq!(hosts[0].identity_file, Some(dir.path().join("id_ed25519")));
        assert!(hosts[0].is_default_identity);
        assert_eq!(
            hosts[1].identity_file.as_deref(),
            Some(Path::new("/keys/work"))
        );
        assert!(!hosts[1].is_default_identity);
    }

    #[test]
    fn no_default_key_leaves_the_identity_empty() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = parse_ssh_config("Host plain\n", dir.path());
        assert_eq!(hosts[0].identity_file, None);
        assert!(!hosts[0].is_default_identity);
    }

    #[test]
    fn glob_matching() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("dev-*", "dev-box"));
        assert!(!glob_match("dev-*", "prod-box"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("*.example.com", "web.example.com"));
    }

    #[test]
    fn termius_export_parses_hosts_keys_groups_tags_snippets() {
        let json = r#"{
            "format": "termius-export",
            "version": 1,
            "groups": [
                {"id": "g1", "name": "infra", "parent_id": null},
                {"id": "g2", "name": "web", "parent_id": "g1"}
            ],
            "tags": [
                {"id": "t1", "name": "vpn"},
                {"id": "t2", "name": " prod "}
            ],
            "keys": [
                {"id": "k1", "name": "id_ed25519", "pem": "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----", "passphrase": null}
            ],
            "snippets": [
                {"id": "s1", "name": " tail syslog ", "script": "tail -f /var/log/syslog"},
                {"id": "s2", "name": "blank", "script": "   "}
            ],
            "hosts": [
                {
                    "label": "web-1", "host": "203.0.113.10", "port": 2222,
                    "username": "deploy", "os": "ubuntu", "group": "g2",
                    "tags": ["t1", "t2"], "startup_snippet": "s1",
                    "auth": {"method": "key", "key_id": "k1", "password": "fallback-pw"}
                },
                {
                    "label": "", "host": "203.0.113.11",
                    "username": "", "os": null, "group": null, "tags": [],
                    "startup_snippet": "s2",
                    "auth": {"method": "password", "key_id": null, "password": "secret"}
                },
                {
                    "label": "bare", "host": "203.0.113.12", "port": 22,
                    "username": "root",
                    "auth": {"method": "none"}
                }
            ]
        }"#;

        let import = parse_termius_export(json).unwrap();
        let hosts = &import.hosts;
        assert_eq!(hosts.len(), 3);

        // The script-less snippet is dropped, so nothing links to it either.
        assert_eq!(import.snippets.len(), 1);
        assert_eq!(import.snippets[0].name, "tail syslog");
        assert_eq!(import.snippets[0].script, "tail -f /var/log/syslog");

        let web1 = &hosts[0];
        assert_eq!(web1.label, "web-1");
        assert_eq!(web1.port, 2222);
        assert_eq!(web1.username.as_deref(), Some("deploy"));
        assert_eq!(web1.os.as_deref(), Some("ubuntu"));
        assert!(web1.key_pem.as_deref().unwrap().contains("PRIVATE KEY"));
        assert_eq!(web1.key_name.as_deref(), Some("id_ed25519"));
        // Password is kept as a fallback even when a key is attached.
        assert_eq!(web1.password.as_deref(), Some("fallback-pw"));
        assert_eq!(
            web1.group_path,
            vec!["infra".to_string(), "web".to_string()]
        );
        assert_eq!(web1.tags, vec!["vpn".to_string(), "prod".to_string()]);
        assert_eq!(web1.startup_snippet.as_deref(), Some("tail syslog"));
        assert_eq!(web1.source, ImportSource::Termius);

        let pw_host = &hosts[1];
        assert_eq!(pw_host.label, "");
        assert_eq!(pw_host.port, 22); // default when unset
        assert_eq!(pw_host.username, None);
        assert!(pw_host.key_pem.is_none());
        assert_eq!(pw_host.password.as_deref(), Some("secret"));
        assert!(pw_host.group_path.is_empty());
        assert_eq!(pw_host.startup_snippet, None);

        let bare = &hosts[2];
        assert!(bare.key_pem.is_none());
        assert!(bare.password.is_none());

        assert_eq!(
            import.skipped,
            vec!["snippet \"blank\" (no name or no script)".to_string()]
        );
    }

    #[test]
    fn termius_export_rejects_other_formats() {
        assert!(
            parse_termius_export(r#"{"format": "something-else", "version": 1}"#)
                .unwrap_err()
                .contains("something-else")
        );
    }

    #[test]
    fn termius_export_rejects_other_versions() {
        let error =
            parse_termius_export(r#"{"format": "termius-export", "version": 2}"#).unwrap_err();
        assert_eq!(error, "unsupported termius-export version 2");
    }

    #[test]
    fn a_group_cycle_ends_the_folder_path() {
        let json = r#"{
            "format": "termius-export",
            "version": 1,
            "groups": [
                {"id": "g1", "name": "", "parent_id": "g1"},
                {"id": "g2", "name": "a", "parent_id": "g3"},
                {"id": "g3", "name": "b", "parent_id": "g2"}
            ],
            "hosts": [
                {"label": "nameless", "host": "203.0.113.1", "group": "g1"},
                {"label": "named", "host": "203.0.113.2", "group": "g2"}
            ]
        }"#;

        let import = parse_termius_export(json).unwrap();
        assert!(import.hosts[0].group_path.is_empty());
        assert_eq!(
            import.hosts[1].group_path,
            vec!["b".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn debug_output_hides_passwords_and_keys() {
        let host = ImportedHost {
            key_pem: Some("PEM-BODY".to_string()),
            key_passphrase: Some("key-pass".to_string()),
            password: Some("login-pass".to_string()),
            ..ImportedHost::new(
                "web".into(),
                "203.0.113.1".into(),
                22,
                None,
                ImportSource::Termius,
            )
        };

        let text = format!("{host:?}");
        assert!(
            text.contains("web") && text.contains("<redacted>"),
            "{text}"
        );
        for secret in ["PEM-BODY", "key-pass", "login-pass"] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[test]
    fn only_a_bare_tilde_expands_to_the_home_directory() {
        assert_eq!(
            expand_path("~alice/.ssh/id_rsa"),
            PathBuf::from("~alice/.ssh/id_rsa")
        );
        assert_eq!(expand_path("~backup"), PathBuf::from("~backup"));
        assert_eq!(expand_path("%dir/key"), PathBuf::from("%dir/key"));
        assert_eq!(expand_path("/keys/work"), PathBuf::from("/keys/work"));

        let Some(home) = dirs::home_dir() else { return };
        assert_eq!(expand_path("~/.ssh/id_rsa"), home.join(".ssh/id_rsa"));
        assert_eq!(expand_path("%d/.ssh/id_rsa"), home.join(".ssh/id_rsa"));
        assert_eq!(expand_path("~"), home);
    }

    #[test]
    fn termius_export_tells_broken_json_from_a_wrong_shape() {
        for broken in ["not json", r#"{"format": "termius-export", "#, ""] {
            let error = parse_termius_export(broken).unwrap_err();
            assert!(error.starts_with("not valid JSON"), "{error}");
        }

        // Valid JSON, but not this format: never called "not valid JSON".
        for other in [
            r#"{"hosts": []}"#,
            r#"[1, 2, 3]"#,
            r#"{"format": "termius-export", "version": 1, "hosts": {"a": 1}}"#,
        ] {
            let error = parse_termius_export(other).unwrap_err();
            assert!(error.starts_with("not a termius-export file"), "{error}");
        }
    }

    #[test]
    fn termius_export_skips_broken_records_and_keeps_the_rest() {
        let json = r#"{
            "format": "termius-export",
            "version": 1,
            "groups": [{"name": "no id"}, {"id": "g1", "name": "infra"}],
            "keys": [
                {"id": "k1", "name": "good", "pem": "PEM"},
                {"id": "k2", "name": "lost"}
            ],
            "snippets": [{"id": "s1", "name": "ok", "script": "ls"}, "junk"],
            "hosts": [
                {"label": "good", "host": "203.0.113.1", "group": "g1",
                 "auth": {"method": "key", "key_id": "k1"}},
                {"label": "no-address", "auth": {"method": "none"}},
                {"label": "bad-port", "host": "203.0.113.2", "port": "ssh"},
                {"label": "blank", "host": "  "},
                {"label": "lost-key", "host": "203.0.113.3",
                 "auth": {"method": "key", "key_id": "k2"}},
                42
            ]
        }"#;

        let import = parse_termius_export(json).unwrap();

        let labels: Vec<&str> = import.hosts.iter().map(|h| h.label.as_str()).collect();
        assert_eq!(labels, vec!["good", "lost-key"]);
        assert_eq!(import.hosts[0].key_pem.as_deref(), Some("PEM"));
        assert_eq!(import.hosts[0].group_path, vec!["infra".to_string()]);
        // Its key record was skipped, so the host comes in without a key.
        assert!(import.hosts[1].key_pem.is_none());
        assert_eq!(import.snippets.len(), 1);

        let skipped = &import.skipped;
        assert_eq!(skipped.len(), 7, "{skipped:?}");
        assert!(
            skipped[0].starts_with("group \"no id\" (missing field `id`"),
            "{skipped:?}"
        );
        assert!(
            skipped[1].starts_with("key \"lost\" (missing field `pem`"),
            "{skipped:?}"
        );
        assert!(skipped[1].contains("imported without a key"), "{skipped:?}");
        assert!(skipped[2].starts_with("snippet #2 ("), "{skipped:?}");
        assert!(
            skipped[3].starts_with("host \"no-address\" (missing field `host`"),
            "{skipped:?}"
        );
        assert!(skipped[4].starts_with("host \"bad-port\" ("), "{skipped:?}");
        assert!(skipped[5].starts_with("host #6 ("), "{skipped:?}");
        assert_eq!(skipped[6], "host \"blank\" (no address)");
    }

    #[test]
    fn termius_export_reads_a_file_with_a_byte_order_mark_and_null_lists() {
        let json = "\u{feff}{\"format\": \"termius-export\", \"version\": 1, \
                    \"groups\": null, \"tags\": null, \"keys\": null, \"snippets\": null, \
                    \"hosts\": [{\"label\": \"a\", \"host\": \"203.0.113.1\"}]}";

        let import = parse_termius_export(json).unwrap();
        assert_eq!(import.hosts.len(), 1);
        assert!(import.skipped.is_empty(), "{:?}", import.skipped);
    }

    #[test]
    fn termius_export_names_snippets_it_cannot_use() {
        let json = r#"{
            "format": "termius-export",
            "version": 1,
            "snippets": [
                {"id": "s1", "name": "ok", "script": "ls"},
                {"id": "s2", "name": "empty", "script": "  "},
                {"id": "s3", "script": "uptime"}
            ]
        }"#;

        let import = parse_termius_export(json).unwrap();
        assert_eq!(import.snippets.len(), 1);
        assert_eq!(
            import.skipped,
            vec![
                "snippet \"empty\" (no name or no script)".to_string(),
                "snippet \"s3\" (no name or no script)".to_string(),
            ]
        );
    }

    #[test]
    fn termius_host_without_auth_has_no_credentials() {
        let json = r#"{
            "format": "termius-export",
            "version": 1,
            "hosts": [
                {"label": "missing", "host": "203.0.113.1"},
                {"label": "null", "host": "203.0.113.2", "auth": null},
                {"label": "empty", "host": "203.0.113.3", "auth": {}}
            ]
        }"#;

        let import = parse_termius_export(json).unwrap();
        assert!(import.skipped.is_empty(), "{:?}", import.skipped);
        assert_eq!(import.hosts.len(), 3);
        for host in &import.hosts {
            assert!(host.key_pem.is_none());
            assert!(host.password.is_none());
        }
    }

    #[test]
    fn putty_percent_decode() {
        assert_eq!(percent_decode("my%20server"), "my server");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("trail%2"), "trail%2");
        assert_eq!(percent_decode("end%41"), "endA");
        // The two bytes after `%` end inside a two-byte character.
        assert_eq!(percent_decode("50%1\u{e9}"), "50%1\u{e9}");
    }
}
