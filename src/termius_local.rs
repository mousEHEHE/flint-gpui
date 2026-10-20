//! Import hosts, keys and snippets straight from an installed Termius.
//! Reads the Chromium IndexedDB (LevelDB), decrypts the fields with the vault
//! key from Credential Manager (libsodium secretbox), and builds the same
//! `termius-export` model the file importer consumes.
//!
//! Windows only (gated in `main.rs`): on macOS and Linux a signed-in Termius
//! encrypts its hosts with an account key that is not stored locally.
//!
//! The LevelDB and V8 parsers track the on-disk formats Termius ships today; an
//! Electron bump can change either and break the read.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{json, Value as Json};

mod keychain;
mod leveldb;
mod secretbox;
mod v8;

use secretbox::Decryptor;
use v8::V8Value;

/// What was read from the installed Termius, plus user-facing warnings about
/// how complete the read is.
pub struct InstalledImport {
    pub import: crate::import::TermiusImport,
    pub warnings: Vec<String>,
}

/// Read the installed Termius database and return its hosts, keys and
/// snippets. The error string is user-facing.
pub fn import_from_installed_termius() -> Result<InstalledImport, String> {
    let dir = find_leveldb_dir().ok_or_else(|| {
        "Could not find Termius data on this computer. Is Termius installed and signed in?"
            .to_string()
    })?;
    tracing::info!(dir = %dir.display(), "reading installed Termius database");

    let mut warnings = Vec::new();
    if is_termius_running() {
        // A live Termius keeps rewriting the database, so the read can miss
        // recent changes. Still try, but tell the user.
        tracing::warn!("Termius appears to be running; import may be incomplete");
        warnings.push(
            "Termius is running, so recent changes may be missing. \
             Quit Termius and import again for a complete read."
                .to_string(),
        );
    }

    let key = keychain::read_local_key().map_err(|e| {
        tracing::error!(?e, "could not read Termius local key");
        format!("Could not read the Termius encryption key: {e}")
    })?;
    let decryptor = Decryptor::new(key);

    let records = leveldb::extract_records(&dir, &decryptor).map_err(|e| {
        tracing::error!(?e, "could not read Termius LevelDB");
        format!("Could not read the Termius database: {e}")
    })?;
    let opened = decryptor.opened_count();
    let failed = decryptor.failed_count();
    tracing::info!(
        records = records.len(),
        opened,
        failed,
        "parsed Termius records"
    );

    if failed > 0 && opened == 0 {
        return Err(format!(
            "The Termius key in the system keychain does not match its database \
             ({failed} encrypted fields could not be opened). \
             Open Termius and sign in again, then retry."
        ));
    }
    if failed > 0 {
        tracing::warn!(failed, opened, "some Termius fields could not be decrypted");
        warnings.push(format!(
            "{failed} encrypted fields could not be decrypted and were left empty."
        ));
    }

    let export = build_export(records);
    let text = serde_json::to_string(&export).map_err(|e| e.to_string())?;
    let import = crate::import::parse_termius_export(&text)?;
    if import.hosts.is_empty() && import.snippets.is_empty() {
        tracing::warn!(dir = %dir.display(), "Termius database read but no hosts or snippets found");
        return Err(
            "Found the Termius database but could not read any hosts or snippets from it. \
             This Termius version may store its data in a way flint cannot read yet."
                .to_string(),
        );
    }
    Ok(InstalledImport { import, warnings })
}

/// One record read from the database: its fields by name. `local_id` carries
/// the record's own id (from the value or recovered from the LevelDB key).
type Record = BTreeMap<String, V8Value>;

/// Termius keeps its data under `%APPDATA%\Termius`. The folder name
/// `file__0.indexeddb.leveldb` is Chromium's per-origin name for a `file://`
/// renderer.
fn find_leveldb_dir() -> Option<PathBuf> {
    let base = dirs::config_dir()?.join("Termius");
    let dir = base.join("IndexedDB").join("file__0.indexeddb.leveldb");
    dir.is_dir().then_some(dir)
}

