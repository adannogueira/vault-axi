use crate::args::{flag_string, parse_args, resolve_content, FlagValue};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::markdown::frontmatter::{
    coerce_value, set_frontmatter_field, unset_frontmatter_field,
};
use crate::markdown::sections::{
    edit_section, replace_anchor, SectionError, SectionErrorKind, SectionMode,
};
use crate::vault::{normalize_note_path, resolve_in_vault};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub fn update_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(usage("Missing file path"));
    };
    let note_path = normalize_note_path(rel);
    let _ = ctx.pull_before_mutation();
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let abs_path = Path::new(&abs);
    if !abs_path.exists() {
        return Err(AxiError::new(
            format!("Note not found: {note_path}"),
            "NOT_FOUND",
            vec!["Use `vault write` to create it".to_string()],
        ));
    }
    let raw = fs::read_to_string(abs_path)
        .map_err(|e| AxiError::new(format!("Read error: {e}"), "INTERNAL_ERROR", vec![]))?;

    let section = flag_string(&parsed.flags, "section");
    let frontmatter = collect_frontmatter(&parsed.flags);
    let anchor = flag_string(&parsed.flags, "anchor");

    let mut summary = Map::new();
    let out: String;

    let map_sec_err = |e: SectionError| -> AxiError {
        let code = match e.kind {
            SectionErrorKind::NotFound => "NOT_FOUND",
            SectionErrorKind::Ambiguous => "VALIDATION_ERROR",
        };
        AxiError::new(
            e.message,
            code,
            vec![format!("vault read {note_path} --outline")],
        )
    };

    if let Some(sec) = section {
        let mode_raw = flag_string(&parsed.flags, "mode").unwrap_or_else(|| "replace".to_string());
        let Some(mode) = SectionMode::from_str(&mode_raw) else {
            return Err(usage(&format!(
                "Invalid --mode \"{mode_raw}\" (replace|append|prepend)"
            )));
        };
        let content = resolve_content(&parsed.flags)?;
        out = edit_section(&raw, &sec, mode, &content).map_err(map_sec_err)?;
        summary.insert("edit".to_string(), json!("section"));
        summary.insert("section".to_string(), json!(sec));
        summary.insert("mode".to_string(), json!(mode.as_str()));
    } else if !frontmatter.is_empty() {
        let mut cur = raw.clone();
        let mut changed = Vec::new();
        for (key, value) in frontmatter {
            if key.is_empty() || key.contains(':') || key.contains('\n') || key.contains('\r') {
                return Err(AxiError::new(
                    format!("Invalid frontmatter key: \"{key}\""),
                    "VALIDATION_ERROR",
                    vec!["Use --frontmatter key=value (or --frontmatter key to unset)".to_string()],
                ));
            }
            cur = match value {
                None => unset_frontmatter_field(&cur, &key),
                Some(v) => set_frontmatter_field(&cur, &key, &coerce_value(&v)),
            };
            changed.push(key);
        }
        out = cur;
        summary.insert("edit".to_string(), json!("frontmatter"));
        summary.insert("fields".to_string(), json!(changed));
    } else if let Some(anc) = anchor {
        let Some(replacement) = flag_string(&parsed.flags, "replace") else {
            return Err(usage("--anchor requires --replace <text>"));
        };
        out = replace_anchor(&raw, &anc, &replacement).map_err(map_sec_err)?;
        summary.insert("edit".to_string(), json!("anchor"));
    } else {
        return Err(usage("Nothing to update"));
    }

    let mut resp = Map::new();
    resp.insert("ok".to_string(), json!(true));
    if out == raw {
        resp.insert("changed".to_string(), json!(false));
        resp.insert("file".to_string(), json!(note_path));
        resp.insert(
            "note".to_string(),
            json!("No change (content already matched)"),
        );
        for (k, v) in summary {
            resp.insert(k, v);
        }
        return Ok(Value::Object(resp));
    }

    fs::write(abs_path, &out)
        .map_err(|e| AxiError::new(format!("Write error: {e}"), "INTERNAL_ERROR", vec![]))?;
    ctx.sync_after_mutation()?;

    resp.insert("changed".to_string(), json!(true));
    resp.insert("file".to_string(), json!(note_path));
    for (k, v) in summary {
        resp.insert(k, v);
    }
    let mut val = Value::Object(resp);
    ctx.attach_sync_warnings(&mut val);
    Ok(val)
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

fn usage(message: &str) -> AxiError {
    AxiError::new(
        message,
        "VALIDATION_ERROR",
        vec![
            "vault update <file> --section \"<heading>\" --mode replace|append|prepend --content \"...\"".to_string(),
            "vault update <file> --frontmatter status=active [--frontmatter tags=\"[a, b]\"]".to_string(),
            "vault update <file> --anchor \"exact text\" --replace \"new text\"".to_string(),
        ],
    )
}
