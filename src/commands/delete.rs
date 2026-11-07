use crate::args::{flag_bool, parse_args};
use crate::axi::AxiError;
use crate::commands::trash::move_file_to_trash;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use crate::vault::{normalize_any_vault_path, normalize_note_path, resolve_in_vault};
use rusqlite::params;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;

pub fn delete_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing file path",
            "VALIDATION_ERROR",
            vec!["Usage: vault delete <file> [--permanent]".to_string()],
        ));
    };
    let permanent = flag_bool(&parsed.flags, "permanent") || flag_bool(&parsed.flags, "purge");
    let _ = ctx.pull_before_mutation();

    // Support deleting either a markdown note (`foo` -> `foo.md`) or an attachment (`assets/img.png`)
    let cand_any = normalize_any_vault_path(rel);
    let cand_md = normalize_note_path(rel);
    let (note_path, abs) = {
        let abs_any = resolve_in_vault(&root, &cand_any)
            .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
        if Path::new(&abs_any).exists() {
            (cand_any, abs_any)
        } else {
            let abs_md = resolve_in_vault(&root, &cand_md)
                .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
            (cand_md, abs_md)
        }
    };

    if !Path::new(&abs).exists() {
        return Ok(json!({
            "ok": true,
            "deleted": false,
            "file": note_path,
            "note": "Already absent"
        }));
    }

    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(false),
    })?;

    let mut backlinks: Vec<String> = Vec::new();
    let mut trashed = false;
    {
        let db = ctx.db()?;
        if let Ok(mut stmt) = db.prepare(
            "SELECT DISTINCT src_path FROM links WHERE target_path = ?1 AND src_path != ?1",
        ) {
            if let Ok(rows) = stmt.query_map(params![&note_path], |r| r.get::<_, String>(0)) {
                backlinks = rows.flatten().collect();
            }
        }
        if !permanent {
            if move_file_to_trash(db, &note_path, Path::new(&abs)).is_some() {
                trashed = true;
            }
        } else {
            let _ = db.execute("DELETE FROM trash WHERE path = ?1", params![&note_path]);
        }
    }

    fs::remove_file(&abs)
        .map_err(|e| AxiError::new(format!("Delete error: {e}"), "INTERNAL_ERROR", vec![]))?;
    ctx.sync_after_mutation()?;

    let mut out = Map::new();
    out.insert("ok".to_string(), json!(true));
    out.insert("deleted".to_string(), json!(true));
    out.insert("file".to_string(), json!(note_path));
    out.insert("trashed".to_string(), json!(trashed));
    if trashed {
        out.insert("retentionDays".to_string(), json!(7));
        out.insert(
            "restoreHint".to_string(),
            json!(format!("Recover within 7 days via `vault restore {note_path}`")),
        );
    }
    if !backlinks.is_empty() {
        out.insert(
            "warning".to_string(),
            json!(format!("{} note(s) now have dangling links", backlinks.len())),
        );
        out.insert("danglingIn".to_string(), json!(backlinks));
    }
    let mut val = Value::Object(out);
    ctx.attach_sync_warnings(&mut val);
    Ok(val)
}
