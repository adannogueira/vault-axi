//! Vault path rules shared by every client.

/// Vault paths under this prefix belong to vault-axi itself (e.g. the keyring) and are never
/// synced as files.
pub const RESERVED_PREFIX: &str = ".vault-axi/";

pub fn is_reserved(rel_path: &str) -> bool {
    rel_path.starts_with(RESERVED_PREFIX)
}

/// Directories never synced, wherever they appear.
pub const IGNORED_DIRS: &[&str] = &[".smart-env", ".trash", ".git", "node_modules", "target"];
/// Per-device Obsidian files that must not travel between devices.
const OBSIDIAN_LOCAL_NAMES: &[&str] = &[
    "workspace.json",
    "workspace-mobile.json",
    ".DS_Store",
    "cache",
];
/// The mobile plugin's own settings are per-device.
pub const PLUGIN_DATA_PATH: &str = ".obsidian/plugins/vault-axi-sync/data.json";
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_OBSIDIAN_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Whether a vault file (vault-relative, `/`-separated) is synced. Every client enumerates its
/// files and filters them through this, so all devices agree on the synced set.
///
/// - Notes (`.md`): everywhere except ignored directories.
/// - `.obsidian/**` up to 10 MiB, minus workspace layouts, caches and this plugin's settings.
/// - Other files (attachments) up to 25 MiB, outside hidden directories, minus partial downloads.
pub fn is_syncable(rel_path: &str, size: u64) -> bool {
    if rel_path.is_empty() || is_reserved(rel_path) {
        return false;
    }
    let segments: Vec<&str> = rel_path.split('/').collect();
    let (file, dirs) = segments
        .split_last()
        .expect("split yields at least one segment");

    if segments[0] == ".obsidian" {
        return !dirs.is_empty()
            && rel_path != PLUGIN_DATA_PATH
            && !segments.iter().any(|s| OBSIDIAN_LOCAL_NAMES.contains(s))
            && size <= MAX_OBSIDIAN_FILE_BYTES;
    }
    if dirs.iter().any(|d| IGNORED_DIRS.contains(d)) {
        return false;
    }
    let lower = file.to_ascii_lowercase();
    if lower.ends_with(".md") {
        return true;
    }
    !segments.iter().any(|s| s.starts_with('.'))
        && !lower.ends_with(".tmp")
        && !lower.ends_with(".part")
        && size <= MAX_ATTACHMENT_BYTES
}

/// Name of the copy that keeps the local side of an unmergeable concurrent edit,
/// e.g. `plans/a.md` -> `plans/a.conflict-1700000000.md`.
pub fn conflict_copy_path(rel_path: &str, ts_ms: i64) -> String {
    let ts_sec = (ts_ms / 1000).max(1);
    let file_start = rel_path.rfind('/').map_or(0, |i| i + 1);
    match rel_path[file_start..].rfind('.') {
        Some(dot) if dot > 0 => {
            let dot = file_start + dot;
            format!("{}.conflict-{ts_sec}{}", &rel_path[..dot], &rel_path[dot..])
        }
        _ => format!("{rel_path}.conflict-{ts_sec}.md"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syncable_rules() {
        let yes = [
            "a.md",
            "notes/deep/b.MD",
            ".hidden/note.md",
            "img/photo.png",
            ".obsidian/app.json",
            ".obsidian/plugins/dataview/main.js",
            ".obsidian/plugins/vault-axi-sync/main.js",
        ];
        for p in yes {
            assert!(is_syncable(p, 10), "{p} should sync");
        }
        let no = [
            "",
            ".vault-axi/keyring.json",
            ".obsidian",
            ".obsidian/workspace.json",
            ".obsidian/workspace-mobile.json",
            ".obsidian/cache/x.json",
            ".obsidian/plugins/vault-axi-sync/data.json",
            ".git/config",
            "node_modules/x/readme.md",
            "a/.trash/old.md",
            ".hidden/image.png",
            "img/.DS_Store",
            "dl/file.part",
            "dl/file.tmp",
        ];
        for p in no {
            assert!(!is_syncable(p, 10), "{p} should not sync");
        }
        assert!(is_syncable("big.md", 100 * 1024 * 1024));
        assert!(!is_syncable("big.pdf", MAX_ATTACHMENT_BYTES + 1));
        assert!(is_syncable("ok.pdf", MAX_ATTACHMENT_BYTES));
        assert!(!is_syncable(
            ".obsidian/big.js",
            MAX_OBSIDIAN_FILE_BYTES + 1
        ));
    }

    #[test]
    fn reserved_paths() {
        assert!(is_reserved(crate::crypto::keyring::KEYRING_PATH));
        assert!(!is_reserved("notes/.vault-axi/x.md"));
        assert!(!is_reserved(".vault-axi-notes/x.md"));
    }

    #[test]
    fn conflict_copy_names() {
        assert_eq!(
            conflict_copy_path("plans/roadmap.md", 1_700_005_000_000),
            "plans/roadmap.conflict-1700005000.md"
        );
        assert_eq!(
            conflict_copy_path("img/photo.png", 2_000),
            "img/photo.conflict-2.png"
        );
        assert_eq!(conflict_copy_path("README", 0), "README.conflict-1.md");
        // A dot in a folder name is not an extension.
        assert_eq!(
            conflict_copy_path("v1.2/notes", 5_000),
            "v1.2/notes.conflict-5.md"
        );
        assert_eq!(
            conflict_copy_path(".hidden", 5_000),
            ".hidden.conflict-5.md"
        );
    }
}
