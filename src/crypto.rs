use crate::config::{load_config, RemoteBackendConfig};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use vault_axi_core::crypto::{v1, Key};

/// Resolve the active 256-bit encryption key for database-at-rest protection.
/// Precedence:
/// 1. `VAULT_ENCRYPTION_KEY` environment variable
/// 2. `encryptionKey` configured via `vault setup --encryption-key <passphrase>`
/// 3. Per-account cloud key derived from `user_id` + `vault_id` (if logged in)
/// 4. Local vault key from `localEncryptionKey` or deterministic local fallback
pub fn resolve_active_key() -> Key {
    if let Ok(env_key) = std::env::var("VAULT_ENCRYPTION_KEY") {
        let trimmed = env_key.trim();
        if !trimmed.is_empty() {
            return v1::derive_key(trimmed);
        }
    }

    let cfg = load_config();
    if let Some(ref custom_key) = cfg.encryption_key {
        let trimmed = custom_key.trim();
        if !trimmed.is_empty() {
            return v1::derive_key(trimmed);
        }
    }

    if let Some(RemoteBackendConfig::Cloud {
        ref user_id,
        ref vault_id,
        ..
    }) = cfg.remote
    {
        if let Some(uid) = user_id.as_deref().filter(|s| !s.is_empty()) {
            let vid = vault_id.as_deref().unwrap_or("default");
            return v1::derive_key(&format!("vault-axi-cloud-e2ee:{uid}:{vid}"));
        }
    }

    if let Some(ref local_key) = cfg.local_encryption_key {
        let trimmed = local_key.trim();
        if !trimmed.is_empty() {
            return v1::derive_key(trimmed);
        }
    }

    let vault_seed = cfg.vault_path.as_deref().unwrap_or("default-local-vault");
    v1::derive_key(&format!("vault-axi-local-at-rest:{vault_seed}"))
}

/// Decrypt with one key; strings without an `enc:v1:` prefix are legacy plaintext and pass through.
pub fn decrypt_with_key(stored: &str, key: &Key) -> Result<String, String> {
    if !v1::is_v1(stored) {
        return Ok(stored.to_string());
    }
    v1::decrypt(stored, key).map_err(|e| e.to_string())
}

/// Short 16-hex-character fingerprint of the currently active encryption key.
/// Used to detect when remote or local rows need re-encryption after a key change.
pub fn active_key_fingerprint() -> String {
    let full = vault_axi_core::hash::sha256_hex(&resolve_active_key());
    full[..16].to_string()
}

/// Encrypt a string at rest using the active vault encryption key.
pub fn encrypt_at_rest(plaintext: &str) -> String {
    v1::encrypt(plaintext, &resolve_active_key())
}

