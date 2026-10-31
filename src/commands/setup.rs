use crate::args::{flag_bool, flag_string, parse_args};
use crate::axi::{install_session_start_hooks, AxiError};
use crate::config::{config_path, load_config, resolve_abs_string, save_config};
use crate::context::Ctx;
use serde_json::{json, Map, Value};
use std::fs;
use std::path::Path;

const EMBEDDED_SKILL_MD: &str = include_str!("../../.agents/skills/vault/SKILL.md");

pub fn setup_command(args: &[String], _ctx: &mut Ctx) -> Result<Value, AxiError> {
    let parsed = parse_args(args);
    let want_hooks = parsed.positionals.first().map(|s| s.as_str()) == Some("hooks")
        || flag_bool(&parsed.flags, "hooks");
    let vault = flag_string(&parsed.flags, "vault");

    if vault.is_none() && !want_hooks {
        let current = load_config()
            .vault_path
            .unwrap_or_else(|| "(unset)".to_string());
        return Ok(json!({
            "ok": true,
            "help": "Nothing to do",
            "usage": [
                "vault setup --vault <path>",
                "vault setup hooks   # SessionStart dashboard + the `vault` agent skill",
                "vault setup --vault <path> --hooks"
            ],
            "current": current
        }));
    }

    let mut out = Map::new();
    out.insert("ok".to_string(), json!(true));

    if let Some(ref v) = vault {
        let abs = resolve_abs_string(v);
        let p = Path::new(&abs);
        if !p.exists() || !p.is_dir() {
            return Err(AxiError::new(
                format!("Not a directory: {abs}"),
                "VALIDATION_ERROR",
                vec!["Pass --vault <path to your Obsidian vault>".to_string()],
            ));
        }
        let mut config = load_config();
        config.vault_path = Some(abs.clone());
        save_config(&config);
        out.insert("vault".to_string(), json!(abs));
        out.insert(
            "config".to_string(),
            json!(config_path().to_string_lossy().to_string()),
        );
    }

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
    let groups = settings
        .get("hooks")?
        .get("SessionStart")?
        .as_array()?;
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
