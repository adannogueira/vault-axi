use crate::args::{flag_bool, flag_string, parse_args};
use crate::axi::{install_session_start_hooks, AxiError};
use crate::config::{
    config_path, load_config, resolve_abs_string, save_config, RemoteBackendConfig,
};
use crate::context::Ctx;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;

const EMBEDDED_SKILL_MD: &str = include_str!("../../.agents/skills/vault/SKILL.md");

pub fn setup_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let parsed = parse_args(args);
    let want_hooks = parsed.positionals.first().map(|s| s.as_str()) == Some("hooks")
        || flag_bool(&parsed.flags, "hooks");
    let vault = flag_string(&parsed.flags, "vault");
    let remote = flag_string(&parsed.flags, "remote");
    let auto_sync_flag = parsed
        .flags
        .get("auto-sync")
        .map(|_| flag_bool(&parsed.flags, "auto-sync"));
    let auto_update_flag = parsed
        .flags
        .get("auto-update")
        .map(|_| flag_bool(&parsed.flags, "auto-update"));
    let capture_note = flag_string(&parsed.flags, "capture-note");
    let capture_section = flag_string(&parsed.flags, "capture-section");
    let encryption_key = flag_string(&parsed.flags, "encryption-key")
        .or_else(|| flag_string(&parsed.flags, "encryption-passphrase"));

    if vault.is_none()
        && !want_hooks
        && remote.is_none()
        && auto_sync_flag.is_none()
        && auto_update_flag.is_none()
        && capture_note.is_none()
        && capture_section.is_none()
        && encryption_key.is_none()
    {
        let cfg = load_config();
        return Ok(json!({
            "ok": true,
            "help": "Nothing to do",
            "usage": [
                "vault setup --vault <path>",
                "vault setup --capture-note <path|daily> [--capture-section Captures]",
                "vault setup --encryption-key <passphrase>",
                "vault setup --auto-update true|false",
                "vault setup --remote git:<repo-url> [--branch main]   # Self-hosted Git sync",
                "vault setup --remote s3://<bucket>/<prefix> --endpoint <https://...>   # Self-hosted S3/R2 sync",
                "vault register --email <e> --password <p>             # Zero-config Hosted Cloud",
                "vault login --email <e> --password <p>                # Sign in to Hosted Cloud",
                "vault setup hooks   # SessionStart dashboard + the `vault` agent skill"
            ],
            "current": cfg.vault_path.unwrap_or_else(|| "(unset)".to_string()),
            "autoSync": cfg.auto_sync.unwrap_or(true),
            "autoUpdate": cfg.auto_update.unwrap_or(true),
            "captureNote": cfg.capture_note.unwrap_or_else(|| "daily/YYYY-MM-DD.md".to_string()),
            "encryptionConfigured": cfg.encryption_key.is_some()
        }));
    }

    let mut out = Map::new();
    out.insert("ok".to_string(), json!(true));
    let mut config = load_config();

    if let Some(ref v) = vault {
        let abs = resolve_abs_string(v);
        let p = Path::new(&abs);
        if !p.exists() {
            let _ = fs::create_dir_all(p);
        }
        if !p.is_dir() {
            return Err(AxiError::new(
                format!("Not a directory: {abs}"),
                "VALIDATION_ERROR",
                vec!["Pass --vault <path to your Obsidian vault>".to_string()],
            ));
        }
        config.vault_path = Some(abs.clone());
        out.insert("vault".to_string(), json!(abs));
    }

    if let Some(ref rem) = remote {
        if let Some(s3_rest) = rem.strip_prefix("s3://") {
            let endpoint = flag_string(&parsed.flags, "endpoint").ok_or_else(|| {
                AxiError::new(
                    "--remote s3:// requires --endpoint <https://...>",
                    "VALIDATION_ERROR",
                    vec![],
                )
            })?;
            let (bucket, prefix) = match s3_rest.find('/') {
                Some(idx) => (
                    s3_rest[..idx].to_string(),
                    Some(s3_rest[idx + 1..].to_string()),
                ),
                None => (s3_rest.to_string(), None),
            };
            config.remote = Some(RemoteBackendConfig::S3 {
                endpoint,
                bucket,
                prefix,
            });
            out.insert("remote".to_string(), json!("s3"));
        } else {
            let git_url = rem.strip_prefix("git:").unwrap_or(rem).to_string();
            let branch = flag_string(&parsed.flags, "branch");
            config.remote = Some(RemoteBackendConfig::Git {
                repo_url: if git_url.is_empty() || git_url == "local" {
                    None
                } else {
                    Some(git_url)
                },
                branch,
            });
            out.insert("remote".to_string(), json!("git"));
        }
        if config.auto_sync.is_none() {
            config.auto_sync = Some(true);
        }
    }

    if let Some(as_val) = auto_sync_flag {
        config.auto_sync = Some(as_val);
        out.insert("autoSync".to_string(), json!(as_val));
    }

    if let Some(au_val) = auto_update_flag {
        config.auto_update = Some(au_val);
        out.insert("autoUpdate".to_string(), json!(au_val));
    }

    if let Some(cn) = capture_note {
        let trimmed = cn.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("daily") {
            config.capture_note = None;
            out.insert("captureNote".to_string(), json!("daily/YYYY-MM-DD.md"));
        } else {
            let norm = crate::vault::normalize_note_path(trimmed);
            config.capture_note = Some(norm.clone());
            out.insert("captureNote".to_string(), json!(norm));
        }
    }

    if let Some(cs) = capture_section {
        let trimmed = cs.trim().trim_start_matches('#').trim().to_string();
        if trimmed.is_empty() {
            config.capture_section = None;
            out.insert("captureSection".to_string(), json!("Captures"));
        } else {
            config.capture_section = Some(trimmed.clone());
            out.insert("captureSection".to_string(), json!(trimmed));
        }
    }

    let encryption_changed = match &encryption_key {
        Some(ek) => {
            let new_pass = ek.trim().to_string();
            let old_pass = config
                .encryption_key
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .to_string();
            if new_pass != old_pass {
                let cached = crate::crypto::key_scope(&config)
                    .and_then(|scope| crate::crypto::KeyCache::default().get(&scope));
                if let Some(data_key) = cached {
                    if new_pass.is_empty() {
                        return Err(AxiError::new(
                            "This vault is end-to-end encrypted; its passphrase cannot be removed",
                            "ENCRYPTION_IN_USE",
                            vec!["Change it instead: `vault setup --encryption-key <new passphrase>`".to_string()],
                        ));
                    }
                    let rewrapped = crate::sync::rewrap_keyring(&config, &data_key, &new_pass)?;
                    out.insert("keyringRewrapped".to_string(), json!(rewrapped));
                }
                if !old_pass.is_empty() {
                    config.local_encryption_key = Some(old_pass);
                }
                config.encryption_key = (!new_pass.is_empty()).then_some(new_pass);
            }
            out.insert(
                "encryptionConfigured".to_string(),
                json!(config.encryption_key.is_some()),
            );
            true
        }
        None => false,
    };

    save_config(&config);
    if encryption_changed {
        if let Ok(db) = ctx.db() {
            crate::sync::ensure_sync_table(db);
            let report = crate::crypto::reencrypt_local_db(db, &crate::crypto::keychain());
            out.insert("localRowsReencrypted".to_string(), json!(report.updated));
            if report.unreadable > 0 {
                out.insert("localRowsUnreadable".to_string(), json!(report.unreadable));
            }
        }
    }
    out.insert(
        "config".to_string(),
        json!(config_path().to_string_lossy().to_string()),
    );

    if want_hooks {
        match install_session_start_hooks("vault") {
            Ok(_) => {}
            Err(e) => {
                out.insert("hookError".to_string(), json!(e));
            }
        }
        let hook = find_vault_session_hook();
        let hook_status = match hook {
            Some(h) => format!("installed (Claude Code SessionStart: `{h}`)"),
            None => "failed (no SessionStart entry written)".to_string(),
        };
        out.insert("sessionHook".to_string(), json!(hook_status));

        let mut skill_err = None;
        let skill_status = install_skill(&mut skill_err);
        out.insert("skill".to_string(), json!(skill_status));
        if let Some(err) = skill_err {
            out.insert("skillError".to_string(), json!(err));
        }
    }

    out.insert(
        "next".to_string(),
        json!(["vault reindex", "vault search \"<query>\""]),
    );
    Ok(Value::Object(out))
}

