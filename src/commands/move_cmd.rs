use crate::args::{flag_bool, parse_args};
use crate::axi::AxiError;
use crate::commands::history::migrate_version_paths;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use crate::markdown::links::{rewrite_wiki_links, WikiLink};
use crate::vault::{normalize_note_path, resolve_in_vault};
use rusqlite::params;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn base_no_ext(path: &str) -> String {
    let base = match path.rfind('/') {
        Some(idx) => &path[idx + 1..],
        None => path,
    };
    if base.to_lowercase().ends_with(".md") {
        base[..base.len() - 3].to_string()
    } else {
        base.to_string()
    }
}

pub fn move_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let (Some(old_raw), Some(new_raw)) = (parsed.positionals.first(), parsed.positionals.get(1))
    else {
        return Err(AxiError::new(
            "Usage: vault move <old> <new>",
            "VALIDATION_ERROR",
            vec!["Rewrites [[wikilinks]] across the vault".to_string()],
        ));
    };
    let old_path = normalize_note_path(old_raw);
    let new_path = normalize_note_path(new_raw);
    let _ = ctx.pull_before_mutation();
    let old_abs = resolve_in_vault(&root, &old_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let new_abs = resolve_in_vault(&root, &new_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;

    if !Path::new(&old_abs).exists() {
        return Err(AxiError::new(
            format!("Note not found: {old_path}"),
            "NOT_FOUND",
            vec![],
        ));
    }
    if old_path == new_path {
        return Ok(json!({
            "ok": true,
            "changed": false,
            "note": "Source and target are identical"
        }));
    }
    if Path::new(&new_abs).exists() && !flag_bool(&parsed.flags, "force") {
        return Err(AxiError::new(
            format!("Target already exists: {new_path}"),
            "VALIDATION_ERROR",
            vec!["Pass --force to overwrite".to_string()],
        ));
    }

    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(false),
    })?;
    let db = ctx.db()?;

    let mut referrers: Vec<String> = Vec::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT DISTINCT src_path FROM links WHERE target_path = ?1 AND src_path != ?1",
    ) {
        if let Ok(rows) = stmt.query_map(params![old_path], |r| r.get::<_, String>(0)) {
            referrers = rows.flatten().collect();
        }
    }

    let old_base = base_no_ext(&old_path).to_lowercase();
    let old_full = old_path[..old_path.len() - 3].to_lowercase();
    let new_base = base_no_ext(&new_path);
    let new_full = new_path[..new_path.len() - 3].to_string();

    let matches_old = |link: &WikiLink| -> bool {
        let t = link
            .target
            .strip_prefix("./")
            .unwrap_or(&link.target)
            .to_lowercase();
        t == old_base || t == old_full
    };

    if let Some(parent) = Path::new(&new_abs).parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::rename(&old_abs, &new_abs)
        .map_err(|e| AxiError::new(format!("Move error: {e}"), "INTERNAL_ERROR", vec![]))?;

    let mut rewritten_links = 0;
    let mut touched = Vec::new();
    let mut check_paths = referrers.clone();
    check_paths.push(new_path.clone());
    for ref_path in &check_paths {
        let Ok(abs) = resolve_in_vault(&root, ref_path) else {
            continue;
        };
        if !Path::new(&abs).exists() {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&abs) else {
            continue;
        };
        let (text, count) = rewrite_wiki_links(&raw, |link| {
            if !matches_old(link) {
                return None;
            }
            if link.target.contains('/') {
                Some(new_full.clone())
            } else {
                Some(new_base.clone())
            }
        });
        if count > 0 && text != raw {
            let _ = fs::write(&abs, &text);
            rewritten_links += count;
            touched.push(ref_path.clone());
        }
    }

    // Force-invalidate index entries for touched files and update link target_path
    // so sequential renames within the same millisecond never use stale link targets.
    if let Ok(db) = ctx.db() {
        migrate_version_paths(db, &old_path, &new_path);
        let _ = db.execute(
            "UPDATE links SET target_path = ?1 WHERE target_path = ?2",
            params![new_path, old_path],
        );
        for t_path in &touched {
            let _ = db.execute("DELETE FROM files WHERE path = ?1", params![t_path]);
        }
        let _ = db.execute("DELETE FROM files WHERE path = ?1", params![new_path]);
    }

    ctx.sync_after_mutation()?;

    let files_touched = if touched.is_empty() {
        json!("(none)")
    } else {
        json!(touched)
    };

    let mut resp = json!({
        "ok": true,
        "changed": true,
        "from": old_path,
        "to": new_path,
        "referrers": referrers.len(),
        "linksRewritten": rewritten_links,
        "filesTouched": files_touched
    });
    ctx.attach_sync_warnings(&mut resp);
    Ok(resp)
}
