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

/// True when `arg` is shaped like a flag token (`--name` or `--name=value`, where
/// `name` starts with a letter and contains only `[A-Za-z0-9_-]`). Anything else that
/// merely *starts* with `--` (e.g. a `---` frontmatter fence, or "--force broke it")
/// is data, so it can be consumed as a flag value or positional.
pub fn looks_like_flag(arg: &str) -> bool {
    let Some(body) = arg.strip_prefix("--") else {
        return false;
    };
    let name = match body.find('=') {
        Some(eq) => &body[..eq],
        None => body,
    };
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Minimal flag parser. Supports `--key value`, `--key=value`, boolean `--flag`,
/// and repeated `--key` (collected into an array). Everything else is positional.
/// A bare `--` ends flag parsing (everything after it is positional).
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
        if looks_like_flag(arg) {
            let body = &arg[2..];
            let (key, value) = if let Some(eq) = body.find('=') {
                let k = body[..eq].to_string();
                let v = FlagValue::Single(body[eq + 1..].to_string());
                (k, v)
            } else {
                let k = body.to_string();
                if let Some(next) = args.get(i + 1).map(|s| s.as_ref()) {
                    if !looks_like_flag(next) && next != "--" {
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
    resolve_optional_content(flags)?.ok_or_else(|| {
        AxiError::new(
            "No content provided",
            "VALIDATION_ERROR",
            vec![
                "Pass --from <file> (best for long notes), --content \"text\", or --content - / --stdin"
                    .to_string(),
            ],
        )
    })
}

fn strip_utf8_bom(s: String) -> String {
    if let Some(stripped) = s.strip_prefix('\u{feff}') {
        stripped.to_string()
    } else {
        s
    }
}

/// Like [`resolve_content`], but returns `Ok(None)` when no content source was given,
/// while still surfacing real errors (e.g. a missing `--from` file).
pub fn resolve_optional_content(
    flags: &HashMap<String, FlagValue>,
) -> Result<Option<String>, AxiError> {
    if let Some(from) = flag_string(flags, "from") {
        if from == "-" {
            return Ok(Some(strip_utf8_bom(read_stdin())));
        }
        let path = PathBuf::from(&from);
        if !path.is_file() {
            return Err(AxiError::new(
                format!("--from file not found: {from}"),
                "VALIDATION_ERROR",
                vec!["Pass a path to a file whose contents become the note body".to_string()],
            ));
        }
        return fs::read_to_string(&path)
            .map(strip_utf8_bom)
            .map(Some)
            .map_err(|e| {
                AxiError::new(
                    format!("Failed to read --from file {from}: {e}"),
                    "VALIDATION_ERROR",
                    vec![],
                )
            });
    }
    if let Some(content) = flag_string(flags, "content") {
        if content != "-" {
            return Ok(Some(strip_utf8_bom(content)));
        }
        return Ok(Some(strip_utf8_bom(read_stdin())));
    }
    if flag_bool(flags, "stdin") {
        return Ok(Some(strip_utf8_bom(read_stdin())));
    }
    Ok(None)
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
        assert_eq!(resolve_optional_content(&parsed_none.flags).unwrap(), None);
    }

    #[test]
    fn looks_like_flag_edge_cases() {
        assert!(looks_like_flag("--a"));
        assert!(looks_like_flag("--Z"));
        assert!(looks_like_flag("--foo-bar_1=baz"));
        assert!(!looks_like_flag("-a"));
        assert!(!looks_like_flag("--"));
        assert!(!looks_like_flag("---"));
        assert!(!looks_like_flag("--1abc"));
        assert!(!looks_like_flag("--=val"));
        assert!(!looks_like_flag("--foo$bar"));
        assert!(!looks_like_flag("plain"));
    }

    #[test]
    fn double_dash_and_repeated_flags_and_typed_accessors() {
        let parsed = parse_args(&[
            "pos1",
            "--tag",
            "one",
            "--tag=two",
            "--tag",
            "three",
            "--bool-flag",
            "--str-bool=true",
            "--str-false=false",
            "--limit",
            "25",
            "--bad-int",
            "notanint",
            "--",
            "--not-a-flag",
            "pos3",
        ]);
        assert_eq!(parsed.positionals, vec!["pos1", "--not-a-flag", "pos3"]);
        assert_eq!(
            parsed.flags.get("tag"),
            Some(&FlagValue::Multiple(vec![
                "one".to_string(),
                "two".to_string(),
                "three".to_string()
            ]))
        );
        // flag_string on Multiple returns the last item; on Bool returns None
        assert_eq!(flag_string(&parsed.flags, "tag").as_deref(), Some("three"));
        assert_eq!(flag_string(&parsed.flags, "bool-flag"), None);
        assert_eq!(flag_string(&parsed.flags, "missing"), None);

        // flag_bool across Bool, Single("true"/"false"), Multiple, None
        assert!(flag_bool(&parsed.flags, "bool-flag"));
        assert!(flag_bool(&parsed.flags, "str-bool"));
        assert!(!flag_bool(&parsed.flags, "str-false"));
        assert!(!flag_bool(&parsed.flags, "tag"));
        assert!(!flag_bool(&parsed.flags, "missing"));

        let parsed_multi_bool = parse_args(&["--b=false", "--b=true"]);
        assert!(flag_bool(&parsed_multi_bool.flags, "b"));
        let parsed_multi_bool2 = parse_args(&["--b=true", "--b=false"]);
        assert!(!flag_bool(&parsed_multi_bool2.flags, "b"));

        // flag_int
        assert_eq!(flag_int(&parsed.flags, "limit", 10), 25);
        assert_eq!(flag_int(&parsed.flags, "bad-int", 10), 10);
        assert_eq!(flag_int(&parsed.flags, "missing", 42), 42);
    }
}
