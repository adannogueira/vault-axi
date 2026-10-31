use crate::args::parse_args;
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use crate::vault::{normalize_note_path, resolve_in_vault};
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
            vec!["Usage: vault delete <file>".to_string()],
        ));
    };
    let note_path = normalize_note_path(rel);
    let _ = ctx.pull_before_mutation();
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;

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
    let db = ctx.db()?;

    let mut backlinks: Vec<String> = Vec::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT DISTINCT src_path FROM links WHERE target_path = ?1 AND src_path != ?1",
    ) {
        if let Ok(rows) = stmt.query_map(params![note_path], |r| r.get::<_, String>(0)) {
            backlinks = rows.flatten().collect();
        }
    }

    fs::remove_file(&abs)
        .map_err(|e| AxiError::new(format!("Delete error: {e}"), "INTERNAL_ERROR", vec![]))?;
    ctx.sync_after_mutation()?;

    let mut out = Map::new();
    out.insert("ok".to_string(), json!(true));
    out.insert("deleted".to_string(), json!(true));
    out.insert("file".to_string(), json!(note_path));
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
