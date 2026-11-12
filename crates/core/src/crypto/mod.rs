//! Payload encryption.
//!
//! - [`v1`]: legacy `enc:v1:` envelope. Still produced for vaults without a passphrase and
//!   decryptable forever.
//! - [`v2`]: `enc:v2:` envelope under a random per-vault data key from the [`keyring`].
//! - [`Keychain`]: the keys one client holds, and the single entry point for encrypt/decrypt.

mod keychain;
pub mod keyring;
pub mod v1;
pub mod v2;

pub use keychain::Keychain;

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::OsRng;
use std::fmt;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// A 256-bit AES key.
pub type Key = [u8; 32];

/// A vault data key together with its public id. Zeroed on drop; `Debug` never prints the key.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct DataKey {
    id: String,
    key: Key,
}

impl DataKey {
    pub fn new(id: String, key: Key) -> Self {
        Self { id, key }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn bytes(&self) -> &Key {
        &self.key
    }
}

impl fmt::Debug for DataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DataKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    OsRng.fill_bytes(&mut out);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    Malformed(&'static str),
    /// Authentication failed: wrong key or passphrase, or the ciphertext was tampered with.
    WrongKey,
    NotUtf8,
    /// A v2 payload was encrypted under a data key this client does not hold.
    UnknownKeyId(String),
    /// A v2 payload was found but no data key is unlocked.
    Locked(String),
    InvalidKeyring(&'static str),
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "Malformed encrypted payload ({what})"),
            Self::WrongKey => write!(
                f,
                "Decryption failed (authentication tag mismatch - check VAULT_ENCRYPTION_KEY or `vault setup --encryption-key`)"
            ),
            Self::NotUtf8 => write!(f, "Decrypted payload is not valid UTF-8"),
            Self::UnknownKeyId(id) => write!(f, "Payload is encrypted with unknown vault key {id}"),
            Self::Locked(id) => write!(
                f,
                "Payload is encrypted with vault key {id}, but no encryption passphrase is configured"
            ),
            Self::InvalidKeyring(why) => write!(f, "Invalid keyring: {why}"),
        }
    }
}

impl std::error::Error for CryptoError {}