fn is_termius_running() -> bool {
    use std::os::windows::process::CommandExt as _;
    // Keeps tasklist from flashing a console window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq Termius.exe", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("termius.exe")
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Classify records and join them into the termius-export model.
// ---------------------------------------------------------------------------

/// The classified record tables, keyed by `local_id`. A record's table is
/// decided by which fields it has.
#[derive(Default)]
struct Tables {
    hosts: BTreeMap<i64, Record>,
    configs: BTreeMap<i64, Record>,
    identities: BTreeMap<i64, Record>,
    keys: BTreeMap<i64, Record>,
    config_identity: BTreeMap<i64, Record>,
    groups: BTreeMap<i64, Record>,
    tags: BTreeMap<i64, Record>,
    tag_hosts: Vec<Record>,
    snippets: BTreeMap<i64, Record>,
}

fn build_export(records: Vec<Record>) -> Json {
    let tables = classify(records);
    let config_to_identity = map_config_to_identity(&tables);
    let host_tags = map_host_tags(&tables);

    let keys = build_keys(&tables);
    let groups = build_groups(&tables);
    let tags = build_tags(&tables);
    let snippets = build_snippets(&tables);
    let hosts = build_hosts(&tables, &config_to_identity, &host_tags, &snippets);

    json!({
        "format": "termius-export",
        "version": 1,
        "groups": groups,
        "tags": tags,
        "keys": keys.values().collect::<Vec<_>>(),
        "snippets": snippets.values().collect::<Vec<_>>(),
        "hosts": hosts,
    })
}

fn classify(records: Vec<Record>) -> Tables {
    let mut tables = Tables::default();
    for record in records {
        let has = |k: &str| record.contains_key(k);
        if has("address") && has("label") {
            merge_into(&mut tables.hosts, record);
        } else if has("parent_group") {
            merge_into(&mut tables.groups, record);
        } else if has("host") && has("tag") {
            tables.tag_hosts.push(record);
        } else if has("private_key") {
            merge_into(&mut tables.keys, record);
        } else if has("username") && has("ssh_key") {
            merge_into(&mut tables.identities, record);
        } else if has("ssh_config") && has("identity") {
            merge_into(&mut tables.config_identity, record);
        } else if (has("port") || has("identity")) && has("env_variables") {
            merge_into(&mut tables.configs, record);
        } else if has("script") && has("label") {
            // Snippets carry {label, script}. Command history shares `script`
            // but has no label, so both keys are required. This must precede
            // the tag rule: a snippet also looks like {label, is_shared}.
            merge_into(&mut tables.snippets, record);
        } else if has("label")
            && has("is_shared")
            && !(has("username") || has("private_key") || has("ssh_config") || has("script"))
        {
            merge_into(&mut tables.tags, record);
        }
    }
    tables
}

/// Merge a record into its table by `local_id`. A later partial record fills
/// only the fields the earlier one left empty.
fn merge_into(table: &mut BTreeMap<i64, Record>, record: Record) {
    let Some(id) = local_id(&record) else {
        return;
    };
    match table.get_mut(&id) {
        Some(existing) => {
            for (key, value) in record {
                if value.is_meaningful()
                    && existing
                        .get(&key)
                        .map(|v| !v.is_meaningful())
                        .unwrap_or(true)
                {
                    existing.insert(key, value);
                }
            }
        }
        None => {
            table.insert(id, record);
        }
    }
}

fn local_id(record: &Record) -> Option<i64> {
    record.get("local_id").and_then(V8Value::as_i64)
}

/// A foreign key is either a nested record (take its `local_id`) or a scalar
/// id.
fn foreign_key(value: Option<&V8Value>) -> Option<i64> {
    let value = value?;
    match value.as_object() {
        Some(map) => map.get("local_id").and_then(V8Value::as_i64),
        None => value.as_i64(),
    }
}

/// ssh_config id -> identity id, from the join table.
fn map_config_to_identity(tables: &Tables) -> BTreeMap<i64, i64> {
    let mut map = BTreeMap::new();
    for record in tables.config_identity.values() {
        let config = foreign_key(record.get("ssh_config"));
        let identity = foreign_key(record.get("identity"));
        if let (Some(config), Some(identity)) = (config, identity) {
            map.insert(config, identity);
        }
    }
    map
}

/// host id -> tag export ids, from the tag_hosts junction.
fn map_host_tags(tables: &Tables) -> BTreeMap<i64, Vec<String>> {
    let mut map: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for record in &tables.tag_hosts {
        let host = foreign_key(record.get("host"));
        let tag = foreign_key(record.get("tag"));
        if let (Some(host), Some(tag)) = (host, tag) {
            map.entry(host).or_default().push(format!("t{tag}"));
        }
    }
    map
}

fn build_keys(tables: &Tables) -> BTreeMap<i64, Json> {
    let mut keys = BTreeMap::new();
    for (&id, key) in &tables.keys {
        keys.insert(
            id,
            json!({
                "id": format!("k{id}"),
                "name": string_field(key, "label"),
                "pem": string_field(key, "private_key"),
                "passphrase": optional_field(key, "passphrase"),
            }),
        );
    }
    keys
}

fn build_groups(tables: &Tables) -> Vec<Json> {
    tables
        .groups
        .iter()
        .map(|(&id, group)| {
            let parent = foreign_key(group.get("parent_group"));
            json!({
                "id": format!("g{id}"),
                "name": string_field(group, "label"),
                "parent_id": parent.map(|p| format!("g{p}")),
            })
        })
        .collect()
}

fn build_tags(tables: &Tables) -> Vec<Json> {
    tables
        .tags
        .iter()
        .map(|(&id, tag)| {
            json!({
                "id": format!("t{id}"),
                "name": string_field(tag, "label"),
            })
        })
        .collect()
}

fn build_snippets(tables: &Tables) -> BTreeMap<i64, Json> {
    let mut snippets = BTreeMap::new();
    for (&id, snippet) in &tables.snippets {
        snippets.insert(
            id,
            json!({
                "id": format!("s{id}"),
                "name": string_field(snippet, "label"),
                "script": string_field(snippet, "script"),
            }),
        );
    }
    snippets
}

fn build_hosts(
    tables: &Tables,
    config_to_identity: &BTreeMap<i64, i64>,
    host_tags: &BTreeMap<i64, Vec<String>>,
    snippets: &BTreeMap<i64, Json>,
) -> Vec<Json> {
    let mut hosts = Vec::new();
    for (&host_id, host) in &tables.hosts {
        hosts.push(build_host(
            host_id,
            host,
            tables,
            config_to_identity,
            host_tags,
            snippets,
        ));
    }
    hosts.sort_by(|a, b| host_label(a).cmp(host_label(b)));
    hosts
}

fn build_host(
    host_id: i64,
    host: &Record,
    tables: &Tables,
    config_to_identity: &BTreeMap<i64, i64>,
    host_tags: &BTreeMap<i64, Vec<String>>,
    snippets: &BTreeMap<i64, Json>,
) -> Json {
    let config = host.get("ssh_config");
    let config_id = foreign_key(config);
    let port = inline_port(config).unwrap_or(22);

    let identity = resolve_identity(config, config_id, config_to_identity, tables);
    let username = identity
        .map(|i| string_field(i, "username"))
        .unwrap_or_default();
    let password = identity
        .and_then(|i| non_empty(string_field(i, "password")))
        .unwrap_or_default();
    let key_id = identity.and_then(|i| foreign_key(i.get("ssh_key")));
    // A key whose PEM could not be decrypted is empty: treat it as no key.
    let key_ref = key_id
        .filter(|id| {
            tables
                .keys
                .get(id)
                .is_some_and(|key| !string_field(key, "private_key").is_empty())
        })
        .map(|id| format!("k{id}"));

    let method = if key_ref.is_some() {
        "key"
    } else if !password.is_empty() {
        "password"
    } else {
        "none"
    };

    let group = foreign_key(host.get("group")).map(|g| format!("g{g}"));
    let tags = host_tags.get(&host_id).cloned().unwrap_or_default();
    let startup = resolve_startup_snippet(config, config_id, tables, snippets);

    json!({
        "label": string_field(host, "label"),
        "host": string_field(host, "address"),
        "port": port,
        "username": username,
        "os": optional_field(host, "os_name"),
        "group": group,
        "tags": tags,
        "startup_snippet": startup,
        "auth": {
            "method": method,
            "key_id": key_ref,
            "password": non_empty(password),
        },
    })
}

/// The identity for a host: inline on the ssh_config, else via the join table.
fn resolve_identity<'a>(
    config: Option<&'a V8Value>,
    config_id: Option<i64>,
    config_to_identity: &BTreeMap<i64, i64>,
    tables: &'a Tables,
) -> Option<&'a Record> {
    let inline = config
        .and_then(V8Value::as_object)
        .and_then(|map| foreign_key(map.get("identity")));
    let identity_id =
        inline.or_else(|| config_id.and_then(|id| config_to_identity.get(&id).copied()))?;
    tables.identities.get(&identity_id)
}

