use crate::args::parse_args;
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::vault::{normalize_note_path, resolve_in_vault};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub fn path_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Ok(json!({
            "vault": root,
            "hint": [
                "Write .md files anywhere under this folder; vault indexes them on the next read/search",
                "vault path <note> resolves and prepares a specific note's path"
            ]
        }));
    };

    let note_path = normalize_note_path(rel);
    let abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
    let abs_path = Path::new(&abs);
    if let Some(parent) = abs_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let exists = abs_path.exists();

    let next = if exists {
        vec![
            format!("Note exists - for a partial change use `vault update {note_path} --section \"<h>\"`"),
            format!("To overwrite, write your file tool directly to: {abs}"),
        ]
    } else {
        vec![
            format!("Write your Markdown directly to: {abs}"),
            format!("Then `vault read {note_path} --outline`"),
        ]
    };

    Ok(json!({
        "file": note_path,
        "path": abs,
        "exists": exists,
        "indexing": "automatic on next `vault` read/search",
        "next": next
    }))
}
