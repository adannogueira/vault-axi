//! Vault sync: Git (plain `git pull/push`), and the encrypted backends (hosted cloud, S3) driven
//! by the shared planner in [`vault_axi_core::plan`] through [`engine`].

mod account;
mod cloud;
mod engine;
mod git;
mod s3;

pub use account::{handle_mfa, login_hosted, manage_api_tokens, register_hosted};

use crate::axi::AxiError;
use crate::config::{load_config, RemoteBackendConfig, VaultConfig};
use crate::crypto::KeyCache;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use vault_axi_core::crypto::DataKey;
use vault_axi_core::paths;
use vault_axi_core::payload;

const SYNC_COOLDOWN_MS: i64 = 15_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictInfo {
    pub path: String,
    pub resolution: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_copy: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncSummary {
    pub backend: String,
    pub pulled: usize,
    pub pushed: usize,
    pub deleted_remote: usize,
    pub deleted_local: usize,
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictInfo>,
    /// Paths that could not be transferred this run (retried on the next sync).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
}

pub fn ensure_sync_table(db: &Connection) {
    let _ = db.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sync_state (
          path          TEXT PRIMARY KEY,
          remote_hash   TEXT NOT NULL,
          remote_mtime  INTEGER NOT NULL,
          synced_at     INTEGER NOT NULL,
          base_content  TEXT NOT NULL DEFAULT ''
        );
        "#,
    );
    let _ = db.execute(
        "ALTER TABLE sync_state ADD COLUMN base_content TEXT NOT NULL DEFAULT ''",
        [],
    );
}

/// Every vault file the encrypted backends sync, as sorted vault-relative paths (see
/// [`paths::is_syncable`]).
pub fn list_all_syncable_files(vault_root: &str) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            let full = entry.path();
            if ft.is_dir() {
                let name = entry.file_name();
                if !paths::IGNORED_DIRS.contains(&name.to_string_lossy().as_ref()) {
                    walk(&full, root, out);
                }
            } else if ft.is_file() {
                let rel = crate::vault::to_rel(root, &full);
                let size = entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
                if paths::is_syncable(&rel, size) {
                    out.push(rel);
                }
            }
        }
    }
    let root = Path::new(vault_root);
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// Read a vault file into its sync payload (see [`payload::from_file`]).
pub fn read_sync_payload(abs_p: &Path, rel_path: &str) -> Option<String> {
    fs::read(abs_p)
        .ok()
        .map(|bytes| payload::from_file(rel_path, &bytes))
}

/// Write a sync payload back to disk.
pub fn write_sync_payload(abs_p: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = abs_p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(abs_p, payload::to_file(content))
}

