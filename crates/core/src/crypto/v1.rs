//! `enc:v1:<nonce_b64>:<ciphertext_and_tag_b64>`: AES-256-GCM, no associated data, key from
//! PBKDF2-HMAC-SHA256 with a fixed salt. Kept bit-for-bit compatible with vault-axi <= 1.1.

use super::{CryptoError, Key};
use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

pub const PREFIX: &str = "enc:v1:";
const KDF_SALT: &[u8] = b"vault-axi-at-rest-salt-v1";
const KDF_ITERATIONS: u32 = 10_000;

pub fn derive_key(secret: &str) -> Key {
    pbkdf2::pbkdf2_hmac_array::<sha2::Sha256, 32>(secret.as_bytes(), KDF_SALT, KDF_ITERATIONS)
}

pub fn encrypt(plaintext: &str, key: &Key) -> String {
    let cipher = Aes256Gcm::new(key.into());
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .expect("AES-GCM encryption only fails for inputs over 64 GiB");
    format!("{PREFIX}{}:{}", B64.encode(nonce), B64.encode(ct))
}

/// Short public id of a v1 key (first 16 hex chars of its SHA-256), stored as `enc_key_id`.
pub fn fingerprint(key: &Key) -> String {
    crate::hash::sha256_hex(key)[..16].to_string()
}

pub fn is_v1(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

/// Decrypt an `enc:v1:` payload. Callers decide what to do with non-v1 input; this
/// function rejects it.
pub fn decrypt(stored: &str, key: &Key) -> Result<String, CryptoError> {
    let rest = stored
        .strip_prefix(PREFIX)
        .ok_or(CryptoError::Malformed("missing enc:v1 prefix"))?;
    let (nonce_b64, ct_b64) = rest
        .split_once(':')
        .ok_or(CryptoError::Malformed("missing nonce separator"))?;
    let nonce = B64
        .decode(nonce_b64)
        .map_err(|_| CryptoError::Malformed("nonce is not base64"))?;
    if nonce.len() != 12 {
        return Err(CryptoError::Malformed("nonce is not 12 bytes"));
    }
    let ct = B64
        .decode(ct_b64)
        .map_err(|_| CryptoError::Malformed("ciphertext is not base64"))?;
    let plain = Aes256Gcm::new(key.into())
        .decrypt(Nonce::from_slice(&nonce), ct.as_ref())
        .map_err(|_| CryptoError::WrongKey)?;
    String::from_utf8(plain).map_err(|_| CryptoError::NotUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_key() {
        let k1 = derive_key("one");
        let k2 = derive_key("two");
        let ct = encrypt("# Secret\ncafé 東京", &k1);
        assert!(is_v1(&ct));
        assert!(!ct.contains("Secret"));
        assert_eq!(decrypt(&ct, &k1).unwrap(), "# Secret\ncafé 東京");
        assert_eq!(decrypt(&ct, &k2), Err(CryptoError::WrongKey));
        assert_ne!(
            encrypt("same", &k1),
            encrypt("same", &k1),
            "nonce must be random"
        );
    }

    #[test]
    fn malformed_inputs() {
        let k = derive_key("k");
        assert!(matches!(
            decrypt("plain", &k),
            Err(CryptoError::Malformed(_))
        ));
        assert!(matches!(
            decrypt("enc:v1:abc", &k),
            Err(CryptoError::Malformed(_))
        ));
        assert!(matches!(
            decrypt("enc:v1:AAAA:AAAA", &k),
            Err(CryptoError::Malformed(_))
        ));
    }
}
