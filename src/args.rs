use crate::axi::AxiError;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub enum FlagValue {
    Bool(bool),
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone, Default)]
pub struct ParsedArgs {
    pub positionals: Vec<String>,
    pub flags: HashMap<String, FlagValue>,
}

/// Minimal flag parser. Supports `--key value`, `--key=value`, boolean `--flag`,
/// and repeated `--key` (collected into an array). Everything else is positional.
pub fn parse_args<S: AsRef<str>>(args: &[S]) -> ParsedArgs {
    let mut positionals = Vec::new();
    let mut flags: HashMap<String, FlagValue> = HashMap::new();

    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_ref();
        if arg == "--" {
            for rest in &args[i + 1..] {
                positionals.push(rest.as_ref().to_string());
            }
            break;
        }
        if let Some(body) = arg.strip_prefix("--") {
            let (key, value) = if let Some(eq) = body.find('=') {
                let k = body[..eq].to_string();
                let v = FlagValue::Single(body[eq + 1..].to_string());
                (k, v)
            } else {
                let k = body.to_string();
                if let Some(next) = args.get(i + 1).map(|s| s.as_ref()) {
                    if !next.starts_with("--") {
                        i += 1;
                        (k, FlagValue::Single(next.to_string()))
                    } else {
                        (k, FlagValue::Bool(true))
                    }
                } else {
                    (k, FlagValue::Bool(true))
                }
            };

            let as_str = |fv: &FlagValue| -> String {
                match fv {
                    FlagValue::Bool(b) => b.to_string(),
                    FlagValue::Single(s) => s.clone(),
                    FlagValue::Multiple(v) => v.join(","),
                }
            };

            if let Some(existing) = flags.get_mut(&key) {
                let val_str = as_str(&value);
                match existing {
                    FlagValue::Multiple(list) => list.push(val_str),
                    prev => {
                        let first = as_str(prev);
                        *prev = FlagValue::Multiple(vec![first, val_str]);
                    }
                }
            } else {
                flags.insert(key, value);
            }
        } else {
            positionals.push(arg.to_string());
        }
        i += 1;
    }

    ParsedArgs { positionals, flags }
}

pub fn flag_string(flags: &HashMap<String, FlagValue>, key: &str) -> Option<String> {
    match flags.get(key)? {
        FlagValue::Bool(_) => None,
        FlagValue::Single(s) => Some(s.clone()),
        FlagValue::Multiple(list) => list.last().cloned(),
    }
}

pub fn flag_bool(flags: &HashMap<String, FlagValue>, key: &str) -> bool {
    match flags.get(key) {
        Some(FlagValue::Bool(b)) => *b,
        Some(FlagValue::Single(s)) => s == "true",
        Some(FlagValue::Multiple(list)) => list.last().map(|s| s == "true").unwrap_or(false),
        None => false,
    }
}

pub fn flag_int(flags: &HashMap<String, FlagValue>, key: &str, fallback: i64) -> i64 {
    flag_string(flags, key)
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(fallback)
}

/// Resolve content for write/update commands. Precedence:
///   --from <file>   read the file (best for long notes; --from - means stdin)
///   --content "…"   inline text (--content - means stdin)
///   --stdin         read stdin
/// Returns a validation error if none is given.
pub fn resolve_content(flags: &HashMap<String, FlagValue>) -> Result<String, AxiError> {
    if let Some(from) = flag_string(flags, "from") {
        if from == "-" {
            return Ok(read_stdin());
        }
        let path = PathBuf::from(&from);
        if !path.exists() {
            return Err(AxiError::new(
                format!("--from file not found: {from}"),
                "VALIDATION_ERROR",
                vec!["Pass a path to a file whose contents become the note body".to_string()],
            ));
        }
        return fs::read_to_string(&path).map_err(|e| {
            AxiError::new(
                format!("Failed to read --from file {from}: {e}"),
                "VALIDATION_ERROR",
                vec![],
            )
        });
    }
    if let Some(content) = flag_string(flags, "content") {
        if content != "-" {
            return Ok(content);
        }
        return Ok(read_stdin());
    }
    if flag_bool(flags, "stdin") {
        return Ok(read_stdin());
    }
    Err(AxiError::new(
        "No content provided",
        "VALIDATION_ERROR",
        vec![
            "Pass --from <file> (best for long notes), --content \"text\", or --content - / --stdin"
                .to_string(),
        ],
    ))
}

pub fn read_stdin() -> String {
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_positionals_key_value_equals_and_booleans() {
        let parsed = parse_args(&[
            "notes/a.md",
            "--section",
            "Design",
            "--mode=replace",
            "--force",
        ]);
        assert_eq!(parsed.positionals, vec!["notes/a.md"]);
        assert_eq!(flag_string(&parsed.flags, "section").as_deref(), Some("Design"));
        assert_eq!(flag_string(&parsed.flags, "mode").as_deref(), Some("replace"));
        assert!(flag_bool(&parsed.flags, "force"));
    }

    #[test]
    fn resolves_content_from_file_and_inline() {
        let dir = std::env::temp_dir().join(format!("vx-args-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("body.md");
        fs::write(&file, "# Big Note\n\nlots of content\n").unwrap();

        let parsed = parse_args(&["--from", file.to_str().unwrap()]);
        assert_eq!(
            resolve_content(&parsed.flags).unwrap(),
            "# Big Note\n\nlots of content\n"
        );
        let _ = fs::remove_dir_all(&dir);

        let parsed_inline = parse_args(&["--content", "hello"]);
        assert_eq!(resolve_content(&parsed_inline.flags).unwrap(), "hello");

        let parsed_missing = parse_args(&["--from", "/no/such/file-xyz.md"]);
        let err = resolve_content(&parsed_missing.flags).unwrap_err();
        assert!(err.message.contains("--from file not found"));

        let parsed_none = parse_args(&["notes/a.md"]);
        let err2 = resolve_content(&parsed_none.flags).unwrap_err();
        assert!(err2.message.contains("No content provided"));
    }
}
