use crate::axi::AxiError;
use crate::config::{
    load_config, save_config, RemoteBackendConfig, HOSTED_SUPABASE_ANON_KEY, HOSTED_SUPABASE_URL,
};
use crate::vault::{file_stat, list_markdown_files, resolve_in_vault};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const SYNC_COOLDOWN_MS: i64 = 15_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncSummary {
    pub backend: String,
    pub pulled: usize,
    pub pushed: usize,
    pub deleted_remote: usize,
    pub deleted_local: usize,
    pub message: Option<String>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn sha1_hex(text: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn ensure_sync_table(db: &Connection) {
    let _ = db.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS sync_state (
          path          TEXT PRIMARY KEY,
          remote_hash   TEXT NOT NULL,
          remote_mtime  INTEGER NOT NULL,
          synced_at     INTEGER NOT NULL
        );
        "#,
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
            false,
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
            false,
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
                "  vault setup --remote s3://<bucket>/<prefix> --endpoint <https://...>".to_string(),
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
        let dirty = !String::from_utf8_lossy(&status_out.stdout).trim().is_empty();
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
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteNoteMeta {
    path: String,
    hash: String,
    mtime_ms: i64,
    #[serde(default)]
    deleted: bool,
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

    let manifest_url = format!(
        "{clean_url}/rest/v1/vault_notes?vault_id=eq.{vault_id}&select=path,hash,mtime_ms,deleted"
    );
    let mut resp = ureq::get(&manifest_url)
        .set("apikey", anon_key)
        .set("Authorization", &format!("Bearer {bearer}"))
        .call();

    // Automatically refresh expired JWT if refresh_token is available
    if resp.is_err() {
        if let Some(rt) = refresh_token {
            if let Some((new_access, _)) = try_refresh_hosted_token(rt) {
                bearer = new_access;
                resp = ureq::get(&manifest_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .call();
            }
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
    let remote_manifest: Vec<RemoteNoteMeta> = serde_json::from_str(&body_str).unwrap_or_default();
    let remote_map: HashMap<String, RemoteNoteMeta> = remote_manifest
        .into_iter()
        .map(|m| (m.path.clone(), m))
        .collect();

    let mut synced_state: HashMap<String, (String, i64)> = HashMap::new();
    if let Ok(mut stmt) = db.prepare("SELECT path, remote_hash, remote_mtime FROM sync_state") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        }) {
            for (p, h, m) in rows.flatten() {
                synced_state.insert(p, (h, m));
            }
        }
    }

    let mut summary = SyncSummary {
        backend: "vault-cloud".to_string(),
        ..Default::default()
    };
    let now = now_ms();

    // Pull changed remote files
    if do_pull {
        for (r_path, r_meta) in &remote_map {
            let Ok(abs) = resolve_in_vault(vault_root, r_path) else {
                continue;
            };
            let abs_p = Path::new(&abs);
            let local_exists = abs_p.exists();
            let local_hash = if local_exists {
                fs::read_to_string(abs_p).ok().map(|s| sha1_hex(&s))
            } else {
                None
            };

            if r_meta.deleted {
                if local_exists {
                    let _ = fs::remove_file(abs_p);
                    summary.deleted_local += 1;
                }
                let _ = db.execute("DELETE FROM sync_state WHERE path = ?1", params![r_path]);
                continue;
            }

            let needs_download = match &local_hash {
                None => true,
                Some(lh) if lh != &r_meta.hash => match synced_state.get(r_path) {
                    Some((last_h, _)) => last_h != &r_meta.hash,
                    None => true,
                },
                _ => false,
            };

            if needs_download {
                let fetch_url = format!(
                    "{clean_url}/rest/v1/vault_notes?vault_id=eq.{vault_id}&path=eq.{}&select=path,content,hash,mtime_ms,deleted",
                    urlencoding_simple(r_path)
                );
                if let Ok(r) = ureq::get(&fetch_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .call()
                {
                    if let Ok(s) = r.into_string() {
                        if let Ok(mut list) = serde_json::from_str::<Vec<RemoteNoteFull>>(&s) {
                            if let Some(full) = list.pop() {
                                if let Some(parent) = abs_p.parent() {
                                    let _ = fs::create_dir_all(parent);
                                }
                                if fs::write(abs_p, &full.content).is_ok() {
                                    let _ = db.execute(
                                        "INSERT INTO sync_state(path, remote_hash, remote_mtime, synced_at) VALUES (?1, ?2, ?3, ?4)
                                         ON CONFLICT(path) DO UPDATE SET remote_hash=excluded.remote_hash, remote_mtime=excluded.remote_mtime, synced_at=excluded.synced_at",
                                        params![r_path, full.hash, full.mtime_ms, now],
                                    );
                                    summary.pulled += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Push local additions, edits, and deletions
    if do_push {
        let local_paths = list_markdown_files(vault_root);
        let local_set: HashSet<String> = local_paths.iter().cloned().collect();

        for l_path in &local_paths {
            let Ok(abs) = resolve_in_vault(vault_root, l_path) else {
                continue;
            };
            let Ok(content) = fs::read_to_string(&abs) else {
                continue;
            };
            let l_hash = sha1_hex(&content);
            let l_mtime = file_stat(&abs).map(|s| s.mtime_ms).unwrap_or(now);

            let needs_upload = match remote_map.get(l_path) {
                None => true,
                Some(rm) => rm.deleted || rm.hash != l_hash,
            };

            if needs_upload {
                let upsert_url =
                    format!("{clean_url}/rest/v1/vault_notes?on_conflict=user_id,vault_id,path");
                let mut payload = json!({
                    "vault_id": vault_id,
                    "path": l_path,
                    "content": content,
                    "hash": l_hash,
                    "mtime_ms": l_mtime,
                    "deleted": false,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                });
                if let Some(uid) = user_id {
                    payload["user_id"] = json!(uid);
                }
                if let Ok(r) = ureq::post(&upsert_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .set("Content-Type", "application/json")
                    .set("Prefer", "resolution=merge-duplicates")
                    .send_string(&payload.to_string())
                {
                    if r.status() < 300 {
                        let _ = db.execute(
                            "INSERT INTO sync_state(path, remote_hash, remote_mtime, synced_at) VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(path) DO UPDATE SET remote_hash=excluded.remote_hash, remote_mtime=excluded.remote_mtime, synced_at=excluded.synced_at",
                            params![l_path, l_hash, l_mtime, now],
                        );
                        summary.pushed += 1;
                    }
                }
            }
        }

        for (synced_path, _) in &synced_state {
            if !local_set.contains(synced_path) {
                let upsert_url =
                    format!("{clean_url}/rest/v1/vault_notes?on_conflict=user_id,vault_id,path");
                let mut payload = json!({
                    "vault_id": vault_id,
                    "path": synced_path,
                    "content": "",
                    "hash": "",
                    "mtime_ms": now,
                    "deleted": true,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                });
                if let Some(uid) = user_id {
                    payload["user_id"] = json!(uid);
                }
                let _ = ureq::post(&upsert_url)
                    .set("apikey", anon_key)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .set("Content-Type", "application/json")
                    .set("Prefer", "resolution=merge-duplicates")
                    .send_string(&payload.to_string());
                let _ =
                    db.execute("DELETE FROM sync_state WHERE path = ?1", params![synced_path]);
                summary.deleted_remote += 1;
            }
        }
    }

    summary.message = Some(format!(
        "Vault Cloud sync complete (pulled {}, pushed {})",
        summary.pulled, summary.pushed
    ));
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

    let mut summary = SyncSummary {
        backend: "s3".to_string(),
        ..Default::default()
    };
    let now = now_ms();

    if do_pull {
        for (r_path, r_hash) in &remote_hashes {
            let Ok(abs) = resolve_in_vault(vault_root, r_path) else {
                continue;
            };
            let abs_p = Path::new(&abs);
            let local_hash = fs::read_to_string(abs_p).ok().map(|s| sha1_hex(&s));
            if local_hash.as_ref() != Some(r_hash) {
                let file_url = format!("{base}/{r_path}");
                if let Ok(resp) = ureq::get(&file_url).call() {
                    if let Ok(content) = resp.into_string() {
                        if let Some(parent) = abs_p.parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        if fs::write(abs_p, &content).is_ok() {
                            let _ = db.execute(
                                "INSERT INTO sync_state(path, remote_hash, remote_mtime, synced_at) VALUES (?1, ?2, ?3, ?4)
                                 ON CONFLICT(path) DO UPDATE SET remote_hash=excluded.remote_hash, remote_mtime=excluded.remote_mtime, synced_at=excluded.synced_at",
                                params![r_path, r_hash, now, now],
                            );
                            summary.pulled += 1;
                        }
                    }
                }
            }
        }
    }

    if do_push {
        let mut manifest_changed = false;
        for l_path in list_markdown_files(vault_root) {
            let Ok(abs) = resolve_in_vault(vault_root, &l_path) else {
                continue;
            };
            let Ok(content) = fs::read_to_string(&abs) else {
                continue;
            };
            let l_hash = sha1_hex(&content);
            if remote_hashes.get(&l_path) != Some(&l_hash) {
                let file_url = format!("{base}/{l_path}");
                if let Ok(r) = ureq::put(&file_url)
                    .set("Content-Type", "text/markdown; charset=utf-8")
                    .send_string(&content)
                {
                    if r.status() < 300 {
                        remote_hashes.insert(l_path.clone(), l_hash.clone());
                        manifest_changed = true;
                        summary.pushed += 1;
                    }
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
    let body: Value = serde_json::from_str(&resp.into_string().unwrap_or_default())
        .unwrap_or_else(|_| json!({}));
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
    eprintln!("==> Waiting for you to click the confirmation link in your email... (Ctrl+C to skip)");

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
        Ok(r) => serde_json::from_str(&r.into_string().unwrap_or_default())
            .unwrap_or_else(|_| json!({})),
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
                vec![
                    "Ensure the email is valid and password is at least 6 characters".to_string(),
                ],
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

    let mut final_token = token.map(|s| s.to_string());
    let mut final_refresh: Option<String> = None;
    let mut final_user_id: Option<String> = None;

    if final_token.is_none() {
        let (Some(em), Some(pw)) = (email, password) else {
            return Err(AxiError::new(
                "Missing email or password",
                "VALIDATION_ERROR",
                vec![
                    "Usage: vault login --email <email> [--mfa-code <6-digit-totp>]".to_string(),
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

    let local_cache = save_hosted_session(
        final_token,
        final_refresh,
        final_user_id.clone(),
        email.map(|s| s.to_string()),
        vault_id,
    );

    Ok(json!({
        "ok": true,
        "backend": "vault-cloud",
        "email": email,
        "userId": final_user_id,
        "vaultId": vault_id.unwrap_or("default"),
        "localVaultCache": local_cache,
        "autoSync": true,
        "next": ["vault sync", "vault ls"]
    }))
}

fn save_hosted_session(
    access_token: Option<String>,
    refresh_token: Option<String>,
    user_id: Option<String>,
    email: Option<String>,
    vault_id: Option<&str>,
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
                .map_err(|e| AxiError::new(format!("MFA enroll failed: {e}"), "AUTH_ERROR", vec![]))?;
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
        .map_err(|e| AxiError::new(format!("Failed to fetch MFA factors: {e}"), "AUTH_ERROR", vec![]))?;
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
        .map_err(|e| AxiError::new(format!("MFA TOTP verification failed: {e}"), "AUTH_ERROR", vec![]))?;
    let v_body: Value = serde_json::from_str(&v_resp.into_string().unwrap_or_default())
        .unwrap_or_else(|_| json!({}));
    let new_access = v_body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AxiError::new("MFA verify did not return access_token", "AUTH_ERROR", vec![]))?
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
        assert_eq!(
            sha1_hex("abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
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
        assert_eq!(parse_query_param("access_token=&other=1", "access_token"), None);

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
}
