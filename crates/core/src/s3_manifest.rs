//! The S3 backend's manifest: the single source of truth for which object holds each path's
//! current version. Shared so every client reads and writes the same format.

use crate::hash::sha256_hex;
use crate::paths::RESERVED_PREFIX;
use crate::plan::RemoteFile;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MANIFEST_PATH: &str = ".vault-manifest.json";
const VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub hash: String,
    #[serde(default)]
    pub key_id: String,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub mtime: i64,
    /// Object key relative to the prefix; `None` for tombstones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub entries: BTreeMap<String, Entry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: VERSION,
            entries: BTreeMap::new(),
        }
    }
}

impl Manifest {
    /// Parse a v2 manifest, or a vault-axi <= 1.1 one (`{"path": "hash:keyId"}`, objects stored
    /// at their plain path), which is upgraded in memory.
    pub fn parse(body: &str) -> Result<Self, String> {
        if let Ok(m) = serde_json::from_str::<Manifest>(body) {
            if m.version == VERSION {
                return Ok(m);
            }
        }
        let legacy: BTreeMap<String, String> = serde_json::from_str(body)
            .map_err(|e| format!("The S3 manifest is unreadable: {e}"))?;
        let entries = legacy
            .into_iter()
            .map(|(path, value)| {
                let (hash, key_id) = value.split_once(':').unwrap_or((&value, ""));
                let entry = Entry {
                    hash: hash.to_string(),
                    key_id: key_id.to_string(),
                    deleted: false,
                    mtime: 0,
                    object: Some(path.clone()),
                };
                (path, entry)
            })
            .collect();
        Ok(Self {
            version: VERSION,
            entries,
        })
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("manifest serializes")
    }

    pub fn remote_files(&self) -> Vec<RemoteFile> {
        self.entries
            .iter()
            .map(|(path, e)| RemoteFile {
                path: path.clone(),
                hash: e.hash.clone(),
                deleted: e.deleted,
                key_id: e.key_id.clone(),
            })
            .collect()
    }
}

/// Content-addressed object key for one version of a path, so concurrent writers never
/// overwrite each other's data.
pub fn object_key(path: &str, hash: &str, key_id: &str) -> String {
    let path_id = &sha256_hex(path.as_bytes())[..32];
    let key = if key_id.is_empty() { "plain" } else { key_id };
    format!("{RESERVED_PREFIX}objects/{path_id}/{hash}.{key}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_manifests_are_upgraded_on_read() {
        let m = Manifest::parse(r#"{"a.md": "h1:abcd", "img/x.png": "h2"}"#).unwrap();
        assert_eq!(m.version, 2);
        let a = &m.entries["a.md"];
        assert_eq!((a.hash.as_str(), a.key_id.as_str()), ("h1", "abcd"));
        assert_eq!(a.object.as_deref(), Some("a.md"));
        assert_eq!(m.entries["img/x.png"].key_id, "");
    }

    #[test]
    fn v2_roundtrip_and_remote_files() {
        let mut m = Manifest::default();
        m.entries.insert(
            "a.md".into(),
            Entry {
                hash: "h".into(),
                key_id: "k2_1".into(),
                deleted: false,
                mtime: 7,
                object: Some(object_key("a.md", "h", "k2_1")),
            },
        );
        let json = m.to_json();
        assert!(json.contains("\"keyId\"") && json.contains("\"version\": 2"));
        assert_eq!(Manifest::parse(&json).unwrap(), m);
        assert!(Manifest::parse("not json").is_err());
        assert_eq!(m.remote_files()[0].key_id, "k2_1");
    }

    #[test]
    fn object_keys_are_reserved_and_content_addressed() {
        let k = object_key("notes/a.md", "abc", "k2_1");
        assert!(k.starts_with(".vault-axi/objects/"));
        assert!(k.ends_with("/abc.k2_1"));
        assert_ne!(k, object_key("notes/b.md", "abc", "k2_1"));
        assert_ne!(k, object_key("notes/a.md", "abc", "k2_2"));
        assert!(object_key("a.md", "h", "").ends_with("h.plain"));
    }
}
