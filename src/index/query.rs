use crate::index::embed::{cosine, embed, from_blob, init_embedder, is_available};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct BlockRow {
    pub id: i64,
    pub path: String,
    pub heading_path: String,
    pub text: String,
    pub embedding: Option<Vec<u8>>,
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\p{L}\p{N}_]+").unwrap())
}

fn fts_stopwords() -> HashSet<&'static str> {
    HashSet::from(["and", "or", "not", "near"])
}

/// Build a safe FTS5 MATCH expression from free text: prefix-match tokens OR-joined.
pub fn build_fts_query(query: &str) -> String {
    let lower = query.to_lowercase();
    let stop = fts_stopwords();
    let tokens: Vec<String> = token_re()
        .find_iter(&lower)
        .map(|m| m.as_str().to_string())
        .filter(|t| t.chars().count() > 1 && !stop.contains(t.as_str()))
        .take(12)
        .collect();
    if tokens.is_empty() {
        return String::new();
    }
    tokens
        .into_iter()
        .map(|t| format!("{t}*"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub id: i64,
    pub score: f64,
}

/// Keyword search via FTS5. Returns hits best-first (higher score = better).
pub fn fts_search(db: &Connection, query: &str, limit: usize) -> Vec<Hit> {
    let match_expr = build_fts_query(query);
    if match_expr.is_empty() {
        return Vec::new();
    }
    let Ok(mut stmt) = db.prepare(
        "SELECT rowid AS id, bm25(blocks_fts) AS rank FROM blocks_fts WHERE blocks_fts MATCH ?1 ORDER BY rank LIMIT ?2",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map(params![match_expr, limit as i64], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    rows.flatten()
        .map(|(id, rank)| Hit { id, score: -rank })
        .collect()
}

/// Semantic search over stored block vectors. Returns None if embeddings unavailable.
pub fn semantic_search(db: &Connection, query: &str, limit: usize) -> Option<Vec<Hit>> {
    init_embedder();
    if !is_available() {
        return None;
    }
    let vecs = embed(&[query.to_string()])?;
    let qvec = vecs.first()?;
    Some(rank_by_vector(db, qvec, limit))
}

pub fn rank_by_vector(db: &Connection, qvec: &[f32], limit: usize) -> Vec<Hit> {
    let Ok(mut stmt) = db.prepare("SELECT id, embedding FROM blocks WHERE embedding IS NOT NULL")
    else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let mut scored: Vec<Hit> = rows
        .flatten()
        .map(|(id, blob)| Hit {
            id,
            score: cosine(qvec, &from_blob(&blob)) as f64,
        })
        .collect();
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

/// Reciprocal-rank fusion of several ranked lists (by block id).
pub fn rrf_fuse(lists: &[Vec<Hit>], k: f64) -> HashMap<i64, f64> {
    let mut fused: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        for (rank, hit) in list.iter().enumerate() {
            let entry = fused.entry(hit.id).or_insert(0.0);
            *entry += 1.0 / (k + (rank as f64) + 1.0);
        }
    }
    fused
}

/// Mean (re-normalized) vector across a file's block embeddings, or None.
pub fn file_mean_vector(db: &Connection, path: &str) -> Option<Vec<f32>> {
    let mut stmt = db
        .prepare("SELECT embedding FROM blocks WHERE path = ?1 AND embedding IS NOT NULL")
        .ok()?;
    let blobs: Vec<Vec<u8>> = stmt
        .query_map(params![path], |r| r.get::<_, Vec<u8>>(0))
        .ok()?
        .flatten()
        .collect();
    if blobs.is_empty() {
        return None;
    }
    let first = from_blob(&blobs[0]);
    let dim = first.len();
    if dim == 0 {
        return None;
    }
    let mut acc = vec![0.0f32; dim];
    for blob in &blobs {
        let v = from_blob(blob);
        for (i, &val) in v.iter().take(dim).enumerate() {
            acc[i] += val;
        }
    }
    let norm = acc.iter().map(|x| x * x).sum::<f32>().sqrt();
    let div = if norm > 0.0 { norm } else { 1.0 };
    for x in &mut acc {
        *x /= div;
    }
    Some(acc)
}

pub fn block_by_id(db: &Connection, id: i64) -> Option<BlockRow> {
    db.query_row(
        "SELECT id, path, heading_path, text, embedding FROM blocks WHERE id = ?1",
        params![id],
        |r| {
            Ok(BlockRow {
                id: r.get(0)?,
                path: r.get(1)?,
                heading_path: r.get(2)?,
                text: r.get(3)?,
                embedding: r.get(4)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// Build a Smart-Connections-style breadcrumb from a block.
pub fn breadcrumb(db: &Connection, path: &str, heading_path: &str) -> String {
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
    let base = match title {
        Some(t) if !t.is_empty() => format!("{path} ({t})"),
        _ => path.to_string(),
    };
    if heading_path.is_empty() || heading_path == "(preamble)" {
        base
    } else {
        format!("{base} > {heading_path}")
    }
}