/// Re-encrypt all rows in local SQLite (`sync_state`, `note_versions`, `trash`) that are either
/// plaintext or encrypted with an older key, migrating them to `new_key`.
/// Returns the total number of local rows re-encrypted.
pub fn reencrypt_local_db(
    db: &rusqlite::Connection,
    old_key: Option<&Key>,
    new_key: &Key,
) -> usize {
    let mut updated = 0usize;

    let decrypt_any = |raw: &str| -> Option<String> {
        if !v1::is_v1(raw) {
            return Some(raw.to_string());
        }
        if let Some(ok) = old_key {
            if let Ok(plain) = decrypt_with_key(raw, ok) {
                return Some(plain);
            }
        }
        decrypt_at_rest(raw).ok()
    };

    // 1. sync_state.base_content
    if let Ok(mut stmt) = db.prepare("SELECT path, COALESCE(base_content, '') FROM sync_state") {
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        for (path, raw) in rows {
            if raw.is_empty() {
                continue;
            }
            let already_on_new = v1::is_v1(&raw) && decrypt_with_key(&raw, new_key).is_ok();
            if !already_on_new {
                if let Some(plain) = decrypt_any(&raw) {
                    let reenc = v1::encrypt(&plain, new_key);
                    if db
                        .execute(
                            "UPDATE sync_state SET base_content = ?1 WHERE path = ?2",
                            rusqlite::params![reenc, path],
                        )
                        .is_ok()
                    {
                        updated += 1;
                    }
                }
            }
        }
    }

    // 2. note_versions.content
    if let Ok(mut stmt) = db.prepare("SELECT id, content FROM note_versions") {
        let rows: Vec<(i64, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        for (id, raw) in rows {
            if raw.is_empty() {
                continue;
            }
            let already_on_new = v1::is_v1(&raw) && decrypt_with_key(&raw, new_key).is_ok();
            if !already_on_new {
                if let Some(plain) = decrypt_any(&raw) {
                    let reenc = v1::encrypt(&plain, new_key);
                    if db
                        .execute(
                            "UPDATE note_versions SET content = ?1 WHERE id = ?2",
                            rusqlite::params![reenc, id],
                        )
                        .is_ok()
                    {
                        updated += 1;
                    }
                }
            }
        }
    }

    // 3. trash.content
    if let Ok(mut stmt) = db.prepare("SELECT path, content FROM trash") {
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        for (path, raw) in rows {
            if raw.is_empty() {
                continue;
            }
            let already_on_new = v1::is_v1(&raw) && decrypt_with_key(&raw, new_key).is_ok();
            if !already_on_new {
                if let Some(plain) = decrypt_any(&raw) {
                    let reenc = v1::encrypt(&plain, new_key);
                    if db
                        .execute(
                            "UPDATE trash SET content = ?1 WHERE path = ?2",
                            rusqlite::params![reenc, path],
                        )
                        .is_ok()
                    {
                        updated += 1;
                    }
                }
            }
        }
    }

    updated
}

/// Ensure all rows in local SQLite (`sync_state`, `note_versions`, `trash`) are encrypted with the active key.
pub fn ensure_local_db_encrypted(db: &rusqlite::Connection) -> usize {
    let key = resolve_active_key();
    reencrypt_local_db(db, None, &key)
}

/// Decrypt a string stored at rest. Falls back to fallback keys if a custom key was recently toggled.
pub fn decrypt_at_rest(stored: &str) -> Result<String, String> {
    if !v1::is_v1(stored) {
        return Ok(stored.to_string());
    }
    if let Ok(plain) = decrypt_with_key(stored, &resolve_active_key()) {
        return Ok(plain);
    }

    // Fallback check against previous local_encryption_key or default account/local key if user recently changed keys
    let cfg = load_config();
    if let Some(ref prev_local) = cfg.local_encryption_key {
        let trimmed = prev_local.trim();
        if !trimmed.is_empty() {
            if let Ok(raw) = B64.decode(trimmed) {
                if let Ok(arr) = Key::try_from(raw.as_slice()) {
                    if let Ok(plain) = decrypt_with_key(stored, &arr) {
                        return Ok(plain);
                    }
                }
            }
            if let Ok(plain) = decrypt_with_key(stored, &v1::derive_key(trimmed)) {
                return Ok(plain);
            }
        }
    }
    if let Some(RemoteBackendConfig::Cloud {
        ref user_id,
        ref vault_id,
        ..
    }) = cfg.remote
    {
        if let Some(uid) = user_id.as_deref().filter(|s| !s.is_empty()) {
            let vid = vault_id.as_deref().unwrap_or("default");
            let cloud_default_key = v1::derive_key(&format!("vault-axi-cloud-e2ee:{uid}:{vid}"));
            if let Ok(plain) = decrypt_with_key(stored, &cloud_default_key) {
                return Ok(plain);
            }
        }
    }
    let vault_seed = cfg.vault_path.as_deref().unwrap_or("default-local-vault");
    decrypt_with_key(
        stored,
        &v1::derive_key(&format!("vault-axi-local-at-rest:{vault_seed}")),
    )
}
