use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Official hosted `vault-axi` cloud endpoint (Supabase project in sa-east-1).
pub const HOSTED_SUPABASE_URL: &str = "https://mlmmzutdfnahbdquslzf.supabase.co";
/// Public publishable / anon key for `vault-axi` hosted cloud (all rows protected by RLS `auth.uid() = user_id`).
pub const HOSTED_SUPABASE_ANON_KEY: &str =
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6Im1sbW16dXRkZm5haGJkcXVzbHpmIiwicm9sZSI6ImFub24iLCJpYXQiOjE3OTM0NTcyNTUsImV4cCI6MjEwOTAzMzI1NX0.5ac5z0VZi1ZWTG9eyd8CtMVawP1mlL4Sxf7cRKTSSNc";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RemoteBackendConfig {
    Git {
        #[serde(rename = "repoUrl", skip_serializing_if = "Option::is_none")]
        repo_url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    #[serde(alias = "supabase")]
    Cloud {
        #[serde(rename = "accessToken", skip_serializing_if = "Option::is_none")]
        access_token: Option<String>,
        #[serde(rename = "refreshToken", skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        #[serde(rename = "userId", skip_serializing_if = "Option::is_none")]
        user_id: Option<String>,
        #[serde(rename = "email", skip_serializing_if = "Option::is_none")]
        email: Option<String>,
        #[serde(rename = "vaultId", skip_serializing_if = "Option::is_none")]
        vault_id: Option<String>,
    },
    S3 {
        endpoint: String,
        bucket: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultConfig {
    #[serde(rename = "vaultPath", skip_serializing_if = "Option::is_none")]
    pub vault_path: Option<String>,
    #[serde(rename = "autoSync", skip_serializing_if = "Option::is_none")]
    pub auto_sync: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteBackendConfig>,
}

/// Directory holding `config.json` and `index.db`. Defaults to `~/.vault-axi`;
/// override with the `VAULT_AXI_HOME` env var (used for isolated tests / multiple profiles).
pub fn config_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("VAULT_AXI_HOME") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".vault-axi")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn db_path() -> PathBuf {
    config_dir().join("index.db")
}

pub fn ensure_config_dir() {
    let dir = config_dir();
    if !dir.exists() {
        let _ = fs::create_dir_all(&dir);
    }
}

/// Read a headless container / agent API key (`vlt_...` or JWT) from the environment.
pub fn env_api_key() -> Option<String> {
    if std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1") {
        return None;
    }
    for var in ["VAULT_API_KEY", "VAULT_AXI_API_KEY", "VAULT_AXI_TOKEN"] {
        if let Ok(val) = std::env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

pub fn load_config() -> VaultConfig {
    let p = config_path();
    let mut cfg: VaultConfig = if p.exists() {
        fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        VaultConfig::default()
    };

    if cfg.remote.is_none() {
        if let Some(key) = env_api_key() {
            if cfg.vault_path.is_none() {
                let default_vault = config_dir().join("cloud-vault");
                let _ = fs::create_dir_all(&default_vault);
                cfg.vault_path = Some(default_vault.to_string_lossy().to_string());
            }
            let vid = std::env::var("VAULT_AXI_VAULT_ID")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "default".to_string());
            cfg.auto_sync = Some(cfg.auto_sync.unwrap_or(true));
            cfg.remote = Some(RemoteBackendConfig::Cloud {
                access_token: Some(key),
                refresh_token: None,
                user_id: None,
                email: None,
                vault_id: Some(vid),
            });
        }
    }

    cfg
}

pub fn save_config(config: &VaultConfig) {
    ensure_config_dir();
    if let Ok(s) = serde_json::to_string_pretty(config) {
        let _ = fs::write(config_path(), format!("{s}\n"));
    }
}

/// Resolve the vault root. Precedence: VAULT_AXI_VAULT env var, then config file.
/// Returns an absolute path or None if the vault has not been configured.
pub fn resolve_vault_path() -> Option<String> {
    if let Ok(from_env) = std::env::var("VAULT_AXI_VAULT") {
        let trimmed = from_env.trim();
        if !trimmed.is_empty() {
            return Some(resolve_abs_string(trimmed));
        }
    }
    if let Some(from_config) = load_config().vault_path {
        let trimmed = from_config.trim();
        if !trimmed.is_empty() {
            return Some(resolve_abs_string(trimmed));
        }
    }
    None
}

pub fn resolve_abs_string(p: &str) -> String {
    let path = PathBuf::from(p);
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    normalize_os_path(&abs)
}

pub fn normalize_os_path(path: &std::path::Path) -> String {
    let mut components = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if matches!(components.last(), Some(std::path::Component::Normal(_))) {
                    components.pop();
                }
            }
            c => components.push(c),
        }
    }
    let rebuilt: PathBuf = components.iter().collect();
    rebuilt.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn normalize_os_path_collapses_dots_and_never_pops_root() {
        let p = Path::new("/a/b/./c/../d");
        let norm = normalize_os_path(p).replace('\\', "/");
        assert!(norm.ends_with("/a/b/d"));

        let escape = Path::new("/a/../../../../../../etc/passwd");
        let norm_escape = normalize_os_path(escape).replace('\\', "/");
        assert!(norm_escape.starts_with('/'));
        assert!(norm_escape.ends_with("/etc/passwd"));
    }

    #[test]
    fn remote_backend_config_serde_supports_cloud_and_supabase_alias() {
        let raw_supabase = r#"{"vaultPath":"/tmp/v","autoSync":true,"remote":{"type":"supabase","vaultId":"v1"}}"#;
        let cfg: VaultConfig = serde_json::from_str(raw_supabase).unwrap();
        assert_eq!(cfg.vault_path.as_deref(), Some("/tmp/v"));
        assert_eq!(cfg.auto_sync, Some(true));
        match cfg.remote.unwrap() {
            RemoteBackendConfig::Cloud { vault_id, .. } => {
                assert_eq!(vault_id.as_deref(), Some("v1"));
            }
            other => panic!("Expected Cloud, got {other:?}"),
        }

        let raw_s3 = r#"{"remote":{"type":"s3","endpoint":"https://s3.example.com","bucket":"b","prefix":"p"}}"#;
        let cfg_s3: VaultConfig = serde_json::from_str(raw_s3).unwrap();
        match cfg_s3.remote.unwrap() {
            RemoteBackendConfig::S3 {
                endpoint,
                bucket,
                prefix,
            } => {
                assert_eq!(endpoint, "https://s3.example.com");
                assert_eq!(bucket, "b");
                assert_eq!(prefix.as_deref(), Some("p"));
            }
            other => panic!("Expected S3, got {other:?}"),
        }
    }
}
