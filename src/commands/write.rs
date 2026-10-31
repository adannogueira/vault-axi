use crate::args::{flag_bool, parse_args, resolve_content};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use crate::vault::{normalize_note_path, resolve_in_vault};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub fn write_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let Some(rel) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing file path",
            "VALIDATION_ERROR",
            vec!["Usage: vault write <file> --content \"...\"  (or --content - to read stdin)".to_string()],
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
    let mut content = resolve_content(&parsed.flags)?;
    if !content.ends_with('\n') {
        content.push('\n');
    }

    if let Some(parent) = abs_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(abs_path, &content)
        .map_err(|e| AxiError::new(format!("Write error: {e}"), "INTERNAL_ERROR", vec![]))?;
    ctx.sync(ReconcileOpts {
        force: false,
        with_embeddings: Some(false),
    })?;

    Ok(json!({
        "ok": true,
        "action": if existed { "overwritten" } else { "created" },
        "file": note_path,
        "chars": content.chars().count(),
        "next": [format!("vault read {note_path} --outline"), format!("vault links {note_path}")]
    }))
}
