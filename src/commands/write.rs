use crate::args::{flag_bool, flag_string, parse_args, resolve_content, FlagValue};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::markdown::frontmatter::{coerce_value, set_frontmatter_field};
use crate::vault::{normalize_note_path, resolve_in_vault};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub fn write_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing file path",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault write <file> [--title \"...\"] [--frontmatter k=v] (--content \"...\" | --from <src> | --stdin)"
                    .to_string(),
            ],
        ));
    };
    let note_path = normalize_note_path(rel);
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let abs_path = Path::new(&abs);
    let existed = abs_path.exists();
    if existed && !flag_bool(&parsed.flags, "force") {
        return Err(AxiError::new(
            format!("Note already exists: {note_path}"),
            "VALIDATION_ERROR",
            vec!["Pass --force to overwrite, or use `vault update` for a surgical edit".to_string()],
        ));
    }

    let mut content = resolve_content(&parsed.flags).unwrap_or_default();
    let title = flag_string(&parsed.flags, "title");
    let fm_pairs = collect_frontmatter(&parsed.flags);

    if content.is_empty() && title.is_none() && fm_pairs.is_empty() {
        return Err(AxiError::new(
            "No content provided",
            "VALIDATION_ERROR",
            vec![
                "Pass --from <file> (best for long notes), --content \"text\", --stdin, or --title"
                    .to_string(),
            ],
        ));
    }

    if let Some(ref t) = title {
        let trimmed = content.trim_start();
        if !trimmed.starts_with("# ") {
            if content.is_empty() {
                content = format!("# {t}\n");
            } else {
                content = format!("# {t}\n\n{content}");
            }
        }
    }

    for (key, val) in fm_pairs {
        if key.is_empty() || key.contains(':') || key.contains('\n') || key.contains('\r') {
            return Err(AxiError::new(
                format!("Invalid frontmatter key: \"{key}\""),
                "VALIDATION_ERROR",
                vec!["Use --frontmatter key=value".to_string()],
            ));
        }
        if let Some(v) = val {
            content = set_frontmatter_field(&content, &key, &coerce_value(&v));
        }
    }

    if !content.ends_with('\n') {
        content.push('\n');
    }

    if let Some(parent) = abs_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(abs_path, &content)
        .map_err(|e| AxiError::new(format!("Write error: {e}"), "INTERNAL_ERROR", vec![]))?;
    ctx.sync_after_mutation()?;

    Ok(json!({
        "ok": true,
        "action": if existed { "overwritten" } else { "created" },
        "file": note_path,
        "chars": content.chars().count(),
        "next": [format!("vault read {note_path} --outline"), format!("vault links {note_path}")]
    }))
}

fn collect_frontmatter(flags: &HashMap<String, FlagValue>) -> Vec<(String, Option<String>)> {
    let items: Vec<String> = match flags.get("frontmatter") {
        None | Some(FlagValue::Bool(_)) => return Vec::new(),
        Some(FlagValue::Single(s)) => vec![s.clone()],
        Some(FlagValue::Multiple(list)) => list.clone(),
    };
    items
        .into_iter()
        .map(|item| match item.find('=') {
            None => (item.trim().to_string(), None),
            Some(eq) => (
                item[..eq].trim().to_string(),
                Some(item[eq + 1..].to_string()),
            ),
        })
        .collect()
}
