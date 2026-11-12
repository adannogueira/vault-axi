//! Vault path rules shared by every client.

/// Vault paths under this prefix belong to vault-axi itself (e.g. the keyring) and are never
/// synced as files.
pub const RESERVED_PREFIX: &str = ".vault-axi/";

pub fn is_reserved(rel_path: &str) -> bool {
    rel_path.starts_with(RESERVED_PREFIX)
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
