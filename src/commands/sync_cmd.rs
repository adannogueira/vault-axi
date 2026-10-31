use crate::args::{flag_bool, flag_string, parse_args, ParsedArgs};
use crate::axi::AxiError;
use crate::context::Ctx;
use crate::index::reconcile::{reconcile, ReconcileOpts};
use crate::sync::{handle_mfa, login_hosted, register_hosted, run_full_sync};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

fn can_prompt() -> bool {
    if cfg!(test)
        || std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1")
        || std::env::var("VAULT_AXI_NO_PROMPT").as_deref() == Ok("1")
    {
        return false;
    }
    use std::io::IsTerminal;
    io::stdin().is_terminal() || io::stderr().is_terminal()
}

fn prompt_line(label: &str) -> Option<String> {
    if !can_prompt() {
        return None;
    }
    eprint!("{label}");
    let _ = io::stderr().flush();

    #[cfg(unix)]
    {
        if let Ok(tty) = std::fs::File::open("/dev/tty") {
            let mut reader = io::BufReader::new(tty);
            let mut line = String::new();
            if reader.read_line(&mut line).is_ok() {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
            return None;
        }
    }

    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_ok() {
        let trimmed = line.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    None
}

fn prompt_password(label: &str) -> Option<String> {
    if !can_prompt() {
        return None;
    }
    eprint!("{label}");
    let _ = io::stderr().flush();

    #[cfg(unix)]
    {
        if let Ok(tty_in) = std::fs::File::open("/dev/tty") {
            if let Ok(tty_ctl) = std::fs::File::open("/dev/tty") {
                let _ = std::process::Command::new("stty")
                    .arg("-echo")
                    .stdin(tty_ctl)
                    .status();
            }
            let mut reader = io::BufReader::new(tty_in);
            let mut line = String::new();
            let res = reader.read_line(&mut line);
            if let Ok(tty_ctl) = std::fs::File::open("/dev/tty") {
                let _ = std::process::Command::new("stty")
                    .arg("echo")
                    .stdin(tty_ctl)
                    .status();
            }
            eprintln!();
            if res.is_ok() {
                let pw = line.trim_end_matches(['\r', '\n']).to_string();
                if !pw.is_empty() {
                    return Some(pw);
                }
            }
            return None;
        }
    }

    #[cfg(windows)]
    {
        extern "system" {
            fn GetStdHandle(nStdHandle: u32) -> *mut std::ffi::c_void;
            fn GetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, lpMode: *mut u32) -> i32;
            fn SetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, dwMode: u32) -> i32;
        }
        const STD_INPUT_HANDLE: u32 = (-10i32) as u32;
        const ENABLE_ECHO_INPUT: u32 = 0x0004;

        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            let mut orig_mode: u32 = 0;
            let has_mode = GetConsoleMode(handle, &mut orig_mode) != 0;
            if has_mode {
                SetConsoleMode(handle, orig_mode & !ENABLE_ECHO_INPUT);
            }
            let mut line = String::new();
            let res = io::stdin().lock().read_line(&mut line);
            if has_mode {
                SetConsoleMode(handle, orig_mode);
            }
            eprintln!();
            if res.is_ok() {
                let pw = line.trim_end_matches(['\r', '\n']).to_string();
                if !pw.is_empty() {
                    return Some(pw);
                }
            }
            return None;
        }
    }

    #[allow(unreachable_code)]
    {
        let mut line = String::new();
        let res = io::stdin().lock().read_line(&mut line);
        eprintln!();
        if res.is_ok() {
            let pw = line.trim_end_matches(['\r', '\n']).to_string();
            if !pw.is_empty() {
                return Some(pw);
            }
        }
        None
    }
}

fn resolve_credentials(parsed: &ParsedArgs) -> (Option<String>, Option<String>) {
    let email = flag_string(&parsed.flags, "email")
        .or_else(|| parsed.positionals.first().cloned())
        .or_else(|| prompt_line("Email: "));

    let password = if let Some(pw) = flag_string(&parsed.flags, "password") {
        Some(pw)
    } else if flag_bool(&parsed.flags, "password-stdin") {
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line).is_ok() {
            let pw = line.trim_end_matches(['\r', '\n']).to_string();
            if !pw.is_empty() {
                Some(pw)
            } else {
                None
            }
        } else {
            None
        }
    } else if email.is_some() {
        prompt_password("Password: ")
    } else {
        None
    };

    (email, password)
}

pub fn sync_command(args: &[String], ctx: &mut Ctx) -> Result<Value, AxiError> {
    let root = ctx.require_vault()?;
    let parsed = parse_args(args);
    let only_pull = flag_bool(&parsed.flags, "pull");
    let only_push = flag_bool(&parsed.flags, "push");
    let (do_pull, do_push) = match (only_pull, only_push) {
        (true, false) => (true, false),
        (false, true) => (false, true),
        _ => (true, true),
    };

    let db = ctx.db()?;
    let summary = run_full_sync(db, &root, do_pull, do_push)?;
    let rec = reconcile(
        db,
        &root,
        ReconcileOpts {
            force: false,
            with_embeddings: Some(false),
        },
    );

    Ok(json!({
        "ok": true,
        "backend": summary.backend,
        "pulled": summary.pulled,
        "pushed": summary.pushed,
        "deletedLocal": summary.deleted_local,
        "deletedRemote": summary.deleted_remote,
        "indexed": rec.indexed,
        "updated": rec.updated,
        "message": summary.message
    }))
}

pub fn register_command(args: &[String], _ctx: &mut Ctx) -> Result<Value, AxiError> {
    let parsed = parse_args(args);
    let (email, password) = resolve_credentials(&parsed);
    let (Some(em), Some(pw)) = (email, password) else {
        return Err(AxiError::new(
            "Missing email or password",
            "VALIDATION_ERROR",
            vec![
                "Usage: vault register --email <you@example.com>".to_string(),
                "Tip: Omit --password to enter it interactively (avoids bash '!' or '$' escaping issues), or wrap in single quotes: --password 'your!pass'".to_string(),
            ],
        ));
    };

    if pw.chars().count() < 6 {
        return Err(AxiError::new(
            "Password must be at least 6 characters",
            "VALIDATION_ERROR",
            vec!["Choose a password with 6 or more characters".to_string()],
        ));
    }

    let vault_id = flag_string(&parsed.flags, "vault-id");

    register_hosted(&em, &pw, vault_id.as_deref())
}

pub fn login_command(args: &[String], _ctx: &mut Ctx) -> Result<Value, AxiError> {
    let parsed = parse_args(args);

    if flag_bool(&parsed.flags, "mfa-enroll") {
        return handle_mfa("enroll", None);
    }
    if let Some(verify_code) = flag_string(&parsed.flags, "mfa-verify") {
        return handle_mfa("verify", Some(&verify_code));
    }

    let token = flag_string(&parsed.flags, "token");
    let (email, password) = if token.is_some() {
        (flag_string(&parsed.flags, "email"), flag_string(&parsed.flags, "password"))
    } else {
        resolve_credentials(&parsed)
    };
    let mfa_code = flag_string(&parsed.flags, "mfa-code");
    let vault_id = flag_string(&parsed.flags, "vault-id");

    login_hosted(
        email.as_deref(),
        password.as_deref(),
        mfa_code.as_deref(),
        token.as_deref(),
        vault_id.as_deref(),
    )
}
