use crate::args::{flag_string, parse_args, resolve_content};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::markdown::frontmatter::{
    coerce_value, set_frontmatter_field, unset_frontmatter_field,
};
use crate::markdown::sections::{
    append_to_doc, edit_section, insert_at_heading, prepend_to_doc, replace_anchor, HeadingAnchor,
    SectionError, SectionErrorKind, SectionMode,
};
use crate::vault::{normalize_note_path, resolve_in_vault};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BatchOperation {
    Section {
        heading: String,
        #[serde(default = "default_replace")]
        mode: String,
        content: String,
    },
    Anchor {
        old_str: String,
        new_str: String,
    },
    Frontmatter {
        key: String,
        #[serde(default)]
        value: Option<Value>,
    },
    Insert {
        where_anchor: String,
        #[serde(default)]
        heading: Option<String>,
        content: String,
    },
}

fn default_replace() -> String {
    "replace".to_string()
}

pub fn batch_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing file path",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault batch <file> (--ops '<json-array>' | --from <ops.json> | --stdin)"
                    .to_string(),
            ],
        ));
    };
    let note_path = normalize_note_path(rel);
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let abs_path = Path::new(&abs);
    if !abs_path.exists() {
        return Err(AxiError::new(
            format!("Note not found: {note_path}"),
            "NOT_FOUND",
            vec!["Use `vault write` to create it first".to_string()],
        ));
    }

    let raw_ops = if let Some(ops_str) = flag_string(&parsed.flags, "ops") {
        ops_str
    } else {
        resolve_content(&parsed.flags)?
    };

    let ops: Vec<BatchOperation> = serde_json::from_str(&raw_ops).map_err(|e| {
        AxiError::new(
            format!("Invalid JSON operations array: {e}"),
            "VALIDATION_ERROR",
            vec![
                "Pass a JSON array of operations: [{\"op\":\"section\",\"heading\":\"Design\",\"mode\":\"replace\",\"content\":\"...\"}, {\"op\":\"anchor\",\"old_str\":\"...\",\"new_str\":\"...\"}]".to_string(),
            ],
        )
    })?;

    if ops.is_empty() {
        return Err(AxiError::new(
            "Operations array is empty",
            "VALIDATION_ERROR",
            vec![],
        ));
    }

    let raw = fs::read_to_string(abs_path)
        .map_err(|e| AxiError::new(format!("Read error: {e}"), "INTERNAL_ERROR", vec![]))?;
    let mut cur = raw.clone();

    let map_sec_err = |idx: usize, e: SectionError| -> AxiError {
        let code = match e.kind {
            SectionErrorKind::NotFound => "NOT_FOUND",
            SectionErrorKind::Ambiguous => "VALIDATION_ERROR",
        };
        AxiError::new(
            format!("Batch op #{idx} failed: {}", e.message),
            code,
            vec![format!("vault read {note_path} --outline")],
        )
    };

    for (idx, op) in ops.iter().enumerate() {
        let step = idx + 1;
        match op {
            BatchOperation::Section {
                heading,
                mode,
                content,
            } => {
                let Some(m) = SectionMode::from_str(mode) else {
                    return Err(AxiError::new(
                        format!("Batch op #{step}: invalid section mode '{mode}'"),
                        "VALIDATION_ERROR",
                        vec![],
                    ));
                };
                cur = edit_section(&cur, heading, m, content).map_err(|e| map_sec_err(step, e))?;
            }
            BatchOperation::Anchor { old_str, new_str } => {
                cur = replace_anchor(&cur, old_str, new_str).map_err(|e| map_sec_err(step, e))?;
            }
            BatchOperation::Frontmatter { key, value } => {
                cur = match value {
                    None | Some(Value::Null) => unset_frontmatter_field(&cur, key),
                    Some(Value::String(s)) => set_frontmatter_field(&cur, key, &coerce_value(s)),
                    Some(v) => set_frontmatter_field(&cur, key, v),
                };
            }
            BatchOperation::Insert {
                where_anchor,
                heading,
                content,
            } => match where_anchor.as_str() {
                "append" => cur = append_to_doc(&cur, content),
                "prepend" => cur = prepend_to_doc(&cur, content),
                "before-heading" | "before_heading" => {
                    let h = heading.as_deref().unwrap_or("");
                    cur = insert_at_heading(&cur, h, HeadingAnchor::BeforeHeading, content)
                        .map_err(|e| map_sec_err(step, e))?;
                }
                "after-heading" | "after_heading" => {
                    let h = heading.as_deref().unwrap_or("");
                    cur = insert_at_heading(&cur, h, HeadingAnchor::AfterHeading, content)
                        .map_err(|e| map_sec_err(step, e))?;
                }
                other => {
                    return Err(AxiError::new(
                        format!("Batch op #{step}: unknown insert where_anchor '{other}'"),
                        "VALIDATION_ERROR",
                        vec![],
                    ));
                }
            },
        }
    }

    let changed = cur != raw;
    if changed {
        fs::write(abs_path, &cur)
            .map_err(|e| AxiError::new(format!("Write error: {e}"), "INTERNAL_ERROR", vec![]))?;
        ctx.sync_after_mutation()?;
    }

    Ok(json!({
        "ok": true,
        "changed": changed,
        "file": note_path,
        "operationsApplied": ops.len()
    }))
}