fn report_sync_progress(action: &str, current: usize, total: usize, path: &str) {
    if cfg!(test)
        || std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1")
        || std::env::var("VAULT_AXI_QUIET").as_deref() == Ok("1")
    {
        return;
    }
    eprintln!("[{current}/{total}] {action} {path}...");
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn sync_disabled() -> bool {
    std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1")
}

fn last_sync_ms(db: &Connection) -> Option<i64> {
    db.query_row(
        "SELECT value FROM meta WHERE key = 'last_cloud_sync'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse().ok())
}

fn record_sync_time(db: &Connection) {
    let _ = db.execute(
        "INSERT INTO meta(key, value) VALUES ('last_cloud_sync', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![now_ms().to_string()],
    );
}

/// Sync with the configured remote. The encrypted backends retry once when the remote changed
/// underneath them (another device finished a sync first).
fn sync_configured(
    db: &Connection,
    vault_root: &str,
    cfg: &VaultConfig,
    do_pull: bool,
    do_push: bool,
) -> Result<SyncSummary, AxiError> {
    let remote = cfg.remote.as_ref().ok_or_else(no_remote_error)?;
    ensure_sync_table(db);
    let cache = KeyCache::default();
    let summary = match remote {
        RemoteBackendConfig::Git { repo_url, branch } => {
            git::sync_git(vault_root, repo_url.as_deref(), branch.as_deref(), do_push)?
        }
        RemoteBackendConfig::Cloud { .. } => {
            let mut remote = cloud::CloudRemote::from_config(cfg)?;
            engine::run_with_retry(db, vault_root, cfg, &cache, &mut remote, do_pull, do_push)?
        }
        RemoteBackendConfig::S3 {
            endpoint,
            bucket,
            prefix,
        } => {
            let mut remote = s3::S3Remote::new(endpoint, bucket, prefix.as_deref().unwrap_or(""));
            engine::run_with_retry(db, vault_root, cfg, &cache, &mut remote, do_pull, do_push)?
        }
    };
    record_sync_time(db);
    Ok(summary)
}

fn no_remote_error() -> AxiError {
    AxiError::new(
        "No cloud backend configured",
        "VALIDATION_ERROR",
        vec![
            "Sign in or register with the hosted cloud:".to_string(),
            "  vault register --email <email> --password <password>".to_string(),
            "  vault login --email <email> --password <password>".to_string(),
            "Or configure self-hosted Git / S3:".to_string(),
            "  vault setup --remote git:<repo-url>".to_string(),
            "  vault setup --remote s3://<bucket>/<prefix> --endpoint <https://...>".to_string(),
        ],
    )
}

/// Automatic pull before read/search commands if configured and cooldown elapsed.
pub fn auto_pull_if_needed(db: &Connection, vault_root: &str, force: bool) -> Option<SyncSummary> {
    if sync_disabled() {
        return None;
    }
    let cfg = load_config();
    if (cfg.auto_sync == Some(false) && !force) || cfg.remote.is_none() {
        return None;
    }
    ensure_sync_table(db);
    if !force && last_sync_ms(db).is_some_and(|last| now_ms() - last < SYNC_COOLDOWN_MS) {
        return None;
    }
    sync_configured(db, vault_root, &cfg, true, false).ok()
}

/// Automatic push after write/update/insert/move/delete/check/log commands. Pulls first, so
/// remote changes are merged before anything is pushed.
pub fn auto_push_if_needed(db: &Connection, vault_root: &str) -> Option<SyncSummary> {
    if sync_disabled() {
        return None;
    }
    let cfg = load_config();
    if cfg.auto_sync == Some(false) || cfg.remote.is_none() {
        return None;
    }
    sync_configured(db, vault_root, &cfg, true, true).ok()
}

/// Full manual or explicit sync (`vault sync [--pull | --push]`).
pub fn run_full_sync(
    db: &Connection,
    vault_root: &str,
    do_pull: bool,
    do_push: bool,
) -> Result<SyncSummary, AxiError> {
    sync_configured(db, vault_root, &load_config(), do_pull, do_push)
}

/// Re-wrap the configured remote's keyring around `data_key` under `new_passphrase`.
pub fn rewrap_keyring(
    cfg: &VaultConfig,
    data_key: &DataKey,
    new_passphrase: &str,
) -> Result<bool, AxiError> {
    match &cfg.remote {
        Some(RemoteBackendConfig::Cloud { .. }) => {
            let mut remote = cloud::CloudRemote::from_config(cfg)?;
            remote.authenticate()?;
            crate::crypto::rewrap_remote_keyring(&mut remote, data_key, new_passphrase)
        }
        Some(RemoteBackendConfig::S3 {
            endpoint,
            bucket,
            prefix,
        }) => {
            let mut remote = s3::S3Remote::new(endpoint, bucket, prefix.as_deref().unwrap_or(""));
            crate::crypto::rewrap_remote_keyring(&mut remote, data_key, new_passphrase)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_db_at;

    #[test]
    fn sync_table_and_disabled_auto_sync() {
        let db = open_db_at(":memory:").unwrap();
        ensure_sync_table(&db);
        ensure_sync_table(&db);
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);

        std::env::set_var("VAULT_AXI_NO_SYNC", "1");
        assert!(auto_pull_if_needed(&db, ".", true).is_none());
        assert!(auto_push_if_needed(&db, ".").is_none());
    }

    #[test]
    fn syncable_listing_follows_core_rules() {
        let dir = std::env::temp_dir().join(format!("vault-axi-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (rel, body) in [
            ("a.md", "a"),
            ("sub/b.md", "b"),
            ("img/p.png", "p"),
            (".obsidian/app.json", "{}"),
            (".obsidian/workspace.json", "{}"),
            (".obsidian/plugins/vault-axi-sync/data.json", "{}"),
            (".git/HEAD", "x"),
            (".vault-axi/keyring.json", "{}"),
            ("node_modules/m/readme.md", "x"),
        ] {
            let p = dir.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        assert_eq!(
            list_all_syncable_files(&dir.to_string_lossy()),
            [".obsidian/app.json", "a.md", "img/p.png", "sub/b.md"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn payload_files_roundtrip_through_disk() {
        let dir = std::env::temp_dir().join(format!("vault-axi-payload-{}", std::process::id()));
        let p = dir.join("x/bin.dat");
        let bytes = [0u8, 159, 146, 150];
        write_sync_payload(&p, &payload::encode_binary(&bytes)).unwrap();
        assert_eq!(fs::read(&p).unwrap(), bytes);
        assert_eq!(
            read_sync_payload(&p, "x/bin.dat").unwrap(),
            payload::encode_binary(&bytes)
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
