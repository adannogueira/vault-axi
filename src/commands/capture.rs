use crate::args::{flag_string, parse_args, resolve_optional_content, FlagValue};
use crate::axi::AxiError;
use crate::commands::history::record_edit_transition;
use crate::config::load_config;
use crate::context::Ctx;
use crate::markdown::sections::{edit_section, find_headings, parse_doc, SectionMode};
use crate::vault::{normalize_note_path, resolve_in_vault};
use chrono::Local;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// Resolve the target note path for `vault capture`:
/// 1. Explicit `--to <note>` / `--file <note>` flag
/// 2. Configured `captureNote` in `config.json` (expanding `{date}` or `YYYY-MM-DD`)
/// 3. Default daily note `daily/YYYY-MM-DD.md`
pub fn resolve_capture_target(to_flag: Option<&str>, date_str: &str) -> String {
    if let Some(explicit) = to_flag.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let expanded = explicit
            .replace("{date}", date_str)
            .replace("YYYY-MM-DD", date_str);
        return normalize_note_path(&expanded);
    }
    let cfg = load_config();
    if let Some(configured) = cfg
        .capture_note
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let expanded = configured
            .replace("{date}", date_str)
            .replace("YYYY-MM-DD", date_str);
        return normalize_note_path(&expanded);
    }
    normalize_note_path(&format!("daily/{date_str}.md"))
}

pub fn capture_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);

    let text_raw = match resolve_optional_content(&parsed.flags)? {
        Some(c) => c,
        None => {
            if parsed.positionals.is_empty() {
                return Err(AxiError::new(
                    "Missing capture text",
                    "VALIDATION_ERROR",
                    vec![
                        "Usage: vault capture \"<thought or note>\" [--to <note>] [--section <heading>] [--tag <tag>]"
                            .to_string(),
                        "Configure default destination with `vault setup --capture-note <path>`"
                            .to_string(),
                    ],
                ));
            }
            parsed.positionals.join(" ")
        }
    };

    let trimmed = text_raw.trim();
    if trimmed.is_empty() {
        return Err(AxiError::new(
            "Capture text cannot be empty",
            "VALIDATION_ERROR",
            vec!["Usage: vault capture \"<thought or note>\"".to_string()],
        ));
    }

    let now = Local::now();
    let date_str = now.format("%Y-%m-%d").to_string();
    let time_str = now.format("%H:%M").to_string();

    let to_flag = flag_string(&parsed.flags, "to").or_else(|| flag_string(&parsed.flags, "file"));
    let note_path = resolve_capture_target(to_flag.as_deref(), &date_str);

    let cfg = load_config();
    let section_heading = flag_string(&parsed.flags, "section")
        .or(cfg.capture_section)
        .unwrap_or_else(|| "Captures".to_string());

    let tags: Vec<String> = match parsed.flags.get("tag") {
        Some(FlagValue::Single(s)) => vec![s.clone()],
        Some(FlagValue::Multiple(list)) => list.clone(),
        _ => Vec::new(),
    };
    let tag_suffix = if tags.is_empty() {
        String::new()
    } else {
        let formatted_tags: Vec<String> = tags
            .iter()
            .map(|t| {
                let clean = t.trim().trim_start_matches('#');
                format!("#{clean}")
            })
            .filter(|t| t.len() > 1)
            .collect();
        if formatted_tags.is_empty() {
            String::new()
        } else {
            format!(" {}", formatted_tags.join(" "))
        }
    };

    let lines: Vec<&str> = trimmed.lines().collect();
    let mut entry = format!("- {} {}{}", time_str, lines[0], tag_suffix);
    for cont in &lines[1..] {
        entry.push_str(&format!("\n  {cont}"));
    }

    ctx.pull_before_mutation()?;
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let abs_p = Path::new(&abs);
    if let Some(parent) = abs_p.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let old_content = fs::read_to_string(abs_p).ok();
    let new_content = match old_content {
        None => {
            let title = if note_path == format!("daily/{date_str}.md") {
                date_str.clone()
            } else {
                Path::new(&note_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&date_str)
                    .to_string()
            };
            format!("---\ndate: {date_str}\n---\n\n# {title}\n\n## {section_heading}\n{entry}\n")
        }
        Some(ref existing) => {
            let doc = parse_doc(existing);
            let matches = find_headings(&doc, &section_heading);
            if let Some(target_h) = matches.first() {
                let heading_path = target_h.path.join("/");
                match edit_section(
                    existing,
                    &heading_path,
                    SectionMode::Append,
                    &format!("{entry}\n"),
                ) {
                    Ok(updated) => updated,
                    Err(_) => {
                        let sep = if existing.ends_with('\n') { "" } else { "\n" };
                        format!("{existing}{sep}\n{entry}\n")
                    }
                }
            } else {
                let sep = if existing.ends_with("\n\n") {
                    ""
                } else if existing.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                format!("{existing}{sep}## {section_heading}\n{entry}\n")
            }
        }
    };

    fs::write(abs_p, &new_content).map_err(|e| {
        AxiError::new(
            format!("Failed to write capture to {note_path}: {e}"),
            "IO_ERROR",
            vec![],
        )
    })?;

    {
        let db = ctx.db()?;
        record_edit_transition(
            db,
            &note_path,
            old_content.as_deref(),
            &new_content,
            "capture",
        );
    }

    ctx.sync_after_mutation()?;

    let mut out = json!({
        "ok": true,
        "file": note_path,
        "section": section_heading,
        "entry": entry,
    });
    ctx.attach_sync_warnings(&mut out);
    Ok(out)
}
