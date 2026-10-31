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

pub fn config_dir() -> PathBuf {
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

pub fn load_config() -> VaultConfig {
    let p = config_path();
    if !p.exists() {
        return VaultConfig::default();
    }
    fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
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
                components.pop();
            }
            c => components.push(c),
        }
    }
    let rebuilt: PathBuf = components.iter().collect();
    rebuilt.to_string_lossy().to_string()
}
