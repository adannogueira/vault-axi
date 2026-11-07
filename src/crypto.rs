use crate::config::{load_config, RemoteBackendConfig};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::digest::{digest, SHA256};
use ring::pbkdf2::{derive, PBKDF2_HMAC_SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use std::num::NonZeroU32;

pub const ENC_PREFIX: &str = "enc:v1:";
pub const BIN_PREFIX: &str = "b64:v1:";
const KDF_SALT: &[u8] = b"vault-axi-at-rest-salt-v1";
const KDF_ITERATIONS: u32 = 10_000;

/// Compute lowercase hex SHA-256 digest of raw bytes.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let d = digest(&SHA256, bytes);
    d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Derive a 256-bit AES key from a passphrase or secret string using PBKDF2-HMAC-SHA256.
pub fn derive_key_bytes(secret: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    derive(
        PBKDF2_HMAC_SHA256,
        NonZeroU32::new(KDF_ITERATIONS).unwrap(),
        KDF_SALT,
        secret.as_bytes(),
        &mut key,
    );
    key
}

/// Resolve the active 256-bit encryption key for database-at-rest protection.
/// Precedence:
/// 1. `VAULT_ENCRYPTION_KEY` environment variable
/// 2. `encryptionKey` configured via `vault setup --encryption-key <passphrase>`
/// 3. Per-account cloud key derived from `user_id` + `vault_id` (if logged in)
/// 4. Local vault key from `localEncryptionKey` or deterministic local fallback
pub fn resolve_active_key() -> [u8; 32] {
    if let Ok(env_key) = std::env::var("VAULT_ENCRYPTION_KEY") {
        let trimmed = env_key.trim();
        if !trimmed.is_empty() {
            return derive_key_bytes(trimmed);
        }
    }

    let cfg = load_config();
    if let Some(ref custom_key) = cfg.encryption_key {
        let trimmed = custom_key.trim();
        if !trimmed.is_empty() {
            return derive_key_bytes(trimmed);
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
            return derive_key_bytes(&format!("vault-axi-cloud-e2ee:{uid}:{vid}"));
        }
    }

    if let Some(ref local_key) = cfg.local_encryption_key {
        let trimmed = local_key.trim();
        if !trimmed.is_empty() {
            return derive_key_bytes(trimmed);
        }
    }

    let vault_seed = cfg.vault_path.as_deref().unwrap_or("default-local-vault");
    derive_key_bytes(&format!("vault-axi-local-at-rest:{vault_seed}"))
}

/// Encrypt a UTF-8 string using AES-256-GCM with the provided 256-bit key.
/// Output format: `enc:v1:<nonce_b64>:<ciphertext_and_tag_b64>`
pub fn encrypt_with_key(plaintext: &str, key_bytes: &[u8; 32]) -> String {
    if plaintext.starts_with(ENC_PREFIX) {
        return plaintext.to_string();
    }
    let Ok(unbound) = UnboundKey::new(&AES_256_GCM, key_bytes) else {
        return plaintext.to_string();
    };
    let key = LessSafeKey::new(unbound);
    let rng = SystemRandom::new();
    let mut nonce_bytes = [0u8; 12];
    if rng.fill(&mut nonce_bytes).is_err() {
        return plaintext.to_string();
    }
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.as_bytes().to_vec();
    if key
        .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .is_err()
    {
        return plaintext.to_string();
    }
    format!(
        "{ENC_PREFIX}{}:{}",
        B64.encode(nonce_bytes),
        B64.encode(&in_out)
    )
}

/// Decrypt an `enc:v1:...` payload using the provided 256-bit key.
/// Plaintext strings without `enc:v1:` are returned unchanged for backward compatibility.
pub fn decrypt_with_key(stored: &str, key_bytes: &[u8; 32]) -> Result<String, String> {
    let Some(rest) = stored.strip_prefix(ENC_PREFIX) else {
        return Ok(stored.to_string());
    };
    let Some((nonce_b64, ct_b64)) = rest.split_once(':') else {
        return Err("Malformed encrypted payload (missing nonce separator)".to_string());
    };
    let nonce_vec = B64
        .decode(nonce_b64)
        .map_err(|e| format!("Invalid encryption nonce base64: {e}"))?;
    if nonce_vec.len() != 12 {
        return Err(format!("Invalid nonce length: {}", nonce_vec.len()));
    }
    let mut nonce_bytes = [0u8; 12];
    nonce_bytes.copy_from_slice(&nonce_vec);

    let mut ct_buf = B64
        .decode(ct_b64)
        .map_err(|e| format!("Invalid ciphertext base64: {e}"))?;

    let unbound =
        UnboundKey::new(&AES_256_GCM, key_bytes).map_err(|_| "Invalid AES-256 key".to_string())?;
    let key = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);

    let decrypted = key
        .open_in_place(nonce, Aad::empty(), &mut ct_buf)
        .map_err(|_| {
            "Decryption failed (authentication tag mismatch — check VAULT_ENCRYPTION_KEY or `vault setup --encryption-key`)"
                .to_string()
        })?;

    String::from_utf8(decrypted.to_vec())
        .map_err(|e| format!("Decrypted payload is not valid UTF-8: {e}"))
}

