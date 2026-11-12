use crate::args::{flag_bool, flag_int, parse_args};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use crate::markdown::frontmatter::read_frontmatter;
use crate::markdown::sections::{outline, parse_doc};
use crate::render::{truncate_text, PREVIEW_LIMIT};
use crate::vault::{normalize_note_path, resolve_in_vault};
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub fn context_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing file path",
            "VALIDATION_ERROR",
            vec!["Usage: vault context <file> [--limit N] [--full]".to_string()],
        ));
    };
    let note_path = normalize_note_path(rel);
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    if !Path::new(&abs).exists() {
        return Err(AxiError::new(
            format!("Note not found: {note_path}"),
            "NOT_FOUND",
            vec![],
        ));
    }
    let neighbor_limit = flag_int(&parsed.flags, "limit", 6).max(1) as usize;
    let full = flag_bool(&parsed.flags, "full");

    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(false),
    })?;
    let db = ctx.db()?;

    let raw = fs::read_to_string(&abs)
        .map_err(|e| AxiError::new(format!("Read error: {e}"), "INTERNAL_ERROR", vec![]))?;
    let doc = parse_doc(&raw);
    let fm = read_frontmatter(&raw);
    let body_trunc = if full {
        raw.clone()
    } else {
        truncate_text(&raw, PREVIEW_LIMIT).content
    };

    let mut neighbors: Vec<(String, &'static str)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(note_path.clone());

    if let Ok(mut stmt) = db.prepare(
        "SELECT DISTINCT target_path FROM links WHERE src_path = ?1 AND target_path IS NOT NULL",
    ) {
        if let Ok(rows) = stmt.query_map(params![note_path], |r| r.get::<_, String>(0)) {
            for p in rows.flatten() {
                if seen.insert(p.clone()) {
                    neighbors.push((p, "outbound"));
                }
            }
        }
    }

    if let Ok(mut stmt) = db.prepare("SELECT DISTINCT src_path FROM links WHERE target_path = ?1") {
        if let Ok(rows) = stmt.query_map(params![note_path], |r| r.get::<_, String>(0)) {
            for p in rows.flatten() {
                if seen.insert(p.clone()) {
                    neighbors.push((p, "backlink"));
                }
            }
        }
    }

    let mut connected_summaries = Vec::new();
    for (n_path, relation) in neighbors.into_iter().take(neighbor_limit) {
        if let Ok(n_abs) = resolve_in_vault(&root, &n_path) {
            if let Ok(n_raw) = fs::read_to_string(&n_abs) {
                let n_doc = parse_doc(&n_raw);
                let n_outline: Vec<String> = outline(&n_doc).into_iter().map(|e| e.path).collect();
                let preview = truncate_text(&n_raw, 300).content;
                connected_summaries.push(json!({
                    "path": n_path,
                    "relation": relation,
                    "headings": n_outline,
                    "preview": preview
                }));
            }
        }
    }

    Ok(json!({
        "file": note_path,
        "frontmatter": fm.data,
        "outline": outline(&doc),
        "content": body_trunc,
        "connectedCount": connected_summaries.len(),
        "connected": connected_summaries
    }))
}
