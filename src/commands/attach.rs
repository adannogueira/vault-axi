use crate::args::{flag_string, parse_args};
use crate::axi::AxiError;
use crate::commands::history::record_edit_transition;
use crate::context::Ctx;
use crate::index::reconcile::ReconcileOpts;
use crate::markdown::links::parse_wiki_links;
use crate::markdown::sections::{edit_section, SectionMode};
use crate::vault::{
    derive_title, file_stat, is_image_attachment, list_attachment_files, normalize_note_path,
    resolve_attachment_target, resolve_in_vault,
};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

pub fn attach_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);

    let Some(note_arg) = parsed.positionals.first() else {
        return Err(AxiError::new(
            "Missing target note and attachment file path",
            "VALIDATION_ERROR",
            vec!["Usage: vault attach <note> <file_path> [--folder assets] [--section <heading>] [--alias <label>]".to_string()],
        ));
    };
    let Some(src_arg) = parsed.positionals.get(1) else {
        return Err(AxiError::new(
            "Missing attachment file path",
            "VALIDATION_ERROR",
            vec!["Usage: vault attach <note> <file_path> [--folder assets] [--section <heading>] [--alias <label>]".to_string()],
        ));
    };

    let note_path = normalize_note_path(note_arg);
    let note_abs = resolve_in_vault(&root, &note_path)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;

    let folder = flag_string(&parsed.flags, "folder")
        .unwrap_or_else(|| "assets".to_string())
        .trim_matches('/')
        .replace('\\', "/");
    let section = flag_string(&parsed.flags, "section");
    let alias = flag_string(&parsed.flags, "alias");

    // Determine source file location (either external path or already inside vault)
    let src_path_candidate = Path::new(src_arg);
    let resolved_src = if src_path_candidate.exists() {
        src_path_candidate.to_path_buf()
    } else if let Ok(in_vault) = resolve_in_vault(&root, src_arg) {
        let p = Path::new(&in_vault);
        if p.exists() {
            p.to_path_buf()
        } else {
            return Err(AxiError::new(
                format!("Attachment file not found: {src_arg}"),
                "NOT_FOUND",
                vec!["Provide a valid path to an image or document file.".to_string()],
            ));
        }
    } else {
        return Err(AxiError::new(
            format!("Attachment file not found: {src_arg}"),
            "NOT_FOUND",
            vec!["Provide a valid path to an image or document file.".to_string()],
        ));
    };

    let file_name = resolved_src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| {
            AxiError::new(
                format!("Invalid attachment file name: {src_arg}"),
                "VALIDATION_ERROR",
                vec![],
            )
        })?;

    let dest_rel = if folder.is_empty() {
        file_name.clone()
    } else {
        format!("{folder}/{file_name}")
    };
    let dest_abs = resolve_in_vault(&root, &dest_rel)
        .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;

    if resolved_src != Path::new(&dest_abs) {
        if let Some(parent) = Path::new(&dest_abs).parent() {
            fs::create_dir_all(parent)
                .map_err(|e| AxiError::new(format!("IO error: {e}"), "IO_ERROR", vec![]))?;
        }
        fs::copy(&resolved_src, &dest_abs).map_err(|e| {
            AxiError::new(
                format!("Failed to copy attachment into vault: {e}"),
                "IO_ERROR",
                vec![],
            )
        })?;
    }

    let embed_syntax = if let Some(ref a) = alias {
        format!("![[{dest_rel}|{a}]]")
    } else {
        format!("![[{dest_rel}]]")
    };

    let prev_note = fs::read_to_string(&note_abs).ok();
    let updated_note = if let Some(ref raw) = prev_note {
        if let Some(ref sec_heading) = section {
            match edit_section(raw, sec_heading, SectionMode::Append, &format!("{embed_syntax}\n"))
            {
                Ok(text) => text,
                Err(_) => {
                    let trimmed = raw.trim_end_matches(&['\r', '\n'][..]);
                    format!("{trimmed}\n\n## {sec_heading}\n\n{embed_syntax}\n")
                }
            }
        } else if raw.contains(&embed_syntax) {
            raw.clone()
        } else {
            let trimmed = raw.trim_end_matches(&['\r', '\n'][..]);
            format!("{trimmed}\n\n{embed_syntax}\n")
        }
    } else {
        if let Some(parent) = Path::new(&note_abs).parent() {
            fs::create_dir_all(parent)
                .map_err(|e| AxiError::new(format!("IO error: {e}"), "IO_ERROR", vec![]))?;
        }
        let title = derive_title(None, "", &note_path);
        if let Some(ref sec_heading) = section {
            format!("# {title}\n\n## {sec_heading}\n\n{embed_syntax}\n")
        } else {
            format!("# {title}\n\n{embed_syntax}\n")
        }
    };

    fs::write(&note_abs, &updated_note)
        .map_err(|e| AxiError::new(format!("IO error: {e}"), "IO_ERROR", vec![]))?;

    let is_image = is_image_attachment(&dest_rel);
    let mut ocr_text: Option<String> = None;
    if let Ok(db) = ctx.db() {
        record_edit_transition(db, &note_path, prev_note.as_deref(), &updated_note, "attach");
        if is_image {
            ocr_text = crate::ocr::extract_image_ocr(db, &dest_abs);
        }
    }

    let _ = ctx.sync(ReconcileOpts {
        force: true,
        with_embeddings: Some(false),
    });

    let ocr_preview = ocr_text.as_deref().and_then(|t| {
        let trimmed = t.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.chars().take(160).collect::<String>())
        }
    });

    Ok(json!({
        "ok": true,
        "note": note_path,
        "attachment": dest_rel,
        "embed": embed_syntax,
        "isImage": is_image,
        "ocrExtracted": ocr_preview.is_some(),
        "ocrPreview": ocr_preview
    }))
}

