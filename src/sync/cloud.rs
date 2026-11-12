//! Hosted cloud backend: the `vault_notes` table behind PostgREST, protected by RLS
//! (`auth.uid() = user_id`).

use super::account::{exchange_api_key, try_refresh_hosted_token};
use super::engine::{Fetched, Put, PutOutcome, Remote};
use super::now_ms;
use crate::axi::AxiError;
use crate::config::{
    env_api_key, RemoteBackendConfig, VaultConfig, HOSTED_SUPABASE_ANON_KEY, HOSTED_SUPABASE_URL,
};
use crate::crypto::{invalid_keyring_error, KeyringStore, StoredKeyring};
use serde::Deserialize;
use serde_json::{json, Value};
use vault_axi_core::crypto::keyring::{Keyring, KEYRING_PATH};
use vault_axi_core::crypto::Keychain;
use vault_axi_core::hash::sha1_hex;
use vault_axi_core::plan::RemoteFile;

const PAGE_SIZE: usize = 1000;
const BATCH_ROWS: usize = 20;
const BATCH_BYTES: usize = 256_000;
const BATCHABLE_ROW_BYTES: usize = 48_000;
/// Key id written by vault-axi versions that predate key fingerprints.
const LEGACY_KEY_ID: &str = "enc-v1";

pub struct CloudRemote {
    bearer: String,
    refresh_token: Option<String>,
    user_id: Option<String>,
    vault_id: String,
    scope: String,
    authenticated: bool,
    /// Self-hosted tables may lack the `enc_key_id` column.
    has_key_id_column: bool,
}

#[derive(Deserialize)]
struct Row {
    path: String,
    #[serde(default)]
    hash: String,
    #[serde(default)]
    mtime_ms: i64,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    enc_key_id: String,
    #[serde(default)]
    content: String,
}

fn http_error(context: &str, e: ureq::Error) -> AxiError {
    AxiError::new(
        format!("{context}: {e}"),
        "SYNC_ERROR",
        vec![
            "Check your network connection or run `vault login --email <email> --password <password>`"
                .to_string(),
        ],
    )
}

fn read_body(resp: ureq::Response) -> String {
    resp.into_string().unwrap_or_default()
}

