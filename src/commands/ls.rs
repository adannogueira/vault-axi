use crate::args::{flag_int, flag_string, parse_args};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use serde_json::{json, Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn parse_since_ms(since_str: &str) -> Option<i64> {
    let trimmed = since_str.trim();
    if trimmed.is_empty() {
        return None;
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if let Some(days) = trimmed.strip_suffix('d').and_then(|s| s.parse::<i64>().ok()) {
        if days < 0 {
            return None;
        }
        let delta = days.checked_mul(86_400_000)?;
        return now_ms.checked_sub(delta);
    }
    if let Some(hours) = trimmed.strip_suffix('h').and_then(|s| s.parse::<i64>().ok()) {
        if hours < 0 {
            return None;
        }
        let delta = hours.checked_mul(3_600_000)?;
        return now_ms.checked_sub(delta);
    }
    if let Some(weeks) = trimmed.strip_suffix('w').and_then(|s| s.parse::<i64>().ok()) {
        if weeks < 0 {
            return None;
        }
        let delta = weeks.checked_mul(7 * 86_400_000)?;
        return now_ms.checked_sub(delta);
    }
    if let Ok(naive) = chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let dt = naive.and_hms_opt(0, 0, 0)?.and_utc();
        return Some(dt.timestamp_millis());
    }
    None
}

pub fn normalize_dir_filter(raw_dir: &str) -> Option<String> {
    let mut d = raw_dir
        .trim()
        .replace(std::path::MAIN_SEPARATOR, "/")
        .replace('\\', "/");
    while let Some(stripped) = d.strip_prefix("./").or_else(|| d.strip_prefix('/')) {
        d = stripped.to_string();
    }
    d = d.trim_end_matches('/').to_string();
    if d.is_empty() || d == "." {
        None
    } else {
        Some(d)
    }
}

pub fn matches_metadata_filter(
    fm_str: Option<&str>,
    mtime_ms: i64,
    status_filter: Option<&str>,
    tag_filter: Option<&str>,
    since_ms: Option<i64>,
) -> bool {
    if let Some(min_ms) = since_ms {
        if mtime_ms < min_ms {
            return false;
        }
    }
    if status_filter.is_none() && tag_filter.is_none() {
        return true;
    }
    let Some(s) = fm_str else {
        return false;
    };
    let Ok(Value::Object(fm)) = serde_json::from_str::<Value>(s) else {
        return false;
    };

    if let Some(expected_status) = status_filter {
        let actual = fm.get("status").and_then(|v| v.as_str()).unwrap_or("");
        if !actual.eq_ignore_ascii_case(expected_status) {
            return false;
        }
    }

    if let Some(expected_tag) = tag_filter {
        let has_tag = match fm.get("tags") {
            Some(Value::Array(arr)) => arr.iter().any(|t| {
                t.as_str()
                    .map(|s| s.eq_ignore_ascii_case(expected_tag))
                    .unwrap_or(false)
            }),
            Some(Value::String(st)) => st.eq_ignore_ascii_case(expected_tag),
            _ => false,
        };
        if !has_tag {
            return false;
        }
    }

    true
}

pub fn ls_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let _ = ctx.require_vault()?;
    let parsed = parse_args(args);
    let dir = parsed
        .positionals
        .first()
        .and_then(|s| normalize_dir_filter(s));
    let limit = flag_int(&parsed.flags, "limit", 50).max(1) as usize;
    let status_filter = flag_string(&parsed.flags, "status");
    let tag_filter = flag_string(&parsed.flags, "tag");
    let since_ms = if let Some(s) = flag_string(&parsed.flags, "since") {
        Some(parse_since_ms(&s).ok_or_else(|| {
            AxiError::new(
                format!("Invalid --since value: \"{s}\" (use e.g. 7d, 24h, 2w, or YYYY-MM-DD)"),
                "VALIDATION_ERROR",
                vec![],
            )
        })?)
    } else {
        None
    };

    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(false),
    })?;
    let db = ctx.db()?;

    let mut rows: Vec<(String, Option<String>, Option<String>, i64)> = Vec::new();
    if let Ok(mut stmt) =
        db.prepare("SELECT path, title, frontmatter, mtime_ms FROM files ORDER BY path")
    {
        if let Ok(mapped) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
            ))
        }) {
            rows = mapped.flatten().collect();
        }
    }

    if let Some(ref d) = dir {
        let prefix = format!("{d}/");
        rows.retain(|(path, _, _, _)| path.starts_with(&prefix) || path == d);
    }

    let filtered: Vec<(String, Option<String>, Option<String>, i64)> = rows
        .into_iter()
        .filter(|(_, _, fm, mtime)| {
            matches_metadata_filter(
                fm.as_deref(),
                *mtime,
                status_filter.as_deref(),
                tag_filter.as_deref(),
                since_ms,
            )
        })
        .collect();

    let total = filtered.len();
    let files: Vec<Value> = filtered
        .into_iter()
        .take(limit)
        .map(|(path, title, fm_str, _)| {
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
                Some(d) => format!("0 matching notes under \"{d}\""),
                None => "0 matching notes in vault".to_string()
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
