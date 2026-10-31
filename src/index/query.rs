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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_db_at;
    use crate::index::embed::to_blob;

    #[test]
    fn build_fts_query_filters_stopwords_short_tokens_and_caps_at_12() {
        assert_eq!(build_fts_query(""), "");
        assert_eq!(build_fts_query("a b c and or not near"), "");
        // 2-character tokens (> 1 char) must be preserved while 1-character tokens are filtered out
        assert_eq!(build_fts_query("go db a x"), "go* OR db*");
        assert_eq!(
            build_fts_query("Rust AND SQLite! \"injection\" (test)"),
            "rust* OR sqlite* OR injection* OR test*"
        );
        let many = "t01 t02 t03 t04 t05 t06 t07 t08 t09 t10 t11 t12 t13 t14";
        let built = build_fts_query(many);
        assert_eq!(built.split(" OR ").count(), 12);
        assert!(!built.contains("t13"));
    }

    #[test]
    fn rrf_fuse_and_vector_ranking_and_mean_vector_and_breadcrumb() {
        let l1 = vec![Hit { id: 10, score: 5.0 }, Hit { id: 20, score: 3.0 }];
        let l2 = vec![Hit { id: 20, score: 0.9 }, Hit { id: 30, score: 0.8 }];
        let fused = rrf_fuse(&[l1, l2], 60.0);
        // id 20 appears in both lists so its RRF score must beat id 10 and id 30
        assert!(fused[&20] > fused[&10]);
        assert!(fused[&10] > fused[&30]);

        let db = open_db_at(":memory:").unwrap();
        db.execute(
            "INSERT INTO files(path, mtime_ms, size, hash, title, frontmatter, indexed_at) VALUES ('a.md', 1, 1, 'h', 'Alpha Title', NULL, 1)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO files(path, mtime_ms, size, hash, title, frontmatter, indexed_at) VALUES ('b.md', 1, 1, 'h', '', NULL, 1)",
            [],
        )
        .unwrap();

        let v1 = to_blob(&[1.0f32, 0.0, 0.0]);
        let v2 = to_blob(&[0.0f32, 1.0, 0.0]);
        db.execute(
            "INSERT INTO blocks(id, path, heading_path, start_line, end_line, text, embedding) VALUES (1, 'a.md', '(preamble)', 1, 2, 'block one', ?1)",
            params![v1],
        )
        .unwrap();
        db.execute(
            "INSERT INTO blocks(id, path, heading_path, start_line, end_line, text, embedding) VALUES (2, 'a.md', 'Design > Sub', 3, 5, 'block two', ?1)",
            params![v2],
        )
        .unwrap();

        let ranked = rank_by_vector(&db, &[1.0f32, 0.0, 0.0], 10);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].id, 1);
        assert!((ranked[0].score - 1.0).abs() < 1e-5);

        let mean = file_mean_vector(&db, "a.md").unwrap();
        let norm: f32 = mean.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert!((mean[0] - mean[1]).abs() < 1e-5);
        assert!(file_mean_vector(&db, "b.md").is_none());

        assert_eq!(breadcrumb(&db, "a.md", "(preamble)"), "a.md (Alpha Title)");
        assert_eq!(
            breadcrumb(&db, "a.md", "Design > Sub"),
            "a.md (Alpha Title) > Design > Sub"
        );
        assert_eq!(breadcrumb(&db, "b.md", "Sec"), "b.md > Sec");
    }
}
