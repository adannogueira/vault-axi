use super::{v1, v2, CryptoError, DataKey, Key};
use zeroize::Zeroizing;

/// Every key one client can use, with the rules for choosing between them.
///
/// - Encrypts with the v2 data key when one is unlocked, otherwise with the active v1 key.
/// - Decrypts v2 payloads only with the matching data key (never falls back to v1), v1 payloads
///   with the active v1 key then each fallback in order, and passes legacy plaintext through.
pub struct Keychain {
    data_key: Option<DataKey>,
    v1_active: Zeroizing<Key>,
    v1_fallbacks: Vec<Zeroizing<Key>>,
}

impl std::fmt::Debug for Keychain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keychain")
            .field("data_key", &self.data_key)
            .field("v1_keys", &(1 + self.v1_fallbacks.len()))
            .finish()
    }
}

impl Keychain {
    pub fn new(v1_active: Key) -> Self {
        Self {
            data_key: None,
            v1_active: Zeroizing::new(v1_active),
            v1_fallbacks: Vec::new(),
        }
    }

    pub fn with_data_key(mut self, data_key: DataKey) -> Self {
        self.data_key = Some(data_key);
        self
    }

    /// Older v1 keys that may still have encrypted some payloads (tried in insertion order).
    pub fn with_v1_fallback(mut self, key: Key) -> Self {
        if key != *self.v1_active && !self.v1_fallbacks.iter().any(|k| **k == key) {
            self.v1_fallbacks.push(Zeroizing::new(key));
        }
        self
    }

    pub fn data_key(&self) -> Option<&DataKey> {
        self.data_key.as_ref()
    }

    /// Id recorded next to remote payloads so other devices can tell what needs re-encrypting:
    /// the data key id, or the v1 key fingerprint (as written by vault-axi <= 1.1).
    pub fn active_key_id(&self) -> String {
        match &self.data_key {
            Some(dk) => dk.id().to_string(),
            None => v1::fingerprint(&self.v1_active),
        }
    }

    /// `context` is the payload's vault-relative path; v2 binds it into the ciphertext.
    pub fn encrypt(&self, plaintext: &str, context: &str) -> String {
        match &self.data_key {
            Some(dk) => v2::encrypt(plaintext, dk, context),
            None => v1::encrypt(plaintext, &self.v1_active),
        }
    }

    pub fn decrypt(&self, stored: &str, context: &str) -> Result<String, CryptoError> {
        if v2::is_v2(stored) {
            let key_id = v2::key_id_of(stored).unwrap_or_default();
            return match &self.data_key {
                Some(dk) => v2::decrypt(stored, dk, context),
                None => Err(CryptoError::Locked(key_id.to_string())),
            };
        }
        if !v1::is_v1(stored) {
            return Ok(stored.to_string());
        }
        let mut last_err = CryptoError::WrongKey;
        for key in std::iter::once(&self.v1_active).chain(&self.v1_fallbacks) {
            match v1::decrypt(stored, key) {
                Ok(plain) => return Ok(plain),
                Err(CryptoError::WrongKey) => {}
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Whether `stored` is already encrypted the way [`Self::encrypt`] would encrypt it now.
    pub fn is_current(&self, stored: &str, context: &str) -> bool {
        match &self.data_key {
            Some(dk) => {
                v2::key_id_of(stored) == Some(dk.id()) && v2::decrypt(stored, dk, context).is_ok()
            }
            None => v1::is_v1(stored) && v1::decrypt(stored, &self.v1_active).is_ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dk(id: &str) -> DataKey {
        DataKey::new(id.into(), [7; 32])
    }

    #[test]
    fn without_data_key_behaves_like_v1() {
        let active = v1::derive_key("active");
        let kc = Keychain::new(active);
        let ct = kc.encrypt("hello", "a.md");
        assert!(v1::is_v1(&ct));
        assert_eq!(kc.decrypt(&ct, "anything").unwrap(), "hello");
        assert_eq!(kc.active_key_id(), v1::fingerprint(&active));
        assert!(kc.is_current(&ct, "a.md"));
        assert_eq!(
            kc.decrypt("legacy plaintext", "a.md").unwrap(),
            "legacy plaintext"
        );
    }

    #[test]
    fn v1_fallbacks_are_tried_in_order() {
        let old = v1::derive_key("old");
        let ct_old = v1::encrypt("from old", &old);
        let kc = Keychain::new(v1::derive_key("new"));
        assert_eq!(kc.decrypt(&ct_old, "a.md"), Err(CryptoError::WrongKey));
        let kc = kc
            .with_v1_fallback(v1::derive_key("other"))
            .with_v1_fallback(old);
        assert_eq!(kc.decrypt(&ct_old, "a.md").unwrap(), "from old");
        assert!(!kc.is_current(&ct_old, "a.md"));
    }

    #[test]
    fn data_key_encrypts_v2_and_still_reads_v1() {
        let legacy = v1::derive_key("pass");
        let ct_v1 = v1::encrypt("old note", &legacy);
        let kc = Keychain::new(legacy).with_data_key(dk("k2_1"));
        let ct = kc.encrypt("new note", "n.md");
        assert!(v2::is_v2(&ct));
        assert_eq!(kc.active_key_id(), "k2_1");
        assert_eq!(kc.decrypt(&ct, "n.md").unwrap(), "new note");
        assert_eq!(kc.decrypt(&ct_v1, "n.md").unwrap(), "old note");
        assert!(kc.is_current(&ct, "n.md"));
        assert!(!kc.is_current(&ct_v1, "n.md"));
        assert!(!kc.is_current(&ct, "moved.md"));
    }

    #[test]
    fn v2_payload_without_data_key_is_locked() {
        let ct = v2::encrypt("x", &dk("k2_9"), "a.md");
        let kc = Keychain::new(v1::derive_key("p"));
        assert_eq!(
            kc.decrypt(&ct, "a.md"),
            Err(CryptoError::Locked("k2_9".into()))
        );
    }
}
