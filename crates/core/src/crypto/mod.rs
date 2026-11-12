//! Payload encryption.
//!
//! - [`v1`]: legacy `enc:v1:` envelope. Still produced for vaults without a passphrase and
//!   decryptable forever.

pub mod v1;

use std::fmt;

/// A 256-bit AES key.
pub type Key = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    Malformed(&'static str),
    /// Authentication failed: wrong key, or the ciphertext was tampered with.
    WrongKey,
    NotUtf8,
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
        }
    }
}

impl std::error::Error for CryptoError {}
