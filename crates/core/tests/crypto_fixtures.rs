//! Payloads produced by vault-axi 1.1.2 (ring-based). They must decrypt forever.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use vault_axi_core::crypto::{v1, Key};

#[test]
fn frozen_v1_payloads_decrypt() {
    let doc: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/crypto-v1.json")).unwrap();
    let cases = doc["cases"].as_array().unwrap();
    assert!(cases.len() >= 6);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let key_spec = &case["key"];
        let key: Key = match key_spec["kind"].as_str().unwrap() {
            "pbkdf2" => v1::derive_key(key_spec["input"].as_str().unwrap()),
            "raw" => B64
                .decode(key_spec["b64"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap(),
            other => panic!("{name}: unknown key kind {other}"),
        };
        let plain = v1::decrypt(case["ciphertext"].as_str().unwrap(), &key)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(plain, case["plaintext"].as_str().unwrap(), "{name}");
    }
}

/// Keyring and payloads produced by the first v2 implementation. They must unlock and decrypt forever.
#[test]
fn frozen_v2_keyring_and_payloads_decrypt() {
    use vault_axi_core::crypto::keyring::Keyring;
    use vault_axi_core::crypto::{CryptoError, Keychain};

    let doc: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/crypto-v2.json")).unwrap();
    let keyring = Keyring::from_json(&doc["keyring"].to_string()).unwrap();
    let data_key = keyring.unlock(doc["passphrase"].as_str().unwrap()).unwrap();
    assert_eq!(keyring.unlock("wrong"), Err(CryptoError::WrongKey));

    let keychain = Keychain::new(v1::derive_key("unrelated")).with_data_key(data_key);
    for case in doc["cases"].as_array().unwrap() {
        let path = case["path"].as_str().unwrap();
        let ct = case["ciphertext"].as_str().unwrap();
        assert_eq!(
            keychain.decrypt(ct, path).unwrap(),
            case["plaintext"].as_str().unwrap(),
            "{path}"
        );
        assert!(keychain.is_current(ct, path));
    }
}