/// Encrypt a string at rest using the active vault encryption key.
pub fn encrypt_at_rest(plaintext: &str) -> String {
    let key = resolve_active_key();
    encrypt_with_key(plaintext, &key)
}

/// Decrypt a string stored at rest. Falls back to fallback keys if a custom key was recently toggled.
pub fn decrypt_at_rest(stored: &str) -> Result<String, String> {
    if !stored.starts_with(ENC_PREFIX) {
        return Ok(stored.to_string());
    }
    let primary_key = resolve_active_key();
    if let Ok(plain) = decrypt_with_key(stored, &primary_key) {
        return Ok(plain);
    }

    // Fallback check against default account/local key if user recently changed keys
    let cfg = load_config();
    if let Some(RemoteBackendConfig::Cloud {
        ref user_id,
        ref vault_id,
        ..
    }) = cfg.remote
    {
        if let Some(uid) = user_id.as_deref().filter(|s| !s.is_empty()) {
            let vid = vault_id.as_deref().unwrap_or("default");
            let cloud_default_key = derive_key_bytes(&format!("vault-axi-cloud-e2ee:{uid}:{vid}"));
            if let Ok(plain) = decrypt_with_key(stored, &cloud_default_key) {
                return Ok(plain);
            }
        }
    }
    let vault_seed = cfg.vault_path.as_deref().unwrap_or("default-local-vault");
    let local_default_key = derive_key_bytes(&format!("vault-axi-local-at-rest:{vault_seed}"));
    decrypt_with_key(stored, &local_default_key)
}

/// Encode binary file bytes as a `b64:v1:<base64>` string suitable for encrypted cloud/trash storage.
pub fn encode_binary_payload(bytes: &[u8]) -> String {
    format!("{BIN_PREFIX}{}", B64.encode(bytes))
}

/// Decode a `b64:v1:<base64>` string back into raw binary bytes.
pub fn decode_binary_payload(payload: &str) -> Option<Vec<u8>> {
    let b64_str = payload.strip_prefix(BIN_PREFIX)?;
    B64.decode(b64_str.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_256_gcm_roundtrip_and_tamper_detection_and_legacy_plaintext() {
        let key1 = derive_key_bytes("super-secret-passphrase");
        let key2 = derive_key_bytes("wrong-passphrase");

        let original = "# Secret Note\n\nContains sensitive credentials & UTF-8: café, ação, 東京!\n";
        let encrypted = encrypt_with_key(original, &key1);
        assert!(encrypted.starts_with(ENC_PREFIX));
        assert!(!encrypted.contains("Secret Note"));

        // Encrypting an already-encrypted payload is idempotent
        assert_eq!(encrypt_with_key(&encrypted, &key1), encrypted);

        // Decrypt with correct key succeeds
        let decrypted = decrypt_with_key(&encrypted, &key1).unwrap();
        assert_eq!(decrypted, original);

        // Decrypt with wrong key fails authentication tag verification
        assert!(decrypt_with_key(&encrypted, &key2).is_err());

        // Legacy unencrypted plaintext passes through transparently
        assert_eq!(
            decrypt_with_key("plain markdown note", &key1).unwrap(),
            "plain markdown note"
        );

        // Binary encoding roundtrip
        let raw_bin = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF];
        let encoded_bin = encode_binary_payload(&raw_bin);
        assert!(encoded_bin.starts_with(BIN_PREFIX));
        let enc_bin = encrypt_with_key(&encoded_bin, &key1);
        let dec_bin_str = decrypt_with_key(&enc_bin, &key1).unwrap();
        let decoded_bin = decode_binary_payload(&dec_bin_str).unwrap();
        assert_eq!(decoded_bin, raw_bin);

        // SHA-256 test vector
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
