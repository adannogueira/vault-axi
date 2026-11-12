//! JavaScript bindings for the Obsidian plugin. Structured values cross the boundary as plain
//! JS objects (camelCase) via `serde-wasm-bindgen`; keys travel as base64.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use vault_axi_core::crypto::keyring::{KdfParams, Keyring};
use vault_axi_core::crypto::{
    resolve_vault_key, v1, CryptoError, DataKey, Key, Keychain, VaultKeyError, VaultKeyStep,
};
use vault_axi_core::{hash, paths, payload, plan};
use wasm_bindgen::prelude::*;

fn js_err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn crypto_err(e: CryptoError) -> JsError {
    let code = match &e {
        CryptoError::WrongKey => "WRONG_KEY",
        CryptoError::Locked(_) => "LOCKED",
        CryptoError::UnknownKeyId(_) => "UNKNOWN_KEY_ID",
        CryptoError::InvalidKeyring(_) => "INVALID_KEYRING",
        _ => "CRYPTO_ERROR",
    };
    JsError::new(&format!("{code}: {e}"))
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsError> {
    value
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .map_err(js_err)
}

fn from_js<T: for<'de> Deserialize<'de>>(value: JsValue) -> Result<T, JsError> {
    serde_wasm_bindgen::from_value(value).map_err(js_err)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsDataKey {
    id: String,
    key: String,
}

impl JsDataKey {
    fn from_key(dk: &DataKey) -> Self {
        Self {
            id: dk.id().to_string(),
            key: B64.encode(dk.bytes()),
        }
    }

    fn into_key(self) -> Result<DataKey, JsError> {
        let bytes = B64.decode(&self.key).map_err(js_err)?;
        let key = Key::try_from(bytes).map_err(|_| JsError::new("data key must be 32 bytes"))?;
        Ok(DataKey::new(self.id, key))
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct KeychainOptions {
    passphrase: Option<String>,
    /// Hosted-cloud account ids, for vaults written with the legacy server-derivable key.
    cloud_user_id: Option<String>,
    cloud_vault_id: Option<String>,
    data_key: Option<JsDataKey>,
}

/// The keys this device holds. Mirrors the CLI: passphrase-derived v1 key first, then the
/// legacy cloud key, plus the vault data key once the keyring is unlocked.
#[wasm_bindgen(js_name = Keychain)]
pub struct JsKeychain(Keychain);

#[wasm_bindgen(js_class = Keychain)]
impl JsKeychain {
    #[wasm_bindgen(constructor)]
    pub fn new(options: JsValue) -> Result<JsKeychain, JsError> {
        let opts: KeychainOptions = if options.is_undefined() || options.is_null() {
            KeychainOptions::default()
        } else {
            from_js(options)?
        };
        let mut keys: Vec<Key> = Vec::new();
        if let Some(p) = opts
            .passphrase
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            keys.push(v1::derive_key(p));
        }
        if let Some(uid) = opts.cloud_user_id.as_deref().filter(|u| !u.is_empty()) {
            let vid = opts.cloud_vault_id.as_deref().unwrap_or("default");
            keys.push(v1::derive_key(&format!("vault-axi-cloud-e2ee:{uid}:{vid}")));
        }
        let mut iter = keys.into_iter();
        let Some(active) = iter.next() else {
            return Err(JsError::new(
                "NO_KEY: set an encryption passphrase or sign in to the hosted cloud",
            ));
        };
        let mut kc = Keychain::new(active);
        for k in iter {
            kc = kc.with_v1_fallback(k);
        }
        if let Some(dk) = opts.data_key {
            kc = kc.with_data_key(dk.into_key()?);
        }
        Ok(JsKeychain(kc))
    }

    #[wasm_bindgen(js_name = activeKeyId)]
    pub fn active_key_id(&self) -> String {
        self.0.active_key_id()
    }

    #[wasm_bindgen(js_name = hasDataKey)]
    pub fn has_data_key(&self) -> bool {
        self.0.data_key().is_some()
    }

    pub fn encrypt(&self, plaintext: &str, context: &str) -> String {
        self.0.encrypt(plaintext, context)
    }

    pub fn decrypt(&self, stored: &str, context: &str) -> Result<String, JsError> {
        self.0.decrypt(stored, context).map_err(crypto_err)
    }

    #[wasm_bindgen(js_name = isCurrent)]
    pub fn is_current(&self, stored: &str, context: &str) -> bool {
        self.0.is_current(stored, context)
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum JsVaultKeyStep {
    Use {
        #[serde(rename = "dataKey")]
        data_key: JsDataKey,
    },
    Legacy,
    Create {
        keyring: String,
        #[serde(rename = "dataKey")]
        data_key: JsDataKey,
    },
}

/// Decide which vault key to use (see `vault_axi_core::crypto::resolve_vault_key`). Returns
/// `{kind: "use", dataKey}`, `{kind: "legacy"}` or `{kind: "create", keyring, dataKey}`.
/// Errors start with `ENCRYPTION_KEY_MISMATCH:`, `ENCRYPTION_KEY_REQUIRED:` or `KEYRING_LOST:`.
#[wasm_bindgen(js_name = resolveVaultKey)]
pub fn resolve_vault_key_js(
    remote_keyring_json: Option<String>,
    passphrase: Option<String>,
    cached: JsValue,
    created_at: &str,
) -> Result<JsValue, JsError> {
    let remote = remote_keyring_json
        .as_deref()
        .map(Keyring::from_json)
        .transpose()
        .map_err(crypto_err)?;
    let cached = if cached.is_undefined() || cached.is_null() {
        None
    } else {
        Some(from_js::<JsDataKey>(cached)?.into_key()?)
    };
    let step = resolve_vault_key(
        remote.as_ref(),
        passphrase.as_deref(),
        cached,
        created_at,
        KdfParams::recommended(),
    )
    .map_err(|e| match e {
        VaultKeyError::Mismatch => JsError::new(
            "ENCRYPTION_KEY_MISMATCH: the passphrase does not unlock this vault's keyring",
        ),
        VaultKeyError::PassphraseRequired => {
            JsError::new("ENCRYPTION_KEY_REQUIRED: this vault is encrypted with a passphrase")
        }
        VaultKeyError::KeyringLost => JsError::new(
            "KEYRING_LOST: the remote has no keyring; set the passphrase to restore it",
        ),
        VaultKeyError::Crypto(e) => crypto_err(e),
    })?;
    to_js(&match step {
        VaultKeyStep::Use(dk) => JsVaultKeyStep::Use {
            data_key: JsDataKey::from_key(&dk),
        },
        VaultKeyStep::LegacyOnly => JsVaultKeyStep::Legacy,
        VaultKeyStep::Create { keyring, data_key } => JsVaultKeyStep::Create {
            keyring: keyring.to_json(),
            data_key: JsDataKey::from_key(&data_key),
        },
    })
}

/// Re-wrap the keyring around `dataKey` under a new passphrase; returns the new keyring JSON.
#[wasm_bindgen(js_name = rewrapKeyring)]
pub fn rewrap_keyring(
    keyring_json: &str,
    data_key: JsValue,
    new_passphrase: &str,
) -> Result<String, JsError> {
    let keyring = Keyring::from_json(keyring_json).map_err(crypto_err)?;
    let dk = from_js::<JsDataKey>(data_key)?.into_key()?;
    Ok(keyring
        .rewrap(&dk, new_passphrase, KdfParams::recommended())
        .map_err(crypto_err)?
        .to_json())
}

#[wasm_bindgen(js_name = planPull)]
pub fn plan_pull(
    local: JsValue,
    base: JsValue,
    remote: JsValue,
    pull_all: bool,
) -> Result<JsValue, JsError> {
    let local: Vec<plan::LocalFile> = from_js(local)?;
    let base: Vec<plan::BaseEntry> = from_js(base)?;
    let remote: Vec<plan::RemoteFile> = from_js(remote)?;
    to_js(&plan::plan_pull(&local, &base, &remote, pull_all))
}

#[wasm_bindgen(js_name = planPush)]
pub fn plan_push(
    local: JsValue,
    base: JsValue,
    remote: JsValue,
    current_key_id: &str,
) -> Result<JsValue, JsError> {
    let local: Vec<plan::LocalFile> = from_js(local)?;
    let base: Vec<plan::BaseEntry> = from_js(base)?;
    let remote: Vec<plan::RemoteFile> = from_js(remote)?;
    to_js(&plan::plan_push(&local, &base, &remote, current_key_id))
}

#[wasm_bindgen]
pub fn resolve(
    path: &str,
    base: &str,
    local: &str,
    remote: &str,
    now_ms: f64,
) -> Result<JsValue, JsError> {
    to_js(&plan::resolve(path, base, local, remote, now_ms as i64))
}

#[wasm_bindgen(js_name = payloadFromFile)]
pub fn payload_from_file(rel_path: &str, bytes: &[u8]) -> String {
    payload::from_file(rel_path, bytes)
}

#[wasm_bindgen(js_name = payloadToFile)]
pub fn payload_to_file(content: &str) -> Vec<u8> {
    payload::to_file(content)
}

#[wasm_bindgen(js_name = sha1Hex)]
pub fn sha1_hex(text: &str) -> String {
    hash::sha1_hex(text)
}

#[wasm_bindgen(js_name = isSyncable)]
pub fn is_syncable(rel_path: &str, size: f64) -> bool {
    paths::is_syncable(rel_path, size as u64)
}

#[wasm_bindgen(js_name = isIgnoredDir)]
pub fn is_ignored_dir(name: &str) -> bool {
    paths::IGNORED_DIRS.contains(&name)
}

#[wasm_bindgen(js_name = keyringPath)]
pub fn keyring_path() -> String {
    vault_axi_core::crypto::keyring::KEYRING_PATH.to_string()
}