fn install_skill(on_error: &mut Option<String>) -> String {
    let Some(home) = dirs::home_dir() else {
        *on_error = Some("Could not determine home directory".to_string());
        return "failed".to_string();
    };
    let dest_dir = home.join(".claude").join("skills").join("vault");
    if dest_dir.exists() || dest_dir.symlink_metadata().is_ok() {
        let _ = fs::remove_dir_all(&dest_dir);
        let _ = fs::remove_file(&dest_dir);
    }
    if let Err(e) = fs::create_dir_all(&dest_dir) {
        *on_error = Some(e.to_string());
        return "failed".to_string();
    }
    let skill_path = dest_dir.join("SKILL.md");
    match fs::write(&skill_path, EMBEDDED_SKILL_MD) {
        Ok(_) => format!("installed (copy: {})", skill_path.to_string_lossy()),
        Err(e) => {
            *on_error = Some(e.to_string());
            "failed".to_string()
        }
    }
}

fn find_vault_session_hook() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home.join(".claude").join("settings.json");
    let raw = fs::read_to_string(path).ok()?;
    let settings: Value = serde_json::from_str(&raw).ok()?;
    let groups = settings.get("hooks")?.get("SessionStart")?.as_array()?;
    for group in groups {
        if let Some(hooks) = group.get("hooks").and_then(|h| h.as_array()) {
            for h in hooks {
                if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                    if cmd.contains("vault") {
                        return Some(cmd.to_string());
                    }
                }
            }
        }
    }
    None
}
