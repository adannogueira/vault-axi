use crate::axi::AxiError;
use crate::commands::history::record_edit_transition;
use crate::commands::trash::move_file_to_trash;
use crate::config::{
    env_api_key, load_config, save_config, RemoteBackendConfig, HOSTED_SUPABASE_ANON_KEY,
    HOSTED_SUPABASE_URL,
};
use crate::crypto::{decrypt_at_rest, encrypt_at_rest};
use crate::vault::{
    file_stat, list_attachment_files, list_markdown_files, list_obsidian_config_files,
    resolve_in_vault,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use vault_axi_core::crypto::v1;
use vault_axi_core::hash::sha1_hex;
use vault_axi_core::merge::{merge_three_way, MergeOutcome};
use vault_axi_core::paths::conflict_copy_path;
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
}

fn upsert_sync_state(
    db: &Connection,
    path: &str,
    remote_hash: &str,
    remote_mtime: i64,
    synced_at: i64,
    base_content: &str,
) {
    let enc_base = encrypt_at_rest(base_content);
    let _ = db.execute(
        "INSERT INTO sync_state(path, remote_hash, remote_mtime, synced_at, base_content) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET remote_hash=excluded.remote_hash, remote_mtime=excluded.remote_mtime, synced_at=excluded.synced_at, base_content=excluded.base_content",
        params![path, remote_hash, remote_mtime, synced_at, enc_base],
    );
}

/// Collect all syncable vault-relative paths: markdown notes, attachments, and `.obsidian/` config files.
pub fn list_all_syncable_files(vault_root: &str) -> Vec<String> {
    let mut all = list_markdown_files(vault_root);
    all.extend(list_attachment_files(vault_root));
    all.extend(list_obsidian_config_files(vault_root));
    all.sort();
    all.dedup();
    all
}

/// Read a vault file into its logical sync payload (see [`vault_axi_core::payload::from_file`]).
pub fn read_sync_payload(abs_p: &Path, rel_path: &str) -> Option<String> {
    fs::read(abs_p)
        .ok()
        .map(|bytes| payload::from_file(rel_path, &bytes))
}

/// Write a logical sync payload back to disk.
pub fn write_sync_payload(abs_p: &Path, payload: &str) -> std::io::Result<()> {
    if let Some(parent) = abs_p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(abs_p, payload::to_file(payload))
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

/// Automatic pull before read/search commands if configured and cooldown elapsed.
pub fn auto_pull_if_needed(db: &Connection, vault_root: &str, force: bool) -> Option<SyncSummary> {
    if std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1") {
        return None;
    }
    let cfg = load_config();
    if cfg.auto_sync == Some(false) && !force {
        return None;
    }
    let remote = cfg.remote.as_ref()?;
    ensure_sync_table(db);

    if !force {
        let last_sync: Option<i64> = db
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_cloud_sync'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| s.parse::<i64>().ok());
        if let Some(last) = last_sync {
            if now_ms() - last < SYNC_COOLDOWN_MS {
                return None;
            }
        }
    }

    let res = match remote {
        RemoteBackendConfig::Git { repo_url, branch } => {
            sync_git(vault_root, repo_url.as_deref(), branch.as_deref(), false).ok()
        }
        RemoteBackendConfig::Cloud {
            access_token,
            refresh_token,
            user_id,
            vault_id,
            ..
        } => sync_hosted_cloud(
            db,
            vault_root,
            access_token.as_deref(),
            refresh_token.as_deref(),
            user_id.as_deref(),
            vault_id.as_deref().unwrap_or("default"),
            true,
            false,
        )
        .ok(),
        RemoteBackendConfig::S3 {
            endpoint,
            bucket,
            prefix,
        } => sync_s3_manifest(
            db,
            vault_root,
            endpoint,
            bucket,
            prefix.as_deref().unwrap_or(""),
            true,
            false,
        )
        .ok(),
    };

    if res.is_some() {
        let _ = db.execute(
            "INSERT INTO meta(key, value) VALUES ('last_cloud_sync', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![now_ms().to_string()],
        );
    }
    res
}

/// Automatic push after write/update/insert/move/delete/check/log commands.
/// Runs with `do_pull = true, do_push = true` so remote changes are checked and merged before pushing.
pub fn auto_push_if_needed(db: &Connection, vault_root: &str) -> Option<SyncSummary> {
    if std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1") {
        return None;
    }
    let cfg = load_config();
    if cfg.auto_sync == Some(false) {
        return None;
    }
    let remote = cfg.remote.as_ref()?;
    ensure_sync_table(db);

    let res = match remote {
        RemoteBackendConfig::Git { repo_url, branch } => {
            sync_git(vault_root, repo_url.as_deref(), branch.as_deref(), true).ok()
        }
        RemoteBackendConfig::Cloud {
            access_token,
            refresh_token,
            user_id,
            vault_id,
            ..
        } => sync_hosted_cloud(
            db,
            vault_root,
            access_token.as_deref(),
            refresh_token.as_deref(),
            user_id.as_deref(),
            vault_id.as_deref().unwrap_or("default"),
            true,
            true,
        )
        .ok(),
        RemoteBackendConfig::S3 {
            endpoint,
            bucket,
            prefix,
        } => sync_s3_manifest(
            db,
            vault_root,
            endpoint,
            bucket,
            prefix.as_deref().unwrap_or(""),
            true,
            true,
        )
        .ok(),
    };

    if res.is_some() {
        let _ = db.execute(
            "INSERT INTO meta(key, value) VALUES ('last_cloud_sync', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![now_ms().to_string()],
        );
    }
    res
}

/// Full manual or explicit sync (`vault sync [--pull | --push]`).
pub fn run_full_sync(
    db: &Connection,
    vault_root: &str,
    do_pull: bool,
    do_push: bool,
) -> Result<SyncSummary, AxiError> {
    let cfg = load_config();
    let Some(remote) = cfg.remote.as_ref() else {
        return Err(AxiError::new(
            "No cloud backend configured",
            "VALIDATION_ERROR",
            vec![
                "Sign in or register with the hosted cloud:".to_string(),
                "  vault register --email <email> --password <password>".to_string(),
                "  vault login --email <email> --password <password>".to_string(),
                "Or configure self-hosted Git / S3:".to_string(),
                "  vault setup --remote git:<repo-url>".to_string(),
                "  vault setup --remote s3://<bucket>/<prefix> --endpoint <https://...>"
                    .to_string(),
            ],
        ));
    };
    ensure_sync_table(db);

    let summary = match remote {
        RemoteBackendConfig::Git { repo_url, branch } => {
            sync_git(vault_root, repo_url.as_deref(), branch.as_deref(), do_push)?
        }
        RemoteBackendConfig::Cloud {
            access_token,
            refresh_token,
            user_id,
            vault_id,
            ..
        } => sync_hosted_cloud(
            db,
            vault_root,
            access_token.as_deref(),
            refresh_token.as_deref(),
            user_id.as_deref(),
            vault_id.as_deref().unwrap_or("default"),
            do_pull,
            do_push,
        )?,
        RemoteBackendConfig::S3 {
            endpoint,
            bucket,
            prefix,
        } => sync_s3_manifest(
            db,
            vault_root,
            endpoint,
            bucket,
            prefix.as_deref().unwrap_or(""),
            do_pull,
            do_push,
        )?,
    };

    let _ = db.execute(
        "INSERT INTO meta(key, value) VALUES ('last_cloud_sync', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![now_ms().to_string()],
    );
    Ok(summary)
}

fn sync_git(
    vault_root: &str,
    repo_url: Option<&str>,
    branch: Option<&str>,
    push: bool,
) -> Result<SyncSummary, AxiError> {
    let git_dir = Path::new(vault_root).join(".git");
    if !git_dir.exists() {
        if let Some(url) = repo_url {
            let _ = Command::new("git")
                .args(["init"])
                .current_dir(vault_root)
                .output();
            let _ = Command::new("git")
                .args(["remote", "add", "origin", url])
                .current_dir(vault_root)
                .output();
        } else {
            return Err(AxiError::new(
                "Vault is not a git repository and no remote URL is configured",
                "SYNC_ERROR",
                vec!["Run `vault setup --remote git:<repo-url>`".to_string()],
            ));
        }
    }

    let br = branch.unwrap_or("main");
    let mut pulled = 0;
    let mut pushed = 0;

    let pull_out = Command::new("git")
        .args(["pull", "--rebase", "--autostash", "origin", br])
        .current_dir(vault_root)
        .output();
    if let Ok(out) = pull_out {
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("Updating") || stdout.contains("Fast-forward") {
            pulled = 1;
        }
    }

    if push {
        let status_out = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(vault_root)
            .output()
            .map_err(|e| AxiError::new(format!("git error: {e}"), "SYNC_ERROR", vec![]))?;
        let dirty = !String::from_utf8_lossy(&status_out.stdout)
            .trim()
            .is_empty();
        if dirty {
            let _ = Command::new("git")
                .args(["add", "-A"])
                .current_dir(vault_root)
                .output();
            let _ = Command::new("git")
                .args(["commit", "-m", "vault-axi: auto-sync"])
                .current_dir(vault_root)
                .output();
            let push_res = Command::new("git")
                .args(["push", "-u", "origin", br])
                .current_dir(vault_root)
                .output();
            if let Ok(p) = push_res {
                if p.status.success() {
                    pushed = 1;
                }
            }
        }
    }

    Ok(SyncSummary {
        backend: "git".to_string(),
        pulled,
        pushed,
        deleted_remote: 0,
        deleted_local: 0,
        message: Some(format!("Git sync complete (branch: {br})")),
        conflicts: Vec::new(),
    })
}

