//! `enc:v2:<keyId>:<nonce_b64>:<ciphertext_and_tag_b64>`: AES-256-GCM under a vault data key,
//! with the key id and the payload's vault path bound as associated data, so a ciphertext moved
//! to another path (or row) fails authentication.

use super::{random_bytes, CryptoError, DataKey};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

pub const PREFIX: &str = "enc:v2:";

fn aad(key_id: &str, context: &str) -> String {
    format!("vault-axi/v2/{key_id}/{context}")
}

pub fn is_v2(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

/// The key id a v2 payload was encrypted under.
pub fn key_id_of(stored: &str) -> Option<&str> {
    stored.strip_prefix(PREFIX)?.split(':').next()
}

pub fn encrypt(plaintext: &str, key: &DataKey, context: &str) -> String {
    let nonce: [u8; 12] = random_bytes();
    let ct = Aes256Gcm::new(key.bytes().into())
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext.as_bytes(),
                aad: aad(key.id(), context).as_bytes(),
            },
        )
        .expect("AES-GCM encryption only fails for inputs over 64 GiB");
    format!(
        "{PREFIX}{}:{}:{}",
        key.id(),
        B64.encode(nonce),
        B64.encode(ct)
    )
}

pub fn decrypt(stored: &str, key: &DataKey, context: &str) -> Result<String, CryptoError> {
    let rest = stored
        .strip_prefix(PREFIX)
        .ok_or(CryptoError::Malformed("missing enc:v2 prefix"))?;
    let mut parts = rest.splitn(3, ':');
    let (Some(key_id), Some(nonce_b64), Some(ct_b64)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(CryptoError::Malformed("expected keyId:nonce:ciphertext"));
    };
    if key_id != key.id() {
        return Err(CryptoError::UnknownKeyId(key_id.to_string()));
    }
    let nonce = B64
        .decode(nonce_b64)
        .map_err(|_| CryptoError::Malformed("nonce is not base64"))?;
    if nonce.len() != 12 {
        return Err(CryptoError::Malformed("nonce is not 12 bytes"));
    }
    let ct = B64
        .decode(ct_b64)
        .map_err(|_| CryptoError::Malformed("ciphertext is not base64"))?;
    let plain = Aes256Gcm::new(key.bytes().into())
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ct,
                aad: aad(key_id, context).as_bytes(),
            },
        )
        .map_err(|_| CryptoError::WrongKey)?;
    String::from_utf8(plain).map_err(|_| CryptoError::NotUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str, byte: u8) -> DataKey {
        DataKey::new(id.to_string(), [byte; 32])
    }

    #[test]
    fn roundtrip_and_format() {
        let k = key("k2_0011223344556677", 1);
        let ct = encrypt("# Note\ncafé", &k, "notes/a.md");
        assert!(ct.starts_with("enc:v2:k2_0011223344556677:"));
        assert_eq!(key_id_of(&ct), Some("k2_0011223344556677"));
        assert_eq!(decrypt(&ct, &k, "notes/a.md").unwrap(), "# Note\ncafé");
        assert_eq!(decrypt(&encrypt("", &k, "e.md"), &k, "e.md").unwrap(), "");
    }

    #[test]
    fn ciphertext_is_bound_to_its_path() {
        let k = key("k2_a", 1);
        let ct = encrypt("secret", &k, "notes/a.md");
        assert_eq!(decrypt(&ct, &k, "notes/b.md"), Err(CryptoError::WrongKey));
    }

    #[test]
    fn rejects_other_keys_and_tampering() {
        let k = key("k2_a", 1);
        let ct = encrypt("secret", &k, "a.md");
        assert_eq!(
            decrypt(&ct, &key("k2_b", 1), "a.md"),
            Err(CryptoError::UnknownKeyId("k2_a".into()))
        );
        assert_eq!(
            decrypt(&ct, &key("k2_a", 2), "a.md"),
            Err(CryptoError::WrongKey)
        );
        let relabeled = ct.replace("k2_a", "k2_b");
        assert_eq!(
            decrypt(&relabeled, &key("k2_b", 1), "a.md"),
            Err(CryptoError::WrongKey)
        );
        assert!(matches!(
            decrypt("enc:v2:k2_a:xx", &k, "a.md"),
            Err(CryptoError::Malformed(_))
        ));
    }
}
