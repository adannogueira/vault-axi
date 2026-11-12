//! CLI glue around [`vault_axi_core::crypto`]: which keys this machine holds (config, env,
//! cached vault data key) and how the vault keyring is fetched, created and re-wrapped through a
//! sync backend.

use crate::axi::AxiError;
use crate::config::{config_dir, load_config, RemoteBackendConfig, VaultConfig};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use vault_axi_core::crypto::keyring::{KdfParams, Keyring};
use vault_axi_core::crypto::{
    resolve_vault_key, v1, CryptoError, DataKey, Key, Keychain, VaultKeyError, VaultKeyStep,
};
use vault_axi_core::hash::sha256_hex;

/// Associated-data context for rows in the local SQLite database. Local rows are not bound to
/// their path because history rows are re-pathed when a note moves.
pub const LOCAL_CONTEXT: &str = "local-db";

pub fn crypto_error(e: CryptoError) -> AxiError {
    let code = match e {
        CryptoError::Locked(_) => "ENCRYPTION_KEY_REQUIRED",
        _ => "CRYPTO_ERROR",
    };
    AxiError::new(e.to_string(), code, vec![])
}

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// The vault passphrase: `VAULT_ENCRYPTION_KEY`, else `vault setup --encryption-key`.
pub fn configured_passphrase(cfg: &VaultConfig) -> Option<String> {
    let env = std::env::var("VAULT_ENCRYPTION_KEY").ok();
    non_empty(env.as_deref())
        .or_else(|| non_empty(cfg.encryption_key.as_deref()))
        .map(str::to_string)
}

/// PBKDF2 is deliberately slow and the same few secrets are derived on every command; memoize.
fn derive_v1(secret: &str) -> Key {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Key>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(key) = cache.lock().ok().and_then(|c| c.get(secret).copied()) {
        return key;
    }
    let key = v1::derive_key(secret);
    if let Ok(mut c) = cache.lock() {
        c.insert(secret.to_string(), key);
    }
    key
}

fn cloud_derived_key(cfg: &VaultConfig) -> Option<Key> {
    let Some(RemoteBackendConfig::Cloud {
        user_id, vault_id, ..
    }) = &cfg.remote
    else {
        return None;
    };
    let uid = non_empty(user_id.as_deref())?;
    let vid = vault_id.as_deref().unwrap_or("default");
    Some(derive_v1(&format!("vault-axi-cloud-e2ee:{uid}:{vid}")))
}

fn local_derived_key(cfg: &VaultConfig) -> Key {
    let seed = cfg.vault_path.as_deref().unwrap_or("default-local-vault");
    derive_v1(&format!("vault-axi-local-at-rest:{seed}"))
}

/// v1 keys in the order vault-axi <= 1.1 chose them: the first is the active one, the rest are
/// fallbacks for payloads written before a key change.
fn v1_keys(cfg: &VaultConfig) -> Vec<Key> {
    let previous = non_empty(cfg.local_encryption_key.as_deref());
    let mut keys = Vec::new();
    if let Some(p) = configured_passphrase(cfg) {
        keys.push(derive_v1(&p));
    }
    keys.extend(cloud_derived_key(cfg));
    if let Some(prev) = previous {
        keys.push(derive_v1(prev));
        if let Some(raw) = B64.decode(prev).ok().and_then(|b| Key::try_from(b).ok()) {
            keys.push(raw);
        }
    }
    keys.push(local_derived_key(cfg));
    keys
}