#[derive(Debug, Clone)]
struct SyncedEntry {
    remote_hash: String,
    #[allow(dead_code)]
    remote_mtime: i64,
    base_content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteNoteMeta {
    path: String,
    hash: String,
    mtime_ms: i64,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    enc_key_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteNoteFull {
    path: String,
    content: String,
    hash: String,
    mtime_ms: i64,
    #[serde(default)]
    deleted: bool,
}

fn fetch_remote_note(
    clean_url: &str,
    anon_key: &str,
    bearer: &str,
    vault_id: &str,
    path: &str,
) -> Option<RemoteNoteFull> {
    let fetch_url = format!(
        "{clean_url}/rest/v1/vault_notes?vault_id=eq.{vault_id}&path=eq.{}&select=path,content,hash,mtime_ms,deleted",
        postgrest_eq_val(path)
    );
    let mut req = ureq::get(&fetch_url)
        .set("apikey", anon_key)
        .set("Authorization", &format!("Bearer {bearer}"));
    if let Some(env_key) = env_api_key() {
        req = req.set("x-vault-api-key", &env_key);
    }
    let r = req.call().ok()?;
    let s = r.into_string().ok()?;
    let mut list: Vec<RemoteNoteFull> = serde_json::from_str(&s).ok()?;
    let mut note = list.pop()?;
    note.content = decrypt_at_rest(&note.content).ok()?;
    Some(note)
}

fn fetch_raw_remote_content(
    clean_url: &str,
    anon_key: &str,
    bearer: &str,
    vault_id: &str,
    path: &str,
) -> Option<String> {
    let fetch_url = format!(
        "{clean_url}/rest/v1/vault_notes?vault_id=eq.{vault_id}&path=eq.{}&select=content",
        postgrest_eq_val(path)
    );
    let mut req = ureq::get(&fetch_url)
        .set("apikey", anon_key)
        .set("Authorization", &format!("Bearer {bearer}"));
    if let Some(env_key) = env_api_key() {
        req = req.set("x-vault-api-key", &env_key);
    }
    let r = req.call().ok()?;
    let s = r.into_string().ok()?;
    let mut list: Vec<Value> = serde_json::from_str(&s).ok()?;
    let item = list.pop()?;
    item.get("content")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Resolve a concurrent modification where both local (`local_content`) and remote (`full`)
/// changed relative to `base_content`.
#[allow(clippy::too_many_arguments)]
fn resolve_concurrent_edit(
    db: &Connection,
    vault_root: &str,
    r_path: &str,
    abs_p: &Path,
    local_content: &str,
    full: &RemoteNoteFull,
    base_content: &str,
    now: i64,
    synced_state: &mut HashMap<String, SyncedEntry>,
    remote_map: &mut HashMap<String, RemoteNoteMeta>,
    summary: &mut SyncSummary,
) {
    if let Some(parent) = abs_p.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let is_binary = payload::is_binary(local_content) || payload::is_binary(&full.content);
    let outcome = if is_binary {
        if local_content == full.content {
            MergeOutcome::Clean(local_content.to_string())
        } else {
            MergeOutcome::Conflict
        }
    } else {
        merge_three_way(base_content, local_content, &full.content)
    };

    match outcome {
        MergeOutcome::Clean(merged) => {
            let merged_hash = sha1_hex(&merged);
            if r_path.to_lowercase().ends_with(".md") {
                record_edit_transition(db, r_path, Some(local_content), &merged, "sync-merge");
            }
            let _ = write_sync_payload(abs_p, &merged);
            if merged_hash == full.hash {
                upsert_sync_state(db, r_path, &full.hash, full.mtime_ms, now, &full.content);
                synced_state.insert(
                    r_path.to_string(),
                    SyncedEntry {
                        remote_hash: full.hash.clone(),
                        remote_mtime: full.mtime_ms,
                        base_content: full.content.clone(),
                    },
                );
                summary.pulled += 1;
            } else {
                // Local was the superset; update expected remote hash to full.hash so do_push uploads cleanly
                synced_state.insert(
                    r_path.to_string(),
                    SyncedEntry {
                        remote_hash: full.hash.clone(),
                        remote_mtime: full.mtime_ms,
                        base_content: full.content.clone(),
                    },
                );
            }
            remote_map.insert(
                r_path.to_string(),
                RemoteNoteMeta {
                    path: r_path.to_string(),
                    hash: full.hash.clone(),
                    mtime_ms: full.mtime_ms,
                    deleted: false,
                    enc_key_id: crate::crypto::active_key_fingerprint(),
                },
            );
        }
        MergeOutcome::AutoMerged(merged) => {
            if write_sync_payload(abs_p, &merged).is_ok() {
                if r_path.to_lowercase().ends_with(".md") {
                    record_edit_transition(db, r_path, Some(local_content), &merged, "sync-merge");
                }
                // Set synced_state to the remote version so `do_push` sees `merged` as dirty
                // relative to `full.hash` and pushes the merged result!
                upsert_sync_state(db, r_path, &full.hash, full.mtime_ms, now, &full.content);
                synced_state.insert(
                    r_path.to_string(),
                    SyncedEntry {
                        remote_hash: full.hash.clone(),
                        remote_mtime: full.mtime_ms,
                        base_content: full.content.clone(),
                    },
                );
                remote_map.insert(
                    r_path.to_string(),
                    RemoteNoteMeta {
                        path: r_path.to_string(),
                        hash: full.hash.clone(),
                        mtime_ms: full.mtime_ms,
                        deleted: false,
                        enc_key_id: crate::crypto::active_key_fingerprint(),
                    },
                );
                summary.pulled += 1;
                summary.conflicts.push(ConflictInfo {
                    path: r_path.to_string(),
                    resolution: "auto_merged".to_string(),
                    conflict_copy: None,
                    message: format!(
                        "Concurrent edit detected on '{r_path}': non-overlapping remote changes were automatically 3-way merged with your local edits."
                    ),
                });
            }
        }
        MergeOutcome::Conflict => {
            let conflict_rel = conflict_copy_path(r_path, now);
            if let Ok(conflict_abs) = resolve_in_vault(vault_root, &conflict_rel) {
                let conflict_p = Path::new(&conflict_abs);
                let _ = write_sync_payload(conflict_p, local_content);
            }
            if write_sync_payload(abs_p, &full.content).is_ok() {
                if r_path.to_lowercase().ends_with(".md") {
                    record_edit_transition(
                        db,
                        r_path,
                        Some(local_content),
                        &full.content,
                        "sync-conflict",
                    );
                }
                upsert_sync_state(db, r_path, &full.hash, full.mtime_ms, now, &full.content);
                synced_state.insert(
                    r_path.to_string(),
                    SyncedEntry {
                        remote_hash: full.hash.clone(),
                        remote_mtime: full.mtime_ms,
                        base_content: full.content.clone(),
                    },
                );
                remote_map.insert(
                    r_path.to_string(),
                    RemoteNoteMeta {
                        path: r_path.to_string(),
                        hash: full.hash.clone(),
                        mtime_ms: full.mtime_ms,
                        deleted: false,
                        enc_key_id: crate::crypto::active_key_fingerprint(),
                    },
                );
                summary.pulled += 1;
                summary.conflicts.push(ConflictInfo {
                    path: r_path.to_string(),
                    resolution: "conflict_copy_saved".to_string(),
                    conflict_copy: Some(conflict_rel.clone()),
                    message: format!(
                        "Concurrent edit conflict on '{r_path}': overlapping lines were modified on another device. Remote version is in '{r_path}' and your conflicting version was saved to '{conflict_rel}'."
                    ),
                });
            }
        }
    }
}

fn try_refresh_hosted_token(refresh_token: &str) -> Option<(String, String)> {
    let url = format!("{HOSTED_SUPABASE_URL}/auth/v1/token?grant_type=refresh_token");
    let payload = json!({ "refresh_token": refresh_token });
    let resp = ureq::post(&url)
        .set("apikey", HOSTED_SUPABASE_ANON_KEY)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .ok()?;
    let body: Value = serde_json::from_str(&resp.into_string().ok()?).ok()?;
    let new_access = body.get("access_token")?.as_str()?.to_string();
    let new_refresh = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or(refresh_token)
        .to_string();

    let mut cfg = load_config();
    if let Some(RemoteBackendConfig::Cloud {
        access_token,
        refresh_token: rt,
        ..
    }) = &mut cfg.remote
    {
        *access_token = Some(new_access.clone());
        *rt = Some(new_refresh.clone());
        save_config(&cfg);
    }
    Some((new_access, new_refresh))
}

/// Exchange a long-lived `vlt_...` API key for a fresh Supabase JWT session via the `token-exchange` Edge Function.
#[allow(clippy::type_complexity)]
fn exchange_api_key(
    api_key: &str,
) -> Result<
    (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
    ),
    String,
> {
    let url = format!("{HOSTED_SUPABASE_URL}/functions/v1/token-exchange");
    let payload = json!({
        "action": "exchange",
        "api_key": api_key.trim()
    });
    let resp = ureq::post(&url)
        .set("apikey", HOSTED_SUPABASE_ANON_KEY)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .map_err(extract_auth_error)?;
    let body: Value =
        serde_json::from_str(&resp.into_string().unwrap_or_default()).unwrap_or_else(|_| json!({}));
    let Some(access_token) = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    else {
        return Err("token-exchange did not return an access_token".to_string());
    };
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let user_id = body
        .get("user_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let email = body
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let vault_id = body
        .get("vault_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("write")
        .to_string();

    save_hosted_session_with_scope(
        Some(access_token.clone()),
        refresh_token.clone(),
        user_id.clone(),
        email.clone(),
        Some(&vault_id),
        Some(&scope),
    );
    Ok((access_token, refresh_token, user_id, email, vault_id, scope))
}

/// Delta sync with Hosted Cloud (`vault_notes` table protected by RLS `auth.uid() = user_id`).
#[allow(clippy::too_many_arguments)]
fn sync_hosted_cloud(
    db: &Connection,
    vault_root: &str,
    access_token: Option<&str>,
    refresh_token: Option<&str>,
    user_id: Option<&str>,
    vault_id: &str,
    do_pull: bool,
    do_push: bool,
) -> Result<SyncSummary, AxiError> {
    let clean_url = HOSTED_SUPABASE_URL;
    let anon_key = HOSTED_SUPABASE_ANON_KEY;
    let mut bearer = access_token.unwrap_or(anon_key).to_string();
    let mut effective_user_id: Option<String> = user_id.map(|s| s.to_string());
    let mut effective_vault_id: String = vault_id.to_string();
    let mut effective_scope: String = match load_config().remote {
        Some(RemoteBackendConfig::Cloud {
            scope: Some(sc), ..
        }) => sc,
        _ => "write".to_string(),
    };

    // If `bearer` is a `vlt_...` API key (e.g. from VAULT_API_KEY in a headless container),
    // silently exchange it for a short-lived JWT session before querying PostgREST.
    if bearer.starts_with("vlt_") {
        let (new_access, _, uid, _, vid, sc) = exchange_api_key(&bearer).map_err(|msg| {
            AxiError::new(
                format!("VAULT_API_KEY authentication failed: {msg}"),
                "AUTH_ERROR",
                vec![
                    "Verify your VAULT_API_KEY environment variable or create a new key with `vault token create`"
                        .to_string(),
                ],
            )
        })?;
        bearer = new_access;
        if uid.is_some() {
            effective_user_id = uid;
        }
        if effective_vault_id == "default" && vid != "default" {
            effective_vault_id = vid;
        }
        effective_scope = sc;
    }

    if do_push && !do_pull && effective_scope.eq_ignore_ascii_case("read") {
        return Err(AxiError::new(
            "API token has read-only scope ('read') and cannot push changes",
            "PERMISSION_DENIED",
            vec![
                "Create a read-write token with `vault token create --scope write`".to_string(),
                "Or run `vault sync --pull` to pull remote changes only.".to_string(),
            ],
        ));
    }

    let _ = crate::crypto::ensure_local_db_encrypted(db);
    let current_key_id = crate::crypto::active_key_fingerprint();

    // Fetch the entire remote manifest in pages of 1000 (PostgREST caps single responses at 1000 rows).
    let mut remote_manifest: Vec<RemoteNoteMeta> = Vec::new();
    let mut page_offset = 0usize;
    let page_size = 1000usize;
    let mut select_cols = "path,hash,mtime_ms,deleted,enc_key_id";

    loop {
        let manifest_url = format!(
            "{clean_url}/rest/v1/vault_notes?vault_id=eq.{effective_vault_id}&select={select_cols}&order=path.asc&limit={page_size}&offset={page_offset}"
        );
        let mut req = ureq::get(&manifest_url)
            .set("apikey", anon_key)
            .set("Authorization", &format!("Bearer {bearer}"));
        if let Some(env_key) = env_api_key() {
            req = req.set("x-vault-api-key", &env_key);
        }
        let mut resp = req.call();

        // If a self-hosted table lacks `enc_key_id`, fall back to the 4 base columns
        if let Err(ureq::Error::Status(400, _)) = resp {
            if select_cols.contains("enc_key_id") {
                select_cols = "path,hash,mtime_ms,deleted";
                let fallback_url = format!(
                    "{clean_url}/rest/v1/vault_notes?vault_id=eq.{effective_vault_id}&select={select_cols}&order=path.asc&limit={page_size}&offset={page_offset}"
                );
                let mut fb_req = ureq::get(&fallback_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"));
                if let Some(env_key) = env_api_key() {
                    fb_req = fb_req.set("x-vault-api-key", &env_key);
                }
                resp = fb_req.call();
            }
        }

        // Automatically refresh expired JWT via refresh_token or VAULT_API_KEY if available
        if resp.is_err() && page_offset == 0 {
            let mut refreshed = false;
            if let Some(rt) = refresh_token {
                if let Some((new_access, _)) = try_refresh_hosted_token(rt) {
                    bearer = new_access;
                    refreshed = true;
                }
            }
            if !refreshed {
                if let Some(env_key) = env_api_key() {
                    if env_key.starts_with("vlt_") {
                        if let Ok((new_access, _, uid, _, _, sc)) = exchange_api_key(&env_key) {
                            bearer = new_access;
                            if uid.is_some() {
                                effective_user_id = uid;
                            }
                            effective_scope = sc;
                            refreshed = true;
                        }
                    }
                }
            }
            if refreshed {
                let retry_url = format!(
                    "{clean_url}/rest/v1/vault_notes?vault_id=eq.{effective_vault_id}&select={select_cols}&order=path.asc&limit={page_size}&offset={page_offset}"
                );
                let mut retry_req = ureq::get(&retry_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"));
                if let Some(env_key) = env_api_key() {
                    retry_req = retry_req.set("x-vault-api-key", &env_key);
                }
                resp = retry_req.call();
            }
        }

        let resp = resp.map_err(|e| {
            AxiError::new(
                format!("Hosted cloud sync request failed: {e}"),
                "SYNC_ERROR",
                vec![
                    "Check your network connection or run `vault login --email <email> --password <password>`"
                        .to_string(),
                ],
            )
        })?;

        let body_str = resp.into_string().unwrap_or_else(|_| "[]".to_string());
        let page: Vec<RemoteNoteMeta> = serde_json::from_str(&body_str).unwrap_or_default();
        let count = page.len();
        remote_manifest.extend(page);
        if count < page_size {
            break;
        }
        page_offset += count;
    }

    let mut remote_map: HashMap<String, RemoteNoteMeta> = remote_manifest
        .into_iter()
        .map(|m| (m.path.clone(), m))
        .collect();

    // If any remote rows have `enc_key_id == "enc-v1"` (encrypted before key fingerprinting was added),
    // probe one such active row to see if it already decrypts with the active encryption key.
    if let Some(sample_path) = remote_map
        .values()
        .find(|m| !m.deleted && m.enc_key_id == "enc-v1")
        .map(|m| m.path.clone())
    {
        if let Some(raw_ct) = fetch_raw_remote_content(
            clean_url,
            anon_key,
            &bearer,
            &effective_vault_id,
            &sample_path,
        ) {
            let active_key = crate::crypto::resolve_active_key();
            if v1::is_v1(&raw_ct) && crate::crypto::decrypt_with_key(&raw_ct, &active_key).is_ok() {
                // Stamp all `enc-v1` rows in this vault with `current_key_id` so they don't need re-uploading
                let stamp_url = format!(
                    "{clean_url}/rest/v1/vault_notes?vault_id=eq.{effective_vault_id}&enc_key_id=eq.enc-v1"
                );
                let stamp_payload = json!({ "enc_key_id": current_key_id });
                let mut stamp_req = ureq::request("PATCH", &stamp_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .set("Content-Type", "application/json");
                if let Some(env_key) = env_api_key() {
                    stamp_req = stamp_req.set("x-vault-api-key", &env_key);
                }
                let _ = stamp_req.send_string(&stamp_payload.to_string());
                for rm in remote_map.values_mut() {
                    if rm.enc_key_id == "enc-v1" {
                        rm.enc_key_id = current_key_id.clone();
                    }
                }
            }
        }
    }

    let mut synced_state: HashMap<String, SyncedEntry> = HashMap::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT path, remote_hash, remote_mtime, COALESCE(base_content, '') FROM sync_state",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| {
            let raw_base: String = r.get(3)?;
            let dec_base = decrypt_at_rest(&raw_base).unwrap_or(raw_base);
            Ok((
                r.get::<_, String>(0)?,
                SyncedEntry {
                    remote_hash: r.get::<_, String>(1)?,
                    remote_mtime: r.get::<_, i64>(2)?,
                    base_content: dec_base,
                },
            ))
        }) {
            for (p, entry) in rows.flatten() {
                synced_state.insert(p, entry);
            }
        }
    }

    let mut summary = SyncSummary {
        backend: "vault-cloud".to_string(),
        ..Default::default()
    };
    let now = now_ms();

    // Pull changed remote files (with 3-way merge / conflict copy when local is also dirty)
    if do_pull {
        let mut remote_keys: Vec<String> = remote_map.keys().cloned().collect();
        remote_keys.sort();

        // Pre-count how many remote files actually need downloading so progress ends at N/N
        let mut to_download_paths: Vec<String> = Vec::new();
        for r_path in &remote_keys {
            let Some(r_meta) = remote_map.get(r_path).cloned() else {
                continue;
            };
            let Ok(abs) = resolve_in_vault(vault_root, r_path) else {
                continue;
            };
            let abs_p = Path::new(&abs);
            let local_content = read_sync_payload(abs_p, r_path);
            let local_hash = local_content.as_ref().map(|s| sha1_hex(s));
            let synced_entry = synced_state.get(r_path).cloned();

            if r_meta.deleted {
                if let Some(ref lh) = local_hash {
                    let local_modified = match &synced_entry {
                        Some(st) => lh != &st.remote_hash,
                        None => true,
                    };
                    if local_modified {
                        // Local user modified a note that was deleted on another device:
                        // keep the local note, clear sync_state so do_push re-uploads it, and warn.
                        let _ =
                            db.execute("DELETE FROM sync_state WHERE path = ?1", params![r_path]);
                        synced_state.remove(r_path);
                        summary.conflicts.push(ConflictInfo {
                            path: r_path.clone(),
                            resolution: "kept_local_edit_over_remote_delete".to_string(),
                            conflict_copy: None,
                            message: format!(
                                "Conflict on '{r_path}': note was deleted on another device but modified locally; kept your local version."
                            ),
                        });
                    } else {
                        let _ = move_file_to_trash(db, r_path, abs_p);
                        let _ = fs::remove_file(abs_p);
                        summary.deleted_local += 1;
                        let _ =
                            db.execute("DELETE FROM sync_state WHERE path = ?1", params![r_path]);
                        synced_state.remove(r_path);
                    }
                } else {
                    let _ = db.execute("DELETE FROM sync_state WHERE path = ?1", params![r_path]);
                    synced_state.remove(r_path);
                }
                continue;
            }

            // If local file already matches remote hash, ensure sync_state is up-to-date
            if local_hash.as_deref() == Some(r_meta.hash.as_str()) {
                if synced_entry.as_ref().map(|s| s.remote_hash.as_str())
                    != Some(r_meta.hash.as_str())
                {
                    if let Some(ref lc) = local_content {
                        upsert_sync_state(db, r_path, &r_meta.hash, r_meta.mtime_ms, now, lc);
                        synced_state.insert(
                            r_path.clone(),
                            SyncedEntry {
                                remote_hash: r_meta.hash.clone(),
                                remote_mtime: r_meta.mtime_ms,
                                base_content: lc.clone(),
                            },
                        );
                    }
                }
                continue;
            }

            let remote_changed_since_sync = match &synced_entry {
                Some(st) => st.remote_hash != r_meta.hash,
                None => true,
            };

            if remote_changed_since_sync {
                to_download_paths.push(r_path.clone());
            }
        }

        let total_to_download = to_download_paths.len().max(1);
        for (idx, r_path) in to_download_paths.into_iter().enumerate() {
            report_sync_progress("Downloading", idx + 1, total_to_download, &r_path);
            let Ok(abs) = resolve_in_vault(vault_root, &r_path) else {
                continue;
            };
            let abs_p = Path::new(&abs);
            let local_content = read_sync_payload(abs_p, &r_path);
            let local_hash = local_content.as_ref().map(|s| sha1_hex(s));
            let synced_entry = synced_state.get(&r_path).cloned();

            let Some(full) =
                fetch_remote_note(clean_url, anon_key, &bearer, &effective_vault_id, &r_path)
            else {
                continue;
            };

            let local_modified_since_sync = match (&local_hash, &synced_entry) {
                (None, _) => false,
                (Some(lh), Some(st)) => lh != &st.remote_hash,
                (Some(_), None) => true,
            };

            if !local_modified_since_sync {
                // Clean fast-forward pull: local file was untouched since last sync
                if write_sync_payload(abs_p, &full.content).is_ok() {
                    if r_path.to_lowercase().ends_with(".md") {
                        record_edit_transition(
                            db,
                            &r_path,
                            local_content.as_deref(),
                            &full.content,
                            "sync-pull",
                        );
                    }
                    upsert_sync_state(db, &r_path, &full.hash, full.mtime_ms, now, &full.content);
                    synced_state.insert(
                        r_path.clone(),
                        SyncedEntry {
                            remote_hash: full.hash.clone(),
                            remote_mtime: full.mtime_ms,
                            base_content: full.content.clone(),
                        },
                    );
                    summary.pulled += 1;
                }
            } else if let Some(ref lc) = local_content {
                // Both local and remote changed since last sync -> 3-way merge or conflict copy!
                let base_str = synced_entry
                    .as_ref()
                    .map(|s| s.base_content.as_str())
                    .unwrap_or("");
                resolve_concurrent_edit(
                    db,
                    vault_root,
                    &r_path,
                    abs_p,
                    lc,
                    &full,
                    base_str,
                    now,
                    &mut synced_state,
                    &mut remote_map,
                    &mut summary,
                );
            }
        }
    }

    // Push local additions, edits, and deletions (only if token scope allows write)
    let can_push = do_push && !effective_scope.eq_ignore_ascii_case("read");
    if can_push {
        let local_paths = list_all_syncable_files(vault_root);
        let local_set: HashSet<String> = local_paths.iter().cloned().collect();

        struct PendingUpload {
            l_path: String,
            abs_p: std::path::PathBuf,
            content: String,
            l_hash: String,
            l_mtime: i64,
            expected_remote_hash: Option<String>,
            can_batch_upsert: bool,
        }

        let mut upload_queue: Vec<PendingUpload> = Vec::new();

        for l_path in &local_paths {
            let Ok(abs) = resolve_in_vault(vault_root, l_path) else {
                continue;
            };
            let abs_p = Path::new(&abs);
            let Some(mut content) = read_sync_payload(abs_p, l_path) else {
                continue;
            };
            let mut l_hash = sha1_hex(&content);
            let l_mtime = file_stat(&abs).map(|s| s.mtime_ms).unwrap_or(now);

            // Safeguard 1: Only push if the file is locally dirty relative to `sync_state`
            // (or does not exist remotely yet), so an untouched local file never overwrites a newer remote edit.
            let is_locally_dirty = match synced_state.get(l_path) {
                None => true,
                Some(st) => st.remote_hash != l_hash,
            };

            // If `do_pull` was false (e.g. `vault sync --push`) and both local and remote changed
            // since `synced_state`, run 3-way merge / conflict resolution before pushing!
            if is_locally_dirty {
                if let Some(rm) = remote_map.get(l_path).cloned() {
                    if !rm.deleted && rm.hash != l_hash {
                        let base_moved = match synced_state.get(l_path) {
                            Some(st) => st.remote_hash != rm.hash,
                            None => true,
                        };
                        if base_moved {
                            if let Some(full) = fetch_remote_note(
                                clean_url,
                                anon_key,
                                &bearer,
                                &effective_vault_id,
                                l_path,
                            ) {
                                let base_str = synced_state
                                    .get(l_path)
                                    .map(|s| s.base_content.clone())
                                    .unwrap_or_default();
                                resolve_concurrent_edit(
                                    db,
                                    vault_root,
                                    l_path,
                                    abs_p,
                                    &content,
                                    &full,
                                    &base_str,
                                    now,
                                    &mut synced_state,
                                    &mut remote_map,
                                    &mut summary,
                                );
                                content = read_sync_payload(abs_p, l_path).unwrap_or(content);
                                l_hash = sha1_hex(&content);
                            }
                        }
                    }
                }
            }

            let is_still_dirty = match synced_state.get(l_path) {
                None => true,
                Some(st) => st.remote_hash != l_hash,
            };
            let remote_key_mismatch = match remote_map.get(l_path) {
                Some(rm) => !rm.deleted && rm.enc_key_id != current_key_id,
                None => false,
            };
            let needs_upload = match remote_map.get(l_path) {
                None => true,
                Some(rm) => {
                    remote_key_mismatch || (is_still_dirty && (rm.deleted || rm.hash != l_hash))
                }
            };

            if needs_upload {
                let existing_active_hash = remote_map
                    .get(l_path)
                    .filter(|rm| !rm.deleted)
                    .map(|rm| rm.hash.clone());
                let is_pure_key_or_new = match existing_active_hash.as_deref() {
                    None => true,
                    Some(eh) => eh == l_hash,
                };
                let can_batch_upsert = is_pure_key_or_new && content.len() <= 48_000;
                upload_queue.push(PendingUpload {
                    l_path: l_path.clone(),
                    abs_p: abs_p.to_path_buf(),
                    content,
                    l_hash,
                    l_mtime,
                    expected_remote_hash: existing_active_hash,
                    can_batch_upsert,
                });
            }
        }

        let total_to_upload = upload_queue.len().max(1);
        let mut push_idx = 0usize;
        let mut q_idx = 0usize;

        while q_idx < upload_queue.len() {
            // Fast path: batch consecutive small new / re-encrypted files (up to 20 files or ~256 KB)
            if upload_queue[q_idx].can_batch_upsert {
                let batch_start = q_idx;
                let mut batch_bytes = 0usize;
                let mut batch_rows: Vec<Value> = Vec::new();

                while q_idx < upload_queue.len()
                    && upload_queue[q_idx].can_batch_upsert
                    && batch_rows.len() < 20
                    && (batch_bytes + upload_queue[q_idx].content.len() <= 256_000
                        || batch_rows.is_empty())
                {
                    let item = &upload_queue[q_idx];
                    push_idx += 1;
                    report_sync_progress("Uploading", push_idx, total_to_upload, &item.l_path);
                    let encrypted_content = encrypt_at_rest(&item.content);
                    batch_bytes += encrypted_content.len();
                    let mut row = json!({
                        "vault_id": effective_vault_id,
                        "path": item.l_path,
                        "content": encrypted_content,
                        "hash": item.l_hash,
                        "mtime_ms": item.l_mtime,
                        "deleted": false,
                        "enc_key_id": current_key_id,
                        "updated_at": chrono::Utc::now().to_rfc3339()
                    });
                    if let Some(ref uid) = effective_user_id {
                        row["user_id"] = json!(uid);
                    }
                    batch_rows.push(row);
                    q_idx += 1;
                }

                let upsert_url =
                    format!("{clean_url}/rest/v1/vault_notes?on_conflict=user_id,vault_id,path");
                let mut post_req = ureq::post(&upsert_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .set("Content-Type", "application/json")
                    .set("Prefer", "resolution=merge-duplicates");
                if let Some(env_key) = env_api_key() {
                    post_req = post_req.set("x-vault-api-key", &env_key);
                }

                let mut batch_ok = false;
                if let Ok(r) = post_req.send_string(&Value::Array(batch_rows.clone()).to_string()) {
                    if r.status() < 300 {
                        batch_ok = true;
                    }
                }
                if !batch_ok {
                    // Refresh token if needed and retry individual items in the batch
                    if let Some(rt) = refresh_token {
                        if let Some((new_access, _)) = try_refresh_hosted_token(rt) {
                            bearer = new_access;
                        }
                    }
                    for (offset, row_val) in batch_rows.into_iter().enumerate() {
                        let item = &upload_queue[batch_start + offset];
                        let mut single_req = ureq::post(&upsert_url)
                            .set("apikey", anon_key)
                            .set("Authorization", &format!("Bearer {bearer}"))
                            .set("Content-Type", "application/json")
                            .set("Prefer", "resolution=merge-duplicates");
                        if let Some(env_key) = env_api_key() {
                            single_req = single_req.set("x-vault-api-key", &env_key);
                        }
                        if let Ok(r) = single_req.send_string(&row_val.to_string()) {
                            if r.status() < 300 {
                                upsert_sync_state(
                                    db,
                                    &item.l_path,
                                    &item.l_hash,
                                    item.l_mtime,
                                    now,
                                    &item.content,
                                );
                                synced_state.insert(
                                    item.l_path.clone(),
                                    SyncedEntry {
                                        remote_hash: item.l_hash.clone(),
                                        remote_mtime: item.l_mtime,
                                        base_content: item.content.clone(),
                                    },
                                );
                                summary.pushed += 1;
                            }
                        }
                    }
                } else {
                    for item in &upload_queue[batch_start..q_idx] {
                        upsert_sync_state(
                            db,
                            &item.l_path,
                            &item.l_hash,
                            item.l_mtime,
                            now,
                            &item.content,
                        );
                        synced_state.insert(
                            item.l_path.clone(),
                            SyncedEntry {
                                remote_hash: item.l_hash.clone(),
                                remote_mtime: item.l_mtime,
                                base_content: item.content.clone(),
                            },
                        );
                        summary.pushed += 1;
                    }
                }
                continue;
            }

            // Standard single-item path (content edit on existing remote row or large binary attachment)
            let item = &upload_queue[q_idx];
            q_idx += 1;
            push_idx += 1;
            let l_path = &item.l_path;
            let abs_p = item.abs_p.as_path();
            let mut content = item.content.clone();
            let mut l_hash = item.l_hash.clone();
            let l_mtime = item.l_mtime;

            report_sync_progress("Uploading", push_idx, total_to_upload, l_path);
            let encrypted_content = encrypt_at_rest(&content);

            if let Some(ref expected_r_hash) = item.expected_remote_hash {
                let patch_url = format!(
                    "{clean_url}/rest/v1/vault_notes?vault_id=eq.{effective_vault_id}&path=eq.{}&hash=eq.{expected_r_hash}&select=path,hash",
                    postgrest_eq_val(l_path)
                );
                let patch_payload = json!({
                    "content": encrypted_content,
                    "hash": l_hash,
                    "mtime_ms": l_mtime,
                    "deleted": false,
                    "enc_key_id": current_key_id,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                });
                let mut patch_req = ureq::request("PATCH", &patch_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .set("Content-Type", "application/json")
                    .set("Prefer", "return=representation");
                if let Some(env_key) = env_api_key() {
                    patch_req = patch_req.set("x-vault-api-key", &env_key);
                }
                if let Ok(r) = patch_req.send_string(&patch_payload.to_string()) {
                    if r.status() < 300 {
                        let body = r.into_string().unwrap_or_else(|_| "[]".to_string());
                        let updated_rows: Vec<Value> =
                            serde_json::from_str(&body).unwrap_or_default();
                        if !updated_rows.is_empty() {
                            upsert_sync_state(db, l_path, &l_hash, l_mtime, now, &content);
                            synced_state.insert(
                                l_path.clone(),
                                SyncedEntry {
                                    remote_hash: l_hash,
                                    remote_mtime: l_mtime,
                                    base_content: content,
                                },
                            );
                            summary.pushed += 1;
                            continue;
                        } else if let Some(full) = fetch_remote_note(
                            clean_url,
                            anon_key,
                            &bearer,
                            &effective_vault_id,
                            l_path,
                        ) {
                            // Another device updated the row in the milliseconds since manifest fetch!
                            let base_str = synced_state
                                .get(l_path)
                                .map(|s| s.base_content.clone())
                                .unwrap_or_default();
                            resolve_concurrent_edit(
                                db,
                                vault_root,
                                l_path,
                                abs_p,
                                &content,
                                &full,
                                &base_str,
                                now,
                                &mut synced_state,
                                &mut remote_map,
                                &mut summary,
                            );
                            content = read_sync_payload(abs_p, l_path).unwrap_or(content);
                            l_hash = sha1_hex(&content);
                            if l_hash == full.hash {
                                continue;
                            }
                        }
                    }
                }
            }

            let encrypted_content_final = encrypt_at_rest(&content);
            let upsert_url =
                format!("{clean_url}/rest/v1/vault_notes?on_conflict=user_id,vault_id,path");
            let mut payload = json!({
                "vault_id": effective_vault_id,
                "path": l_path,
                "content": encrypted_content_final,
                "hash": l_hash,
                "mtime_ms": l_mtime,
                "deleted": false,
                "enc_key_id": current_key_id,
                "updated_at": chrono::Utc::now().to_rfc3339()
            });
            if let Some(ref uid) = effective_user_id {
                payload["user_id"] = json!(uid);
            }
            let mut post_req = ureq::post(&upsert_url)
                .set("apikey", anon_key)
                .set("Authorization", &format!("Bearer {bearer}"))
                .set("Content-Type", "application/json")
                .set("Prefer", "resolution=merge-duplicates");
            if let Some(env_key) = env_api_key() {
                post_req = post_req.set("x-vault-api-key", &env_key);
            }
            if let Ok(r) = post_req.send_string(&payload.to_string()) {
                if r.status() < 300 {
                    upsert_sync_state(db, l_path, &l_hash, l_mtime, now, &content);
                    synced_state.insert(
                        l_path.clone(),
                        SyncedEntry {
                            remote_hash: l_hash,
                            remote_mtime: l_mtime,
                            base_content: content,
                        },
                    );
                    summary.pushed += 1;
                }
            }
        }

        let synced_keys: Vec<(String, String)> = synced_state
            .iter()
            .map(|(k, v)| (k.clone(), v.remote_hash.clone()))
            .collect();
        for (synced_path, last_remote_hash) in synced_keys {
            if !local_set.contains(&synced_path) {
                // If another device modified this note remotely since we last synced it,
                // restore the newer remote edit instead of deleting it!
                if let Some(rm) = remote_map.get(&synced_path) {
                    if !rm.deleted && rm.hash != last_remote_hash {
                        if let Some(full) = fetch_remote_note(
                            clean_url,
                            anon_key,
                            &bearer,
                            &effective_vault_id,
                            &synced_path,
                        ) {
                            if let Ok(abs) = resolve_in_vault(vault_root, &synced_path) {
                                let abs_p = Path::new(&abs);
                                if write_sync_payload(abs_p, &full.content).is_ok() {
                                    upsert_sync_state(
                                        db,
                                        &synced_path,
                                        &full.hash,
                                        full.mtime_ms,
                                        now,
                                        &full.content,
                                    );
                                    summary.pulled += 1;
                                    summary.conflicts.push(ConflictInfo {
                                        path: synced_path.clone(),
                                        resolution: "restored_remote_edit_over_local_delete"
                                            .to_string(),
                                        conflict_copy: None,
                                        message: format!(
                                            "Conflict on '{synced_path}': note was deleted locally but modified on another device; restored the newer remote version."
                                        ),
                                    });
                                    continue;
                                }
                            }
                        }
                    }
                }

                let upsert_url =
                    format!("{clean_url}/rest/v1/vault_notes?on_conflict=user_id,vault_id,path");
                let mut payload = json!({
                    "vault_id": effective_vault_id,
                    "path": synced_path,
                    "content": "",
                    "hash": "",
                    "mtime_ms": now,
                    "deleted": true,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                });
                if let Some(ref uid) = effective_user_id {
                    payload["user_id"] = json!(uid);
                }
                let mut del_req = ureq::post(&upsert_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .set("Content-Type", "application/json")
                    .set("Prefer", "resolution=merge-duplicates");
                if let Some(env_key) = env_api_key() {
                    del_req = del_req.set("x-vault-api-key", &env_key);
                }
                let _ = del_req.send_string(&payload.to_string());
                let _ = db.execute(
                    "DELETE FROM sync_state WHERE path = ?1",
                    params![&synced_path],
                );
                summary.deleted_remote += 1;
            }
        }
    }

    summary.message = Some(if do_push && effective_scope.eq_ignore_ascii_case("read") {
        format!(
            "Vault Cloud sync complete (pulled {}, push skipped: read-only token)",
            summary.pulled
        )
    } else if summary.conflicts.is_empty() {
        format!(
            "Vault Cloud sync complete (pulled {}, pushed {})",
            summary.pulled, summary.pushed
        )
    } else {
        format!(
            "Vault Cloud sync complete (pulled {}, pushed {}, {} conflict(s) handled)",
            summary.pulled,
            summary.pushed,
            summary.conflicts.len()
        )
    });
    Ok(summary)
}

/// Sync with an S3 / Cloudflare R2 HTTP endpoint using a JSON manifest.
fn sync_s3_manifest(
    db: &Connection,
    vault_root: &str,
    endpoint: &str,
    bucket: &str,
    prefix: &str,
    do_pull: bool,
    do_push: bool,
) -> Result<SyncSummary, AxiError> {
    let clean_ep = endpoint.trim_end_matches('/');
    let clean_prefix = prefix.trim_matches('/');
    let base = if clean_prefix.is_empty() {
        format!("{clean_ep}/{bucket}")
    } else {
        format!("{clean_ep}/{bucket}/{clean_prefix}")
    };

    let manifest_url = format!("{base}/.vault-manifest.json");
    let mut remote_hashes: HashMap<String, String> = match ureq::get(&manifest_url).call() {
        Ok(resp) => resp
            .into_string()
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        Err(_) => HashMap::new(),
    };

    let _ = crate::crypto::ensure_local_db_encrypted(db);
    let current_key_id = crate::crypto::active_key_fingerprint();
    let mut summary = SyncSummary {
        backend: "s3".to_string(),
        ..Default::default()
    };
    let now = now_ms();

    if do_pull {
        let mut to_pull: Vec<(String, String)> = Vec::new();
        for (r_path, r_entry) in &remote_hashes {
            let r_hash = r_entry.split(':').next().unwrap_or(r_entry);
            let Ok(abs) = resolve_in_vault(vault_root, r_path) else {
                continue;
            };
            let abs_p = Path::new(&abs);
            let local_hash = read_sync_payload(abs_p, r_path).map(|s| sha1_hex(&s));
            if local_hash.as_deref() != Some(r_hash) {
                to_pull.push((r_path.clone(), r_hash.to_string()));
            }
        }
        to_pull.sort_by(|a, b| a.0.cmp(&b.0));
        let total_to_pull = to_pull.len().max(1);
        for (idx, (r_path, r_hash)) in to_pull.into_iter().enumerate() {
            let Ok(abs) = resolve_in_vault(vault_root, &r_path) else {
                continue;
            };
            let abs_p = Path::new(&abs);
            report_sync_progress("Downloading", idx + 1, total_to_pull, &r_path);
            let file_url = format!("{base}/{r_path}");
            if let Ok(resp) = ureq::get(&file_url).call() {
                if let Ok(raw_content) = resp.into_string() {
                    let Ok(content) = decrypt_at_rest(&raw_content) else {
                        continue;
                    };
                    if write_sync_payload(abs_p, &content).is_ok() {
                        upsert_sync_state(db, &r_path, &r_hash, now, now, &content);
                        summary.pulled += 1;
                    }
                }
            }
        }
    }

    if do_push {
        let mut manifest_changed = false;
        let local_files = list_all_syncable_files(vault_root);
        let mut to_push: Vec<(String, String, String)> = Vec::new();
        for l_path in local_files {
            let Ok(abs) = resolve_in_vault(vault_root, &l_path) else {
                continue;
            };
            let abs_p = Path::new(&abs);
            let Some(content) = read_sync_payload(abs_p, &l_path) else {
                continue;
            };
            let l_hash = sha1_hex(&content);
            let expected_entry = format!("{l_hash}:{current_key_id}");
            let matches_remote = match remote_hashes.get(&l_path) {
                Some(existing) => existing == &expected_entry || existing == &l_hash,
                None => false,
            };
            if !matches_remote {
                to_push.push((l_path, content, l_hash));
            }
        }
        let total_to_push = to_push.len().max(1);
        for (idx, (l_path, content, l_hash)) in to_push.into_iter().enumerate() {
            report_sync_progress("Uploading", idx + 1, total_to_push, &l_path);
            let enc_content = encrypt_at_rest(&content);
            let file_url = format!("{base}/{l_path}");
            if let Ok(r) = ureq::put(&file_url)
                .set("Content-Type", "text/plain; charset=utf-8")
                .send_string(&enc_content)
            {
                if r.status() < 300 {
                    remote_hashes.insert(l_path.clone(), format!("{l_hash}:{current_key_id}"));
                    upsert_sync_state(db, &l_path, &l_hash, now, now, &content);
                    manifest_changed = true;
                    summary.pushed += 1;
                }
            }
        }
        if manifest_changed {
            if let Ok(json_str) = serde_json::to_string_pretty(&remote_hashes) {
                let _ = ureq::put(&manifest_url)
                    .set("Content-Type", "application/json")
                    .send_string(&json_str);
            }
        }
    }

    summary.message = Some(format!(
        "S3 sync complete (pulled {}, pushed {})",
        summary.pulled, summary.pushed
    ));
    Ok(summary)
}

fn urlencoding_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Format and URL-encode a value for PostgREST `col=eq.<val>` filters so characters like
/// parentheses, commas, or quotes in file paths (e.g. `Note (conflict ...).md`) don't break parsing.
fn postgrest_eq_val(s: &str) -> String {
    if s.chars().any(|c| matches!(c, ',' | '(' | ')' | '"' | '\\')) {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        urlencoding_simple(&format!("\"{escaped}\""))
    } else {
        urlencoding_simple(s)
    }
}

fn extract_auth_error(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, resp) => {
            if let Ok(raw) = resp.into_string() {
                if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                    if let Some(msg) = v
                        .get("msg")
                        .or_else(|| v.get("error_description"))
                        .or_else(|| v.get("message"))
                        .or_else(|| v.get("error"))
                        .and_then(|m| m.as_str())
                    {
                        return msg.to_string();
                    }
                }
            }
            format!("HTTP status {code}")
        }
        ureq::Error::Transport(t) => format!("Network error: {t}"),
    }
}

fn try_password_login(
    clean_url: &str,
    anon_key: &str,
    email: &str,
    password: &str,
) -> Result<(String, Option<String>, Option<String>), String> {
    let auth_url = format!("{clean_url}/auth/v1/token?grant_type=password");
    let payload = json!({ "email": email, "password": password });
    let resp = ureq::post(&auth_url)
        .set("apikey", anon_key)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .map_err(extract_auth_error)?;
    let body: Value =
        serde_json::from_str(&resp.into_string().unwrap_or_default()).unwrap_or_else(|_| json!({}));
    let Some(access_token) = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    else {
        return Err("Missing access_token in response".to_string());
    };
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let user_id = body
        .get("user")
        .and_then(|u| u.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok((access_token, refresh_token, user_id))
}

const CONFIRM_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>vault-axi — Email Confirmed</title>
  <style>
    body {
      margin: 0;
      min-height: 100vh;
      display: flex;
      align-items: center;
      justify-content: center;
      background: #0d1117;
      color: #e6edf3;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    }
    .card {
      max-width: 420px;
      padding: 32px;
      border-radius: 12px;
      background: #161b22;
      border: 1px solid #30363d;
      text-align: center;
      box-shadow: 0 12px 32px rgba(0, 0, 0, 0.4);
    }
    .icon {
      width: 48px;
      height: 48px;
      margin: 0 auto 16px;
      border-radius: 50%;
      background: rgba(46, 160, 67, 0.15);
      color: #3fb950;
      display: flex;
      align-items: center;
      justify-content: center;
      font-size: 24px;
      font-weight: bold;
    }
    h1 { margin: 0 0 10px; font-size: 20px; }
    p { margin: 0; color: #8b949e; line-height: 1.5; font-size: 14px; }
    code { color: #79c0ff; background: rgba(110,118,129,0.2); padding: 2px 6px; border-radius: 4px; }
  </style>
</head>
<body>
  <div class="card">
    <div class="icon">&#10003;</div>
    <h1>Email Confirmed</h1>
    <p id="msg">Your <code>vault-axi</code> cloud account is active and signed in. You can close this tab and return to your terminal.</p>
  </div>
  <script>
    if (window.location.hash && window.location.hash.length > 1) {
      fetch('/auth-callback?' + window.location.hash.substring(1)).catch(function(){});
      history.replaceState(null, '', window.location.pathname);
    }
  </script>
</body>
</html>"#;

fn parse_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key && !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn serve_confirm_request(mut stream: std::net::TcpStream) -> Option<(String, Option<String>)> {
    use std::io::{Read, Write};
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");

    if let Some(q) = path.strip_prefix("/auth-callback?") {
        let token = parse_query_param(q, "access_token");
        let refresh = parse_query_param(q, "refresh_token");
        let body = r#"{"ok":true}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
        if let Some(t) = token {
            return Some((t, refresh));
        }
        return None;
    }

    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        CONFIRM_HTML.len(),
        CONFIRM_HTML
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
    None
}

fn wait_for_email_confirmation(
    clean_url: &str,
    anon_key: &str,
    email: &str,
    password: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    if cfg!(test)
        || std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1")
        || std::env::var("VAULT_AXI_NO_WAIT").as_deref() == Ok("1")
    {
        return None;
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:3000").ok();
    if let Some(ref l) = listener {
        let _ = l.set_nonblocking(true);
    }

    eprintln!("==> Confirmation email sent to {email}.");
    eprintln!(
        "==> Waiting for you to click the confirmation link in your email... (Ctrl+C to skip)"
    );

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(180);
    let mut last_poll = std::time::Instant::now();

    while start.elapsed() < timeout {
        if let Some(ref l) = listener {
            while let Ok((stream, _)) = l.accept() {
                if let Some((tok, refr)) = serve_confirm_request(stream) {
                    eprintln!("==> Email confirmed! Logging in...");
                    return Some((tok, refr, None));
                }
            }
        }

        if last_poll.elapsed() >= std::time::Duration::from_millis(2500) {
            last_poll = std::time::Instant::now();
            if let Ok((tok, refr, uid)) = try_password_login(clean_url, anon_key, email, password) {
                // Keep serving localhost:3000 briefly in case the browser redirect is still in flight
                if let Some(ref l) = listener {
                    let grace = std::time::Instant::now();
                    while grace.elapsed() < std::time::Duration::from_millis(700) {
                        if let Ok((stream, _)) = l.accept() {
                            let _ = serve_confirm_request(stream);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
                eprintln!("==> Email confirmed! Logged in as {email}.");
                return Some((tok, refr, uid));
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    None
}

/// Register a new user account on the official Hosted Cloud.
pub fn register_hosted(
    email: &str,
    password: &str,
    vault_id: Option<&str>,
) -> Result<Value, AxiError> {
    let clean_url = HOSTED_SUPABASE_URL;
    let anon_key = HOSTED_SUPABASE_ANON_KEY;

    let edge_register_url = format!("{clean_url}/functions/v1/register");
    let edge_payload = json!({
        "email": email,
        "password": password,
    });
    let resp = match ureq::post(&edge_register_url)
        .set("apikey", anon_key)
        .set("Content-Type", "application/json")
        .send_string(&edge_payload.to_string())
    {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(404, _)) => {
            let signup_url = format!("{clean_url}/auth/v1/signup");
            let payload = json!({
                "email": email,
                "password": password,
                "options": {
                    "email_redirect_to": "http://localhost:3000"
                }
            });
            ureq::post(&signup_url)
                .set("apikey", anon_key)
                .set("Content-Type", "application/json")
                .send_string(&payload.to_string())
        }
        Err(e) => Err(e),
    };

    let body: Value = match resp {
        Ok(r) => {
            serde_json::from_str(&r.into_string().unwrap_or_default()).unwrap_or_else(|_| json!({}))
        }
        Err(e) => {
            // If the account already exists and is confirmed, log in directly
            if let Ok((tok, refr, uid)) = try_password_login(clean_url, anon_key, email, password) {
                let local_cache = save_hosted_session(
                    Some(tok),
                    refr,
                    uid.clone(),
                    Some(email.to_string()),
                    vault_id,
                );
                return Ok(json!({
                    "ok": true,
                    "action": "logged_in_existing_account",
                    "email": email,
                    "userId": uid,
                    "vaultId": vault_id.unwrap_or("default"),
                    "localVaultCache": local_cache,
                    "autoSync": true,
                    "next": ["vault sync", "vault ls"]
                }));
            }
            let msg = extract_auth_error(e);
            return Err(AxiError::new(
                format!("Registration failed: {msg}"),
                "AUTH_ERROR",
                vec!["Ensure the email is valid and password is at least 6 characters".to_string()],
            ));
        }
    };

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let user_id = body
        .get("user")
        .and_then(|u| u.get("id"))
        .or_else(|| body.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if access_token.is_some() {
        let local_cache = save_hosted_session(
            access_token,
            refresh_token,
            user_id.clone(),
            Some(email.to_string()),
            vault_id,
        );
        return Ok(json!({
            "ok": true,
            "action": "registered_and_logged_in",
            "email": email,
            "userId": user_id,
            "vaultId": vault_id.unwrap_or("default"),
            "localVaultCache": local_cache,
            "autoSync": true,
            "next": ["vault sync", "vault ls", "vault login --mfa-enroll"]
        }));
    }

    // If the user already confirmed their account earlier, password login will succeed immediately
    if let Ok((tok, refr, uid)) = try_password_login(clean_url, anon_key, email, password) {
        let final_uid = uid.or(user_id);
        let local_cache = save_hosted_session(
            Some(tok),
            refr,
            final_uid.clone(),
            Some(email.to_string()),
            vault_id,
        );
        return Ok(json!({
            "ok": true,
            "action": "registered_and_logged_in",
            "email": email,
            "userId": final_uid,
            "vaultId": vault_id.unwrap_or("default"),
            "localVaultCache": local_cache,
            "autoSync": true,
            "next": ["vault sync", "vault ls", "vault login --mfa-enroll"]
        }));
    }

    // Wait for the user to click the confirmation link (serving localhost:3000 + polling)
    if let Some((tok, refr, uid)) =
        wait_for_email_confirmation(clean_url, anon_key, email, password)
    {
        let final_uid = uid.or(user_id);
        let local_cache = save_hosted_session(
            Some(tok),
            refr,
            final_uid.clone(),
            Some(email.to_string()),
            vault_id,
        );
        return Ok(json!({
            "ok": true,
            "action": "registered_and_logged_in",
            "email": email,
            "userId": final_uid,
            "vaultId": vault_id.unwrap_or("default"),
            "localVaultCache": local_cache,
            "autoSync": true,
            "next": ["vault sync", "vault ls", "vault login --mfa-enroll"]
        }));
    }

    Ok(json!({
        "ok": true,
        "status": "awaiting_email_confirmation",
        "email": email,
        "message": format!("Confirmation email sent to {email}. Once confirmed, run `vault login --email {email}`.")
    }))
}

/// Log in to the official Hosted Cloud with email + password (+ optional TOTP MFA code).
pub fn login_hosted(
    email: Option<&str>,
    password: Option<&str>,
    mfa_code: Option<&str>,
    token: Option<&str>,
    vault_id: Option<&str>,
) -> Result<Value, AxiError> {
    let clean_url = HOSTED_SUPABASE_URL;
    let anon_key = HOSTED_SUPABASE_ANON_KEY;

    let mut final_token = token.map(|s| s.to_string()).or_else(env_api_key);
    let mut final_refresh: Option<String> = None;
    let mut final_user_id: Option<String> = None;
    let mut final_email: Option<String> = email.map(|s| s.to_string());
    let mut final_vault_id: String = vault_id.unwrap_or("default").to_string();
    let mut final_scope: String = "write".to_string();

    if let Some(ref tok) = final_token.clone() {
        if tok.starts_with("vlt_") {
            let (new_access, refr, uid, em, vid, sc) = exchange_api_key(tok).map_err(|msg| {
                AxiError::new(
                    format!("API key login failed: {msg}"),
                    "AUTH_ERROR",
                    vec![
                        "Verify your vlt_... API key or generate a new one with `vault token create`"
                            .to_string(),
                    ],
                )
            })?;
            final_token = Some(new_access);
            final_refresh = refr;
            final_user_id = uid;
            if em.is_some() {
                final_email = em;
            }
            if vault_id.is_none() {
                final_vault_id = vid;
            }
            final_scope = sc;
        }
    }

    if final_token.is_none() {
        let (Some(em), Some(pw)) = (email, password) else {
            return Err(AxiError::new(
                "Missing email or password",
                "VALIDATION_ERROR",
                vec![
                    "Usage: vault login --email <email> [--mfa-code <6-digit-totp>]".to_string(),
                    "Headless container? Set VAULT_API_KEY=vlt_... (created via `vault token create`)"
                        .to_string(),
                    "New user? Run `vault register --email <email>`".to_string(),
                ],
            ));
        };
        match try_password_login(clean_url, anon_key, em, pw) {
            Ok((tok, refr, uid)) => {
                final_token = Some(tok);
                final_refresh = refr;
                final_user_id = uid;
            }
            Err(msg) => {
                if msg.to_lowercase().contains("not confirmed") {
                    if let Some((tok, refr, uid)) =
                        wait_for_email_confirmation(clean_url, anon_key, em, pw)
                    {
                        final_token = Some(tok);
                        final_refresh = refr;
                        final_user_id = uid;
                    } else {
                        return Err(AxiError::new(
                            format!("Login failed: {msg}"),
                            "AUTH_ERROR",
                            vec![
                                "Check your email inbox and click the confirmation link, then run `vault login` again".to_string(),
                            ],
                        ));
                    }
                } else {
                    return Err(AxiError::new(
                        format!("Login failed: {msg}"),
                        "AUTH_ERROR",
                        vec![
                            "Verify your email and password, or register first with `vault register --email <email>`".to_string(),
                        ],
                    ));
                }
            }
        }

        // If user provided an MFA TOTP code, elevate session to AAL2
        if let (Some(code), Some(ref aal1_token)) = (mfa_code, &final_token) {
            let (aal2_access, aal2_refresh) =
                verify_mfa_totp(clean_url, anon_key, aal1_token, code)?;
            final_token = Some(aal2_access);
            final_refresh = Some(aal2_refresh);
        }
    }

    let local_cache = save_hosted_session_with_scope(
        final_token,
        final_refresh,
        final_user_id.clone(),
        final_email.clone(),
        Some(&final_vault_id),
        Some(&final_scope),
    );

    Ok(json!({
        "ok": true,
        "backend": "vault-cloud",
        "email": final_email,
        "userId": final_user_id,
        "vaultId": final_vault_id,
        "scope": final_scope,
        "localVaultCache": local_cache,
        "autoSync": true,
        "next": ["vault sync", "vault ls", "vault token create --name agent"]
    }))
}

/// Create, list, or revoke long-lived `vlt_...` API keys for headless containers and agents.
pub fn manage_api_tokens(
    subaction: &str,
    name: Option<&str>,
    vault_id: Option<&str>,
    scope: Option<&str>,
    target: Option<&str>,
) -> Result<Value, AxiError> {
    let cfg = load_config();
    let (mut bearer, refresh_tok, cfg_vault_id) = match cfg.remote {
        Some(RemoteBackendConfig::Cloud {
            access_token: Some(tok),
            refresh_token,
            vault_id: vid,
            ..
        }) => (tok, refresh_token, vid),
        _ => {
            return Err(AxiError::new(
                "Not logged into Vault Cloud",
                "AUTH_ERROR",
                vec!["Run `vault login --email <email>` first".to_string()],
            ));
        }
    };

    if bearer.starts_with("vlt_") {
        if let Ok((new_access, _, _, _, _, _)) = exchange_api_key(&bearer) {
            bearer = new_access;
        }
    }

    let url = format!("{HOSTED_SUPABASE_URL}/functions/v1/token-exchange");
    let effective_vault_id = vault_id
        .map(|s| s.to_string())
        .or(cfg_vault_id)
        .unwrap_or_else(|| "default".to_string());
    let effective_scope = scope.unwrap_or("write").to_lowercase();
    if effective_scope != "read" && effective_scope != "write" {
        return Err(AxiError::new(
            format!("Invalid token scope \"{effective_scope}\" (use 'read' or 'write')"),
            "VALIDATION_ERROR",
            vec!["Usage: vault token create [--name <label>] [--vault-id <id>] [--scope read|write] [--read-only]".to_string()],
        ));
    }

    let payload = match subaction {
        "create" => json!({
            "action": "create",
            "name": name.unwrap_or("agent"),
            "vault_id": effective_vault_id,
            "scope": effective_scope,
        }),
        "list" => json!({ "action": "list" }),
        "revoke" => {
            let Some(t) = target.filter(|s| !s.trim().is_empty()) else {
                return Err(AxiError::new(
                    "Missing API key id or prefix to revoke",
                    "VALIDATION_ERROR",
                    vec![
                        "Usage: vault token revoke <id-or-prefix>".to_string(),
                        "Run `vault token list` to see active keys".to_string(),
                    ],
                ));
            };
            json!({
                "action": "revoke",
                "target": t,
            })
        }
        other => {
            return Err(AxiError::new(
                format!("Unknown token subcommand: {other} (use 'create', 'list', or 'revoke')"),
                "VALIDATION_ERROR",
                vec![
                    "vault token create [--name <label>] [--vault-id <id>] [--scope read|write] [--read-only]".to_string(),
                    "vault token list".to_string(),
                    "vault token revoke <id-or-prefix>".to_string(),
                ],
            ));
        }
    };

    let mut resp = ureq::post(&url)
        .set("apikey", HOSTED_SUPABASE_ANON_KEY)
        .set("Authorization", &format!("Bearer {bearer}"))
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string());

    if resp.is_err() {
        if let Some(rt) = refresh_tok.as_deref() {
            if let Some((new_access, _)) = try_refresh_hosted_token(rt) {
                bearer = new_access;
                resp = ureq::post(&url)
                    .set("apikey", HOSTED_SUPABASE_ANON_KEY)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .set("Content-Type", "application/json")
                    .send_string(&payload.to_string());
            }
        }
    }

    let r = resp.map_err(|e| {
        let msg = extract_auth_error(e);
        AxiError::new(
            format!("Token operation failed: {msg}"),
            "AUTH_ERROR",
            vec!["Run `vault login --email <email>` to refresh your session".to_string()],
        )
    })?;

    let body: Value =
        serde_json::from_str(&r.into_string().unwrap_or_default()).unwrap_or_else(|_| json!({}));

    if subaction == "create" {
        let api_key = body.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
        let returned_scope = body
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or(&effective_scope);
        return Ok(json!({
            "ok": true,
            "action": "token_created",
            "id": body.get("id"),
            "name": body.get("name"),
            "vaultId": body.get("vault_id").unwrap_or(&json!(effective_vault_id)),
            "scope": returned_scope,
            "keyPrefix": body.get("key_prefix"),
            "apiKey": api_key,
            "usage": format!("export VAULT_API_KEY=\"{api_key}\""),
            "note": "Save this key now; it will not be shown again. Any container with VAULT_API_KEY set will silently authenticate on first command."
        }));
    }

    Ok(body)
}

fn save_hosted_session(
    access_token: Option<String>,
    refresh_token: Option<String>,
    user_id: Option<String>,
    email: Option<String>,
    vault_id: Option<&str>,
) -> Option<String> {
    save_hosted_session_with_scope(
        access_token,
        refresh_token,
        user_id,
        email,
        vault_id,
        Some("write"),
    )
}

fn save_hosted_session_with_scope(
    access_token: Option<String>,
    refresh_token: Option<String>,
    user_id: Option<String>,
    email: Option<String>,
    vault_id: Option<&str>,
    scope: Option<&str>,
) -> Option<String> {
    let mut cfg = load_config();
    if cfg.vault_path.is_none() {
        let default_vault = crate::config::config_dir().join("cloud-vault");
        let _ = fs::create_dir_all(&default_vault);
        cfg.vault_path = Some(default_vault.to_string_lossy().to_string());
    }
    cfg.auto_sync = Some(true);
    cfg.remote = Some(RemoteBackendConfig::Cloud {
        access_token,
        refresh_token,
        user_id,
        email,
        vault_id: Some(vault_id.unwrap_or("default").to_string()),
        scope: Some(scope.unwrap_or("write").to_string()),
    });
    save_config(&cfg);
    cfg.vault_path
}

/// Enroll or verify TOTP Multi-Factor Authentication (MFA) on the hosted account.
pub fn handle_mfa(action: &str, code: Option<&str>) -> Result<Value, AxiError> {
    let cfg = load_config();
    let Some(RemoteBackendConfig::Cloud {
        access_token: Some(token),
        ..
    }) = cfg.remote
    else {
        return Err(AxiError::new(
            "Not logged into Vault Cloud",
            "AUTH_ERROR",
            vec!["Run `vault login --email <e> --password <p>` first".to_string()],
        ));
    };

    let clean_url = HOSTED_SUPABASE_URL;
    let anon_key = HOSTED_SUPABASE_ANON_KEY;
    match action {
        "enroll" => {
            let enroll_url = format!("{clean_url}/auth/v1/factors");
            let payload = json!({
                "factor_type": "totp",
                "friendly_name": "vault-axi-cli"
            });
            let resp = ureq::post(&enroll_url)
                .set("apikey", anon_key)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(&payload.to_string())
                .map_err(|e| {
                    AxiError::new(format!("MFA enroll failed: {e}"), "AUTH_ERROR", vec![])
                })?;
            let body: Value = serde_json::from_str(&resp.into_string().unwrap_or_default())
                .unwrap_or_else(|_| json!({}));
            Ok(json!({
                "ok": true,
                "action": "mfa_enrolled",
                "factorId": body.get("id"),
                "totpSecret": body.get("totp").and_then(|t| t.get("secret")),
                "totpUri": body.get("totp").and_then(|t| t.get("uri")),
                "next": ["Add totpSecret to your authenticator app, then verify with `vault login --mfa-verify <6-digit-code>`"]
            }))
        }
        "verify" => {
            let Some(c) = code else {
                return Err(AxiError::new(
                    "Missing 6-digit TOTP code",
                    "VALIDATION_ERROR",
                    vec!["Usage: vault login --mfa-verify <6-digit-code>".to_string()],
                ));
            };
            let (new_access, new_refresh) = verify_mfa_totp(clean_url, anon_key, &token, c)?;
            let mut updated_cfg = load_config();
            if let Some(RemoteBackendConfig::Cloud {
                access_token,
                refresh_token,
                ..
            }) = &mut updated_cfg.remote
            {
                *access_token = Some(new_access);
                *refresh_token = Some(new_refresh);
                save_config(&updated_cfg);
            }
            Ok(json!({
                "ok": true,
                "action": "mfa_verified_aal2",
                "assuranceLevel": "aal2"
            }))
        }
        _ => Err(AxiError::new(
            format!("Unknown MFA action: {action} (use 'enroll' or 'verify')"),
            "VALIDATION_ERROR",
            vec![],
        )),
    }
}

fn verify_mfa_totp(
    clean_url: &str,
    anon_key: &str,
    token: &str,
    code: &str,
) -> Result<(String, String), AxiError> {
    let user_url = format!("{clean_url}/auth/v1/user");
    let user_resp = ureq::get(&user_url)
        .set("apikey", anon_key)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| {
            AxiError::new(
                format!("Failed to fetch MFA factors: {e}"),
                "AUTH_ERROR",
                vec![],
            )
        })?;
    let user_body: Value = serde_json::from_str(&user_resp.into_string().unwrap_or_default())
        .unwrap_or_else(|_| json!({}));
    let factor_id = user_body
        .get("factors")
        .and_then(|f| f.as_array())
        .and_then(|arr| arr.first())
        .and_then(|f| f.get("id"))
        .and_then(|id| id.as_str())
        .ok_or_else(|| {
            AxiError::new(
                "No TOTP MFA factor found on account",
                "AUTH_ERROR",
                vec!["Enroll first with `vault login --mfa-enroll`".to_string()],
            )
        })?;

    let challenge_url = format!("{clean_url}/auth/v1/factors/{factor_id}/challenge");
    let ch_resp = ureq::post(&challenge_url)
        .set("apikey", anon_key)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string("{}")
        .map_err(|e| AxiError::new(format!("MFA challenge failed: {e}"), "AUTH_ERROR", vec![]))?;
    let ch_body: Value = serde_json::from_str(&ch_resp.into_string().unwrap_or_default())
        .unwrap_or_else(|_| json!({}));
    let challenge_id = ch_body
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AxiError::new("Invalid MFA challenge response", "AUTH_ERROR", vec![]))?;

    let verify_url = format!("{clean_url}/auth/v1/factors/{factor_id}/verify");
    let v_payload = json!({
        "challenge_id": challenge_id,
        "code": code
    });
    let v_resp = ureq::post(&verify_url)
        .set("apikey", anon_key)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(&v_payload.to_string())
        .map_err(|e| {
            AxiError::new(
                format!("MFA TOTP verification failed: {e}"),
                "AUTH_ERROR",
                vec![],
            )
        })?;
    let v_body: Value = serde_json::from_str(&v_resp.into_string().unwrap_or_default())
        .unwrap_or_else(|_| json!({}));
    let new_access = v_body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            AxiError::new(
                "MFA verify did not return access_token",
                "AUTH_ERROR",
                vec![],
            )
        })?
        .to_string();
    let new_refresh = v_body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((new_access, new_refresh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_db_at;

    #[test]
    fn urlencoding_and_sha1_and_sync_table_helpers() {
        assert_eq!(
            urlencoding_simple("plans/my task#1?.md"),
            "plans/my%20task%231%3F.md"
        );
        assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        let db = open_db_at(":memory:").unwrap();
        ensure_sync_table(&db);
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // VAULT_AXI_NO_SYNC=1 short-circuits auto_pull_if_needed and auto_push_if_needed
        std::env::set_var("VAULT_AXI_NO_SYNC", "1");
        assert!(auto_pull_if_needed(&db, ".", true).is_none());
        assert!(auto_push_if_needed(&db, ".").is_none());

        // login_hosted without email/password or token returns VALIDATION_ERROR
        let err = login_hosted(Some("user@example.com"), None, None, None, None).unwrap_err();
        assert_eq!(err.code, "VALIDATION_ERROR");
        let err2 = login_hosted(None, Some("secret"), None, None, None).unwrap_err();
        assert_eq!(err2.code, "VALIDATION_ERROR");

        // parse_query_param and local confirmation server HTML/callback handling
        assert_eq!(
            parse_query_param("access_token=abc&refresh_token=def", "access_token"),
            Some("abc".to_string())
        );
        assert_eq!(
            parse_query_param("access_token=abc&refresh_token=def", "refresh_token"),
            Some("def".to_string())
        );
        assert_eq!(
            parse_query_param("access_token=&other=1", "access_token"),
            None
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // 1. Landing page request `GET /` returns HTML confirmation page
        let client_thread = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut client = std::net::TcpStream::connect(addr).unwrap();
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut resp = String::new();
            client.read_to_string(&mut resp).unwrap();
            resp
        });
        let (srv_stream, _) = listener.accept().unwrap();
        assert!(serve_confirm_request(srv_stream).is_none());
        let html_resp = client_thread.join().unwrap();
        assert!(html_resp.contains("200 OK"));
        assert!(html_resp.contains("Email Confirmed"));

        // 2. Callback request `GET /auth-callback?access_token=...` extracts tokens
        let client_thread2 = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut client = std::net::TcpStream::connect(addr).unwrap();
            client
                .write_all(
                    b"GET /auth-callback?access_token=jwt123&refresh_token=ref456 HTTP/1.1\r\nHost: localhost\r\n\r\n",
                )
                .unwrap();
            let mut resp = String::new();
            client.read_to_string(&mut resp).unwrap();
            resp
        });
        let (srv_stream2, _) = listener.accept().unwrap();
        let extracted = serve_confirm_request(srv_stream2);
        assert_eq!(
            extracted,
            Some(("jwt123".to_string(), Some("ref456".to_string())))
        );
        let json_resp = client_thread2.join().unwrap();
        assert!(json_resp.contains(r#"{"ok":true}"#));
    }

    #[test]
    fn concurrent_edit_conflict_saves_copy_and_warns() {
        let base = "# Plan\n\n## Objective\nInitial objective\n\n## Build Log\n- `2026-10-31 10:00` — Started\n";
        let local_sec_edit = "# Plan\n\n## Objective\nUpdated objective on Machine B\n\n## Build Log\n- `2026-10-31 10:00` — Started\n";

        let remote_conflicting_sec = "# Plan\n\n## Objective\nConflicting objective on Machine A\n\n## Build Log\n- `2026-10-31 10:00` — Started\n";

        // resolve_concurrent_edit saves a conflict copy and records a warning for the last writer
        let tmp = std::env::temp_dir().join(format!(
            "vault_axi_sync_conflict_test_{}_{}",
            std::process::id(),
            now_ms()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let root = tmp.to_string_lossy().to_string();
        let db = open_db_at(":memory:").unwrap();
        ensure_sync_table(&db);

        let note_rel = "plans/roadmap.md";
        let note_abs = tmp.join("plans").join("roadmap.md");
        fs::create_dir_all(note_abs.parent().unwrap()).unwrap();
        fs::write(&note_abs, local_sec_edit).unwrap();

        let full_remote = RemoteNoteFull {
            path: note_rel.to_string(),
            content: remote_conflicting_sec.to_string(),
            hash: sha1_hex(remote_conflicting_sec),
            mtime_ms: 1700005000000,
            deleted: false,
        };
        let mut synced_state = HashMap::new();
        let mut remote_map = HashMap::new();
        let mut summary = SyncSummary::default();

        resolve_concurrent_edit(
            &db,
            &root,
            note_rel,
            &note_abs,
            local_sec_edit,
            &full_remote,
            base,
            1700005000000,
            &mut synced_state,
            &mut remote_map,
            &mut summary,
        );

        assert_eq!(summary.conflicts.len(), 1);
        assert_eq!(summary.conflicts[0].resolution, "conflict_copy_saved");
        let conflict_copy_rel = summary.conflicts[0].conflict_copy.as_ref().unwrap();
        assert_eq!(conflict_copy_rel, "plans/roadmap.conflict-1700005000.md");

        let conflict_copy_abs = tmp.join(conflict_copy_rel);
        assert!(conflict_copy_abs.exists());
        assert_eq!(
            fs::read_to_string(&conflict_copy_abs).unwrap(),
            local_sec_edit
        );
        assert_eq!(
            fs::read_to_string(&note_abs).unwrap(),
            remote_conflicting_sec
        );

        // Verify Ctx::attach_sync_warnings surfaces the warning and conflict list to the last writer
        let mut ctx = crate::context::Ctx::with_vault_and_db(&root, db);
        ctx.sync_conflicts.extend(summary.conflicts);
        let mut cmd_out = json!({ "ok": true, "changed": true, "file": note_rel });
        ctx.attach_sync_warnings(&mut cmd_out);
        assert!(cmd_out["warning"]
            .as_str()
            .unwrap()
            .contains("Concurrent edit conflict on 'plans/roadmap.md'"));
        assert_eq!(cmd_out["conflicts"].as_array().unwrap().len(), 1);
        let _ = fs::remove_dir_all(&tmp);
    }
}
