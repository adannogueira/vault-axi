use crate::config::resolve_abs_string;
use crate::markdown::sections::parse_doc;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

fn ignored_dirs() -> HashSet<&'static str> {
    HashSet::from([".obsidian", ".smart-env", ".trash", ".git", "node_modules", "target"])
}

/// List all markdown files under the vault, as vault-relative POSIX paths.
pub fn list_markdown_files(vault_root: &str) -> Vec<String> {
    let mut out = Vec::new();
    let root = Path::new(vault_root);
    let ignored = ignored_dirs();
    walk(root, root, &ignored, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, root: &Path, ignored: &HashSet<&str>, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') && ignored.contains(name_str.as_ref()) {
            continue;
        }
        let full = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            if ignored.contains(name_str.as_ref()) {
                continue;
            }
            walk(&full, root, ignored, out);
        } else if ft.is_file() && name_str.to_lowercase().ends_with(".md") {
            out.push(to_rel(root, &full));
        }
    }
}

pub fn to_rel(vault_root: &Path, abs_path: &Path) -> String {
    match abs_path.strip_prefix(vault_root) {
        Ok(rel) => rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => abs_path.to_string_lossy().replace('\\', "/"),
    }
}

/// Resolve a vault-relative path to an absolute path, refusing to escape the vault.
pub fn resolve_in_vault(vault_root: &str, rel_path: &str) -> Result<String, String> {
    let clean = rel_path.trim_start_matches(['/', '\\']);
    let root_resolved = resolve_abs_string(vault_root);
    let joined = PathBuf::from(&root_resolved).join(clean);
    let abs = resolve_abs_string(&joined.to_string_lossy());
    let sep = std::path::MAIN_SEPARATOR;
    let prefix = format!("{root_resolved}{sep}");
    if abs != root_resolved && !abs.starts_with(&prefix) {
        return Err(format!("Path escapes the vault: {rel_path}"));
    }
    Ok(abs)
}

/// Normalize a user-supplied note path: strip leading slash, add .md if missing.
pub fn normalize_note_path(rel_path: &str) -> String {
    let mut p = rel_path
        .trim_start_matches('/')
        .replace(std::path::MAIN_SEPARATOR, "/")
        .replace('\\', "/");
    if !p.to_lowercase().ends_with(".md") {
        p.push_str(".md");
    }
    p
}

#[derive(Debug, Clone, Copy)]
pub struct FileStat {
    pub mtime_ms: i64,
    pub size: i64,
}

pub fn file_stat(abs_path: &str) -> Option<FileStat> {
    let meta = fs::metadata(abs_path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let size = meta.len() as i64;
    Some(FileStat { mtime_ms, size })
}

pub fn derive_title(
    frontmatter: Option<&serde_json::Map<String, Value>>,
    raw: &str,
    rel_path: &str,
) -> String {
    if let Some(fm) = frontmatter {
        if let Some(Value::String(title)) = fm.get("title") {
            let trimmed = title.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    let doc = parse_doc(raw);
    if let Some(h1) = doc.headings.iter().find(|h| h.level == 1) {
        return h1.title.clone();
    }
    let base = match rel_path.rfind('/') {
        Some(idx) => &rel_path[idx + 1..],
        None => rel_path,
    };
    if base.to_lowercase().ends_with(".md") {
        base[..base.len() - 3].to_string()
    } else {
        base.to_string()
    }
}
