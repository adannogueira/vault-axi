//! S3 / R2 backend over plain HTTP: a JSON manifest plus one object per file version.
//!
//! Objects are content-addressed (`.vault-axi/objects/<path id>/<hash>.<key id>`), so concurrent
//! writers never overwrite each other's data; the manifest is the single source of truth and is
//! committed with `If-Match` (or `If-None-Match: *` when creating it). A rejected commit means
//! another device synced first, and the run is retried against its manifest.
//!
//! Manifests written by vault-axi <= 1.1 (`{"path": "hash:keyId"}`, objects at their plain
//! path) are read transparently and upgraded on the next commit.

use super::engine::{Fetched, Put, PutOutcome, Remote, REMOTE_CHANGED};
use crate::axi::AxiError;
use crate::crypto::{invalid_keyring_error, KeyringStore, StoredKeyring};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use vault_axi_core::crypto::keyring::{Keyring, KEYRING_PATH};
use vault_axi_core::hash::sha256_hex;
use vault_axi_core::paths::RESERVED_PREFIX;
use vault_axi_core::plan::RemoteFile;

const MANIFEST: &str = ".vault-manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    hash: String,
    #[serde(default)]
    key_id: String,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    mtime: i64,
    /// Object key relative to the prefix; `None` for tombstones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    object: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

impl Manifest {
    fn parse(body: &str) -> Result<Self, AxiError> {
        if let Ok(m) = serde_json::from_str::<Manifest>(body) {
            if m.version == 2 {
                return Ok(m);
            }
        }
        let legacy: BTreeMap<String, String> = serde_json::from_str(body).map_err(|e| {
            AxiError::new(
                format!("The S3 manifest is unreadable: {e}"),
                "SYNC_ERROR",
                vec![],
            )
        })?;
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
            version: 2,
            entries,
        })
    }
}

fn object_key(path: &str, hash: &str, key_id: &str) -> String {
    let path_id = &sha256_hex(path.as_bytes())[..32];
    let key = if key_id.is_empty() { "plain" } else { key_id };
    format!("{RESERVED_PREFIX}objects/{path_id}/{hash}.{key}")
}

pub struct S3Remote {
    base: String,
    manifest: Manifest,
    /// ETag of the manifest as listed; `None` when no manifest existed.
    etag: Option<String>,
    dirty: bool,
    /// Objects the committed manifest no longer references (deleted after the commit).
    superseded: BTreeSet<String>,
}

fn s3_error(context: &str, e: ureq::Error) -> AxiError {
    AxiError::new(format!("{context}: {e}"), "SYNC_ERROR", vec![])
}

impl S3Remote {
    pub fn new(endpoint: &str, bucket: &str, prefix: &str) -> Self {
        let endpoint = endpoint.trim_end_matches('/');
        let prefix = prefix.trim_matches('/');
        let base = if prefix.is_empty() {
            format!("{endpoint}/{bucket}")
        } else {
            format!("{endpoint}/{bucket}/{prefix}")
        };
        Self {
            base,
            manifest: Manifest::default(),
            etag: None,
            dirty: false,
            superseded: BTreeSet::new(),
        }
    }

    fn url(&self, key: &str) -> String {
        format!("{}/{key}", self.base)
    }

    fn replace_entry(&mut self, path: &str, entry: Entry) {
        if let Some(old) = self.manifest.entries.insert(path.to_string(), entry) {
            if let Some(obj) = old.object {
                self.superseded.insert(obj);
            }
        }
        self.dirty = true;
    }
}

