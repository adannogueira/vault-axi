//! Which vault data key a client should use, given the remote keyring, its passphrase and its
//! cached key. Clients do the I/O (fetch, create-if-absent, cache) around this decision.

use super::keyring::{KdfParams, Keyring};
use super::{CryptoError, DataKey};

#[derive(Debug)]
pub enum VaultKeyStep {
    /// Encrypt with this data key (from the cache or freshly unlocked); cache it.
    Use(DataKey),
    /// No keyring and no passphrase: the vault stays on legacy v1 keys.
    LegacyOnly,
    /// Upload this keyring with create-if-absent semantics. If another device created one
    /// first, fetch it and decide again.
    Create { keyring: Keyring, data_key: DataKey },
}

#[derive(Debug, PartialEq, Eq)]
pub enum VaultKeyError {
    /// The passphrase does not unlock the remote keyring.
    Mismatch,
    /// The remote is encrypted with a passphrase this device doesn't have.
    PassphraseRequired,
    /// This device holds a data key but the remote lost its keyring and no passphrase is set
    /// to restore it.
    KeyringLost,
    Crypto(CryptoError),
}

pub fn resolve_vault_key(
    remote: Option<&Keyring>,
    passphrase: Option<&str>,
    cached: Option<DataKey>,
    created_at: &str,
    kdf: KdfParams,
) -> Result<VaultKeyStep, VaultKeyError> {
    let passphrase = passphrase.map(str::trim).filter(|p| !p.is_empty());
    match (remote, passphrase) {
        (Some(kr), _) if cached.as_ref().is_some_and(|c| c.id() == kr.key_id) => {
            Ok(VaultKeyStep::Use(cached.expect("checked above")))
        }
        (Some(kr), Some(pass)) => kr.unlock(pass).map(VaultKeyStep::Use).map_err(|e| match e {
            CryptoError::WrongKey => VaultKeyError::Mismatch,
            other => VaultKeyError::Crypto(other),
        }),
        (Some(_), None) => Err(VaultKeyError::PassphraseRequired),
        (None, Some(pass)) => {
            // Re-wrap a cached key if the remote lost its keyring, so data this device already
            // encrypted stays readable; otherwise start a fresh vault key.
            let (keyring, data_key) = match cached {
                Some(dk) => (
                    Keyring::create_for(&dk, pass, kdf, created_at)
                        .map_err(VaultKeyError::Crypto)?,
                    dk,
                ),
                None => Keyring::create(pass, kdf, created_at).map_err(VaultKeyError::Crypto)?,
            };
            Ok(VaultKeyStep::Create { keyring, data_key })
        }
        (None, None) if cached.is_some() => Err(VaultKeyError::KeyringLost),
        (None, None) => Ok(VaultKeyStep::LegacyOnly),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kdf() -> KdfParams {
        KdfParams::with_cost(8 * 1024, 1, 1)
    }

    fn existing(pass: &str) -> (Keyring, DataKey) {
        Keyring::create(pass, kdf(), "t").unwrap()
    }

    #[test]
    fn decision_matrix() {
        let (kr, dk) = existing("pass");

        let step = resolve_vault_key(Some(&kr), None, Some(dk.clone()), "t", kdf()).unwrap();
        assert!(
            matches!(step, VaultKeyStep::Use(k) if k == dk),
            "cached key wins without passphrase"
        );

        let step = resolve_vault_key(Some(&kr), Some(" pass "), None, "t", kdf()).unwrap();
        assert!(matches!(step, VaultKeyStep::Use(k) if k == dk));

        assert_eq!(
            resolve_vault_key(Some(&kr), Some("nope"), None, "t", kdf()).unwrap_err(),
            VaultKeyError::Mismatch
        );
        assert_eq!(
            resolve_vault_key(Some(&kr), None, None, "t", kdf()).unwrap_err(),
            VaultKeyError::PassphraseRequired
        );
        let (_, stale) = existing("x");
        assert_eq!(
            resolve_vault_key(Some(&kr), Some(""), Some(stale), "t", kdf()).unwrap_err(),
            VaultKeyError::PassphraseRequired,
            "a cached key for another keyring is ignored"
        );

        match resolve_vault_key(None, Some("pass"), None, "t", kdf()).unwrap() {
            VaultKeyStep::Create { keyring, data_key } => {
                assert_eq!(keyring.unlock("pass").unwrap(), data_key)
            }
            other => panic!("{other:?}"),
        }
        match resolve_vault_key(None, Some("pass"), Some(dk.clone()), "t", kdf()).unwrap() {
            VaultKeyStep::Create { keyring, data_key } => {
                assert_eq!(
                    data_key, dk,
                    "a lost keyring is restored around the cached key"
                );
                assert_eq!(keyring.unlock("pass").unwrap(), dk);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            resolve_vault_key(None, None, Some(dk), "t", kdf()).unwrap_err(),
            VaultKeyError::KeyringLost
        );
        assert!(matches!(
            resolve_vault_key(None, None, None, "t", kdf()).unwrap(),
            VaultKeyStep::LegacyOnly
        ));
    }
}
