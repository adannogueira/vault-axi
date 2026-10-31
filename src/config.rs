use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VaultConfig {
    #[serde(rename = "vaultPath", skip_serializing_if = "Option::is_none")]
    pub vault_path: Option<String>,
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