pub(super) fn urlencoding_simple(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Encode a value for a PostgREST `eq.` filter, quoting reserved characters.
pub(super) fn postgrest_eq_val(s: &str) -> String {
    if s.chars().any(|c| matches!(c, ',' | '(' | ')' | '"' | '\\')) {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        urlencoding_simple(&format!("\"{escaped}\""))
    } else {
        urlencoding_simple(s)
    }
}

impl CloudRemote {
    pub fn from_config(cfg: &VaultConfig) -> Result<Self, AxiError> {
        let Some(RemoteBackendConfig::Cloud {
            access_token,
            refresh_token,
            user_id,
            vault_id,
            scope,
            ..
        }) = &cfg.remote
        else {
            return Err(AxiError::new(
                "Hosted cloud is not configured",
                "VALIDATION_ERROR",
                vec![],
            ));
        };
        Ok(Self {
            bearer: access_token
                .clone()
                .unwrap_or_else(|| HOSTED_SUPABASE_ANON_KEY.to_string()),
            refresh_token: refresh_token.clone(),
            user_id: user_id.clone(),
            vault_id: vault_id.clone().unwrap_or_else(|| "default".to_string()),
            scope: scope.clone().unwrap_or_else(|| "write".to_string()),
            authenticated: false,
            has_key_id_column: true,
        })
    }

    fn adopt_session(&mut self, access: String, uid: Option<String>, vid: String, scope: String) {
        self.bearer = access;
        if uid.is_some() {
            self.user_id = uid;
        }
        if self.vault_id == "default" && vid != "default" {
            self.vault_id = vid;
        }
        self.scope = scope;
    }

    /// A `vlt_` API key is exchanged for a short-lived JWT before any table request.
    pub fn authenticate(&mut self) -> Result<(), AxiError> {
        if self.authenticated {
            return Ok(());
        }
        if self.bearer.starts_with("vlt_") {
            let (access, _, uid, _, vid, scope) = exchange_api_key(&self.bearer).map_err(|msg| {
                AxiError::new(
                    format!("VAULT_API_KEY authentication failed: {msg}"),
                    "AUTH_ERROR",
                    vec![
                        "Verify your VAULT_API_KEY environment variable or create a new key with `vault token create`"
                            .to_string(),
                    ],
                )
            })?;
            self.adopt_session(access, uid, vid, scope);
        }
        self.authenticated = true;
        Ok(())
    }

    /// Refresh an expired session from the refresh token or `VAULT_API_KEY`.
    fn refresh(&mut self) -> bool {
        if let Some((access, refresh)) = self
            .refresh_token
            .as_deref()
            .and_then(try_refresh_hosted_token)
        {
            self.bearer = access;
            self.refresh_token = Some(refresh);
            return true;
        }
        if let Some(key) = env_api_key().filter(|k| k.starts_with("vlt_")) {
            if let Ok((access, _, uid, _, vid, scope)) = exchange_api_key(&key) {
                self.adopt_session(access, uid, vid, scope);
                return true;
            }
        }
        false
    }

    fn request(&self, method: &str, url: &str) -> ureq::Request {
        let mut req = ureq::request(method, url)
            .set("apikey", HOSTED_SUPABASE_ANON_KEY)
            .set("Authorization", &format!("Bearer {}", self.bearer));
        if let Some(env_key) = env_api_key() {
            req = req.set("x-vault-api-key", &env_key);
        }
        req
    }

    fn table_url(&self) -> String {
        format!("{HOSTED_SUPABASE_URL}/rest/v1/vault_notes")
    }

    fn row_url(&self, path: &str) -> String {
        format!(
            "{}?vault_id=eq.{}&path=eq.{}",
            self.table_url(),
            self.vault_id,
            postgrest_eq_val(path)
        )
    }

    fn upsert_url(&self) -> String {
        format!("{}?on_conflict=user_id,vault_id,path", self.table_url())
    }

    fn row(&self, path: &str, content: &str, hash: &str, mtime_ms: i64, key_id: &str) -> Value {
        let mut row = json!({
            "vault_id": self.vault_id,
            "path": path,
            "content": content,
            "hash": hash,
            "mtime_ms": mtime_ms,
            "deleted": false,
            "updated_at": chrono::Utc::now().to_rfc3339()
        });
        if self.has_key_id_column {
            row["enc_key_id"] = json!(key_id);
        }
        if let Some(uid) = &self.user_id {
            row["user_id"] = json!(uid);
        }
        row
    }

    fn list_page(&self, offset: usize) -> Result<ureq::Response, Box<ureq::Error>> {
        let cols = if self.has_key_id_column {
            "path,hash,mtime_ms,deleted,enc_key_id"
        } else {
            "path,hash,mtime_ms,deleted"
        };
        let url = format!(
            "{}?vault_id=eq.{}&select={cols}&order=path.asc&limit={PAGE_SIZE}&offset={offset}",
            self.table_url(),
            self.vault_id
        );
        self.request("GET", &url).call().map_err(Box::new)
    }

    fn upsert(&self, rows: &Value) -> Result<(), String> {
        self.request("POST", &self.upsert_url())
            .set("Content-Type", "application/json")
            .set("Prefer", "resolution=merge-duplicates")
            .send_string(&rows.to_string())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Update only if the row still holds `expected_hash`.
    fn conditional_update(&self, put: &Put, expected_hash: &str) -> PutOutcome {
        let url = format!(
            "{}&hash=eq.{expected_hash}&select=path",
            self.row_url(&put.path)
        );
        let mut payload = self.row(
            &put.path,
            &put.content,
            &put.hash,
            put.mtime_ms,
            &put.key_id,
        );
        if let Some(obj) = payload.as_object_mut() {
            for key in ["vault_id", "path", "user_id"] {
                obj.remove(key);
            }
        }
        match self
            .request("PATCH", &url)
            .set("Content-Type", "application/json")
            .set("Prefer", "return=representation")
            .send_string(&payload.to_string())
        {
            Ok(resp) => {
                let updated: Vec<Value> =
                    serde_json::from_str(&read_body(resp)).unwrap_or_default();
                if updated.is_empty() {
                    PutOutcome::Conflict
                } else {
                    PutOutcome::Stored
                }
            }
            Err(e) => PutOutcome::Failed(e.to_string()),
        }
    }

    fn put_batch(&mut self, batch: &[&Put]) -> Vec<PutOutcome> {
        let rows: Vec<Value> = batch
            .iter()
            .map(|p| self.row(&p.path, &p.content, &p.hash, p.mtime_ms, &p.key_id))
            .collect();
        if self.upsert(&Value::Array(rows.clone())).is_ok() {
            return vec![PutOutcome::Stored; batch.len()];
        }
        self.refresh();
        rows.iter()
            .map(|row| match self.upsert(row) {
                Ok(()) => PutOutcome::Stored,
                Err(e) => PutOutcome::Failed(e),
            })
            .collect()
    }
}

impl Remote for CloudRemote {
    fn name(&self) -> &'static str {
        "vault-cloud"
    }

    fn read_only(&self) -> bool {
        self.scope.eq_ignore_ascii_case("read")
    }

    fn list(&mut self) -> Result<Vec<RemoteFile>, AxiError> {
        self.authenticate()?;
        let mut files = Vec::new();
        let mut offset = 0;
        loop {
            let mut resp = self.list_page(offset);
            let missing_column =
                matches!(&resp, Err(e) if matches!(**e, ureq::Error::Status(400, _)));
            if missing_column && self.has_key_id_column {
                self.has_key_id_column = false;
                resp = self.list_page(offset);
            }
            if resp.is_err() && offset == 0 && self.refresh() {
                resp = self.list_page(offset);
            }
            let body =
                read_body(resp.map_err(|e| http_error("Hosted cloud sync request failed", *e))?);
            let page: Vec<Row> = serde_json::from_str(&body).unwrap_or_default();
            let count = page.len();
            files.extend(page.into_iter().map(|r| RemoteFile {
                path: r.path,
                hash: r.hash,
                deleted: r.deleted,
                key_id: r.enc_key_id,
            }));
            if count < PAGE_SIZE {
                return Ok(files);
            }
            offset += count;
        }
    }

    /// Rows written before key fingerprints carry `enc-v1`. If one of them already decrypts
    /// under the active key, stamp them all instead of re-uploading everything.
    fn adopt_legacy_key_ids(&mut self, keychain: &Keychain, files: &mut [RemoteFile]) {
        let Some(sample) = files
            .iter()
            .find(|f| !f.deleted && f.key_id == LEGACY_KEY_ID)
            .map(|f| f.path.clone())
        else {
            return;
        };
        let Ok(Some(raw)) = self.fetch(&sample) else {
            return;
        };
        if !keychain.is_current(&raw.content, &sample) {
            return;
        }
        let key_id = keychain.active_key_id();
        let url = format!(
            "{}?vault_id=eq.{}&enc_key_id=eq.{LEGACY_KEY_ID}",
            self.table_url(),
            self.vault_id
        );
        let stamped = self
            .request("PATCH", &url)
            .set("Content-Type", "application/json")
            .send_string(&json!({ "enc_key_id": key_id }).to_string())
            .is_ok();
        if stamped {
            for f in files.iter_mut().filter(|f| f.key_id == LEGACY_KEY_ID) {
                f.key_id = key_id.clone();
            }
        }
    }

    fn fetch(&mut self, path: &str) -> Result<Option<Fetched>, AxiError> {
        let url = format!(
            "{}&select=path,content,hash,mtime_ms,deleted",
            self.row_url(path)
        );
        let body = read_body(
            self.request("GET", &url)
                .call()
                .map_err(|e| http_error("Fetching a remote note failed", e))?,
        );
        let rows: Vec<Row> = serde_json::from_str(&body).unwrap_or_default();
        Ok(rows.into_iter().find(|r| !r.deleted).map(|r| Fetched {
            content: r.content,
            hash: r.hash,
            mtime_ms: r.mtime_ms,
        }))
    }

    fn put(&mut self, items: &[Put]) -> Vec<PutOutcome> {
        let mut outcomes = vec![PutOutcome::Failed("not attempted".into()); items.len()];
        // New rows and pure re-encryptions can't lose a concurrent edit: batch-upsert them.
        let (batchable, conditional): (Vec<usize>, Vec<usize>) = (0..items.len()).partition(|&i| {
            let p = &items[i];
            p.expected_remote_hash.as_ref().is_none_or(|h| *h == p.hash)
                && p.content.len() <= BATCHABLE_ROW_BYTES
        });

        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut group_bytes = 0;
        for i in batchable {
            let len = items[i].content.len();
            match groups.last_mut() {
                Some(g) if g.len() < BATCH_ROWS && group_bytes + len <= BATCH_BYTES => {
                    g.push(i);
                    group_bytes += len;
                }
                _ => {
                    groups.push(vec![i]);
                    group_bytes = len;
                }
            }
        }
        for group in groups {
            let puts: Vec<&Put> = group.iter().map(|&i| &items[i]).collect();
            for (i, outcome) in group.into_iter().zip(self.put_batch(&puts)) {
                outcomes[i] = outcome;
            }
        }

        for i in conditional {
            let put = &items[i];
            outcomes[i] = match &put.expected_remote_hash {
                Some(expected) => self.conditional_update(put, expected),
                None => match self.upsert(&self.row(
                    &put.path,
                    &put.content,
                    &put.hash,
                    put.mtime_ms,
                    &put.key_id,
                )) {
                    Ok(()) => PutOutcome::Stored,
                    Err(e) => PutOutcome::Failed(e),
                },
            };
        }
        outcomes
    }

    fn tombstone(&mut self, path: &str, mtime_ms: i64) -> Result<(), AxiError> {
        let mut row = self.row(path, "", "", mtime_ms, "");
        row["deleted"] = json!(true);
        if let Some(obj) = row.as_object_mut() {
            obj.remove("enc_key_id");
        }
        self.upsert(&row).map_err(|e| {
            AxiError::new(
                format!("Deleting '{path}' remotely failed: {e}"),
                "SYNC_ERROR",
                vec![],
            )
        })
    }
}

fn keyring_changed_error() -> AxiError {
    AxiError::new(
        "The vault keyring changed on another device while updating it",
        "KEYRING_CHANGED",
        vec!["Run the command again".to_string()],
    )
}

/// The keyring is a plaintext `vault_notes` row at the reserved [`KEYRING_PATH`]; its content
/// hash is the compare-and-swap version.
impl KeyringStore for CloudRemote {
    fn fetch_keyring(&mut self) -> Result<Option<StoredKeyring>, AxiError> {
        self.authenticate()?;
        let Some(row) = self.fetch(KEYRING_PATH)? else {
            return Ok(None);
        };
        let keyring =
            Keyring::from_json(&row.content).map_err(|e| invalid_keyring_error(&e.to_string()))?;
        Ok(Some(StoredKeyring {
            keyring,
            version: row.hash,
        }))
    }

    fn create_keyring(&mut self, keyring: &Keyring) -> Result<bool, AxiError> {
        let json = keyring.to_json();
        let row = self.row(KEYRING_PATH, &json, &sha1_hex(&json), now_ms(), "keyring");
        match self
            .request("POST", &self.table_url())
            .set("Content-Type", "application/json")
            .send_string(&row.to_string())
        {
            Ok(_) => Ok(true),
            Err(ureq::Error::Status(409, _)) => Ok(false),
            Err(e) => Err(http_error("Creating the vault keyring failed", e)),
        }
    }

    fn replace_keyring(&mut self, version: &str, next: &Keyring) -> Result<(), AxiError> {
        let json = next.to_json();
        let put = Put {
            path: KEYRING_PATH.to_string(),
            hash: sha1_hex(&json),
            content: json,
            mtime_ms: now_ms(),
            key_id: "keyring".to_string(),
            expected_remote_hash: Some(version.to_string()),
        };
        match self.conditional_update(&put, version) {
            PutOutcome::Stored => Ok(()),
            PutOutcome::Conflict => Err(keyring_changed_error()),
            PutOutcome::Failed(e) => Err(AxiError::new(
                format!("Updating the vault keyring failed: {e}"),
                "SYNC_ERROR",
                vec![],
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgrest_values_are_encoded() {
        assert_eq!(
            urlencoding_simple("plans/my task#1?.md"),
            "plans/my%20task%231%3F.md"
        );
        assert_eq!(postgrest_eq_val("a/b.md"), "a/b.md");
        assert_eq!(postgrest_eq_val("a,(b).md"), "%22a%2C%28b%29.md%22");
        assert_eq!(postgrest_eq_val(KEYRING_PATH), ".vault-axi/keyring.json");
    }

    #[test]
    fn rows_carry_identity_and_key() {
        let cfg = VaultConfig {
            remote: Some(RemoteBackendConfig::Cloud {
                access_token: Some("jwt".into()),
                refresh_token: None,
                user_id: Some("u1".into()),
                email: None,
                vault_id: Some("v1".into()),
                scope: Some("read".into()),
            }),
            ..Default::default()
        };
        let mut remote = CloudRemote::from_config(&cfg).unwrap();
        assert!(remote.read_only());
        let row = remote.row("a.md", "enc", "h", 5, "k2_x");
        assert_eq!(row["user_id"], "u1");
        assert_eq!(row["vault_id"], "v1");
        assert_eq!(row["enc_key_id"], "k2_x");
        remote.has_key_id_column = false;
        assert!(remote
            .row("a.md", "enc", "h", 5, "k2_x")
            .get("enc_key_id")
            .is_none());
        assert!(CloudRemote::from_config(&VaultConfig::default()).is_err());
    }
}
