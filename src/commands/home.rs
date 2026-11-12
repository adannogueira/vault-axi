use crate::axi::AxiError;
use crate::context::Ctx;
use crate::db::get_meta;
use crate::index::reconcile::ReconcileOpts;
use chrono::{TimeZone, Utc};
use serde_json::{json, Value};

pub fn home_command(_args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    if ctx.vault_path.is_none() {
        return Ok(json!({
            "status": "not configured",
            "next": ["vault setup --vault <path>  # point at your Obsidian vault"]
        }));
    }
    let root = ctx.require_vault()?;
    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(false),
    })?;
    let db = ctx.db()?;

    let file_count: i64 = db
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap_or(0);
    let block_count: i64 = db
        .query_row("SELECT COUNT(*) FROM blocks", [], |r| r.get(0))
        .unwrap_or(0);
    let embedded: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM blocks WHERE embedding IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let embeddable: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM blocks WHERE length(text) >= 40",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let link_count: i64 = db
        .query_row("SELECT COUNT(*) FROM links", [], |r| r.get(0))
        .unwrap_or(0);
    let dangling: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM links WHERE target_path IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let mut recent_notes = Vec::new();
    if let Ok(mut stmt) =
        db.prepare("SELECT path, frontmatter FROM files ORDER BY mtime_ms DESC LIMIT 6")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        }) {
            for (path, fm_str) in rows.flatten() {
                let status = fm_str
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .and_then(|v| {
                        v.get("status")
                            .and_then(|st| st.as_str())
                            .map(|s| s.to_string())
                    });
                if let Some(st) = status {
                    recent_notes.push(format!("{path} ({st})"));
                } else {
                    recent_notes.push(path);
                }
            }
        }
    }

    let last_indexed = get_meta(db, "last_reconcile")
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| "never".to_string());

    let embeddings_status = if embedded >= embeddable {
        format!("complete ({embedded} blocks)")
    } else {
        format!("{embedded}/{embeddable} (run `vault reindex`)")
    };

    Ok(json!({
        "vault": root,
        "notes": file_count,
        "blocks": block_count,
        "links": link_count,
        "danglingLinks": dangling,
        "embeddings": embeddings_status,
        "lastIndexed": last_indexed,
        "recent": recent_notes,
        "next": [
            "vault search \"<query>\"",
            "vault read <file> --outline",
            "vault related <file>"
        ]
    }))
}
