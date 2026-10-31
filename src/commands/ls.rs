use crate::args::{flag_int, parse_args};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use rusqlite::params;
use serde_json::{json, Map, Value};

pub fn ls_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let _ = ctx.require_vault()?;
    let parsed = parse_args(args);
    let dir = parsed
        .positionals
        .first()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty());
    let limit = flag_int(&parsed.flags, "limit", 50).max(1) as usize;

    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(false),
    })?;
    let db = ctx.db()?;

    let mut rows: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    if let Some(ref d) = dir {
        let like_pat = format!("{d}/%");
        if let Ok(mut stmt) = db.prepare(
            "SELECT path, title, frontmatter FROM files WHERE path LIKE ?1 ORDER BY path",
        ) {
            if let Ok(mapped) = stmt.query_map(params![like_pat], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            }) {
                rows = mapped.flatten().collect();
            }
        }
    } else if let Ok(mut stmt) =
        db.prepare("SELECT path, title, frontmatter FROM files ORDER BY path")
    {
        if let Ok(mapped) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        }) {
            rows = mapped.flatten().collect();
        }
    }

    let total = rows.len();
    let files: Vec<Value> = rows
        .into_iter()
        .take(limit)
        .map(|(path, title, fm_str)| {
            let mut m = Map::new();
            m.insert("path".to_string(), json!(path));
            if let Some(t) = title {
                if !t.is_empty() {
                    m.insert("title".to_string(), json!(t));
                }
            }
            if let Some(s) = fm_str {
                if let Ok(Value::Object(fm)) = serde_json::from_str::<Value>(&s) {
                    if let Some(st) = fm.get("status") {
                        if !st.is_null() {
                            m.insert("status".to_string(), st.clone());
                        }
                    }
                    if let Some(Value::Array(tags)) = fm.get("tags") {
                        if !tags.is_empty() {
                            m.insert("tags".to_string(), Value::Array(tags.clone()));
                        }
                    }
                }
            }
            Value::Object(m)
        })
        .collect();

    if total == 0 {
        return Ok(json!({
            "where": dir.clone().unwrap_or_else(|| "(vault root)".to_string()),
            "files": match dir {
                Some(d) => format!("0 notes under \"{d}\""),
                None => "0 notes in vault".to_string()
            }
        }));
    }

    let mut out = Map::new();
    out.insert(
        "where".to_string(),
        json!(dir.unwrap_or_else(|| "(whole vault)".to_string())),
    );
    out.insert("total".to_string(), json!(total));
    out.insert("shown".to_string(), json!(files.len()));
    if files.len() < total {
        out.insert(
            "note".to_string(),
            json!(format!(
                "Showing {} of {total}; pass --limit to see more",
                files.len()
            )),
        );
    }
    out.insert("files".to_string(), Value::Array(files));
    Ok(Value::Object(out))
}