impl Remote for S3Remote {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn list(&mut self) -> Result<Vec<RemoteFile>, AxiError> {
        self.dirty = false;
        self.superseded.clear();
        match ureq::get(&self.url(MANIFEST)).call() {
            Ok(resp) => {
                self.etag = resp.header("ETag").map(str::to_string);
                self.manifest = Manifest::parse(&resp.into_string().unwrap_or_default())?;
            }
            Err(ureq::Error::Status(404 | 403, _)) => {
                self.etag = None;
                self.manifest = Manifest {
                    version: 2,
                    entries: BTreeMap::new(),
                };
            }
            Err(e) => return Err(s3_error("Fetching the S3 manifest failed", e)),
        }
        Ok(self
            .manifest
            .entries
            .iter()
            .map(|(path, e)| RemoteFile {
                path: path.clone(),
                hash: e.hash.clone(),
                deleted: e.deleted,
                key_id: e.key_id.clone(),
            })
            .collect())
    }

    fn fetch(&mut self, path: &str) -> Result<Option<Fetched>, AxiError> {
        let Some(entry) = self.manifest.entries.get(path).filter(|e| !e.deleted) else {
            return Ok(None);
        };
        let Some(object) = &entry.object else {
            return Ok(None);
        };
        match ureq::get(&self.url(object)).call() {
            Ok(resp) => Ok(Some(Fetched {
                content: resp.into_string().unwrap_or_default(),
                hash: entry.hash.clone(),
                mtime_ms: entry.mtime,
            })),
            Err(ureq::Error::Status(404, _)) => Err(AxiError::new(
                format!("S3 object for '{path}' is missing; another device is mid-sync"),
                REMOTE_CHANGED,
                vec![],
            )),
            Err(e) => Err(s3_error(&format!("Fetching '{path}' failed"), e)),
        }
    }

    fn put(&mut self, items: &[Put]) -> Vec<PutOutcome> {
        items
            .iter()
            .map(|put| {
                let live = self
                    .manifest
                    .entries
                    .get(&put.path)
                    .filter(|e| !e.deleted)
                    .map(|e| e.hash.as_str());
                if live != put.expected_remote_hash.as_deref() {
                    return PutOutcome::Conflict;
                }
                let object = object_key(&put.path, &put.hash, &put.key_id);
                if let Err(e) = ureq::put(&self.url(&object))
                    .set("Content-Type", "text/plain; charset=utf-8")
                    .send_string(&put.content)
                {
                    return PutOutcome::Failed(e.to_string());
                }
                self.superseded.remove(&object);
                self.replace_entry(
                    &put.path,
                    Entry {
                        hash: put.hash.clone(),
                        key_id: put.key_id.clone(),
                        deleted: false,
                        mtime: put.mtime_ms,
                        object: Some(object),
                    },
                );
                PutOutcome::Stored
            })
            .collect()
    }

    fn tombstone(&mut self, path: &str, mtime_ms: i64) -> Result<(), AxiError> {
        self.replace_entry(
            path,
            Entry {
                hash: String::new(),
                key_id: String::new(),
                deleted: true,
                mtime: mtime_ms,
                object: None,
            },
        );
        Ok(())
    }

    fn finish(&mut self) -> Result<(), AxiError> {
        if !self.dirty {
            return Ok(());
        }
        let body = serde_json::to_string_pretty(&self.manifest).expect("manifest serializes");
        let req = ureq::put(&self.url(MANIFEST)).set("Content-Type", "application/json");
        let req = match &self.etag {
            Some(etag) => req.set("If-Match", etag),
            None => req.set("If-None-Match", "*"),
        };
        match req.send_string(&body) {
            Ok(_) => {}
            Err(ureq::Error::Status(412 | 409, _)) => {
                return Err(AxiError::new(
                    "The S3 manifest changed during sync",
                    REMOTE_CHANGED,
                    vec!["Run `vault sync` again".to_string()],
                ))
            }
            Err(e) => return Err(s3_error("Committing the S3 manifest failed", e)),
        }
        self.dirty = false;
        let referenced: BTreeSet<&String> = self
            .manifest
            .entries
            .values()
            .filter_map(|e| e.object.as_ref())
            .collect();
        for object in std::mem::take(&mut self.superseded) {
            if !referenced.contains(&object) {
                let _ = ureq::delete(&self.url(&object)).call();
            }
        }
        Ok(())
    }
}

