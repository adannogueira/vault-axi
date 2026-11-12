//! Executes the core sync plan against a [`Remote`] (hosted cloud, S3) with this machine's
//! filesystem, history, trash and `sync_state` table.

use super::{
    now_ms, read_sync_payload, report_sync_progress, write_sync_payload, ConflictInfo, SyncSummary,
};
use crate::axi::AxiError;
use crate::commands::history::record_edit_transition;
use crate::commands::trash::move_file_to_trash;
use crate::config::VaultConfig;
use crate::crypto::{
    crypto_error, prepare_sync_keychain, reencrypt_local_db, KeyCache, KeyringStore, LOCAL_CONTEXT,
};
use crate::vault::{file_stat, resolve_in_vault};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use vault_axi_core::crypto::Keychain;
use vault_axi_core::hash::sha1_hex;
use vault_axi_core::plan::{
    self, Action, BaseEntry, LocalFile, RemoteFile, Resolution, ResolutionKind,
};

/// A remote payload as stored (still encrypted).
pub struct Fetched {
    pub content: String,
    pub hash: String,
    pub mtime_ms: i64,
}

/// One upload. `content` is already encrypted for `path`.
#[derive(Clone)]
pub struct Put {
    pub path: String,
    pub content: String,
    pub hash: String,
    pub mtime_ms: i64,
    pub key_id: String,
    /// Compare-and-swap: the live remote version this replaces (`None` = no live version).
    pub expected_remote_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    Stored,
    /// The remote no longer holds `expected_remote_hash`.
    Conflict,
    Failed(String),
}

/// Error code a remote returns when its state changed during the run; the run is retried.
pub const REMOTE_CHANGED: &str = "REMOTE_CHANGED";

/// What an encrypted sync backend must provide. Payloads cross this boundary encrypted.
pub trait Remote: KeyringStore {
    fn name(&self) -> &'static str;
    fn read_only(&self) -> bool {
        false
    }
    /// Every remote path with its hash, tombstone flag and key id.
    fn list(&mut self) -> Result<Vec<RemoteFile>, AxiError>;
    /// Hook for backend-specific key-id bookkeeping once the keychain is known.
    fn adopt_legacy_key_ids(&mut self, _keychain: &Keychain, _files: &mut [RemoteFile]) {}
    fn fetch(&mut self, path: &str) -> Result<Option<Fetched>, AxiError>;
    /// Store every item; outcomes are returned in the same order.
    fn put(&mut self, items: &[Put]) -> Vec<PutOutcome>;
    fn tombstone(&mut self, path: &str, mtime_ms: i64) -> Result<(), AxiError>;
    /// Commit anything buffered during the run.
    fn finish(&mut self) -> Result<(), AxiError> {
        Ok(())
    }
}

pub fn run_with_retry(
    db: &Connection,
    vault_root: &str,
    cfg: &VaultConfig,
    cache: &KeyCache,
    remote: &mut dyn Remote,
    pull: bool,
    push: bool,
) -> Result<SyncSummary, AxiError> {
    let mut attempt = 0;
    loop {
        match run(db, vault_root, cfg, cache, remote, pull, push) {
            Err(e) if e.code == REMOTE_CHANGED && attempt < 2 => attempt += 1,
            result => return result,
        }
    }
}

pub fn run(
    db: &Connection,
    vault_root: &str,
    cfg: &VaultConfig,
    cache: &KeyCache,
    remote: &mut dyn Remote,
    pull: bool,
    push: bool,
) -> Result<SyncSummary, AxiError> {
    if push && !pull && remote.read_only() {
        return Err(AxiError::new(
            "API token has read-only scope ('read') and cannot push changes",
            "PERMISSION_DENIED",
            vec![
                "Create a read-write token with `vault token create --scope write`".to_string(),
                "Or run `vault sync --pull` to pull remote changes only.".to_string(),
            ],
        ));
    }

    let mut remote_files = remote.list()?;
    let keychain = prepare_sync_keychain(cfg, cache, remote)?;
    reencrypt_local_db(db, &keychain);
    remote.adopt_legacy_key_ids(&keychain, &mut remote_files);

    let mut engine = Engine {
        db,
        root: vault_root,
        kc: keychain,
        remote,
        base: load_base(db),
        summary: SyncSummary {
            backend: String::new(),
            ..Default::default()
        },
        now: now_ms(),
    };
    engine.summary.backend = engine.remote.name().to_string();

    let local = engine.scan_local();
    let pull_plan = plan::plan_pull(&local, &engine.base_entries(), &remote_files, pull);
    engine.execute_pull(pull_plan)?;

    let can_push = push && !engine.remote.read_only();
    if can_push {
        let local = engine.scan_local();
        let push_plan = plan::plan_push(
            &local,
            &engine.base_entries(),
            &remote_files,
            &engine.kc.active_key_id(),
        );
        engine.execute_push(push_plan)?;
    }
    engine.remote.finish()?;

    let mut summary = engine.summary;
    summary.message = Some(summary_message(&summary, push && !can_push));
    Ok(summary)
}

