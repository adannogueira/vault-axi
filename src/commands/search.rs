use crate::args::{flag_bool, flag_int, flag_string, parse_args};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::query::{block_by_id, breadcrumb, fts_search, rrf_fuse, semantic_search};
use crate::index::reconcile::ReconcileOpts;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const PREVIEW: usize = 220;

pub fn search_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let _ = ctx.require_vault()?;
    let parsed = parse_args(args);
    let query = parsed.positionals.join(" ").trim().to_string();
    if query.is_empty() {
        return Err(AxiError::new(
            "Missing search query",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault search \"<query>\" [--mode hybrid|fts|semantic] [--limit N] [--folder <dir>]"
                    .to_string(),
            ],
        ));
    }
    let mode = flag_string(&parsed.flags, "mode").unwrap_or_else(|| "hybrid".to_string());
    let limit = flag_int(&parsed.flags, "limit", 8).max(1) as usize;
    let folder = flag_string(&parsed.flags, "folder");
    let full = flag_bool(&parsed.flags, "full");

    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(mode != "fts"),
    })?;
    let db = ctx.db()?;

    let pool = (limit * 3).max(24);
    let fts = fts_search(db, &query, pool);
    let vec_hits = if mode == "fts" {
        None
    } else {
        semantic_search(db, &query, pool)
    };
    let effective_mode = if (mode == "semantic" || mode == "hybrid") && vec_hits.is_none() {
        "fts".to_string()
    } else {
        mode.clone()
    };

    let mut ordered_ids: Vec<i64> = Vec::new();
    let mut score_by_id: HashMap<i64, f64> = HashMap::new();

    if effective_mode == "fts" {
        for h in &fts {
            score_by_id.insert(h.id, h.score);
            ordered_ids.push(h.id);
        }
    } else if effective_mode == "semantic" {
        if let Some(vh) = &vec_hits {
            for h in vh {
                score_by_id.insert(h.id, h.score);
                ordered_ids.push(h.id);
            }
        }
    } else {
        let fused = rrf_fuse(&[fts, vec_hits.unwrap_or_default()], 60.0);
        let mut entries: Vec<(i64, f64)> = fused.into_iter().collect();
        entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (id, s) in entries {
            ordered_ids.push(id);
            score_by_id.insert(id, s);
        }
    }

    let mut results: Vec<Value> = Vec::new();
    for id in ordered_ids {
        if results.len() >= limit {
            break;
        }
        let Some(block) = block_by_id(db, id) else {
            continue;
        };
        if let Some(ref f) = folder {
            let clean_f = f.trim_end_matches('/');
            let prefix = format!("{clean_f}/");
            if !block.path.starts_with(&prefix) && block.path != *f {
                continue;
            }
        }
        let collapsed: String = block.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let preview = if full {
            block.text.clone()
        } else if collapsed.chars().count() > PREVIEW {
            let p: String = collapsed.chars().take(PREVIEW).collect();
            format!("{p}…")
        } else {
            collapsed
        };
        let raw_score = score_by_id.get(&id).copied().unwrap_or(0.0);
        let rounded = (raw_score * 10000.0).round() / 10000.0;
        results.push(json!({
            "path": block.path,
            "breadcrumb": breadcrumb(db, &block.path, &block.heading_path),
            "score": rounded,
            "preview": preview
        }));
    }

    if results.is_empty() {
        return Ok(json!({
            "query": query,
            "mode": effective_mode,
            "results": format!("0 matches for \"{query}\""),
            "hint": "Try fewer/broader terms or --mode semantic"
        }));
    }

    let mut out = Map::new();
    out.insert("query".to_string(), json!(query));
    out.insert("mode".to_string(), json!(effective_mode));
    if effective_mode != mode {
        out.insert(
            "note".to_string(),
            json!("Embeddings unavailable — fell back to FTS5. Run `vault reindex`."),
        );
    }
    out.insert("shown".to_string(), json!(results.len()));
    out.insert("results".to_string(), Value::Array(results));
    Ok(Value::Object(out))
}
