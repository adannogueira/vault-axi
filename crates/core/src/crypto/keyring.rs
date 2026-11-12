//! The per-vault keyring: a random data key wrapped by an Argon2id key derived from the user's
//! passphrase. It is stored next to the vault data (it contains nothing usable without the
//! passphrase), so every device can unlock the same data key.

use super::{random_bytes, CryptoError, DataKey, Key};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// Reserved vault path the keyring is stored under on every backend.
pub const KEYRING_PATH: &str = ".vault-axi/keyring.json";
const VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub alg: String,
    /// Memory cost in KiB.
    pub m: u32,
    pub t: u32,
    pub p: u32,
    pub salt: String,
}

impl KdfParams {
    /// OWASP-recommended Argon2id profile: 64 MiB, 3 passes, 1 lane.
    pub fn recommended() -> Self {
        Self::with_cost(64 * 1024, 3, 1)
    }

    /// Fresh random salt with the given cost. Low costs are for tests only.
    pub fn with_cost(m: u32, t: u32, p: u32) -> Self {
        Self {
            alg: "argon2id".to_string(),
            m,
            t,
            p,
            salt: B64.encode(random_bytes::<16>()),
        }
    }

    /// Bounds keep a hostile keyring from requesting absurd memory or time, and from
    /// downgrading below a usable floor.
    fn validate(&self) -> Result<Vec<u8>, CryptoError> {
        if self.alg != "argon2id" {
            return Err(CryptoError::InvalidKeyring("unsupported KDF"));
        }
        if !(8 * 1024..=1024 * 1024).contains(&self.m)
            || !(1..=16).contains(&self.t)
            || !(1..=8).contains(&self.p)
        {
            return Err(CryptoError::InvalidKeyring("KDF parameters out of bounds"));
        }
        let salt = B64
            .decode(&self.salt)
            .map_err(|_| CryptoError::InvalidKeyring("salt is not base64"))?;
        if salt.len() < 16 {
            return Err(CryptoError::InvalidKeyring("salt shorter than 16 bytes"));
        }
        Ok(salt)
    }

