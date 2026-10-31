use crate::args::{flag_bool, flag_string, parse_args};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::markdown::frontmatter::read_frontmatter;
use crate::markdown::sections::{
    outline, parse_doc, resolve_section, split_lines, SectionErrorKind,
};
use crate::render::{truncate_text, Truncated, PREVIEW_LIMIT};
use crate::vault::{normalize_note_path, resolve_in_vault};
use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

fn lines_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+)-(\d+)$").unwrap())
}

pub fn read_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let _ = ctx.pull_if_due();
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing file path",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault read <file> [--outline|--section <h>|--frontmatter|--lines a-b|--full]"
                    .to_string(),
            ],
        ));
    };
    let note_path = normalize_note_path(rel);
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    if !Path::new(&abs).exists() {
        return Err(AxiError::new(
            format!("Note not found: {note_path}"),
            "NOT_FOUND",
            vec!["Run `vault ls` to list notes, or `vault search <query>`".to_string()],
        ));
    }
    let raw = fs::read_to_string(&abs)
        .map_err(|e| AxiError::new(format!("Read error: {e}"), "INTERNAL_ERROR", vec![]))?;
    let full = flag_bool(&parsed.flags, "full");

    if flag_bool(&parsed.flags, "outline") {
        let entries = outline(&parse_doc(&raw));
        if entries.is_empty() {
            return Ok(json!({ "file": note_path, "outline": "(no headings)" }));
        }
        return Ok(json!({
            "file": note_path,
            "headings": entries.len(),
            "outline": entries
        }));
    }

    if flag_bool(&parsed.flags, "frontmatter") {
        let fm = read_frontmatter(&raw);
        let val = fm
            .data
            .map(Value::Object)
            .unwrap_or_else(|| Value::String("(none)".to_string()));
        return Ok(json!({ "file": note_path, "frontmatter": val }));
    }

    if let Some(lines) = flag_string(&parsed.flags, "lines") {
        let Some(caps) = lines_re().captures(lines.trim()) else {
            return Err(AxiError::new(
                "Invalid --lines range (use a-b)",
                "VALIDATION_ERROR",
                vec![],
            ));
        };
        let (Ok(raw_a), Ok(raw_b)) = (caps[1].parse::<usize>(), caps[2].parse::<usize>()) else {
            return Err(AxiError::new(
                "Invalid --lines range (numbers out of bounds)",
                "VALIDATION_ERROR",
                vec![],
            ));
        };
        if raw_a == 0 || raw_b == 0 || raw_a > raw_b {
            return Err(AxiError::new(
                "Invalid --lines range (start must be >= 1 and <= end)",
                "VALIDATION_ERROR",
                vec![],
            ));
        }
        let (all, eol, _) = split_lines(&raw);
        let a = raw_a;
        let b = raw_b.min(all.len());
        let slice = if a <= b && a <= all.len() {
            all[a - 1..b].join(eol)
        } else {
            String::new()
        };
        return Ok(json!({
            "file": note_path,
            "lines": format!("{raw_a}-{b}"),
            "content": slice
        }));
    }

    if let Some(section) = flag_string(&parsed.flags, "section") {
        let doc = parse_doc(&raw);
        match resolve_section(&doc, &section) {
            Ok(s) => {
                let t = if full {
                    Truncated {
                        content: s.text.clone(),
                        truncated: false,
                        total_chars: s.text.chars().count(),
                    }
                } else {
                    truncate_text(&s.text, PREVIEW_LIMIT)
                };
                let mut obj = serde_json::Map::new();
                obj.insert("file".to_string(), json!(note_path));
                obj.insert("section".to_string(), json!(s.heading.path.join(" / ")));
                obj.insert(
                    "lines".to_string(),
                    json!(format!("{}-{}", s.heading_line + 1, s.body_end)),
                );
                obj.insert("content".to_string(), json!(t.content));
                obj.insert("truncated".to_string(), json!(t.truncated));
                obj.insert("totalChars".to_string(), json!(t.total_chars));
                if t.truncated {
                    obj.insert(
                        "hint".to_string(),
                        json!("Pass --full for the whole section"),
                    );
                }
                return Ok(Value::Object(obj));
            }
            Err(e) => {
                let code = match e.kind {
                    SectionErrorKind::NotFound => "NOT_FOUND",
                    SectionErrorKind::Ambiguous => "VALIDATION_ERROR",
                };
                return Err(AxiError::new(
                    e.message,
                    code,
                    vec![format!("Run `vault read {note_path} --outline` to see headings")],
                ));
            }
        }
    }

    let doc = parse_doc(&raw);
    let t = if full {
        Truncated {
            content: raw.clone(),
            truncated: false,
            total_chars: raw.chars().count(),
        }
    } else {
        truncate_text(&raw, PREVIEW_LIMIT)
    };
    let mut obj = serde_json::Map::new();
    obj.insert("file".to_string(), json!(note_path));
    obj.insert("headings".to_string(), json!(doc.headings.len()));
    obj.insert("content".to_string(), json!(t.content));
    obj.insert("truncated".to_string(), json!(t.truncated));
    obj.insert("totalChars".to_string(), json!(t.total_chars));
    if t.truncated {
        obj.insert(
            "hint".to_string(),
            json!("Use --outline for structure, --section <h> for one part, or --full for everything"),
        );
    }
    Ok(Value::Object(obj))
}
