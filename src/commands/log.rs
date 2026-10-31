use crate::args::{flag_string, parse_args, resolve_content};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::markdown::sections::{
    append_to_doc, edit_section, parse_doc, resolve_section, SectionMode,
};
use crate::vault::{normalize_note_path, resolve_in_vault};
use chrono::Local;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub fn log_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);

    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing target task file",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault log <file> \"<entry>\" [--kind <label>]".to_string(),
                "Example: vault log plans/my-task.md \"Switched to bundled SQLite FTS5\" --kind decision".to_string(),
            ],
        ));
    };

    let note_path = normalize_note_path(rel);
    let message = if let Ok(c) = resolve_content(&parsed.flags) {
        c
    } else if parsed.positionals.len() >= 2 {
        parsed.positionals[1..].join(" ")
    } else {
        return Err(AxiError::new(
            "Missing build log entry message",
            "VALIDATION_ERROR",
            vec![
                format!("Usage: vault log {note_path} \"<entry>\" [--kind <label>]"),
            ],
        ));
    };

    let now = Local::now();
    let date_str = now.format("%Y-%m-%d").to_string();
    let time_str = now.format("%H:%M").to_string();

    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let abs_path = Path::new(&abs);

    let kind_badge = flag_string(&parsed.flags, "kind")
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .map(|k| format!("**[{}]** ", k.to_uppercase()))
        .unwrap_or_default();

    let formatted_entry = format!("- `{date_str} {time_str}` — {kind_badge}{}", message.trim());
    let heading_name =
        flag_string(&parsed.flags, "section").unwrap_or_else(|| "Build Log".to_string());

    if !abs_path.exists() {
        if let Some(parent) = abs_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let title = note_path
            .rsplit('/')
            .next()
            .unwrap_or(&note_path)
            .trim_end_matches(".md");
        let initial = format!(
            "---\ndate: {date_str}\nstatus: active\n---\n\n# {title}\n\n## {heading_name}\n{formatted_entry}\n"
        );
        fs::write(abs_path, &initial)
            .map_err(|e| AxiError::new(format!("Write error: {e}"), "INTERNAL_ERROR", vec![]))?;
    } else {
        let raw = fs::read_to_string(abs_path)
            .map_err(|e| AxiError::new(format!("Read error: {e}"), "INTERNAL_ERROR", vec![]))?;
        let doc = parse_doc(&raw);
        let updated = if resolve_section(&doc, &heading_name).is_ok() {
            edit_section(&raw, &heading_name, SectionMode::Append, &formatted_entry)
                .unwrap_or_else(|_| append_to_doc(&raw, &formatted_entry))
        } else {
            let new_sec = format!("## {heading_name}\n{formatted_entry}");
            append_to_doc(&raw, &new_sec)
        };
        fs::write(abs_path, &updated)
            .map_err(|e| AxiError::new(format!("Write error: {e}"), "INTERNAL_ERROR", vec![]))?;
    }

    ctx.sync_after_mutation()?;

    Ok(json!({
        "ok": true,
        "file": note_path,
        "section": heading_name,
        "entry": formatted_entry
    }))
}