/// The keyring object sits next to the manifest, created with `If-None-Match: *` and replaced
/// with `If-Match: <etag>`.
impl KeyringStore for S3Remote {
    fn fetch_keyring(&mut self) -> Result<Option<StoredKeyring>, AxiError> {
        let resp = match ureq::get(&self.url(KEYRING_PATH)).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(404 | 403, _)) => return Ok(None),
            Err(e) => return Err(s3_error("Fetching the vault keyring failed", e)),
        };
        let version = resp.header("ETag").unwrap_or_default().to_string();
        let body = resp.into_string().unwrap_or_default();
        let keyring =
            Keyring::from_json(&body).map_err(|e| invalid_keyring_error(&e.to_string()))?;
        Ok(Some(StoredKeyring { keyring, version }))
    }

    fn create_keyring(&mut self, keyring: &Keyring) -> Result<bool, AxiError> {
        match ureq::put(&self.url(KEYRING_PATH))
            .set("Content-Type", "application/json")
            .set("If-None-Match", "*")
            .send_string(&keyring.to_json())
        {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(412 | 409, _)) => Ok(false),
            Err(e) => Err(s3_error("Creating the vault keyring failed", e)),
        }
    }

    fn replace_keyring(&mut self, version: &str, next: &Keyring) -> Result<(), AxiError> {
        match ureq::put(&self.url(KEYRING_PATH))
            .set("Content-Type", "application/json")
            .set("If-Match", version)
            .send_string(&next.to_json())
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(412, _)) => Err(AxiError::new(
                "The vault keyring changed on another device while updating it",
                "KEYRING_CHANGED",
                vec!["Run the command again".to_string()],
            )),
            Err(e) => Err(s3_error("Updating the vault keyring failed", e)),
        }
    }
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
    fn v2_manifest_roundtrip() {
        let mut m = Manifest {
            version: 2,
            entries: BTreeMap::new(),
        };
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
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"keyId\""));
        let back = Manifest::parse(&json).unwrap();
        assert_eq!(back.entries, m.entries);
        assert!(Manifest::parse("not json").is_err());
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

    #[test]
    fn base_url_normalizes_slashes() {
        assert_eq!(
            S3Remote::new("https://r2.dev/", "b", "/p/").base,
            "https://r2.dev/b/p"
        );
        assert_eq!(
            S3Remote::new("https://r2.dev", "b", "").base,
            "https://r2.dev/b"
        );
    }
}

/// End-to-end tests against an in-process fake S3 that honours ETags and conditional writes.
#[cfg(test)]
mod http_tests {
    use super::*;
    use crate::config::{RemoteBackendConfig, VaultConfig};
    use crate::crypto::KeyCache;
    use crate::sync::engine;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use vault_axi_core::crypto::{v1, v2};
    use vault_axi_core::hash::sha1_hex;

    type Objects = Arc<Mutex<HashMap<String, (Vec<u8>, u64)>>>;

    struct FakeS3 {
        endpoint: String,
        objects: Objects,
    }

