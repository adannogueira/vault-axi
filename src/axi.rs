use serde_json::{json, Value};
use std::fs;
use std::io::{self, Write};
use toon_format::{encode, EncodeOptions};

#[derive(Debug, Clone)]
pub struct AxiError {
    pub message: String,
    pub code: String,
    pub help: Vec<String>,
}

impl AxiError {
    pub fn new(
        message: impl Into<String>,
        code: impl Into<String>,
        help: Vec<String>,
    ) -> Self {
        Self {
            message: message.into(),
            code: code.into(),
            help,
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self.code.as_str() {
            "VALIDATION_ERROR" | "USAGE_ERROR" | "UNKNOWN_COMMAND" => 2,
            _ => 1,
        }
    }
}

impl std::fmt::Display for AxiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for AxiError {}

pub fn print_toon(val: &Value) {
    let rendered = encode(val, &EncodeOptions::default())
        .unwrap_or_else(|_| serde_json::to_string_pretty(val).unwrap_or_default());
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{}", rendered.trim_end());
}

pub fn print_error(err: &AxiError) {
    let mut obj = serde_json::Map::new();
    obj.insert("error".to_string(), Value::String(err.message.clone()));
    obj.insert("code".to_string(), Value::String(err.code.clone()));
    if !err.help.is_empty() {
        obj.insert(
            "help".to_string(),
            Value::Array(err.help.iter().cloned().map(Value::String).collect()),
        );
    }
    print_toon(&Value::Object(obj));
}

pub fn install_session_start_hooks(marker: &str) -> Result<String, String> {
    let home = dirs::home_dir().ok_or_else(|| "Could not determine home directory".to_string())?;
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).map_err(|e| e.to_string())?;
    let settings_path = claude_dir.join("settings.json");

    let mut settings: Value = if settings_path.exists() {
        let raw = fs::read_to_string(&settings_path).map_err(|e| e.to_string())?;
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    if !settings.is_object() {
        settings = json!({});
    }

    let command = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| marker.to_string());

    let hooks_obj = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks_obj.is_object() {
        *hooks_obj = json!({});
    }

    let session_start = hooks_obj
        .as_object_mut()
        .unwrap()
        .entry("SessionStart")
        .or_insert_with(|| json!([]));
    if !session_start.is_array() {
        *session_start = json!([]);
    }

    let arr = session_start.as_array_mut().unwrap();
    // Remove existing entries containing marker
    arr.retain(|group| {
        if let Some(hooks) = group.get("hooks").and_then(|h| h.as_array()) {
            !hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.contains(marker))
                    .unwrap_or(false)
            })
        } else {
            true
        }
    });

    arr.push(json!({
        "matcher": "",
        "hooks": [
            {
                "type": "command",
                "command": command
            }
        ]
    }));

    let formatted = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    fs::write(&settings_path, format!("{formatted}\n")).map_err(|e| e.to_string())?;
    Ok(command)
}
