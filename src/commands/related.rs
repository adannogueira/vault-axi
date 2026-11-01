use crate::args::{flag_int, parse_args};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::embed::{cosine, from_blob};
use crate::index::query::file_mean_vector;
use crate::index::reconcile::ReconcileOpts;
use crate::vault::{normalize_note_path, resolve_in_vault};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub fn related_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing file path",
            "VALIDATION_ERROR",
            vec!["Usage: vault related <file> [--limit N]".to_string()],
        ));
    };
    let note_path = normalize_note_path(rel);
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    if !Path::new(&abs).exists() {
        return Err(AxiError::new(
            format!("Note not found: {note_path}"),
            "NOT_FOUND",
            vec!["Run `vault ls` or `vault search <query>`".to_string()],
        ));
    }
    let limit = flag_int(&parsed.flags, "limit", 10).max(1) as usize;

    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(true),
    })?;
    let db = ctx.db()?;

    let mut outbound: HashSet<String> = HashSet::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT DISTINCT target_path FROM links WHERE src_path = ?1 AND target_path IS NOT NULL AND target_path != ?1",
    ) {
        if let Ok(rows) = stmt.query_map(params![note_path], |r| r.get::<_, String>(0)) {
            outbound = rows.flatten().collect();
        }
    }

    let mut backlinks: HashSet<String> = HashSet::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT DISTINCT src_path FROM links WHERE target_path = ?1 AND src_path != ?1",
    ) {
        if let Ok(rows) = stmt.query_map(params![note_path], |r| r.get::<_, String>(0)) {
            backlinks = rows.flatten().collect();
        }
    }

    // 2-hop graph proximity: co-citation (shared incoming hub) and bibliographic coupling (shared outbound target)
    let mut co_linked: HashSet<String> = HashSet::new();
    if let Ok(mut stmt) = db.prepare(
        r#"
        SELECT DISTINCT l2.src_path
        FROM links l1
        JOIN links l2 ON l1.target_path = l2.target_path
        WHERE l1.src_path = ?1 AND l1.target_path IS NOT NULL AND l2.src_path != ?1
        UNION
        SELECT DISTINCT l2.target_path
        FROM links l1
        JOIN links l2 ON l1.src_path = l2.src_path
        WHERE l1.target_path = ?1 AND l2.target_path IS NOT NULL AND l2.target_path != ?1
        "#,
    ) {
        if let Ok(rows) = stmt.query_map(params![note_path], |r| r.get::<_, String>(0)) {
            for p in rows.flatten() {
                if !outbound.contains(&p) && !backlinks.contains(&p) {
                    co_linked.insert(p);
                }
            }
        }
    }

    let mut semantic: HashMap<String, f32> = HashMap::new();
    let qvec = file_mean_vector(db, &note_path);
    if let Some(ref qv) = qvec {
        if let Ok(mut stmt) = db.prepare(
            "SELECT path, embedding FROM blocks WHERE embedding IS NOT NULL AND path != ?1",
        ) {
            if let Ok(rows) = stmt.query_map(params![note_path], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            }) {
                for (p, blob) in rows.flatten() {
                    let s = cosine(qv, &from_blob(&blob));
                    let prev = semantic.get(&p).copied().unwrap_or(-1.0);
                    if s > prev {
                        semantic.insert(p, s);
                    }
                }
            }
        }
    }

    let mut candidates: HashSet<String> = HashSet::new();
    candidates.extend(outbound.iter().cloned());
    candidates.extend(backlinks.iter().cloned());
    candidates.extend(co_linked.iter().cloned());
    for (p, &s) in &semantic {
        if s >= 0.14 {
            candidates.insert(p.clone());
        }
    }

    struct ScoredRow {
        path: String,
        title: Option<String>,
        via: String,
        hop_rank: u8,
        score: f64,
    }

    let mut scored: Vec<ScoredRow> = candidates
        .into_iter()
        .map(|path| {
            let mut via = Vec::new();
            if outbound.contains(&path) {
                via.push("link");
            }
            if backlinks.contains(&path) {
                via.push("backlink");
            }
            if co_linked.contains(&path) {
                via.push("co-linked");
            }
            let sem = semantic.get(&path).copied().unwrap_or(0.0);
            let direct_linked = outbound.contains(&path) || backlinks.contains(&path);
            let two_hop_linked = co_linked.contains(&path);
            if sem >= 0.14 && (!direct_linked && !two_hop_linked || sem >= 0.5) {
                via.push("semantic");
            }
            let title: Option<String> = db
                .query_row(
                    "SELECT title FROM files WHERE path = ?1",
                    params![path],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .ok()
                .flatten()
                .flatten();
            let via_str = if via.is_empty() {
                "semantic".to_string()
            } else {
                via.join("+")
            };
            let hop_rank = if direct_linked {
                2
            } else if two_hop_linked {
                1
            } else {
                0
            };
            let rounded = ((sem as f64) * 10000.0).round() / 10000.0;
            ScoredRow {
                path,
                title,
                via: via_str,
                hop_rank,
                score: rounded,
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        b.hop_rank
            .cmp(&a.hop_rank)
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
    });
    scored.truncate(limit);

    if scored.is_empty() {
        return Ok(json!({
            "file": note_path,
            "related": "0 related notes (no links, and no embeddings yet — run `vault reindex`)"
        }));
    }

    let top_json: Vec<Value> = scored
        .into_iter()
        .map(|r| {
            let mut m = Map::new();
            m.insert("path".to_string(), json!(r.path));
            if let Some(t) = r.title {
                m.insert("title".to_string(), json!(t));
            }
            m.insert("via".to_string(), json!(r.via));
            m.insert("linked".to_string(), json!(r.hop_rank > 0));
            m.insert("score".to_string(), json!(r.score));
            Value::Object(m)
        })
        .collect();

    let mut out = Map::new();
    out.insert("file".to_string(), json!(note_path));
    out.insert(
        "linkedCount".to_string(),
        json!(outbound.len() + backlinks.len()),
    );
    out.insert("shown".to_string(), json!(top_json.len()));
    out.insert("related".to_string(), Value::Array(top_json));
    if qvec.is_none() {
        out.insert(
            "note".to_string(),
            json!("No embeddings for this note — showing graph links only. Run `vault reindex`."),
        );
    }
    Ok(Value::Object(out))
}
