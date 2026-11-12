use crate::args::{flag_bool, parse_args};
use crate::axi::AxiError;
use crate::commands::history::record_edit_transition;
use crate::context::Ctx;
use crate::markdown::sections::{join_lines, split_lines};
use crate::vault::{normalize_note_path, resolve_in_vault};
use regex::Regex;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

fn task_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^([ \t]*[-*+][ \t]+\[)([ xX])(\][ \t]+)(.*)$").unwrap())
}

pub fn check_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing file path",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault check <file> [\"<task substring>\"] [--uncheck] [--list]".to_string(),
            ],
        ));
    };
    let note_path = normalize_note_path(rel);
    let query = parsed
        .positionals
        .get(1..)
        .map(|s| s.join(" "))
        .unwrap_or_default();
    let list_only = flag_bool(&parsed.flags, "list") || query.trim().is_empty();
    if list_only {
        let _ = ctx.pull_if_due();
    } else {
        let _ = ctx.pull_before_mutation();
    }
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let abs_path = Path::new(&abs);
    if !abs_path.exists() {
        return Err(AxiError::new(
            format!("Note not found: {note_path}"),
            "NOT_FOUND",
            vec![],
        ));
    }

    let raw = fs::read_to_string(abs_path)
        .map_err(|e| AxiError::new(format!("Read error: {e}"), "INTERNAL_ERROR", vec![]))?;
    let (mut lines, eol, trailing_newline) = split_lines(&raw);

    if list_only {
        let mut tasks = Vec::new();
        for (idx, line) in lines.iter().enumerate() {
            if let Some(caps) = task_re().captures(line) {
                let checked = caps.get(2).map(|m| m.as_str()) != Some(" ");
                let text = caps.get(4).map(|m| m.as_str()).unwrap_or("").trim();
                tasks.push(json!({
                    "line": idx + 1,
                    "checked": checked,
                    "text": text
                }));
            }
        }
        return Ok(json!({
            "file": note_path,
            "totalTasks": tasks.len(),
            "tasks": tasks
        }));
    }

    let uncheck = flag_bool(&parsed.flags, "uncheck");
    let target_lower = query.trim().to_lowercase();
    let mut matches = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = task_re().captures(line) {
            let text = caps.get(4).map(|m| m.as_str()).unwrap_or("");
            if text.to_lowercase().contains(&target_lower) {
                matches.push(idx);
            }
        }
    }

    if matches.is_empty() {
        return Err(AxiError::new(
            format!("No checklist item matching \"{query}\" in {note_path}"),
            "NOT_FOUND",
            vec![format!(
                "Run `vault check {note_path} --list` to see all tasks"
            )],
        ));
    }
    if matches.len() > 1 && !flag_bool(&parsed.flags, "all") {
        return Err(AxiError::new(
            format!(
                "Task query \"{query}\" matched {} items; pass a more specific substring or --all",
                matches.len()
            ),
            "VALIDATION_ERROR",
            vec![format!(
                "Run `vault check {note_path} --list` to inspect tasks"
            )],
        ));
    }

    let new_mark = if uncheck { " " } else { "x" };
    let mut toggled = Vec::new();
    for idx in matches {
        if let Some(caps) = task_re().captures(&lines[idx]).map(|c| {
            (
                c.get(1).unwrap().as_str().to_string(),
                c.get(3).unwrap().as_str().to_string(),
                c.get(4).unwrap().as_str().to_string(),
            )
        }) {
            lines[idx] = format!("{}{new_mark}{}{}", caps.0, caps.1, caps.2);
            toggled.push(caps.2);
        }
    }

    let out = join_lines(&lines, eol, trailing_newline);
    let changed = out != raw;
    if changed {
        fs::write(abs_path, &out)
            .map_err(|e| AxiError::new(format!("Write error: {e}"), "INTERNAL_ERROR", vec![]))?;
        if let Ok(db) = ctx.db() {
            record_edit_transition(db, &note_path, Some(&raw), &out, "check");
        }
        ctx.sync_after_mutation()?;
    }

    let mut resp = json!({
        "ok": true,
        "changed": changed,
        "file": note_path,
        "checked": !uncheck,
        "items": toggled
    });
    ctx.attach_sync_warnings(&mut resp);
    Ok(resp)
}
