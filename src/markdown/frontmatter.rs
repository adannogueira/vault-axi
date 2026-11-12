use regex::Regex;
use serde_json::{Map, Value};
use std::sync::OnceLock;

fn frontmatter_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^---[ \t]*\r?\n((?:[^\r\n]*\r?\n)*?)(?:---|\.\.\.)[ \t]*(?:\r?\n|$)").unwrap()
    })
}

#[derive(Debug, Clone)]
pub struct FrontmatterResult {
    pub data: Option<Map<String, Value>>,
    /// The raw YAML text between the fences, if any.
    pub raw_block: Option<String>,
}

/// Parse a leading YAML frontmatter block. Returns None data when absent.
pub fn read_frontmatter(raw: &str) -> FrontmatterResult {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let Some(caps) = frontmatter_re().captures(raw) else {
        return FrontmatterResult {
            data: None,
            raw_block: None,
        };
    };
    let raw_block = caps
        .get(1)
        .map(|m| {
            m.as_str()
                .strip_suffix("\r\n")
                .or_else(|| m.as_str().strip_suffix('\n'))
                .unwrap_or(m.as_str())
                .to_string()
        })
        .unwrap_or_default();
    if raw_block.trim().is_empty() {
        return FrontmatterResult {
            data: Some(Map::new()),
            raw_block: Some(raw_block),
        };
    }
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
    let trimmed = value.trim();
    match serde_yaml::from_str::<Value>(value) {
        Ok(Value::Null) => Value::String(value.to_string()),
        Ok(Value::Object(_)) if !trimmed.starts_with('{') => Value::String(value.to_string()),
        Ok(Value::Array(_)) if !trimmed.starts_with('[') => Value::String(value.to_string()),
        Ok(v) => v,
        Err(_) => {
            if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let items: Vec<Value> = inner
                    .split(',')
                    .map(|item| item.trim().trim_matches(['"', '\'']).to_string())
                    .filter(|s| !s.is_empty())
                    .map(Value::String)
                    .collect();
                return Value::Array(items);
            }
            Value::String(value.to_string())
        }
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
    if let Value::String(s) = value {
        if s.contains('\n') || s.contains('\r') {
            if let Ok(escaped) = serde_json::to_string(s) {
                return vec![escaped];
            }
        }
    }
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
        .inspect(|_s| {
            let _ = eol;
        })
        .collect()
}

fn render_key_value(key: &str, value: &Value, eol: &str) -> Vec<String> {
    match value {
        Value::Array(arr) if arr.is_empty() => return vec![format!("{key}: []")],
        Value::Object(map) if map.is_empty() => return vec![format!("{key}: {{}}")],
        _ => {}
    }
    let val_lines = format_yaml_value(value, eol);
    if val_lines.is_empty() {
        return vec![format!("{key}: \"\"")];
    }
    if val_lines.len() == 1
        && !val_lines[0].starts_with("- ")
        && !value.is_object()
        && !value.is_array()
    {
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
            || (line.starts_with(&prefix) && line[prefix.len()..].starts_with([' ', '\t']))
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
        let block = caps
            .get(1)
            .map(|m| {
                m.as_str()
                    .strip_suffix("\r\n")
                    .or_else(|| m.as_str().strip_suffix('\n'))
                    .unwrap_or(m.as_str())
            })
            .unwrap_or("");
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
    let block = caps
        .get(1)
        .map(|m| {
            m.as_str()
                .strip_suffix("\r\n")
                .or_else(|| m.as_str().strip_suffix('\n'))
                .unwrap_or(m.as_str())
        })
        .unwrap_or("");
    if block.is_empty() {
        return raw.to_string();
    }
    let mut lines: Vec<String> = block
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect();

    let Some((start, end)) = find_yaml_key_range(&lines, key) else {
        return raw.to_string();
    };
    lines.drain(start..end);
    let rest = &raw[whole.len()..];
    if lines.is_empty() {
        return format!("---{eol}---{eol}{rest}");
    }
    let yaml_text = lines.join(eol);
    format!("---{eol}{yaml_text}{eol}---{eol}{rest}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DOC: &str =
        "---\ndate: 2026-07-01\nstatus: draft\ntags:\n  - axi\n---\n\n# Body\n\nContent here.\n";

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
        // Adversarial: strings containing `: ` or starting with `- ` must NOT become objects/arrays
        assert_eq!(coerce_value("Note: A Study"), json!("Note: A Study"));
        assert_eq!(coerce_value("- bullet item"), json!("- bullet item"));
        assert_eq!(coerce_value("{a: 1}"), json!({"a": 1}));
        assert_eq!(coerce_value("~"), json!("~"));
    }

    #[test]
    fn adversarial_empty_frontmatter_and_thematic_breaks_and_complex_values() {
        // Empty frontmatter followed by heading and thematic break `---` in body
        let raw = "---\n---\n# My Heading\n\n---\nFooter\n";
        let res = read_frontmatter(raw);
        assert_eq!(res.data, Some(Map::new()));
        let updated = set_frontmatter_field(raw, "status", &json!("active"));
        assert!(updated.contains("# My Heading"));
        assert!(updated.contains("Footer"));
        assert_eq!(
            read_frontmatter(&updated).data.unwrap().get("status"),
            Some(&json!("active"))
        );

        // Unsetting the only field in frontmatter leaves a valid empty frontmatter block
        let once_unset = unset_frontmatter_field(&updated, "status");
        assert!(once_unset.starts_with("---\n---\n# My Heading"));
        assert_eq!(read_frontmatter(&once_unset).data, Some(Map::new()));

        // Setting 1-key object, empty array, empty object, multiline string, and colon string
        let mut doc = "# Title\n".to_string();
        doc = set_frontmatter_field(&doc, "title", &coerce_value("My Plan: Part 1"));
        doc = set_frontmatter_field(&doc, "meta", &json!({"owner": "adan"}));
        doc = set_frontmatter_field(&doc, "empty_arr", &json!([]));
        doc = set_frontmatter_field(&doc, "empty_obj", &json!({}));
        doc = set_frontmatter_field(&doc, "multiline", &json!("line 1\nline 2"));

        let parsed = read_frontmatter(&doc).data.unwrap();
        assert_eq!(parsed.get("title"), Some(&json!("My Plan: Part 1")));
        assert_eq!(parsed.get("meta"), Some(&json!({"owner": "adan"})));
        assert_eq!(parsed.get("empty_arr"), Some(&json!([])));
        assert_eq!(parsed.get("empty_obj"), Some(&json!({})));
        assert_eq!(parsed.get("multiline"), Some(&json!("line 1\nline 2")));

        // CRLF preservation
        let crlf_doc = "---\r\nstatus: draft\r\n---\r\n# Body\r\n";
        let crlf_set = set_frontmatter_field(crlf_doc, "status", &json!("done"));
        assert!(crlf_set.starts_with("---\r\nstatus: done\r\n---\r\n# Body\r\n"));
        let crlf_unset = unset_frontmatter_field(&crlf_set, "status");
        assert!(crlf_unset.starts_with("---\r\n---\r\n# Body\r\n"));
    }
}
