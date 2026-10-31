use regex::Regex;
use serde_json::{Map, Value};
use std::sync::OnceLock;

fn frontmatter_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^---\r?\n([\s\S]*?)\r?\n(?:---|\.\.\.)[ \t]*(?:\r?\n|$)").unwrap())
}

#[derive(Debug, Clone)]
pub struct FrontmatterResult {
    pub data: Option<Map<String, Value>>,
    /// The raw YAML text between the fences, if any.
    pub raw_block: Option<String>,
}

/// Parse a leading YAML frontmatter block. Returns None data when absent.
pub fn read_frontmatter(raw: &str) -> FrontmatterResult {
    let Some(caps) = frontmatter_re().captures(raw) else {
        return FrontmatterResult {
            data: None,
            raw_block: None,
        };
    };
    let raw_block = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
    let parsed: Result<Value, _> = serde_yaml::from_str(&raw_block);
    match parsed {
        Ok(Value::Object(map)) => FrontmatterResult {
            data: Some(map),
            raw_block: Some(raw_block),
        },
        _ => FrontmatterResult {
            data: Some(Map::new()),
            raw_block: Some(raw_block),
        },
    }
}

/// Interpret a CLI value string as a typed YAML scalar/list, falling back to string.
pub fn coerce_value(value: &str) -> Value {
    match serde_yaml::from_str::<Value>(value) {
        Ok(Value::Null) => Value::String(value.to_string()),
        Ok(v) => v,
        Err(_) => Value::String(value.to_string()),
    }
}

fn detect_eol(raw: &str) -> &'static str {
    if raw.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn format_yaml_value(value: &Value, eol: &str) -> Vec<String> {
    let yaml_str = serde_yaml::to_string(value).unwrap_or_else(|_| value.to_string());
    let cleaned = yaml_str
        .trim_start_matches("---\n")
        .trim_end_matches('\n')
        .trim_end_matches("...");
    cleaned
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect::<Vec<_>>()
        .into_iter()
        .filter(|s| !s.is_empty() || matches!(value, Value::String(st) if st.is_empty()))
        .map(|s| {
            let _ = eol;
            s
        })
        .collect()
}

fn render_key_value(key: &str, value: &Value, eol: &str) -> Vec<String> {
    let val_lines = format_yaml_value(value, eol);
    if val_lines.is_empty() {
        return vec![format!("{key}: \"\"")];
    }
    if val_lines.len() == 1 && !val_lines[0].starts_with("- ") {
        vec![format!("{key}: {}", val_lines[0])]
    } else {
        let mut out = vec![format!("{key}:")];
        for l in val_lines {
            out.push(format!("  {l}"));
        }
        out
    }
}

/// Find the line range `[start, end)` in `lines` corresponding to top-level YAML `key`.
fn find_yaml_key_range(lines: &[String], key: &str) -> Option<(usize, usize)> {
    let prefix = format!("{key}:");
    let mut start = None;
    for (i, line) in lines.iter().enumerate() {
        if line == &prefix
            || (line.starts_with(&prefix)
                && line[prefix.len()..].starts_with([' ', '\t']))
        {
            start = Some(i);
            break;
        }
    }
    let s = start?;
    let mut end = s + 1;
    while end < lines.len() {
        let l = &lines[end];
        if l.trim().is_empty() || l.starts_with(' ') || l.starts_with('\t') {
            end += 1;
        } else {
            break;
        }
    }
    Some((s, end))
}

/// Set (or create) a single frontmatter field, preserving other fields, comments,
/// and formatting. Only the frontmatter block is rewritten; the body is untouched.
pub fn set_frontmatter_field(raw: &str, key: &str, value: &Value) -> String {
    let eol = detect_eol(raw);
    let rendered = render_key_value(key, value, eol);

    if let Some(caps) = frontmatter_re().captures(raw) {
        let whole = caps.get(0).unwrap().as_str();
        let block = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let mut lines: Vec<String> = if block.is_empty() {
            Vec::new()
        } else {
            block
                .split('\n')
                .map(|s| s.trim_end_matches('\r').to_string())
                .collect()
        };

        if let Some((start, end)) = find_yaml_key_range(&lines, key) {
            lines.splice(start..end, rendered);
        } else {
            lines.extend(rendered);
        }

        let yaml_text = lines.join(eol);
        let rest = &raw[whole.len()..];
        return format!("---{eol}{yaml_text}{eol}---{eol}{rest}");
    }

    let yaml_text = rendered.join(eol);
    let sep = if raw.is_empty() { "" } else { eol };
    format!("---{eol}{yaml_text}{eol}---{eol}{sep}{raw}")
}

/// Remove a frontmatter field. No-op if the field or block is absent.
pub fn unset_frontmatter_field(raw: &str, key: &str) -> String {
    let eol = detect_eol(raw);
    let Some(caps) = frontmatter_re().captures(raw) else {
        return raw.to_string();
    };
    let whole = caps.get(0).unwrap().as_str();
    let block = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    let mut lines: Vec<String> = block
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect();

    let Some((start, end)) = find_yaml_key_range(&lines, key) else {
        return raw.to_string();
    };
    lines.drain(start..end);
    let yaml_text = lines.join(eol);
    let rest = &raw[whole.len()..];
    format!("---{eol}{yaml_text}{eol}---{eol}{rest}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DOC: &str = "---\ndate: 2026-07-01\nstatus: draft\ntags:\n  - axi\n---\n\n# Body\n\nContent here.\n";

    #[test]
    fn read_frontmatter_parses_fields_and_returns_none_when_absent() {
        let res = read_frontmatter(DOC);
        let data = res.data.unwrap();
        assert_eq!(data.get("status"), Some(&json!("draft")));
        assert_eq!(data.get("tags"), Some(&json!(["axi"])));
        assert!(read_frontmatter("# No fm\n").data.is_none());
    }

    #[test]
    fn set_frontmatter_field_updates_adds_and_creates() {
        let out = set_frontmatter_field(DOC, "status", &json!("active"));
        let data = read_frontmatter(&out).data.unwrap();
        assert_eq!(data.get("status"), Some(&json!("active")));
        assert!(out.contains("# Body\n\nContent here."));
        assert!(out.contains("date: 2026-07-01"));

        let out2 = set_frontmatter_field(DOC, "owner", &json!("adan"));
        let data2 = read_frontmatter(&out2).data.unwrap();
        assert_eq!(data2.get("owner"), Some(&json!("adan")));

        let out3 = set_frontmatter_field("# Title\n\nbody\n", "status", &json!("draft"));
        assert!(out3.starts_with("---\nstatus: draft\n---\n"));
        assert!(out3.contains("# Title\n\nbody"));
    }

    #[test]
    fn unset_frontmatter_field_removes_field() {
        let out = unset_frontmatter_field(DOC, "status");
        let data = read_frontmatter(&out).data.unwrap();
        assert!(!data.contains_key("status"));
        assert_eq!(data.get("date"), Some(&json!("2026-07-01")));
    }

    #[test]
    fn coerce_value_types() {
        assert_eq!(coerce_value("2026-07-01"), json!("2026-07-01"));
        assert_eq!(coerce_value("42"), json!(42));
        assert_eq!(coerce_value("true"), json!(true));
        assert_eq!(coerce_value("[a, b]"), json!(["a", "b"]));
    }
}
