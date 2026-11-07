use crate::args::{flag_bool, flag_int, flag_string, parse_args};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::crypto::{decrypt_at_rest, encrypt_at_rest};
use crate::render::truncate_text;
use crate::vault::{normalize_note_path, resolve_in_vault};
use chrono::{TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use sha1::{Digest, Sha1};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_READ_BUDGET: usize = 1200;

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

fn format_ts(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ms.to_string())
}

/// Record a version snapshot of `note_path` in `note_versions` (encrypted at rest).
/// Deduplicates consecutive identical hashes so repeated no-op saves never bloat history.
/// Does NOT touch `blocks` or `blocks_fts`, keeping search completely unaffected by history.
pub fn record_note_version(db: &Connection, note_path: &str, content: &str, source: &str) -> i64 {
    let hash = sha1_hex(content);
    let latest: Option<(i64, String)> = db
        .query_row(
            "SELECT version_num, hash FROM note_versions WHERE path = ?1 ORDER BY version_num DESC LIMIT 1",
            params![note_path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .ok()
        .flatten();

    if let Some((v_num, ref last_hash)) = latest {
        if last_hash == &hash {
            return v_num;
        }
    }

    let next_ver = latest.map(|(v, _)| v + 1).unwrap_or(1);
    let enc_content = encrypt_at_rest(content);
    let size = content.len() as i64;
    let _ = db.execute(
        "INSERT INTO note_versions(path, version_num, hash, size, content, created_at, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            note_path,
            next_ver,
            hash,
            size,
            enc_content,
            now_ms(),
            source
        ],
    );
    next_ver
}

/// Snapshot the existing disk file (if any) before a mutation, and then record the new content.
pub fn record_edit_transition(
    db: &Connection,
    note_path: &str,
    old_content: Option<&str>,
    new_content: &str,
    source: &str,
) -> i64 {
    if let Some(old) = old_content {
        record_note_version(db, note_path, old, "snapshot");
    }
    record_note_version(db, note_path, new_content, source)
}

/// Migrate version history when a note is moved/renamed (`vault move <old> <new>`).
pub fn migrate_version_paths(db: &Connection, old_path: &str, new_path: &str) {
    if old_path == new_path {
        return;
    }
    let max_dest_ver: i64 = db
        .query_row(
            "SELECT COALESCE(MAX(version_num), 0) FROM note_versions WHERE path = ?1",
            params![new_path],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if max_dest_ver == 0 {
        let _ = db.execute(
            "UPDATE note_versions SET path = ?2 WHERE path = ?1",
            params![old_path, new_path],
        );
    } else {
        let _ = db.execute(
            "UPDATE note_versions SET path = ?2, version_num = version_num + ?3 WHERE path = ?1",
            params![old_path, new_path, max_dest_ver],
        );
    }
}

fn compute_simple_diff(old_text: &str, current_text: &str) -> (usize, usize, Vec<String>) {
    let old_lines: Vec<&str> = old_text.lines().collect();
    let cur_lines: Vec<&str> = current_text.lines().collect();
    let old_set: HashSet<&str> = old_lines.iter().copied().collect();
    let cur_set: HashSet<&str> = cur_lines.iter().copied().collect();

    let mut diff_preview = Vec::new();
    let mut removed = 0usize;
    let mut added = 0usize;

    for l in &old_lines {
        if !cur_set.contains(l) {
            removed += 1;
            if diff_preview.len() < 20 {
                diff_preview.push(format!("- {l}"));
            }
        }
    }
    for l in &cur_lines {
        if !old_set.contains(l) {
            added += 1;
            if diff_preview.len() < 20 {
                diff_preview.push(format!("+ {l}"));
            }
        }
    }
    (added, removed, diff_preview)
}

pub fn history_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing <file> argument",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault history <file> [--version <n>] [--diff <n>] [--restore <n>] [--full]"
                    .to_string(),
            ],
        ));
    };

    let note_path = normalize_note_path(rel);
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let current_disk = fs::read_to_string(&abs).ok();

    let db = ctx.db()?;
    if let Some(ref cur) = current_disk {
        record_note_version(db, &note_path, cur, "current");
    }

    // Handle `--restore <v>`
    if let Some(restore_str) = flag_string(&parsed.flags, "restore") {
        let target_ver: i64 = restore_str.parse().map_err(|_| {
            AxiError::new(
                format!("Invalid version number for --restore: {restore_str}"),
                "VALIDATION_ERROR",
                vec![format!("Run `vault history {note_path}` to list version numbers")],
            )
        })?;

        let row: Option<(String, String)> = db
            .query_row(
                "SELECT content, hash FROM note_versions WHERE path = ?1 AND version_num = ?2",
                params![&note_path, target_ver],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()
            .flatten();

        let Some((enc_content, ver_hash)) = row else {
            return Err(AxiError::new(
                format!("Version {target_ver} not found for {note_path}"),
                "NOT_FOUND",
                vec![format!(
                    "Run `vault history {note_path}` to see available versions"
                )],
            ));
        };

        let restored_text = decrypt_at_rest(&enc_content)
            .map_err(|e| AxiError::new(e, "CRYPTO_ERROR", vec![]))?;

        ctx.pull_before_mutation()?;
        let abs_p = Path::new(&abs);
        if let Some(parent) = abs_p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(abs_p, &restored_text).map_err(|e| {
            AxiError::new(
                format!("Failed to write restored note {note_path}: {e}"),
                "IO_ERROR",
                vec![],
            )
        })?;

        let new_ver = {
            let db = ctx.db()?;
            record_note_version(
                db,
                &note_path,
                &restored_text,
                &format!("restore:v{target_ver}"),
            )
        };
        ctx.sync_after_mutation()?;

        let mut out = json!({
            "ok": true,
            "action": "restored_version",
            "file": note_path,
            "restoredFromVersion": target_ver,
            "newVersion": new_ver,
            "hash": ver_hash,
            "chars": restored_text.chars().count(),
        });
        ctx.attach_sync_warnings(&mut out);
        return Ok(out);
    }

    // Handle `--version <v>` or `--diff <v>`
    let inspect_ver_str =
        flag_string(&parsed.flags, "version").or_else(|| flag_string(&parsed.flags, "diff"));
    if let Some(ver_str) = inspect_ver_str {
        let target_ver: i64 = ver_str.parse().map_err(|_| {
            AxiError::new(
                format!("Invalid version number: {ver_str}"),
                "VALIDATION_ERROR",
                vec![format!("Run `vault history {note_path}` to list version numbers")],
            )
        })?;
        let full = flag_bool(&parsed.flags, "full");

        let db = ctx.db()?;
        let row: Option<(String, String, i64, i64, String)> = db
            .query_row(
                "SELECT content, hash, size, created_at, source FROM note_versions WHERE path = ?1 AND version_num = ?2",
                params![&note_path, target_ver],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .ok()
            .flatten();

        let Some((enc_content, ver_hash, size, created_at, source)) = row else {
            return Err(AxiError::new(
                format!("Version {target_ver} not found for {note_path}"),
                "NOT_FOUND",
                vec![format!(
                    "Run `vault history {note_path}` to see available versions"
                )],
            ));
        };

        let ver_text = decrypt_at_rest(&enc_content)
            .map_err(|e| AxiError::new(e, "CRYPTO_ERROR", vec![]))?;

        let (shown_text, truncated) = if full {
            (ver_text.clone(), false)
        } else {
            let t = truncate_text(&ver_text, DEFAULT_READ_BUDGET);
            (t.content, t.truncated)
        };

        let mut out = Map::new();
        out.insert("file".to_string(), json!(note_path));
        out.insert("version".to_string(), json!(target_ver));
        out.insert("hash".to_string(), json!(ver_hash));
        out.insert("size".to_string(), json!(size));
        out.insert("createdAt".to_string(), json!(format_ts(created_at)));
        out.insert("source".to_string(), json!(source));
        out.insert("truncated".to_string(), json!(truncated));
        out.insert("content".to_string(), json!(shown_text));

        if let Some(ref cur) = current_disk {
            let (added, removed, diff_lines) = compute_simple_diff(&ver_text, cur);
            out.insert(
                "diffAgainstCurrent".to_string(),
                json!({
                    "linesAddedInCurrent": added,
                    "linesRemovedFromVersion": removed,
                    "preview": diff_lines,
                }),
            );
        }
        out.insert(
            "hint".to_string(),
            json!(format!(
                "Restore this version with `vault history {note_path} --restore {target_ver}`"
            )),
        );
        return Ok(Value::Object(out));
    }

    // Default: List version history for `<file>`
    let limit = flag_int(&parsed.flags, "limit", 25).max(1) as usize;
    let current_hash = current_disk.as_ref().map(|s| sha1_hex(s));
    let db = ctx.db()?;
    let mut stmt = db
        .prepare(
            "SELECT version_num, hash, size, created_at, source
             FROM note_versions
             WHERE path = ?1
             ORDER BY version_num DESC
             LIMIT ?2",
        )
        .map_err(|e| AxiError::new(format!("DB error: {e}"), "INTERNAL_ERROR", vec![]))?;

    let rows: Vec<Value> = stmt
        .query_map(params![&note_path, limit as i64], |r| {
            let v_num: i64 = r.get(0)?;
            let hash: String = r.get(1)?;
            let size: i64 = r.get(2)?;
            let created_at: i64 = r.get(3)?;
            let source: String = r.get(4)?;
            let is_current = current_hash.as_deref() == Some(hash.as_str());
            Ok(json!({
                "version": v_num,
                "hash": &hash[..hash.len().min(10)],
                "size": size,
                "createdAt": format_ts(created_at),
                "source": source,
                "current": is_current,
            }))
        })
        .map_err(|e| AxiError::new(format!("DB query error: {e}"), "INTERNAL_ERROR", vec![]))?
        .flatten()
        .collect();

    if rows.is_empty() {
        return Err(AxiError::new(
            format!("No history found for {note_path}"),
            "NOT_FOUND",
            vec!["Run `vault ls` to see existing notes".to_string()],
        ));
    }

    Ok(json!({
        "file": note_path,
        "versionsCount": rows.len(),
        "versions": rows,
        "next": [
            format!("vault history {note_path} --version <n>"),
            format!("vault history {note_path} --restore <n>")
        ]
    }))
}
