use crate::args::{flag_bool, flag_string, parse_args, resolve_content};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::markdown::sections::{
    append_to_doc, insert_at_heading, prepend_to_doc, HeadingAnchor, SectionError, SectionErrorKind,
};
use crate::vault::{normalize_note_path, resolve_in_vault};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub fn insert_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(usage("Missing file path"));
    };
    let note_path = normalize_note_path(rel);
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
    let content = resolve_content(&parsed.flags)?;

    let before = flag_string(&parsed.flags, "before-heading");
    let after = flag_string(&parsed.flags, "after-heading");

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

    let (out, where_str) = if let Some(b) = before {
        (
            insert_at_heading(&raw, &b, HeadingAnchor::BeforeHeading, &content)
                .map_err(map_sec_err)?,
            format!("before \"{b}\""),
        )
    } else if let Some(a) = after {
        (
            insert_at_heading(&raw, &a, HeadingAnchor::AfterHeading, &content)
                .map_err(map_sec_err)?,
            format!("after \"{a}\""),
        )
    } else if flag_bool(&parsed.flags, "prepend") {
        (prepend_to_doc(&raw, &content), "document start".to_string())
    } else if flag_bool(&parsed.flags, "append") {
        (append_to_doc(&raw, &content), "document end".to_string())
    } else {
        return Err(usage(
            "Choose --before-heading, --after-heading, --append, or --prepend",
        ));
    };

    if out == raw {
        return Ok(json!({
            "ok": true,
            "changed": false,
            "file": note_path,
            "note": "Empty content, nothing inserted"
        }));
    }
    fs::write(abs_path, &out)
        .map_err(|e| AxiError::new(format!("Write error: {e}"), "INTERNAL_ERROR", vec![]))?;
    ctx.sync_after_mutation()?;
    Ok(json!({
        "ok": true,
        "changed": true,
        "file": note_path,
        "inserted": where_str
    }))
}

fn usage(message: &str) -> AxiError {
    AxiError::new(
        message,
        "VALIDATION_ERROR",
        vec![
            "vault insert <file> --after-heading \"<h>\" --content \"...\"".to_string(),
            "vault insert <file> --before-heading \"<h>\" --content \"...\"".to_string(),
            "vault insert <file> --append --content \"...\"   (or --prepend)".to_string(),
        ],
    )
}