fn summary_message(s: &SyncSummary, push_skipped: bool) -> String {
    let label = match s.backend.as_str() {
        "s3" => "S3",
        _ => "Vault Cloud",
    };
    if push_skipped {
        return format!(
            "{label} sync complete (pulled {}, push skipped: read-only token)",
            s.pulled
        );
    }
    let mut msg = format!(
        "{label} sync complete (pulled {}, pushed {}",
        s.pulled, s.pushed
    );
    if !s.conflicts.is_empty() {
        msg.push_str(&format!(", {} conflict(s) handled", s.conflicts.len()));
    }
    if !s.failures.is_empty() {
        msg.push_str(&format!(", {} failed", s.failures.len()));
    }
    msg.push(')');
    msg
}

struct Base {
    remote_hash: String,
    /// Raw (encrypted) base content; decrypted only when a merge needs it.
    stored_content: String,
}

fn load_base(db: &Connection) -> HashMap<String, Base> {
    let Ok(mut stmt) =
        db.prepare("SELECT path, remote_hash, COALESCE(base_content, '') FROM sync_state")
    else {
        return HashMap::new();
    };
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Base {
                remote_hash: r.get(1)?,
                stored_content: r.get(2)?,
            },
        ))
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// A fetched remote version, decrypted.
struct RemoteVersion {
    content: String,
    hash: String,
    mtime_ms: i64,
}

struct Engine<'a> {
    db: &'a Connection,
    root: &'a str,
    kc: Keychain,
    remote: &'a mut dyn Remote,
    base: HashMap<String, Base>,
    summary: SyncSummary,
    now: i64,
}

