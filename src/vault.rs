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
    if rel_path.contains('\0') {
        return Err("Invalid note path: contains null byte".to_string());
    }
    let normalized = rel_path.trim().replace('\\', "/");
    let clean = normalized.trim_start_matches('/');
    if clean.is_empty() {
        return Err(format!("Path escapes the vault: {rel_path}"));
    }

    let mut depth: usize = 0;
    for comp in Path::new(clean).components() {
        match comp {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(format!("Path escapes the vault: {rel_path}"));
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if depth == 0 {
                    return Err(format!("Path escapes the vault: {rel_path}"));
                }
                depth -= 1;
            }
            std::path::Component::Normal(os) => {
                let s = os.to_string_lossy();
                if s.eq_ignore_ascii_case(".md") {
                    return Err(format!("Invalid note path: {rel_path}"));
                }
                depth += 1;
            }
        }
    }
    if depth == 0 {
        return Err(format!("Path escapes the vault: {rel_path}"));
    }

    let root_resolved = resolve_abs_string(vault_root);
    let joined = PathBuf::from(&root_resolved).join(clean);
    let abs = resolve_abs_string(&joined.to_string_lossy());
    let sep = std::path::MAIN_SEPARATOR;
    let prefix = if root_resolved.ends_with(sep) {
        root_resolved.clone()
    } else {
        format!("{root_resolved}{sep}")
    };
    if !abs.starts_with(&prefix) {
        return Err(format!("Path escapes the vault: {rel_path}"));
    }
    Ok(abs)
}

/// Normalize a user-supplied note path: strip leading slash, add .md if missing.
pub fn normalize_note_path(rel_path: &str) -> String {
    let mut p = rel_path
        .trim()
        .replace(std::path::MAIN_SEPARATOR, "/")
        .replace('\\', "/");
    while let Some(stripped) = p.strip_prefix("./").or_else(|| p.strip_prefix('/')) {
        p = stripped.to_string();
    }
    while p.contains("//") {
        p = p.replace("//", "/");
    }
    while p.contains("/./") {
        p = p.replace("/./", "/");
    }
    if p.is_empty() || p == "." {
        return String::new();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_note_path_handles_slashes_dots_and_extensions() {
        assert_eq!(normalize_note_path("plans/foo"), "plans/foo.md");
        assert_eq!(normalize_note_path("/plans/foo.md"), "plans/foo.md");
        assert_eq!(normalize_note_path("\\plans\\foo.MD"), "plans/foo.MD");
        assert_eq!(normalize_note_path("./plans//./foo"), "plans/foo.md");
        assert_eq!(normalize_note_path(""), "");
        assert_eq!(normalize_note_path("."), "");
        assert_eq!(normalize_note_path("///"), "");
    }

    #[test]
    fn resolve_in_vault_blocks_all_escape_and_invalid_vectors() {
        let dir = std::env::temp_dir().join(format!("vx-vault-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let root = dir.to_string_lossy().to_string();

        // Valid paths
        assert!(resolve_in_vault(&root, "note.md").is_ok());
        assert!(resolve_in_vault(&root, "a/b/../note.md").is_ok());

        // Traversal escapes
        assert!(resolve_in_vault(&root, "../outside.md").is_err());
        assert!(resolve_in_vault(&root, "a/../../outside.md").is_err());
        assert!(resolve_in_vault(&root, "..\\outside.md").is_err());
        assert!(resolve_in_vault(&root, "../../../../../../../../etc/passwd").is_err());

        // Root / empty / null byte / bare .md
        assert!(resolve_in_vault(&root, "").is_err());
        assert!(resolve_in_vault(&root, ".").is_err());
        assert!(resolve_in_vault(&root, "/").is_err());
        assert!(resolve_in_vault(&root, "a/..").is_err());
        assert!(resolve_in_vault(&root, ".md").is_err());
        assert!(resolve_in_vault(&root, "plans/.md").is_err());
        assert!(resolve_in_vault(&root, "bad\0name.md").is_err());

        if cfg!(windows) {
            assert!(resolve_in_vault(&root, "C:\\Windows\\win.ini").is_err());
            assert!(resolve_in_vault(&root, "C:evil.md").is_err());
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn derive_title_precedence_frontmatter_then_h1_then_filename() {
        let mut fm = serde_json::Map::new();
        fm.insert("title".to_string(), json!("From Frontmatter"));
        assert_eq!(
            derive_title(Some(&fm), "# From H1\n", "plans/file-stem.md"),
            "From Frontmatter"
        );

        // Blank frontmatter title falls back to H1
        fm.insert("title".to_string(), json!("   "));
        assert_eq!(
            derive_title(Some(&fm), "## H2 First\n# Real H1\n", "plans/file-stem.md"),
            "Real H1"
        );

        // No H1 (only H2) falls back to filename stem
        assert_eq!(
            derive_title(None, "## Only H2\nbody\n", "plans/file-stem.md"),
            "file-stem"
        );
        assert_eq!(derive_title(None, "just text", "bare_name"), "bare_name");
    }

    #[test]
    fn list_markdown_files_ignores_special_dirs_and_non_md() {
        let dir = std::env::temp_dir().join(format!("vx-walk-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".obsidian")).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::create_dir_all(dir.join("target")).unwrap();
        fs::create_dir_all(dir.join("notes")).unwrap();

        fs::write(dir.join(".obsidian").join("ignore.md"), "# no").unwrap();
        fs::write(dir.join(".git").join("ignore.md"), "# no").unwrap();
        fs::write(dir.join("node_modules").join("ignore.md"), "# no").unwrap();
        fs::write(dir.join("target").join("ignore.md"), "# no").unwrap();
        fs::write(dir.join("notes").join("readme.txt"), "no").unwrap();
        fs::write(dir.join("notes").join("upper.MD"), "# yes").unwrap();
        fs::write(dir.join("root.md"), "# yes").unwrap();

        let files = list_markdown_files(&dir.to_string_lossy());
        assert_eq!(files, vec!["notes/upper.MD", "root.md"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
