use crate::args::{flag_bool, flag_string, parse_args};
use crate::axi::AxiError;
use crate::commands::history::record_note_version;
use crate::context::Ctx;
use crate::crypto::{decrypt_at_rest, encrypt_at_rest};
use crate::vault::{file_stat, normalize_any_vault_path, normalize_note_path, resolve_in_vault};
use chrono::{TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use vault_axi_core::payload::{decode_binary, encode_binary};

pub const TRASH_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn format_ts(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ms.to_string())
}

/// Remove any trashed notes whose 7-day retention window has expired.
pub fn purge_expired_trash(db: &Connection) -> usize {
    let now = now_ms();
    db.execute("DELETE FROM trash WHERE expires_at <= ?1", params![now])
        .unwrap_or(0)
}

/// Save a file's content (encrypted at rest) into the 7-day `trash` table before unlinking it.
/// Returns the expiration timestamp in milliseconds.
pub fn move_file_to_trash(db: &Connection, rel_path: &str, abs_path: &Path) -> Option<i64> {
    purge_expired_trash(db);
    let bytes = fs::read(abs_path).ok()?;
    let now = now_ms();
    let expires_at = now + TRASH_RETENTION_MS;
    let mtime = file_stat(&abs_path.to_string_lossy())
        .map(|s| s.mtime_ms)
        .unwrap_or(now);

    let (raw_payload, is_bin) = match String::from_utf8(bytes.clone()) {
        Ok(text) => {
            if rel_path.to_ascii_lowercase().ends_with(".md") {
                record_note_version(db, rel_path, &text, "pre-delete");
            }
            (text, 0i64)
        }
        Err(_) => (encode_binary(&bytes), 1i64),
    };

    let enc = encrypt_at_rest(&raw_payload);
    let _ = db.execute(
        "INSERT INTO trash(path, content, is_binary, mtime_ms, deleted_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET
           content = excluded.content,
           is_binary = excluded.is_binary,
           mtime_ms = excluded.mtime_ms,
           deleted_at = excluded.deleted_at,
           expires_at = excluded.expires_at",
        params![rel_path, enc, is_bin, mtime, now, expires_at],
    );
    Some(expires_at)
}

/// Save raw text content into the 7-day `trash` table (used when a remote deletion is pulled).
pub fn store_text_in_trash(db: &Connection, rel_path: &str, content: &str, deleted_at_ms: i64) {
    purge_expired_trash(db);
    let now = now_ms();
    let del_ts = if deleted_at_ms > 0 {
        deleted_at_ms
    } else {
        now
    };
    let expires_at = del_ts + TRASH_RETENTION_MS;
    if expires_at <= now {
        return;
    }
    let enc = encrypt_at_rest(content);
    let _ = db.execute(
        "INSERT INTO trash(path, content, is_binary, mtime_ms, deleted_at, expires_at)
         VALUES (?1, ?2, 0, ?3, ?4, ?5)
         ON CONFLICT(path) DO UPDATE SET
           content = excluded.content,
           deleted_at = excluded.deleted_at,
           expires_at = excluded.expires_at",
        params![rel_path, enc, del_ts, del_ts, expires_at],
    );
}

/// `vault trash [--purge]` — list deleted notes recoverable within the 7-day window, or purge trash.
pub fn trash_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let _ = ctx.require_vault()?;
    let _ = ctx.pull_if_due();
    let parsed = parse_args(args);
    let purge_all = flag_bool(&parsed.flags, "purge");

    let db = ctx.db()?;
    let expired_removed = purge_expired_trash(db);

    if purge_all {
        let count = db.execute("DELETE FROM trash", []).unwrap_or(0);
        return Ok(json!({
            "ok": true,
            "action": "purged",
            "purged": count + expired_removed
        }));
    }

    let now = now_ms();
    let mut stmt = db
        .prepare(
            "SELECT path, is_binary, deleted_at, expires_at
             FROM trash
             WHERE expires_at > ?1
             ORDER BY deleted_at DESC",
        )
        .map_err(|e| AxiError::new(format!("DB error: {e}"), "INTERNAL_ERROR", vec![]))?;

    let items: Vec<Value> = stmt
        .query_map(params![now], |r| {
            let path: String = r.get(0)?;
            let is_bin: i64 = r.get(1)?;
            let deleted_at: i64 = r.get(2)?;
            let expires_at: i64 = r.get(3)?;
            let remaining_ms = (expires_at - now).max(0);
            let days_remaining = (remaining_ms as f64 / 86_400_000.0 * 10.0).round() / 10.0;
            Ok(json!({
                "path": path,
                "type": if is_bin != 0 { "attachment" } else { "note" },
                "deletedAt": format_ts(deleted_at),
                "expiresAt": format_ts(expires_at),
                "daysRemaining": days_remaining,
            }))
        })
        .map_err(|e| AxiError::new(format!("DB query error: {e}"), "INTERNAL_ERROR", vec![]))?
        .flatten()
        .collect();

    Ok(json!({
        "retentionDays": 7,
        "count": items.len(),
        "items": items,
        "next": if items.is_empty() {
            vec!["Trash is empty".to_string()]
        } else {
            vec![
                "vault restore <file> [--to <new_path>]".to_string(),
                "vault trash --purge".to_string()
            ]
        }
    }))
}