    impl FakeS3 {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let objects: Objects = Arc::default();
            let store = objects.clone();
            std::thread::spawn(move || {
                let mut version = 0u64;
                for stream in listener.incoming().flatten() {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line.is_empty() {
                        continue;
                    }
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let key = parts
                        .next()
                        .unwrap_or_default()
                        .trim_start_matches('/')
                        .to_string();
                    let mut headers = HashMap::new();
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        let h = h.trim_end();
                        if h.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                    let len: usize = headers
                        .get("content-length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0; len];
                    reader.read_exact(&mut body).unwrap();

                    let mut objs = store.lock().unwrap();
                    let current = objs.get(&key).map(|(_, v)| format!("\"{v}\""));
                    let (status, etag, payload) = match method.as_str() {
                        "GET" => match objs.get(&key) {
                            Some((data, v)) => (200, Some(format!("\"{v}\"")), data.clone()),
                            None => (404, None, Vec::new()),
                        },
                        "PUT" => {
                            let if_match = headers.get("if-match");
                            let if_none = headers.get("if-none-match");
                            if (if_none.is_some() && current.is_some())
                                || if_match.is_some_and(|m| Some(m) != current.as_ref())
                            {
                                (412, None, Vec::new())
                            } else {
                                version += 1;
                                objs.insert(key, (body, version));
                                (200, Some(format!("\"{version}\"")), Vec::new())
                            }
                        }
                        "DELETE" => {
                            objs.remove(&key);
                            (204, None, Vec::new())
                        }
                        _ => (405, None, Vec::new()),
                    };
                    drop(objs);
                    let mut out = stream;
                    let etag_header = etag.map(|e| format!("ETag: {e}\r\n")).unwrap_or_default();
                    let _ = write!(
                        out,
                        "HTTP/1.1 {status} X\r\n{etag_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = out.write_all(&payload);
                }
            });
            Self { endpoint, objects }
        }

        fn get(&self, key: &str) -> Option<String> {
            self.objects
                .lock()
                .unwrap()
                .get(&format!("bucket/vault/{key}"))
                .map(|(d, _)| String::from_utf8_lossy(d).into_owned())
        }

        fn put(&self, key: &str, body: &str) {
            let mut objs = self.objects.lock().unwrap();
            let v = objs.values().map(|(_, v)| *v).max().unwrap_or(0) + 1000;
            objs.insert(format!("bucket/vault/{key}"), (body.as_bytes().to_vec(), v));
        }

        fn keys(&self) -> Vec<String> {
            let mut keys: Vec<String> = self.objects.lock().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        }

        fn remote(&self) -> S3Remote {
            S3Remote::new(&self.endpoint, "bucket", "vault")
        }
    }

