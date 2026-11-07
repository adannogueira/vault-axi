use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use crate::vault::{list_attachment_files, resolve_attachment_target};
use serde_json::{json, Value};
use std::collections::HashSet;

pub fn doctor_command(_args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(false),
    })?;
    let db = ctx.db()?;
    let attachments = list_attachment_files(&root);

    let mut all_files: Vec<(String, Option<String>)> = Vec::new();
    if let Ok(mut stmt) = db.prepare("SELECT path, frontmatter FROM files ORDER BY path") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        }) {
            all_files = rows.flatten().collect();
        }
    }

    let mut dangling_links = Vec::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT src_path, target_raw FROM links WHERE target_path IS NULL ORDER BY src_path",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for (src, raw_target) in rows.flatten() {
                if resolve_attachment_target(&raw_target, &attachments).is_some() {
                    continue;
                }
                dangling_links.push(json!({
                    "from": src,
                    "danglingTarget": raw_target
                }));
            }
        }
    }

    let mut connected_files: HashSet<String> = HashSet::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT src_path, target_path FROM links WHERE target_path IS NOT NULL AND src_path != target_path",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for (s, t) in rows.flatten() {
                connected_files.insert(s);
                connected_files.insert(t);
            }
        }
    }

    let mut orphans = Vec::new();
    let mut missing_frontmatter = Vec::new();
    for (path, fm) in &all_files {
        if !connected_files.contains(path) {
            orphans.push(path.clone());
        }
        if fm.is_none() {
            missing_frontmatter.push(path.clone());
        }
    }

    let healthy = dangling_links.is_empty() && orphans.is_empty();

    Ok(json!({
        "ok": true,
        "healthy": healthy,
        "totalNotes": all_files.len(),
        "danglingLinkCount": dangling_links.len(),
        "danglingLinks": dangling_links,
        "orphanCount": orphans.len(),
        "orphanNotes": orphans,
        "missingFrontmatterCount": missing_frontmatter.len(),
        "missingFrontmatter": missing_frontmatter
    }))
}