pub fn attachments_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let all_attachments = list_attachment_files(&root);

    let filter_note = parsed.positionals.first().map(|p| normalize_note_path(p));
    let filtered: Vec<String> = if let Some(ref note_path) = filter_note {
        let note_abs = resolve_in_vault(&root, note_path)
            .map_err(|e| AxiError::new(e, "VALIDATION_ERROR", vec![]))?;
        let raw = fs::read_to_string(&note_abs).map_err(|_| {
            AxiError::new(
                format!("Note not found: {note_path}"),
                "NOT_FOUND",
                vec![],
            )
        })?;
        let mut refs: Vec<String> = Vec::new();
        for link in parse_wiki_links(&raw) {
            if let Some(att) = resolve_attachment_target(&link.raw, &all_attachments) {
                if !refs.contains(&att) {
                    refs.push(att);
                }
            }
        }
        for part in raw.split("](").skip(1) {
            if let Some(close) = part.find(')') {
                let md_target = part[..close].split_whitespace().next().unwrap_or("");
                if let Some(att) = resolve_attachment_target(md_target, &all_attachments) {
                    if !refs.contains(&att) {
                        refs.push(att);
                    }
                }
            }
        }
        refs
    } else {
        all_attachments
    };

    let db = ctx.db()?;
    let mut items = Vec::new();
    for att in &filtered {
        let Ok(abs) = resolve_in_vault(&root, att) else {
            continue;
        };
        let size = file_stat(&abs).map(|s| s.size).unwrap_or(0);
        let is_img = is_image_attachment(att);
        let ocr = if is_img {
            crate::ocr::extract_image_ocr(db, &abs).and_then(|t| {
                let trimmed = t.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            })
        } else {
            None
        };
        items.push(json!({
            "path": att,
            "size": size,
            "isImage": is_img,
            "ocrIndexed": ocr.is_some(),
            "ocrPreview": ocr.map(|t| t.chars().take(120).collect::<String>())
        }));
    }

    Ok(json!({
        "ok": true,
        "note": filter_note,
        "count": items.len(),
        "attachments": items
    }))
}