/// Startup snippet id for a host: inline on the ssh_config, else on the
/// standalone config record with the same id.
fn resolve_startup_snippet(
    config: Option<&V8Value>,
    config_id: Option<i64>,
    tables: &Tables,
    snippets: &BTreeMap<i64, Json>,
) -> Option<String> {
    let inline = config
        .and_then(V8Value::as_object)
        .and_then(|map| foreign_key(map.get("startup_snippet")));
    let standalone = config_id
        .and_then(|id| tables.configs.get(&id))
        .and_then(|c| foreign_key(c.get("startup_snippet")));
    let snippet_id = inline.or(standalone)?;
    snippets
        .contains_key(&snippet_id)
        .then(|| format!("s{snippet_id}"))
}

fn inline_port(config: Option<&V8Value>) -> Option<u16> {
    config?
        .as_object()
        .and_then(|map| map.get("port"))
        .and_then(V8Value::as_i64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|&p| p != 0)
}

// ---------------------------------------------------------------------------
// Field helpers.
// ---------------------------------------------------------------------------

fn string_field(record: &Record, key: &str) -> String {
    record
        .get(key)
        .and_then(V8Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn optional_field(record: &Record, key: &str) -> Option<String> {
    non_empty(string_field(record, key))
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

fn host_label(host: &Json) -> &str {
    host.get("label").and_then(Json::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> V8Value {
        V8Value::Int(n)
    }
    fn s(text: &str) -> V8Value {
        V8Value::String(text.to_string())
    }
    fn obj(pairs: &[(&str, V8Value)]) -> V8Value {
        let mut map = BTreeMap::new();
        for (k, v) in pairs {
            map.insert(k.to_string(), v.clone());
        }
        V8Value::Object(map)
    }
    fn record(pairs: &[(&str, V8Value)]) -> Record {
        let V8Value::Object(map) = obj(pairs) else {
            unreachable!()
        };
        map
    }

    #[test]
    fn classifies_and_joins_into_hosts() {
        let records = vec![
            record(&[
                ("local_id", int(1)),
                ("label", s("web-1")),
                ("address", s("10.0.0.1")),
                (
                    "ssh_config",
                    obj(&[
                        ("local_id", int(10)),
                        ("port", int(2222)),
                        ("identity", int(20)),
                        ("startup_snippet", int(30)),
                    ]),
                ),
            ]),
            record(&[
                ("local_id", int(20)),
                ("username", s("deploy")),
                ("ssh_key", int(40)),
                ("password", s("secret")),
            ]),
            record(&[
                ("local_id", int(40)),
                ("label", s("mykey")),
                ("private_key", s("-----BEGIN KEY-----")),
            ]),
            record(&[
                ("local_id", int(30)),
                ("label", s("tail logs")),
                ("script", s("tail -f x")),
            ]),
            record(&[
                ("local_id", int(50)),
                ("label", s("prod")),
                ("is_shared", V8Value::Bool(false)),
                ("status", int(0)),
            ]),
            record(&[("local_id", int(60)), ("host", int(1)), ("tag", int(50))]),
            record(&[
                ("local_id", int(70)),
                ("label", s("infra")),
                ("parent_group", V8Value::Null),
            ]),
        ];

        let import = {
            let export = build_export(records);
            let text = serde_json::to_string(&export).unwrap();
            crate::import::parse_termius_export(&text).unwrap()
        };

        assert_eq!(import.hosts.len(), 1);
        let host = &import.hosts[0];
        assert_eq!(host.label, "web-1");
        assert_eq!(host.host, "10.0.0.1");
        assert_eq!(host.port, 2222);
        assert_eq!(host.username.as_deref(), Some("deploy"));
        assert_eq!(host.key_pem.as_deref(), Some("-----BEGIN KEY-----"));
        assert_eq!(host.password.as_deref(), Some("secret"));
        assert_eq!(host.tags, vec!["prod".to_string()]);
        assert_eq!(host.startup_snippet.as_deref(), Some("tail logs"));
        assert_eq!(import.snippets.len(), 1);
    }

    #[test]
    fn identity_joined_through_config_identity_table() {
        let records = vec![
            record(&[
                ("local_id", int(1)),
                ("label", s("db")),
                ("address", s("db.local")),
                ("ssh_config", int(10)),
            ]),
            record(&[
                ("local_id", int(100)),
                ("ssh_config", int(10)),
                ("identity", int(20)),
            ]),
            record(&[
                ("local_id", int(20)),
                ("username", s("root")),
                ("ssh_key", V8Value::Null),
            ]),
        ];
        let export = build_export(records);
        let text = serde_json::to_string(&export).unwrap();
        let import = crate::import::parse_termius_export(&text).unwrap();
        assert_eq!(import.hosts.len(), 1);
        assert_eq!(import.hosts[0].username.as_deref(), Some("root"));
    }
}