    fn derive(&self, passphrase: &str) -> Result<Zeroizing<Key>, CryptoError> {
        let salt = self.validate()?;
        let params = Params::new(self.m, self.t, self.p, Some(32))
            .map_err(|_| CryptoError::InvalidKeyring("KDF parameters rejected"))?;
        let mut out = Zeroizing::new([0u8; 32]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase.as_bytes(), &salt, out.as_mut())
            .map_err(|_| CryptoError::InvalidKeyring("KDF failed"))?;
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Keyring {
    pub version: u32,
    pub key_id: String,
    pub kdf: KdfParams,
    pub wrapped_key: String,
    pub created_at: String,
}

fn wrap_aad(key_id: &str) -> String {
    format!("vault-axi/keyring/v2/{key_id}")
}

fn new_key_id() -> String {
    let id: [u8; 8] = random_bytes();
    format!(
        "k2_{}",
        id.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

impl Keyring {
    /// Create a keyring around a fresh random data key. `created_at` is supplied by the caller
    /// (the core has no clock).
    pub fn create(
        passphrase: &str,
        kdf: KdfParams,
        created_at: &str,
    ) -> Result<(Self, DataKey), CryptoError> {
        let data_key = DataKey::new(new_key_id(), random_bytes());
        let keyring = Self::wrap(&data_key, passphrase, kdf, created_at)?;
        Ok((keyring, data_key))
    }

    /// Create a keyring around an existing data key (e.g. to restore a lost remote keyring).
    pub fn create_for(
        data_key: &DataKey,
        passphrase: &str,
        kdf: KdfParams,
        created_at: &str,
    ) -> Result<Self, CryptoError> {
        Self::wrap(data_key, passphrase, kdf, created_at)
    }

    fn wrap(
        data_key: &DataKey,
        passphrase: &str,
        kdf: KdfParams,
        created_at: &str,
    ) -> Result<Self, CryptoError> {
        let kek = kdf.derive(passphrase)?;
        let nonce: [u8; 12] = random_bytes();
        let ct = Aes256Gcm::new(kek.as_ref().into())
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: data_key.bytes(),
                    aad: wrap_aad(data_key.id()).as_bytes(),
                },
            )
            .expect("wrapping a 32-byte key cannot fail");
        let mut wrapped = nonce.to_vec();
        wrapped.extend_from_slice(&ct);
        Ok(Self {
            version: VERSION,
            key_id: data_key.id().to_string(),
            kdf,
            wrapped_key: B64.encode(wrapped),
            created_at: created_at.to_string(),
        })
    }

    /// Unwrap the data key. A wrong passphrase yields [`CryptoError::WrongKey`].
    pub fn unlock(&self, passphrase: &str) -> Result<DataKey, CryptoError> {
        if self.version != VERSION {
            return Err(CryptoError::InvalidKeyring("unsupported keyring version"));
        }
        let wrapped = B64
            .decode(&self.wrapped_key)
            .map_err(|_| CryptoError::InvalidKeyring("wrappedKey is not base64"))?;
        if wrapped.len() != 12 + 32 + 16 {
            return Err(CryptoError::InvalidKeyring(
                "wrappedKey has the wrong length",
            ));
        }
        let kek = self.kdf.derive(passphrase)?;
        let raw = Zeroizing::new(
            Aes256Gcm::new(kek.as_ref().into())
                .decrypt(
                    Nonce::from_slice(&wrapped[..12]),
                    Payload {
                        msg: &wrapped[12..],
                        aad: wrap_aad(&self.key_id).as_bytes(),
                    },
                )
                .map_err(|_| CryptoError::WrongKey)?,
        );
        let key: Key = raw
            .as_slice()
            .try_into()
            .map_err(|_| CryptoError::InvalidKeyring("data key is not 32 bytes"))?;
        Ok(DataKey::new(self.key_id.clone(), key))
    }

    /// Re-wrap the same data key under a new passphrase (fresh salt). Data stays readable and
    /// does not need re-encryption.
    pub fn rewrap(
        &self,
        data_key: &DataKey,
        new_passphrase: &str,
        kdf: KdfParams,
    ) -> Result<Self, CryptoError> {
        if data_key.id() != self.key_id {
            return Err(CryptoError::UnknownKeyId(data_key.id().to_string()));
        }
        Self::wrap(data_key, new_passphrase, kdf, &self.created_at)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("keyring serializes")
    }

    pub fn from_json(json: &str) -> Result<Self, CryptoError> {
        serde_json::from_str(json)
            .map_err(|_| CryptoError::InvalidKeyring("not a keyring document"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fast() -> KdfParams {
        KdfParams::with_cost(8 * 1024, 1, 1)
    }

    #[test]
    fn create_unlock_and_wrong_passphrase() {
        let (kr, dk) = Keyring::create("hunter2", fast(), "2026-11-12T00:00:00Z").unwrap();
        assert!(kr.key_id.starts_with("k2_") && kr.key_id.len() == 19);
        assert_eq!(kr.unlock("hunter2").unwrap(), dk);
        assert_eq!(kr.unlock("hunter3"), Err(CryptoError::WrongKey));
    }

    #[test]
    fn rewrap_keeps_the_data_key() {
        let (kr, dk) = Keyring::create("old", fast(), "t").unwrap();
        let kr2 = kr.rewrap(&dk, "new", fast()).unwrap();
        assert_eq!(kr2.key_id, kr.key_id);
        assert_ne!(kr2.kdf.salt, kr.kdf.salt);
        assert_eq!(kr2.unlock("new").unwrap(), dk);
        assert_eq!(kr2.unlock("old"), Err(CryptoError::WrongKey));
    }

    #[test]
    fn create_for_wraps_an_existing_key() {
        let dk = DataKey::new("k2_0123456789abcdef".into(), [9; 32]);
        let kr = Keyring::create_for(&dk, "p", fast(), "t").unwrap();
        assert_eq!(kr.key_id, dk.id());
        assert_eq!(kr.unlock("p").unwrap(), dk);
    }

    #[test]
    fn json_roundtrip_uses_camel_case() {
        let (kr, _) = Keyring::create("p", fast(), "t").unwrap();
        let json = kr.to_json();
        assert!(json.contains("\"keyId\"") && json.contains("\"wrappedKey\""));
        assert_eq!(Keyring::from_json(&json).unwrap(), kr);
        assert!(Keyring::from_json("{}").is_err());
    }

    #[test]
    fn hostile_parameters_are_rejected() {
        let (kr, _) = Keyring::create("p", fast(), "t").unwrap();
        for tweak in [
            |k: &mut Keyring| k.kdf.m = 4 * 1024 * 1024,
            |k: &mut Keyring| k.kdf.m = 64,
            |k: &mut Keyring| k.kdf.t = 1000,
            |k: &mut Keyring| k.kdf.alg = "pbkdf2".into(),
            |k: &mut Keyring| k.kdf.salt = B64.encode([0u8; 4]),
            |k: &mut Keyring| k.version = 3,
            |k: &mut Keyring| k.wrapped_key = B64.encode([0u8; 10]),
        ] {
            let mut bad = kr.clone();
            tweak(&mut bad);
            assert!(
                matches!(bad.unlock("p"), Err(CryptoError::InvalidKeyring(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn swapped_key_id_fails_authentication() {
        let (mut kr, _) = Keyring::create("p", fast(), "t").unwrap();
        kr.key_id = "k2_ffffffffffffffff".into();
        assert_eq!(kr.unlock("p"), Err(CryptoError::WrongKey));
    }
}