impl Engine<'_> {
    fn base_entries(&self) -> Vec<BaseEntry> {
        self.base
            .iter()
            .map(|(path, b)| BaseEntry {
                path: path.clone(),
                remote_hash: b.remote_hash.clone(),
            })
            .collect()
    }

    fn scan_local(&self) -> Vec<LocalFile> {
        super::list_all_syncable_files(self.root)
            .into_iter()
            .filter_map(|path| {
                let content = self.read_local(&path)?;
                Some(LocalFile {
                    hash: sha1_hex(&content),
                    path,
                })
            })
            .collect()
    }

    fn abs(&self, path: &str) -> Option<PathBuf> {
        resolve_in_vault(self.root, path).ok().map(PathBuf::from)
    }

    fn read_local(&self, path: &str) -> Option<String> {
        read_sync_payload(&self.abs(path)?, path)
    }

    fn local_mtime(&self, path: &str) -> i64 {
        self.abs(path)
            .and_then(|p| file_stat(&p.to_string_lossy()))
            .map_or(self.now, |s| s.mtime_ms)
    }

    fn write_local(&mut self, path: &str, content: &str, history_source: &str) -> bool {
        let Some(abs) = self.abs(path) else {
            return false;
        };
        let previous = read_sync_payload(&abs, path);
        if let Err(e) = write_sync_payload(&abs, content) {
            self.fail(path, &format!("could not write: {e}"));
            return false;
        }
        if path.to_ascii_lowercase().ends_with(".md") {
            record_edit_transition(self.db, path, previous.as_deref(), content, history_source);
        }
        true
    }

    fn set_base(&mut self, path: &str, remote_hash: &str, remote_mtime: i64, content: &str) {
        let stored_content = self.kc.encrypt(content, LOCAL_CONTEXT);
        let _ = self.db.execute(
            "INSERT INTO sync_state(path, remote_hash, remote_mtime, synced_at, base_content) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET remote_hash=excluded.remote_hash, remote_mtime=excluded.remote_mtime, synced_at=excluded.synced_at, base_content=excluded.base_content",
            params![path, remote_hash, remote_mtime, self.now, stored_content],
        );
        self.base.insert(
            path.to_string(),
            Base {
                remote_hash: remote_hash.to_string(),
                stored_content,
            },
        );
    }

    fn forget_base(&mut self, path: &str) {
        let _ = self
            .db
            .execute("DELETE FROM sync_state WHERE path = ?1", params![path]);
        self.base.remove(path);
    }

    fn base_content(&self, path: &str) -> String {
        self.base
            .get(path)
            .and_then(|b| self.kc.decrypt(&b.stored_content, LOCAL_CONTEXT).ok())
            .unwrap_or_default()
    }

    fn conflict(&mut self, path: &str, resolution: &str, copy: Option<String>, message: String) {
        self.summary.conflicts.push(ConflictInfo {
            path: path.to_string(),
            resolution: resolution.to_string(),
            conflict_copy: copy,
            message,
        });
    }

    fn fail(&mut self, path: &str, why: &str) {
        self.summary.failures.push(format!("{path}: {why}"));
    }

    /// Fetch and decrypt the live remote version. Undecryptable payloads are reported and
    /// skipped rather than written over local data.
    fn fetch(&mut self, path: &str) -> Result<Option<RemoteVersion>, AxiError> {
        let Some(f) = self.remote.fetch(path)? else {
            return Ok(None);
        };
        match self.kc.decrypt(&f.content, path) {
            Ok(content) => Ok(Some(RemoteVersion {
                content,
                hash: f.hash,
                mtime_ms: f.mtime_ms,
            })),
            Err(e) => {
                self.fail(path, &crypto_error(e).message);
                Ok(None)
            }
        }
    }

    fn execute_pull(&mut self, actions: Vec<Action>) -> Result<(), AxiError> {
        let transfers = actions
            .iter()
            .filter(|a| matches!(a, Action::Download { .. } | Action::Merge { .. }))
            .count();
        let mut done = 0;
        for action in actions {
            match action {
                Action::AdoptBase { path, hash } => {
                    if let Some(content) = self.read_local(&path) {
                        self.set_base(&path, &hash, self.now, &content);
                    }
                }
                Action::Download { path } => {
                    done += 1;
                    report_sync_progress("Downloading", done, transfers, &path);
                    if let Some(rv) = self.fetch(&path)? {
                        if self.write_local(&path, &rv.content, "sync-pull") {
                            self.set_base(&path, &rv.hash, rv.mtime_ms, &rv.content);
                            self.summary.pulled += 1;
                        }
                    }
                }
                Action::Merge { path } => {
                    done += 1;
                    report_sync_progress("Merging", done, transfers, &path);
                    self.merge(&path)?;
                }
                Action::DeleteLocal { path } => {
                    if let Some(abs) = self.abs(&path) {
                        let _ = move_file_to_trash(self.db, &path, &abs);
                        let _ = fs::remove_file(&abs);
                        self.summary.deleted_local += 1;
                    }
                    self.forget_base(&path);
                }
                Action::KeepLocalOverRemoteDelete { path } => {
                    self.forget_base(&path);
                    let message = format!(
                        "Conflict on '{path}': note was deleted on another device but modified locally; kept your local version."
                    );
                    self.conflict(&path, "kept_local_edit_over_remote_delete", None, message);
                }
                Action::ForgetBase { path } => self.forget_base(&path),
                other => unreachable!("not a pull action: {other:?}"),
            }
        }
        Ok(())
    }

    /// Resolve a concurrent edit; returns the remote version it was resolved against.
    fn merge(&mut self, path: &str) -> Result<Option<RemoteVersion>, AxiError> {
        let Some(local) = self.read_local(path) else {
            return Ok(None);
        };
        let Some(rv) = self.fetch(path)? else {
            return Ok(None);
        };
        let Resolution {
            kind,
            content,
            conflict_copy,
        } = plan::resolve(
            path,
            &self.base_content(path),
            &local,
            &rv.content,
            self.now,
        );

        if let Some(copy) = &conflict_copy {
            if let Some(abs) = self.abs(copy) {
                if let Err(e) = write_sync_payload(&abs, &local) {
                    self.fail(copy, &format!("could not save conflict copy: {e}"));
                    return Ok(None);
                }
            }
        }
        let source = match kind {
            ResolutionKind::ConflictCopy => "sync-conflict",
            _ => "sync-merge",
        };
        if content != local && !self.write_local(path, &content, source) {
            return Ok(None);
        }
        self.set_base(path, &rv.hash, rv.mtime_ms, &rv.content);

        match kind {
            ResolutionKind::Clean => {
                if sha1_hex(&content) == rv.hash {
                    self.summary.pulled += 1;
                }
            }
            ResolutionKind::AutoMerged => {
                self.summary.pulled += 1;
                let message = format!(
                    "Concurrent edit detected on '{path}': non-overlapping remote changes were automatically 3-way merged with your local edits."
                );
                self.conflict(path, "auto_merged", None, message);
            }
            ResolutionKind::ConflictCopy => {
                self.summary.pulled += 1;
                let copy = conflict_copy.unwrap_or_default();
                let message = format!(
                    "Concurrent edit conflict on '{path}': overlapping lines were modified on another device. Remote version is in '{path}' and your conflicting version was saved to '{copy}'."
                );
                self.conflict(path, "conflict_copy_saved", Some(copy), message);
            }
        }
        Ok(Some(rv))
    }

    fn prepare_put(
        &self,
        path: &str,
        expected_remote_hash: Option<String>,
    ) -> Option<(Put, String)> {
        let content = self.read_local(path)?;
        let put = Put {
            path: path.to_string(),
            content: self.kc.encrypt(&content, path),
            hash: sha1_hex(&content),
            mtime_ms: self.local_mtime(path),
            key_id: self.kc.active_key_id(),
            expected_remote_hash,
        };
        Some((put, content))
    }

    fn execute_push(&mut self, actions: Vec<Action>) -> Result<(), AxiError> {
        let mut uploads: Vec<(Put, String)> = Vec::new();
        for action in actions {
            match action {
                Action::Upload {
                    path,
                    expected_remote_hash,
                } => uploads.extend(self.prepare_put(&path, expected_remote_hash)),
                Action::DeleteRemote { path } => {
                    self.remote.tombstone(&path, self.now)?;
                    self.forget_base(&path);
                    self.summary.deleted_remote += 1;
                }
                Action::RestoreRemote { path } => {
                    if let Some(rv) = self.fetch(&path)? {
                        if self.write_local(&path, &rv.content, "sync-pull") {
                            self.set_base(&path, &rv.hash, rv.mtime_ms, &rv.content);
                            self.summary.pulled += 1;
                            let message = format!(
                                "Conflict on '{path}': note was deleted locally but modified on another device; restored the newer remote version."
                            );
                            self.conflict(
                                &path,
                                "restored_remote_edit_over_local_delete",
                                None,
                                message,
                            );
                        }
                    }
                }
                other => unreachable!("not a push action: {other:?}"),
            }
        }

        let (puts, contents): (Vec<Put>, Vec<String>) = uploads.into_iter().unzip();
        for (i, put) in puts.iter().enumerate() {
            report_sync_progress("Uploading", i + 1, puts.len(), &put.path);
        }
        let outcomes = self.remote.put(&puts);
        for ((put, content), outcome) in puts.into_iter().zip(contents).zip(outcomes) {
            match outcome {
                PutOutcome::Stored => {
                    self.set_base(&put.path, &put.hash, put.mtime_ms, &content);
                    self.summary.pushed += 1;
                }
                PutOutcome::Conflict => self.resolve_upload_conflict(&put.path)?,
                PutOutcome::Failed(why) => self.fail(&put.path, &why),
            }
        }
        Ok(())
    }

    /// Another device updated the path between our listing and our upload: merge with its
    /// version, then upload the result if it differs from what the remote now holds.
    fn resolve_upload_conflict(&mut self, path: &str) -> Result<(), AxiError> {
        let Some(rv) = self.merge(path)? else {
            return Ok(());
        };
        let Some((put, content)) = self.prepare_put(path, Some(rv.hash.clone())) else {
            return Ok(());
        };
        if put.hash == rv.hash {
            return Ok(());
        }
        match self.remote.put(std::slice::from_ref(&put)).pop() {
            Some(PutOutcome::Stored) => {
                self.set_base(path, &put.hash, put.mtime_ms, &content);
                self.summary.pushed += 1;
            }
            Some(PutOutcome::Conflict) => self.fail(
                path,
                "changed again on another device; will retry next sync",
            ),
            Some(PutOutcome::Failed(why)) => self.fail(path, &why),
            None => self.fail(path, "no upload outcome"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RemoteBackendConfig;
    use crate::crypto::StoredKeyring;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use vault_axi_core::crypto::keyring::Keyring;
    use vault_axi_core::crypto::{v1, v2};
    use vault_axi_core::payload;

    #[derive(Clone, Debug)]
    struct Row {
        content: String,
        hash: String,
        deleted: bool,
        key_id: String,
        mtime: i64,
    }

    type StoreHook = Box<dyn FnOnce(&mut Store)>;

    #[derive(Default)]
    struct Store {
        rows: BTreeMap<String, Row>,
        keyring: Option<(Keyring, u32)>,
        /// Runs once right before the next `put`, to simulate a concurrent device.
        before_put: Option<StoreHook>,
    }

    #[derive(Clone)]
    struct MemRemote {
        store: Rc<RefCell<Store>>,
        read_only: bool,
    }

    impl MemRemote {
        fn new() -> Self {
            Self {
                store: Rc::default(),
                read_only: false,
            }
        }
        fn row(&self, path: &str) -> Option<Row> {
            self.store.borrow().rows.get(path).cloned()
        }
    }

    impl KeyringStore for MemRemote {
        fn fetch_keyring(&mut self) -> Result<Option<StoredKeyring>, AxiError> {
            Ok(self
                .store
                .borrow()
                .keyring
                .as_ref()
                .map(|(k, v)| StoredKeyring {
                    keyring: k.clone(),
                    version: v.to_string(),
                }))
        }
        fn create_keyring(&mut self, keyring: &Keyring) -> Result<bool, AxiError> {
            let mut s = self.store.borrow_mut();
            if s.keyring.is_some() {
                return Ok(false);
            }
            s.keyring = Some((keyring.clone(), 1));
            Ok(true)
        }
        fn replace_keyring(&mut self, version: &str, next: &Keyring) -> Result<(), AxiError> {
            let mut s = self.store.borrow_mut();
            let v = s.keyring.as_ref().map(|(_, v)| *v).unwrap_or_default();
            assert_eq!(v.to_string(), version);
            s.keyring = Some((next.clone(), v + 1));
            Ok(())
        }
    }

    impl Remote for MemRemote {
        fn name(&self) -> &'static str {
            "vault-cloud"
        }
        fn read_only(&self) -> bool {
            self.read_only
        }
        fn list(&mut self) -> Result<Vec<RemoteFile>, AxiError> {
            Ok(self
                .store
                .borrow()
                .rows
                .iter()
                .map(|(p, r)| RemoteFile {
                    path: p.clone(),
                    hash: r.hash.clone(),
                    deleted: r.deleted,
                    key_id: r.key_id.clone(),
                })
                .collect())
        }
        fn fetch(&mut self, path: &str) -> Result<Option<Fetched>, AxiError> {
            Ok(self.row(path).filter(|r| !r.deleted).map(|r| Fetched {
                content: r.content,
                hash: r.hash,
                mtime_ms: r.mtime,
            }))
        }
        fn put(&mut self, items: &[Put]) -> Vec<PutOutcome> {
            let hook = self.store.borrow_mut().before_put.take();
            if let Some(hook) = hook {
                hook(&mut self.store.borrow_mut());
            }
            let mut s = self.store.borrow_mut();
            items
                .iter()
                .map(|p| {
                    let live = s
                        .rows
                        .get(&p.path)
                        .filter(|r| !r.deleted)
                        .map(|r| r.hash.clone());
                    if p.expected_remote_hash.is_some() && live != p.expected_remote_hash {
                        return PutOutcome::Conflict;
                    }
                    s.rows.insert(
                        p.path.clone(),
                        Row {
                            content: p.content.clone(),
                            hash: p.hash.clone(),
                            deleted: false,
                            key_id: p.key_id.clone(),
                            mtime: p.mtime_ms,
                        },
                    );
                    PutOutcome::Stored
                })
                .collect()
        }
        fn tombstone(&mut self, path: &str, mtime_ms: i64) -> Result<(), AxiError> {
            self.store.borrow_mut().rows.insert(
                path.to_string(),
                Row {
                    content: String::new(),
                    hash: String::new(),
                    deleted: true,
                    key_id: String::new(),
                    mtime: mtime_ms,
                },
            );
            Ok(())
        }
    }

    struct Device {
        root: std::path::PathBuf,
        db: Connection,
        cfg: VaultConfig,
        cache: KeyCache,
    }

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("vx-engine-{}-{tag}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    impl Device {
        fn new(passphrase: Option<&str>) -> Self {
            let root = unique_dir("vault");
            let cfg = VaultConfig {
                vault_path: Some(root.to_string_lossy().to_string()),
                encryption_key: passphrase.map(str::to_string),
                remote: Some(RemoteBackendConfig::Cloud {
                    access_token: None,
                    refresh_token: None,
                    user_id: Some("u1".into()),
                    email: None,
                    vault_id: Some("v1".into()),
                    scope: None,
                }),
                ..Default::default()
            };
            Self {
                db: crate::db::open_db_at(":memory:").unwrap(),
                cache: KeyCache::at(unique_dir("keys")),
                root,
                cfg,
            }
        }
        fn write(&self, path: &str, content: &str) {
            let p = self.root.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        fn read(&self, path: &str) -> Option<String> {
            fs::read_to_string(self.root.join(path)).ok()
        }
        fn remove(&self, path: &str) {
            fs::remove_file(self.root.join(path)).unwrap();
        }
        fn sync_with(
            &self,
            remote: &mut MemRemote,
            pull: bool,
            push: bool,
        ) -> Result<SyncSummary, AxiError> {
            super::super::ensure_sync_table(&self.db);
            run(
                &self.db,
                &self.root.to_string_lossy(),
                &self.cfg,
                &self.cache,
                remote,
                pull,
                push,
            )
        }
        fn sync(&self, remote: &mut MemRemote) -> SyncSummary {
            self.sync_with(remote, true, true).unwrap()
        }
    }

    /// History/trash rows use the process-wide keychain; keep it off the real home directory.
    fn home() -> crate::test_support::HomeGuard {
        crate::test_support::HomeGuard::new(&unique_dir("home"))
    }

    #[test]
    fn two_devices_converge_on_create_edit_and_delete() {
        let _home = home();
        let mut remote = MemRemote::new();
        let (a, b) = (Device::new(None), Device::new(None));

        a.write("notes/one.md", "# One\n");
        a.write("img/pic.png", "\u{1}\u{2}binary");
        let s = a.sync(&mut remote);
        assert_eq!(s.pushed, 2);
        let row = remote.row("notes/one.md").unwrap();
        assert!(v1::is_v1(&row.content), "payloads are encrypted remotely");
        assert_eq!(a.sync(&mut remote).pushed, 0, "second sync is a no-op");

        let s = b.sync(&mut remote);
        assert_eq!(s.pulled, 2);
        assert_eq!(b.read("notes/one.md").unwrap(), "# One\n");
        assert_eq!(b.read("img/pic.png").unwrap(), "\u{1}\u{2}binary");

        b.write("notes/one.md", "# One\nedited on B\n");
        assert_eq!(b.sync(&mut remote).pushed, 1);
        assert_eq!(a.sync(&mut remote).pulled, 1);
        assert_eq!(a.read("notes/one.md").unwrap(), "# One\nedited on B\n");

        a.remove("img/pic.png");
        assert_eq!(a.sync(&mut remote).deleted_remote, 1);
        assert!(remote.row("img/pic.png").unwrap().deleted);
        assert_eq!(b.sync(&mut remote).deleted_local, 1);
        assert!(b.read("img/pic.png").is_none());
    }

    #[test]
    fn concurrent_edits_merge_or_keep_a_conflict_copy() {
        let _home = home();
        let mut remote = MemRemote::new();
        let (a, b) = (Device::new(None), Device::new(None));
        a.write("plan.md", "title\n\nbody\n\nfooter\n");
        a.sync(&mut remote);
        b.sync(&mut remote);

        a.write("plan.md", "TITLE A\n\nbody\n\nfooter\n");
        b.write("plan.md", "title\n\nbody\n\nfooter B\n");
        a.sync(&mut remote);
        let s = b.sync(&mut remote);
        assert_eq!(s.conflicts[0].resolution, "auto_merged");
        assert_eq!(b.read("plan.md").unwrap(), "TITLE A\n\nbody\n\nfooter B\n");
        a.sync(&mut remote);
        assert_eq!(a.read("plan.md"), b.read("plan.md"), "devices converge");

        a.write("plan.md", "TITLE from A again\n\nbody\n\nfooter B\n");
        b.write("plan.md", "TITLE from B\n\nbody\n\nfooter B\n");
        a.sync(&mut remote);
        let s = b.sync(&mut remote);
        let c = &s.conflicts[0];
        assert_eq!(c.resolution, "conflict_copy_saved");
        let copy = c.conflict_copy.clone().unwrap();
        assert_eq!(b.read(&copy).unwrap(), "TITLE from B\n\nbody\n\nfooter B\n");
        assert_eq!(
            b.read("plan.md").unwrap(),
            "TITLE from A again\n\nbody\n\nfooter B\n"
        );
        a.sync(&mut remote);
        assert_eq!(
            a.read(&copy),
            b.read(&copy),
            "the conflict copy reaches every device"
        );
    }

    #[test]
    fn deletes_never_lose_a_concurrent_edit() {
        let _home = home();
        let mut remote = MemRemote::new();
        let (a, b) = (Device::new(None), Device::new(None));
        a.write("x.md", "x\n");
        a.write("y.md", "y\n");
        a.sync(&mut remote);
        b.sync(&mut remote);

        // Deleted on A, edited on B: B keeps its edit and resurrects the note.
        a.remove("x.md");
        a.sync(&mut remote);
        b.write("x.md", "x edited\n");
        let s = b.sync(&mut remote);
        assert_eq!(
            s.conflicts[0].resolution,
            "kept_local_edit_over_remote_delete"
        );
        assert!(!remote.row("x.md").unwrap().deleted);

        // Edited on A, deleted on B: B restores A's edit instead of deleting it.
        a.sync(&mut remote);
        a.write("y.md", "y edited\n");
        a.sync(&mut remote);
        b.remove("y.md");
        let s = b.sync_with(&mut remote, false, true).unwrap();
        assert_eq!(
            s.conflicts[0].resolution,
            "restored_remote_edit_over_local_delete"
        );
        assert_eq!(b.read("y.md").unwrap(), "y edited\n");
    }

    #[test]
    fn upload_racing_another_device_is_merged_not_overwritten() {
        let _home = home();
        let mut remote = MemRemote::new();
        let (a, b) = (Device::new(None), Device::new(None));
        a.write("n.md", "one\ntwo\nthree\n");
        a.sync(&mut remote);
        b.sync(&mut remote);

        // B's edit lands in a fork of the remote, to be injected mid-upload of A's sync.
        let mut fork = MemRemote::new();
        fork.store.borrow_mut().rows = remote.store.borrow().rows.clone();
        b.write("n.md", "one\ntwo\nTHREE\n");
        b.sync(&mut fork);
        let b_row = fork.row("n.md").unwrap();

        a.write("n.md", "ONE\ntwo\nthree\n");
        remote.store.borrow_mut().before_put = Some(Box::new(move |s: &mut Store| {
            s.rows.insert("n.md".into(), b_row);
        }));
        let s = a.sync(&mut remote);
        assert!(s.failures.is_empty(), "{:?}", s.failures);
        assert_eq!(s.conflicts[0].resolution, "auto_merged");
        assert_eq!(a.read("n.md").unwrap(), "ONE\ntwo\nTHREE\n");

        b.sync(&mut remote);
        assert_eq!(b.read("n.md").unwrap(), "ONE\ntwo\nTHREE\n");
    }

    #[test]
    fn read_only_tokens_pull_but_never_push() {
        let _home = home();
        let mut remote = MemRemote::new();
        let a = Device::new(None);
        a.write("a.md", "a\n");
        a.sync(&mut remote);

        let reader = Device::new(None);
        reader.write("local-only.md", "draft\n");
        let mut ro = MemRemote {
            read_only: true,
            ..remote.clone()
        };
        let err = reader.sync_with(&mut ro, false, true).unwrap_err();
        assert_eq!(err.code, "PERMISSION_DENIED");
        let s = reader.sync(&mut ro);
        assert_eq!((s.pulled, s.pushed), (1, 0));
        assert!(s.message.unwrap().contains("read-only"));
        assert!(remote.row("local-only.md").is_none());
    }

    #[test]
    fn undecryptable_rows_are_reported_and_local_data_is_kept() {
        let _home = home();
        let mut remote = MemRemote::new();
        let a = Device::new(None);
        a.write("a.md", "mine\n");
        remote.store.borrow_mut().rows.insert(
            "b.md".into(),
            Row {
                content: v1::encrypt("foreign", &[7; 32]),
                hash: "h".into(),
                deleted: false,
                key_id: String::new(),
                mtime: 0,
            },
        );
        let s = a.sync(&mut remote);
        assert_eq!(s.failures.len(), 1);
        assert!(s.failures[0].starts_with("b.md:"));
        assert!(a.read("b.md").is_none());
        assert_eq!(a.read("a.md").unwrap(), "mine\n");
    }

    /// The upgrade path: a vault written by vault-axi 1.1 (v1, server-derivable key) is migrated
    /// to the passphrase keyring without losing or changing any file.
    #[test]
    fn v1_vault_migrates_to_end_to_end_encryption() {
        let _home = home();
        let mut remote = MemRemote::new();
        let legacy = Device::new(None);
        legacy.write("a.md", "# A\n");
        legacy.write("sub/b.md", "# B\n");
        legacy.write("img/c.png", "\u{0}png");
        legacy.sync(&mut remote);
        let cloud_key = v1::derive_key("vault-axi-cloud-e2ee:u1:v1");
        for (path, row) in &remote.store.borrow().rows {
            assert_eq!(row.key_id, v1::fingerprint(&cloud_key), "{path}");
            assert!(v1::decrypt(&row.content, &cloud_key).is_ok());
        }

        // The same device sets a passphrase and syncs.
        let mut upgraded = legacy;
        upgraded.cfg.encryption_key = Some("correct horse".into());
        let s = upgraded.sync(&mut remote);
        assert_eq!(s.pushed, 3, "every row is re-encrypted");
        assert_eq!(s.pulled, 0);
        let store = remote.store.borrow();
        let (keyring, _) = store.keyring.as_ref().expect("keyring created");
        let dk = keyring.unlock("correct horse").unwrap();
        for (path, row) in &store.rows {
            assert_eq!(row.key_id, dk.id(), "{path}");
            assert!(v2::is_v2(&row.content), "{path}");
        }
        let a_plain = v2::decrypt(&store.rows["a.md"].content, &dk, "a.md").unwrap();
        assert_eq!(a_plain, "# A\n");
        let png = v2::decrypt(&store.rows["img/c.png"].content, &dk, "img/c.png").unwrap();
        assert_eq!(payload::to_file(&png), b"\x00png");
        drop(store);
        assert_eq!(upgraded.read("a.md").unwrap(), "# A\n");
        let base_rows: Vec<String> = upgraded
            .db
            .prepare("SELECT base_content FROM sync_state")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert!(
            base_rows.iter().all(|c| v2::is_v2(c)),
            "local base rows migrated too"
        );
        assert_eq!(
            upgraded.sync(&mut remote).pushed,
            0,
            "migration is idempotent"
        );

        // A second device needs the passphrase.
        let other = Device::new(None);
        assert_eq!(
            other.sync_with(&mut remote, true, true).unwrap_err().code,
            "ENCRYPTION_KEY_REQUIRED"
        );
        let wrong = Device::new(Some("wrong"));
        assert_eq!(
            wrong.sync_with(&mut remote, true, true).unwrap_err().code,
            "ENCRYPTION_KEY_MISMATCH"
        );
        assert!(
            wrong.read("a.md").is_none(),
            "nothing written on a failed unlock"
        );
        let right = Device::new(Some("correct horse"));
        assert_eq!(right.sync(&mut remote).pulled, 3);
        assert_eq!(right.read("sub/b.md").unwrap(), "# B\n");
        assert_eq!(fs::read(right.root.join("img/c.png")).unwrap(), b"\x00png");
    }

    #[test]
    fn ciphertexts_moved_between_paths_are_rejected() {
        let _home = home();
        let mut remote = MemRemote::new();
        let a = Device::new(Some("p"));
        a.write("public.md", "fine\n");
        a.write("secret.md", "secret\n");
        a.sync(&mut remote);
        {
            let mut s = remote.store.borrow_mut();
            let secret = s.rows["secret.md"].clone();
            s.rows.insert(
                "public.md".into(),
                Row {
                    hash: "swapped".into(),
                    ..secret
                },
            );
        }
        let b = Device::new(Some("p"));
        let s = b.sync(&mut remote);
        assert!(s.failures.iter().any(|f| f.starts_with("public.md:")));
        assert!(b.read("public.md").is_none());
        assert_eq!(b.read("secret.md").unwrap(), "secret\n");
    }
}
