use crate::args::parse_args;
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use crate::vault::normalize_note_path;
use rusqlite::params;
use serde_json::{json, Map, Value};

pub fn links_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let _ = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing file path",
            "VALIDATION_ERROR",
            vec!["Usage: vault links <file>".to_string()],
        ));
    };
    let note_path = normalize_note_path(rel);

    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(false),
    })?;
    let db = ctx.db()?;

    let mut outbound = Vec::new();
    if let Ok(mut stmt) = db.prepare(
        "SELECT target_raw, target_path, heading, alias FROM links WHERE src_path = ?1 ORDER BY target_raw",
    ) {
        if let Ok(rows) = stmt.query_map(params![note_path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        }) {
            for (target_raw, target_path, heading, alias) in rows.flatten() {
                let mut m = Map::new();
                m.insert("link".to_string(), json!(target_raw));
                if let Some(ref tp) = target_path {
                    m.insert("resolvedTo".to_string(), json!(tp));
                }
                if let Some(h) = heading {
                    m.insert("heading".to_string(), json!(h));
                }
                if let Some(a) = alias {
                    m.insert("alias".to_string(), json!(a));
                }
                if target_path.is_none() {
                    m.insert("dangling".to_string(), json!(true));
                }
                outbound.push(Value::Object(m));
            }
        }
    }

    let mut backlinks: Vec<String> = Vec::new();
    if let Ok(mut stmt) =
        db.prepare("SELECT DISTINCT src_path FROM links WHERE target_path = ?1 ORDER BY src_path")
    {
        if let Ok(rows) = stmt.query_map(params![note_path], |r| r.get::<_, String>(0)) {
            backlinks = rows.flatten().collect();
        }
    }

    let outbound_val = if outbound.is_empty() {
        json!("(none)")
    } else {
        Value::Array(outbound.clone())
    };
    let backlinks_val = if backlinks.is_empty() {
        json!("(none)")
    } else {
        json!(backlinks)
    };

    Ok(json!({
        "file": note_path,
        "outboundCount": outbound.len(),
        "backlinkCount": backlinks.len(),
        "outbound": outbound_val,
        "backlinks": backlinks_val
    }))
}