/// Identity of the encrypted remote a data key belongs to, or `None` for backends that never
/// carry a keyring (local-only, Git).
pub fn key_scope(cfg: &VaultConfig) -> Option<String> {
    match cfg.remote.as_ref()? {
        RemoteBackendConfig::Cloud {
            user_id, vault_id, ..
        } => Some(format!(
            "cloud:{}:{}",
            non_empty(user_id.as_deref()).unwrap_or("token"),
            vault_id.as_deref().unwrap_or("default")
        )),
        RemoteBackendConfig::S3 {
            endpoint,
            bucket,
            prefix,
        } => Some(format!(
            "s3:{}/{bucket}/{}",
            endpoint.trim_end_matches('/'),
            prefix.as_deref().unwrap_or("").trim_matches('/')
        )),
        RemoteBackendConfig::Git { .. } => None,
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedDataKey {
    scope: String,
    key_id: String,
    key: String,
}

/// Unlocked data keys, one file per remote scope, so Argon2id runs once per device rather than
/// on every command.
pub struct KeyCache {
    dir: PathBuf,
}

impl Default for KeyCache {
    fn default() -> Self {
        Self::at(config_dir().join("keys"))
    }
}

impl KeyCache {
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, scope: &str) -> PathBuf {
        self.dir
            .join(format!("{}.json", &sha256_hex(scope.as_bytes())[..16]))
    }

    pub fn get(&self, scope: &str) -> Option<DataKey> {
        let raw = fs::read_to_string(self.path(scope)).ok()?;
        let cached: CachedDataKey = serde_json::from_str(&raw).ok()?;
        if cached.scope != scope {
            return None;
        }
        let key = Key::try_from(B64.decode(&cached.key).ok()?).ok()?;
        Some(DataKey::new(cached.key_id, key))
    }

    pub fn put(&self, scope: &str, data_key: &DataKey) -> Result<(), AxiError> {
        let path = self.path(scope);
        let io_err = |e: std::io::Error| {
            AxiError::new(
                format!("Could not cache the vault key at {}: {e}", path.display()),
                "IO_ERROR",
                vec![],
            )
        };
        fs::create_dir_all(&self.dir).map_err(io_err)?;
        let body = serde_json::to_string(&CachedDataKey {
            scope: scope.to_string(),
            key_id: data_key.id().to_string(),
            key: B64.encode(data_key.bytes()),
        })
        .expect("cache entry serializes");
        write_private(&path, &body).map_err(io_err)
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(body.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    fs::write(path, body)
}

/// Every key this machine can use right now, from config, environment and the key cache.
pub fn keychain() -> Keychain {
    keychain_for(&load_config(), &KeyCache::default())
}

pub fn keychain_for(cfg: &VaultConfig, cache: &KeyCache) -> Keychain {
    let mut keys = v1_keys(cfg).into_iter();
    let mut kc = Keychain::new(keys.next().expect("local key is always present"));
    for key in keys {
        kc = kc.with_v1_fallback(key);
    }
    match key_scope(cfg).and_then(|scope| cache.get(&scope)) {
        Some(dk) => kc.with_data_key(dk),
        None => kc,
    }
}

/// A keyring as stored by a backend, with an opaque version for compare-and-swap updates.
pub struct StoredKeyring {
    pub keyring: Keyring,
    pub version: String,
}

/// Where a backend keeps the vault keyring.
pub trait KeyringStore {
    fn fetch_keyring(&mut self) -> Result<Option<StoredKeyring>, AxiError>;
    /// Create the keyring only if none exists. Returns `false` if another device won the race.
    fn create_keyring(&mut self, keyring: &Keyring) -> Result<bool, AxiError>;
    /// Replace the keyring at `version`, failing if it changed since it was fetched.
    fn replace_keyring(&mut self, version: &str, next: &Keyring) -> Result<(), AxiError>;
}

pub fn invalid_keyring_error(detail: &str) -> AxiError {
    AxiError::new(
        format!("The remote vault keyring is unreadable: {detail}"),
        "INVALID_KEYRING",
        vec![],
    )
}

fn vault_key_error(e: VaultKeyError) -> AxiError {
    match e {
        VaultKeyError::Mismatch => mismatch_error(),
        VaultKeyError::PassphraseRequired => AxiError::new(
            "This vault is end-to-end encrypted with a passphrase",
            "ENCRYPTION_KEY_REQUIRED",
            vec![
                "Set it on this device: `vault setup --encryption-key <passphrase>` or VAULT_ENCRYPTION_KEY".to_string(),
            ],
        ),
        VaultKeyError::KeyringLost => AxiError::new(
            "This device holds a vault key but the remote has no keyring",
            "ENCRYPTION_KEY_REQUIRED",
            vec![
                "Set the passphrase so the keyring can be restored: `vault setup --encryption-key <passphrase>`".to_string(),
            ],
        ),
        VaultKeyError::Crypto(e) => crypto_error(e),
    }
}

fn mismatch_error() -> AxiError {
    AxiError::new(
        "The encryption passphrase does not unlock this vault's keyring",
        "ENCRYPTION_KEY_MISMATCH",
        vec![
            "Set the passphrase used on your other devices: `vault setup --encryption-key <passphrase>` or VAULT_ENCRYPTION_KEY".to_string(),
        ],
    )
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Resolve the keys to sync with: unlock the remote keyring with the configured passphrase,
/// create it on first use, and refuse to sync when the passphrase can't unlock it. Without a
/// passphrase and without a remote keyring, this is the v1 keychain unchanged.
pub fn prepare_sync_keychain(
    cfg: &VaultConfig,
    cache: &KeyCache,
    store: &mut dyn KeyringStore,
) -> Result<Keychain, AxiError> {
    let Some(scope) = key_scope(cfg) else {
        return Ok(keychain_for(cfg, cache));
    };
    let passphrase = configured_passphrase(cfg);
    let decide = |remote: Option<&Keyring>| {
        resolve_vault_key(
            remote,
            passphrase.as_deref(),
            cache.get(&scope),
            &now_rfc3339(),
            KdfParams::recommended(),
        )
        .map_err(vault_key_error)
    };
    let remote = store.fetch_keyring()?.map(|s| s.keyring);
    let data_key = match decide(remote.as_ref())? {
        VaultKeyStep::Use(dk) => Some(dk),
        VaultKeyStep::LegacyOnly => None,
        VaultKeyStep::Create { keyring, data_key } => {
            if store.create_keyring(&keyring)? {
                Some(data_key)
            } else {
                // Another device created the keyring first: adopt it.
                let winner = store.fetch_keyring()?.map(|s| s.keyring);
                match decide(winner.as_ref())? {
                    VaultKeyStep::Use(dk) => Some(dk),
                    _ => {
                        return Err(AxiError::new(
                            "Keyring creation conflicted but no keyring was found",
                            "SYNC_ERROR",
                            vec!["Retry `vault sync`".to_string()],
                        ))
                    }
                }
            }
        }
    };

    if let Some(dk) = &data_key {
        if cache.get(&scope).as_ref() != Some(dk) {
            cache.put(&scope, dk)?;
        }
    }
    let kc = keychain_for(cfg, cache);
    Ok(match data_key {
        Some(dk) => kc.with_data_key(dk),
        None => kc,
    })
}

/// Re-wrap the remote keyring around this device's data key under a new passphrase. The data
/// key and all data stay as they are. Returns `false` when the remote has no keyring yet.
pub fn rewrap_remote_keyring(
    store: &mut dyn KeyringStore,
    data_key: &DataKey,
    new_passphrase: &str,
) -> Result<bool, AxiError> {
    let Some(current) = store.fetch_keyring()? else {
        return Ok(false);
    };
    let next = current
        .keyring
        .rewrap(data_key, new_passphrase, KdfParams::recommended())
        .map_err(|e| match e {
            CryptoError::UnknownKeyId(_) => AxiError::new(
                "The remote keyring belongs to a different vault key than this device's",
                "ENCRYPTION_KEY_MISMATCH",
                vec!["Run `vault sync` first so this device adopts the remote keyring".to_string()],
            ),
            other => crypto_error(other),
        })?;
    store.replace_keyring(&current.version, &next)?;
    Ok(true)
}

/// How this machine encrypts synced data, for `vault doctor`.
pub fn encryption_status(cfg: &VaultConfig, cache: &KeyCache) -> serde_json::Value {
    let passphrase = configured_passphrase(cfg).is_some();
    let Some(scope) = key_scope(cfg) else {
        return serde_json::json!({
            "mode": "local",
            "passphraseConfigured": passphrase,
            "detail": "No encrypted remote; only the local database is encrypted at rest."
        });
    };
    let (mode, detail) = match (cache.get(&scope), passphrase) {
        (Some(_), _) => ("end-to-end", "Synced with the vault keyring (enc:v2)."),
        (None, true) => (
            "pending-keyring",
            "A passphrase is set; the next `vault sync` unlocks or creates the vault keyring.",
        ),
        (None, false) if cloud_derived_key(cfg).is_some() => (
            "server-derivable",
            "Cloud data uses a key derived from your account ids; set `vault setup --encryption-key` for end-to-end encryption.",
        ),
        (None, false) => (
            "local-derived",
            "Remote data uses a key derived from the vault path; set `vault setup --encryption-key` for end-to-end encryption.",
        ),
    };
    serde_json::json!({
        "mode": mode,
        "passphraseConfigured": passphrase,
        "vaultKeyId": cache.get(&scope).map(|k| k.id().to_string()),
        "detail": detail
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LocalReencryption {
    pub updated: usize,
    /// Rows no key on this machine can decrypt; left untouched.
    pub unreadable: usize,
}

fn reencrypt_rows(
    db: &Connection,
    select: &str,
    update: &str,
    keychain: &Keychain,
    report: &mut LocalReencryption,
) {
    let Ok(mut stmt) = db.prepare(select) else {
        return;
    };
    let rows: Vec<(rusqlite::types::Value, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    for (id, raw) in rows {
        if raw.is_empty() || keychain.is_current(&raw, LOCAL_CONTEXT) {
            continue;
        }
        match keychain.decrypt(&raw, LOCAL_CONTEXT) {
            Ok(plain) => {
                let reenc = keychain.encrypt(&plain, LOCAL_CONTEXT);
                if db.execute(update, params![reenc, id]).is_ok() {
                    report.updated += 1;
                }
            }
            Err(_) => report.unreadable += 1,
        }
    }
}

/// Bring every encrypted local row (`sync_state`, `note_versions`, `trash`) onto the keychain's
/// active key. Plaintext rows from old versions are encrypted; rows already current are skipped.
pub fn reencrypt_local_db(db: &Connection, keychain: &Keychain) -> LocalReencryption {
    let mut report = LocalReencryption::default();
    reencrypt_rows(
        db,
        "SELECT path, COALESCE(base_content, '') FROM sync_state",
        "UPDATE sync_state SET base_content = ?1 WHERE path = ?2",
        keychain,
        &mut report,
    );
    reencrypt_rows(
        db,
        "SELECT id, content FROM note_versions",
        "UPDATE note_versions SET content = ?1 WHERE id = ?2",
        keychain,
        &mut report,
    );
    reencrypt_rows(
        db,
        "SELECT path, content FROM trash",
        "UPDATE trash SET content = ?1 WHERE path = ?2",
        keychain,
        &mut report,
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use vault_axi_core::crypto::{v1, v2};

    struct MemStore {
        stored: Option<StoredKeyring>,
        /// Simulates another device creating a keyring between our fetch and create.
        racer: Option<Keyring>,
        writes: usize,
    }

    impl MemStore {
        fn empty() -> Self {
            Self {
                stored: None,
                racer: None,
                writes: 0,
            }
        }

        fn with(keyring: Keyring) -> Self {
            Self {
                stored: Some(StoredKeyring {
                    keyring,
                    version: "1".into(),
                }),
                ..Self::empty()
            }
        }
    }

    impl KeyringStore for MemStore {
        fn fetch_keyring(&mut self) -> Result<Option<StoredKeyring>, AxiError> {
            Ok(self.stored.as_ref().map(|s| StoredKeyring {
                keyring: s.keyring.clone(),
                version: s.version.clone(),
            }))
        }

        fn create_keyring(&mut self, keyring: &Keyring) -> Result<bool, AxiError> {
            if let Some(winner) = self.racer.take() {
                self.stored = Some(StoredKeyring {
                    keyring: winner,
                    version: "race".into(),
                });
            }
            if self.stored.is_some() {
                return Ok(false);
            }
            self.writes += 1;
            self.stored = Some(StoredKeyring {
                keyring: keyring.clone(),
                version: "1".into(),
            });
            Ok(true)
        }

        fn replace_keyring(&mut self, version: &str, next: &Keyring) -> Result<(), AxiError> {
            let current = self.stored.as_mut().expect("keyring exists");
            assert_eq!(current.version, version, "compare-and-swap version");
            current.keyring = next.clone();
            current.version = format!("{version}+");
            self.writes += 1;
            Ok(())
        }
    }

    fn temp_cache() -> KeyCache {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("vault-axi-keycache-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        KeyCache::at(dir)
    }

    fn cloud_cfg(passphrase: Option<&str>) -> VaultConfig {
        VaultConfig {
            vault_path: Some("/vaults/test".into()),
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
        }
    }

    fn other_device_keyring(passphrase: &str) -> (Keyring, DataKey) {
        Keyring::create(passphrase, KdfParams::with_cost(8 * 1024, 1, 1), "t").unwrap()
    }

    #[test]
    fn without_passphrase_or_keyring_stays_v1() {
        let cfg = cloud_cfg(None);
        let cache = temp_cache();
        let mut store = MemStore::empty();
        let kc = prepare_sync_keychain(&cfg, &cache, &mut store).unwrap();
        assert!(kc.data_key().is_none());
        assert_eq!(store.writes, 0);
        // The active v1 key is still the server-derivable cloud key, exactly as before.
        let ct = kc.encrypt("x", "a.md");
        assert!(v1::is_v1(&ct));
        let cloud = v1::derive_key("vault-axi-cloud-e2ee:u1:v1");
        assert_eq!(v1::decrypt(&ct, &cloud).unwrap(), "x");
    }

    #[test]
    fn first_sync_with_passphrase_creates_and_caches_the_keyring() {
        let cfg = cloud_cfg(Some("pass"));
        let cache = temp_cache();
        let mut store = MemStore::empty();
        let kc = prepare_sync_keychain(&cfg, &cache, &mut store).unwrap();
        let dk = kc.data_key().expect("data key").clone();
        let stored = store.stored.as_ref().unwrap();
        assert_eq!(stored.keyring.key_id, dk.id());
        assert_eq!(
            stored.keyring.kdf,
            KdfParams {
                salt: stored.keyring.kdf.salt.clone(),
                ..KdfParams::recommended()
            }
        );
        assert_eq!(stored.keyring.unlock("pass").unwrap(), dk);
        assert_eq!(cache.get(&key_scope(&cfg).unwrap()), Some(dk.clone()));
        assert!(v2::is_v2(&kc.encrypt("x", "a.md")));
        // Later commands pick the data key up from the cache without the network.
        assert_eq!(keychain_for(&cfg, &cache).data_key(), Some(&dk));
    }

    #[test]
    fn second_device_unlocks_the_existing_keyring() {
        let (kr, dk) = other_device_keyring("pass");
        let cache = temp_cache();
        let mut store = MemStore::with(kr);
        let kc = prepare_sync_keychain(&cloud_cfg(Some("pass")), &cache, &mut store).unwrap();
        assert_eq!(kc.data_key(), Some(&dk));
        assert_eq!(store.writes, 0);
    }

    #[test]
    fn wrong_passphrase_is_refused_and_nothing_is_cached() {
        let (kr, _) = other_device_keyring("pass");
        let cache = temp_cache();
        let cfg = cloud_cfg(Some("nope"));
        let err = prepare_sync_keychain(&cfg, &cache, &mut MemStore::with(kr)).unwrap_err();
        assert_eq!(err.code, "ENCRYPTION_KEY_MISMATCH");
        assert!(cache.get(&key_scope(&cfg).unwrap()).is_none());
    }

    #[test]
    fn encrypted_vault_without_passphrase_is_refused() {
        let (kr, _) = other_device_keyring("pass");
        let err = prepare_sync_keychain(&cloud_cfg(None), &temp_cache(), &mut MemStore::with(kr))
            .unwrap_err();
        assert_eq!(err.code, "ENCRYPTION_KEY_REQUIRED");
    }

    #[test]
    fn cached_key_keeps_working_without_the_passphrase() {
        let (kr, dk) = other_device_keyring("pass");
        let cfg = cloud_cfg(None);
        let cache = temp_cache();
        cache.put(&key_scope(&cfg).unwrap(), &dk).unwrap();
        let kc = prepare_sync_keychain(&cfg, &cache, &mut MemStore::with(kr)).unwrap();
        assert_eq!(kc.data_key(), Some(&dk));
    }

    #[test]
    fn losing_the_keyring_creation_race_adopts_the_winner() {
        let (winner, winner_dk) = other_device_keyring("pass");
        let mut store = MemStore::empty();
        store.racer = Some(winner);
        let kc =
            prepare_sync_keychain(&cloud_cfg(Some("pass")), &temp_cache(), &mut store).unwrap();
        assert_eq!(kc.data_key(), Some(&winner_dk));
        assert_eq!(store.writes, 0);
    }

    #[test]
    fn lost_remote_keyring_is_restored_around_the_cached_key() {
        let cfg = cloud_cfg(Some("pass"));
        let cache = temp_cache();
        let (_, dk) = other_device_keyring("pass");
        cache.put(&key_scope(&cfg).unwrap(), &dk).unwrap();
        let mut store = MemStore::empty();
        let kc = prepare_sync_keychain(&cfg, &cache, &mut store).unwrap();
        assert_eq!(kc.data_key(), Some(&dk));
        assert_eq!(store.stored.unwrap().keyring.unlock("pass").unwrap(), dk);
    }

    #[test]
    fn cached_key_without_remote_keyring_or_passphrase_is_refused() {
        let cfg = cloud_cfg(None);
        let cache = temp_cache();
        cache
            .put(&key_scope(&cfg).unwrap(), &other_device_keyring("p").1)
            .unwrap();
        let err = prepare_sync_keychain(&cfg, &cache, &mut MemStore::empty()).unwrap_err();
        assert_eq!(err.code, "ENCRYPTION_KEY_REQUIRED");
    }

    #[test]
    fn rewrap_changes_the_passphrase_but_not_the_key() {
        let (kr, dk) = other_device_keyring("old");
        let mut store = MemStore::with(kr);
        assert!(rewrap_remote_keyring(&mut store, &dk, "new").unwrap());
        let stored = store.stored.unwrap().keyring;
        assert_eq!(stored.unlock("new").unwrap(), dk);
        assert!(stored.unlock("old").is_err());

        let (_, unrelated) = other_device_keyring("x");
        let (kr2, _) = other_device_keyring("old");
        let err = rewrap_remote_keyring(&mut MemStore::with(kr2), &unrelated, "new").unwrap_err();
        assert_eq!(err.code, "ENCRYPTION_KEY_MISMATCH");
        assert!(!rewrap_remote_keyring(&mut MemStore::empty(), &dk, "new").unwrap());
    }

    #[test]
    fn v1_key_order_matches_legacy_resolution() {
        let mut cfg = cloud_cfg(Some("current"));
        cfg.local_encryption_key = Some("previous".into());
        let keys = v1_keys(&cfg);
        assert_eq!(keys[0], v1::derive_key("current"));
        assert_eq!(keys[1], v1::derive_key("vault-axi-cloud-e2ee:u1:v1"));
        assert_eq!(keys[2], v1::derive_key("previous"));
        assert_eq!(
            keys.last(),
            Some(&v1::derive_key("vault-axi-local-at-rest:/vaults/test"))
        );

        // Every legacy key still decrypts through the keychain.
        let kc = keychain_for(&cfg, &temp_cache());
        for key in &keys {
            assert_eq!(kc.decrypt(&v1::encrypt("old", key), "a.md").unwrap(), "old");
        }
        let raw_prev = [5u8; 32];
        cfg.local_encryption_key = Some(B64.encode(raw_prev));
        let kc = keychain_for(&cfg, &temp_cache());
        assert_eq!(
            kc.decrypt(&v1::encrypt("raw", &raw_prev), "a.md").unwrap(),
            "raw"
        );
    }

    #[test]
    fn encryption_status_modes() {
        let cache = temp_cache();
        assert_eq!(
            encryption_status(&VaultConfig::default(), &cache)["mode"],
            "local"
        );
        assert_eq!(
            encryption_status(&cloud_cfg(None), &cache)["mode"],
            "server-derivable"
        );
        assert_eq!(
            encryption_status(&cloud_cfg(Some("p")), &cache)["mode"],
            "pending-keyring"
        );
        let (_, dk) = other_device_keyring("p");
        cache
            .put(&key_scope(&cloud_cfg(None)).unwrap(), &dk)
            .unwrap();
        let status = encryption_status(&cloud_cfg(Some("p")), &cache);
        assert_eq!(status["mode"], "end-to-end");
        assert_eq!(status["vaultKeyId"], dk.id());
    }

    #[test]
    fn key_scopes_identify_the_remote() {
        assert_eq!(key_scope(&cloud_cfg(None)).unwrap(), "cloud:u1:v1");
        let s3 = VaultConfig {
            remote: Some(RemoteBackendConfig::S3 {
                endpoint: "https://r2.example/".into(),
                bucket: "b".into(),
                prefix: Some("/vault/".into()),
            }),
            ..Default::default()
        };
        assert_eq!(key_scope(&s3).unwrap(), "s3:https://r2.example/b/vault");
        let git = VaultConfig {
            remote: Some(RemoteBackendConfig::Git {
                repo_url: None,
                branch: None,
            }),
            ..Default::default()
        };
        assert_eq!(key_scope(&git), None);
        assert_eq!(key_scope(&VaultConfig::default()), None);
    }

    #[test]
    fn local_rows_migrate_to_the_data_key() {
        let db = crate::db::open_db_at(":memory:").unwrap();
        crate::sync::ensure_sync_table(&db);
        let cfg = cloud_cfg(Some("pass"));
        let v1_kc = keychain_for(&cfg, &temp_cache());
        let foreign = v1::encrypt("nobody can read this", &[9; 32]);
        db.execute(
            "INSERT INTO sync_state(path, remote_hash, remote_mtime, synced_at, base_content) VALUES ('a.md','h',0,0,?1), ('b.md','h',0,0,'legacy plaintext'), ('c.md','h',0,0,?2)",
            params![v1_kc.encrypt("base a", LOCAL_CONTEXT), foreign],
        )
        .unwrap();
        db.execute(
            "INSERT INTO trash(path, content, is_binary, mtime_ms, deleted_at, expires_at) VALUES ('t.md', ?1, 0, 0, 0, 0)",
            params![v1_kc.encrypt("trashed", LOCAL_CONTEXT)],
        )
        .unwrap();

        let (_, dk) = other_device_keyring("pass");
        let v2_kc = keychain_for(&cfg, &temp_cache()).with_data_key(dk);
        let report = reencrypt_local_db(&db, &v2_kc);
        assert_eq!(
            report,
            LocalReencryption {
                updated: 3,
                unreadable: 1
            }
        );
        assert_eq!(
            reencrypt_local_db(&db, &v2_kc),
            LocalReencryption {
                updated: 0,
                unreadable: 1
            }
        );

        let base_a: String = db
            .query_row(
                "SELECT base_content FROM sync_state WHERE path='a.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(v2::is_v2(&base_a));
        assert_eq!(v2_kc.decrypt(&base_a, LOCAL_CONTEXT).unwrap(), "base a");
        let base_b: String = db
            .query_row(
                "SELECT base_content FROM sync_state WHERE path='b.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            v2_kc.decrypt(&base_b, LOCAL_CONTEXT).unwrap(),
            "legacy plaintext"
        );
        let base_c: String = db
            .query_row(
                "SELECT base_content FROM sync_state WHERE path='c.md'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(base_c, foreign, "unreadable rows are left untouched");
    }
}