/// `vault restore <file> [--to <dest>] [--force]` — recover a deleted note/attachment from 7-day trash.
pub fn restore_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(raw_target) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing <file> argument to restore",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault restore <file> [--to <new_path>] [--force]".to_string(),
                "Run `vault trash` to list recoverable files".to_string(),
            ],
        ));
    };

    let force = flag_bool(&parsed.flags, "force");
    let to_override = flag_string(&parsed.flags, "to");

    let cand_md = normalize_note_path(raw_target);
    let cand_any = normalize_any_vault_path(raw_target);

    ctx.pull_before_mutation()?;
    let now = now_ms();
    let row: Option<(String, String, i64)> = {
        let db = ctx.db()?;
        purge_expired_trash(db);
        db.query_row(
            "SELECT path, content, is_binary FROM trash
             WHERE (path = ?1 OR path = ?2) AND expires_at > ?3
             LIMIT 1",
            params![&cand_md, &cand_any, now],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .ok()
        .flatten()
    };

    let Some((orig_path, enc_content, is_bin)) = row else {
        return Err(AxiError::new(
            format!("File not found in trash (or 7-day recovery window expired): {raw_target}"),
            "NOT_FOUND",
            vec!["Run `vault trash` to see recoverable notes".to_string()],
        ));
    };

    let dest_rel = match to_override {
        Some(ref custom) => {
            if is_bin != 0 {
                normalize_any_vault_path(custom)
            } else {
                normalize_note_path(custom)
            }
        }
        None => orig_path.clone(),
    };

    let dest_abs = resolve_in_vault(&root, &dest_rel)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let dest_p = Path::new(&dest_abs);
    if dest_p.exists() && !force {
        return Err(AxiError::new(
            format!("Destination already exists: {dest_rel}"),
            "VALIDATION_ERROR",
            vec![
                format!("Pass --force to overwrite {dest_rel}"),
                format!("Or restore to a new path: `vault restore {orig_path} --to <new_path>`"),
            ],
        ));
    }

    let decrypted =
        decrypt_at_rest(&enc_content).map_err(|e| AxiError::new(e, "CRYPTO_ERROR", vec![]))?;

    if let Some(parent) = dest_p.parent() {
        let _ = fs::create_dir_all(parent);
    }

    if is_bin != 0 {
        let bytes = decode_binary(&decrypted).unwrap_or_else(|| decrypted.clone().into_bytes());
        fs::write(dest_p, &bytes).map_err(|e| {
            AxiError::new(
                format!("Failed to restore binary attachment {dest_rel}: {e}"),
                "IO_ERROR",
                vec![],
            )
        })?;
    } else {
        fs::write(dest_p, &decrypted).map_err(|e| {
            AxiError::new(
                format!("Failed to restore note {dest_rel}: {e}"),
                "IO_ERROR",
                vec![],
            )
        })?;
    }

    {
        let db = ctx.db()?;
        let _ = db.execute("DELETE FROM trash WHERE path = ?1", params![&orig_path]);
        if is_bin == 0 && dest_rel.to_ascii_lowercase().ends_with(".md") {
            record_note_version(db, &dest_rel, &decrypted, "restored");
        }
    }

    ctx.sync_after_mutation()?;

    let mut out = json!({
        "ok": true,
        "action": "restored",
        "file": dest_rel,
        "restoredFrom": orig_path,
    });
    ctx.attach_sync_warnings(&mut out);
    Ok(out)
}