    struct Device {
        root: std::path::PathBuf,
        db: rusqlite::Connection,
        cfg: VaultConfig,
        cache: KeyCache,
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("vx-s3-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    impl Device {
        fn new(s3: &FakeS3, passphrase: &str) -> Self {
            let root = temp("vault");
            let db = crate::db::open_db_at(":memory:").unwrap();
            crate::sync::ensure_sync_table(&db);
            Self {
                cfg: VaultConfig {
                    vault_path: Some(root.to_string_lossy().to_string()),
                    encryption_key: Some(passphrase.to_string()),
                    remote: Some(RemoteBackendConfig::S3 {
                        endpoint: s3.endpoint.clone(),
                        bucket: "bucket".into(),
                        prefix: Some("vault".into()),
                    }),
                    ..Default::default()
                },
                cache: KeyCache::at(temp("keys")),
                db,
                root,
            }
        }
        fn sync(&self, s3: &FakeS3) -> crate::sync::SyncSummary {
            engine::run_with_retry(
                &self.db,
                &self.root.to_string_lossy(),
                &self.cfg,
                &self.cache,
                &mut s3.remote(),
                true,
                true,
            )
            .unwrap()
        }
        fn write(&self, path: &str, body: &str) {
            let p = self.root.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        fn read(&self, path: &str) -> Option<String> {
            std::fs::read_to_string(self.root.join(path)).ok()
        }
    }

    #[test]
    fn devices_sync_through_s3_with_a_keyring() {
        let _home = crate::test_support::HomeGuard::new(&temp("home"));
        let s3 = FakeS3::start();
        let (a, b) = (Device::new(&s3, "pass"), Device::new(&s3, "pass"));
        a.write("a.md", "# A\n");
        a.write("img/p.png", "\u{0}bin");
        assert_eq!(a.sync(&s3).pushed, 2);

        let keyring = Keyring::from_json(&s3.get(KEYRING_PATH).unwrap()).unwrap();
        let dk = keyring.unlock("pass").unwrap();
        let manifest = Manifest::parse(&s3.get(MANIFEST).unwrap()).unwrap();
        let entry = &manifest.entries["a.md"];
        let object = s3.get(entry.object.as_ref().unwrap()).unwrap();
        assert_eq!(v2::decrypt(&object, &dk, "a.md").unwrap(), "# A\n");

        assert_eq!(b.sync(&s3).pulled, 2);
        assert_eq!(b.read("a.md").unwrap(), "# A\n");

        b.write("a.md", "# A\nfrom B\n");
        b.sync(&s3);
        a.sync(&s3);
        assert_eq!(a.read("a.md").unwrap(), "# A\nfrom B\n");
        let objects = s3
            .keys()
            .into_iter()
            .filter(|k| k.contains("/objects/"))
            .count();
        assert_eq!(objects, 2, "superseded versions are cleaned up");

        std::fs::remove_file(a.root.join("img/p.png")).unwrap();
        a.sync(&s3);
        b.sync(&s3);
        assert!(b.read("img/p.png").is_none());
    }

    #[test]
    fn legacy_s3_vault_upgrades_in_place() {
        let _home = crate::test_support::HomeGuard::new(&temp("home"));
        let s3 = FakeS3::start();
        // Written by vault-axi 1.1 with a passphrase: v1 manifest, plain object paths.
        let old_key = v1::derive_key("pass");
        let body = "# Legacy\n";
        s3.put("notes/old.md", &v1::encrypt(body, &old_key));
        s3.put(
            MANIFEST,
            &format!(
                r#"{{"notes/old.md": "{}:{}"}}"#,
                sha1_hex(body),
                v1::fingerprint(&old_key)
            ),
        );

        let a = Device::new(&s3, "pass");
        let s = a.sync(&s3);
        assert_eq!(s.pulled, 1);
        assert_eq!(s.pushed, 1, "re-encrypted onto the vault key");
        assert_eq!(a.read("notes/old.md").unwrap(), body);
        let manifest = Manifest::parse(&s3.get(MANIFEST).unwrap()).unwrap();
        assert!(s3.get(MANIFEST).unwrap().contains("\"version\": 2"));
        assert!(manifest.entries["notes/old.md"].key_id.starts_with("k2_"));
        assert!(
            s3.get("notes/old.md").is_none(),
            "the legacy plain object is removed"
        );
    }

    #[test]
    fn manifest_commit_is_compare_and_swap() {
        let s3 = FakeS3::start();
        let mut first = s3.remote();
        first.list().unwrap();
        first.tombstone("x.md", 1).unwrap();
        first.finish().unwrap();

        let mut stale = s3.remote();
        stale.list().unwrap();
        let mut winner = s3.remote();
        winner.list().unwrap();
        winner.tombstone("y.md", 2).unwrap();
        winner.finish().unwrap();

        stale.tombstone("z.md", 3).unwrap();
        assert_eq!(stale.finish().unwrap_err().code, REMOTE_CHANGED);
        stale.list().unwrap();
        stale.tombstone("z.md", 3).unwrap();
        stale.finish().unwrap();
        let manifest = Manifest::parse(&s3.get(MANIFEST).unwrap()).unwrap();
        assert_eq!(manifest.entries.len(), 3);
    }

    #[test]
    fn keyring_creation_is_exclusive() {
        let s3 = FakeS3::start();
        let kdf = || vault_axi_core::crypto::keyring::KdfParams::with_cost(8 * 1024, 1, 1);
        let (first, _) = Keyring::create("a", kdf(), "t").unwrap();
        let (second, _) = Keyring::create("b", kdf(), "t").unwrap();
        let mut remote = s3.remote();
        assert!(remote.create_keyring(&first).unwrap());
        assert!(!remote.create_keyring(&second).unwrap());
        let stored = remote.fetch_keyring().unwrap().unwrap();
        assert_eq!(stored.keyring, first);
        remote.replace_keyring(&stored.version, &second).unwrap();
        assert_eq!(
            remote
                .replace_keyring(&stored.version, &first)
                .unwrap_err()
                .code,
            "KEYRING_CHANGED"
        );
    }
}
