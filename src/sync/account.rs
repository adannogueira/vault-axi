//! Hosted-cloud account management: registration, login, MFA, API tokens and the session
//! (JWT refresh, `vlt_` API key exchange) that sync requests run under.

use crate::axi::AxiError;
use crate::config::{
    env_api_key, hosted_url, load_config, save_config, RemoteBackendConfig,
    HOSTED_SUPABASE_ANON_KEY,
};
use serde_json::{json, Value};
use std::fs;

pub(super) fn try_refresh_hosted_token(refresh_token: &str) -> Option<(String, String)> {
    let url = format!("{}/auth/v1/token?grant_type=refresh_token", hosted_url());
    let payload = json!({ "refresh_token": refresh_token });
    let resp = ureq::post(&url)
        .set("apikey", HOSTED_SUPABASE_ANON_KEY)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .ok()?;
    let body: Value = serde_json::from_str(&resp.into_string().ok()?).ok()?;
    let new_access = body.get("access_token")?.as_str()?.to_string();
    let new_refresh = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or(refresh_token)
        .to_string();

    let mut cfg = load_config();
    if let Some(RemoteBackendConfig::Cloud {
        access_token,
        refresh_token: rt,
        ..
    }) = &mut cfg.remote
    {
        *access_token = Some(new_access.clone());
        *rt = Some(new_refresh.clone());
        save_config(&cfg);
    }
    Some((new_access, new_refresh))
}

/// Exchange a long-lived `vlt_...` API key for a fresh Supabase JWT session via the `token-exchange` Edge Function.
#[allow(clippy::type_complexity)]
pub(super) fn exchange_api_key(
    api_key: &str,
) -> Result<
    (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
    ),
    String,
> {
    let url = format!("{}/functions/v1/token-exchange", hosted_url());
    let payload = json!({
        "action": "exchange",
        "api_key": api_key.trim()
    });
    let resp = ureq::post(&url)
        .set("apikey", HOSTED_SUPABASE_ANON_KEY)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .map_err(extract_auth_error)?;
    let body: Value =
        serde_json::from_str(&resp.into_string().unwrap_or_default()).unwrap_or_else(|_| json!({}));
    let Some(access_token) = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    else {
        return Err("token-exchange did not return an access_token".to_string());
    };
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let user_id = body
        .get("user_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let email = body
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let vault_id = body
        .get("vault_id")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("write")
        .to_string();

    save_hosted_session_with_scope(
        Some(access_token.clone()),
        refresh_token.clone(),
        user_id.clone(),
        email.clone(),
        Some(&vault_id),
        Some(&scope),
    );
    Ok((access_token, refresh_token, user_id, email, vault_id, scope))
}

fn extract_auth_error(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, resp) => {
            if let Ok(raw) = resp.into_string() {
                if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                    if let Some(msg) = v
                        .get("msg")
                        .or_else(|| v.get("error_description"))
                        .or_else(|| v.get("message"))
                        .or_else(|| v.get("error"))
                        .and_then(|m| m.as_str())
                    {
                        return msg.to_string();
                    }
                }
            }
            format!("HTTP status {code}")
        }
        ureq::Error::Transport(t) => format!("Network error: {t}"),
    }
}

fn try_password_login(
    clean_url: &str,
    anon_key: &str,
    email: &str,
    password: &str,
) -> Result<(String, Option<String>, Option<String>), String> {
    let auth_url = format!("{clean_url}/auth/v1/token?grant_type=password");
    let payload = json!({ "email": email, "password": password });
    let resp = ureq::post(&auth_url)
        .set("apikey", anon_key)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
        .map_err(extract_auth_error)?;
    let body: Value =
        serde_json::from_str(&resp.into_string().unwrap_or_default()).unwrap_or_else(|_| json!({}));
    let Some(access_token) = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    else {
        return Err("Missing access_token in response".to_string());
    };
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let user_id = body
        .get("user")
        .and_then(|u| u.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok((access_token, refresh_token, user_id))
}

const CONFIRM_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>vault-axi — Email Confirmed</title>
  <style>
    body {
      margin: 0;
      min-height: 100vh;
      display: flex;
      align-items: center;
      justify-content: center;
      background: #0d1117;
      color: #e6edf3;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    }
    .card {
      max-width: 420px;
      padding: 32px;
      border-radius: 12px;
      background: #161b22;
      border: 1px solid #30363d;
      text-align: center;
      box-shadow: 0 12px 32px rgba(0, 0, 0, 0.4);
    }
    .icon {
      width: 48px;
      height: 48px;
      margin: 0 auto 16px;
      border-radius: 50%;
      background: rgba(46, 160, 67, 0.15);
      color: #3fb950;
      display: flex;
      align-items: center;
      justify-content: center;
      font-size: 24px;
      font-weight: bold;
    }
    h1 { margin: 0 0 10px; font-size: 20px; }
    p { margin: 0; color: #8b949e; line-height: 1.5; font-size: 14px; }
    code { color: #79c0ff; background: rgba(110,118,129,0.2); padding: 2px 6px; border-radius: 4px; }
  </style>
</head>
<body>
  <div class="card">
    <div class="icon">&#10003;</div>
    <h1>Email Confirmed</h1>
    <p id="msg">Your <code>vault-axi</code> cloud account is active and signed in. You can close this tab and return to your terminal.</p>
  </div>
  <script>
    if (window.location.hash && window.location.hash.length > 1) {
      fetch('/auth-callback?' + window.location.hash.substring(1)).catch(function(){});
      history.replaceState(null, '', window.location.pathname);
    }
  </script>
</body>
</html>"#;

fn parse_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key && !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn serve_confirm_request(mut stream: std::net::TcpStream) -> Option<(String, Option<String>)> {
    use std::io::{Read, Write};
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");

    if let Some(q) = path.strip_prefix("/auth-callback?") {
        let token = parse_query_param(q, "access_token");
        let refresh = parse_query_param(q, "refresh_token");
        let body = r#"{"ok":true}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
        if let Some(t) = token {
            return Some((t, refresh));
        }
        return None;
    }

    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        CONFIRM_HTML.len(),
        CONFIRM_HTML
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
    None
}

fn wait_for_email_confirmation(
    clean_url: &str,
    anon_key: &str,
    email: &str,
    password: &str,
) -> Option<(String, Option<String>, Option<String>)> {
    if cfg!(test)
        || std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1")
        || std::env::var("VAULT_AXI_NO_WAIT").as_deref() == Ok("1")
    {
        return None;
    }

    let listener = std::net::TcpListener::bind("127.0.0.1:3000").ok();
    if let Some(ref l) = listener {
        let _ = l.set_nonblocking(true);
    }

    eprintln!("==> Confirmation email sent to {email}.");
    eprintln!(
        "==> Waiting for you to click the confirmation link in your email... (Ctrl+C to skip)"
    );

    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(180);
    let mut last_poll = std::time::Instant::now();

    while start.elapsed() < timeout {
        if let Some(ref l) = listener {
            while let Ok((stream, _)) = l.accept() {
                if let Some((tok, refr)) = serve_confirm_request(stream) {
                    eprintln!("==> Email confirmed! Logging in...");
                    return Some((tok, refr, None));
                }
            }
        }

        if last_poll.elapsed() >= std::time::Duration::from_millis(2500) {
            last_poll = std::time::Instant::now();
            if let Ok((tok, refr, uid)) = try_password_login(clean_url, anon_key, email, password) {
                // Keep serving localhost:3000 briefly in case the browser redirect is still in flight
                if let Some(ref l) = listener {
                    let grace = std::time::Instant::now();
                    while grace.elapsed() < std::time::Duration::from_millis(700) {
                        if let Ok((stream, _)) = l.accept() {
                            let _ = serve_confirm_request(stream);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
                eprintln!("==> Email confirmed! Logged in as {email}.");
                return Some((tok, refr, uid));
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    None
}

/// Register a new user account on the official Hosted Cloud.
pub fn register_hosted(
    email: &str,
    password: &str,
    vault_id: Option<&str>,
) -> Result<Value, AxiError> {
    let clean_url = &hosted_url();
    let anon_key = HOSTED_SUPABASE_ANON_KEY;

    let edge_register_url = format!("{clean_url}/functions/v1/register");
    let edge_payload = json!({
        "email": email,
        "password": password,
    });
    let resp = match ureq::post(&edge_register_url)
        .set("apikey", anon_key)
        .set("Content-Type", "application/json")
        .send_string(&edge_payload.to_string())
    {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(404, _)) => {
            let signup_url = format!("{clean_url}/auth/v1/signup");
            let payload = json!({
                "email": email,
                "password": password,
                "options": {
                    "email_redirect_to": "http://localhost:3000"
                }
            });
            ureq::post(&signup_url)
                .set("apikey", anon_key)
                .set("Content-Type", "application/json")
                .send_string(&payload.to_string())
        }
        Err(e) => Err(e),
    };

    let body: Value = match resp {
        Ok(r) => {
            serde_json::from_str(&r.into_string().unwrap_or_default()).unwrap_or_else(|_| json!({}))
        }
        Err(e) => {
            // If the account already exists and is confirmed, log in directly
            if let Ok((tok, refr, uid)) = try_password_login(clean_url, anon_key, email, password) {
                let local_cache = save_hosted_session(
                    Some(tok),
                    refr,
                    uid.clone(),
                    Some(email.to_string()),
                    vault_id,
                );
                return Ok(json!({
                    "ok": true,
                    "action": "logged_in_existing_account",
                    "email": email,
                    "userId": uid,
                    "vaultId": vault_id.unwrap_or("default"),
                    "localVaultCache": local_cache,
                    "autoSync": true,
                    "next": ["vault sync", "vault ls"]
                }));
            }
            let msg = extract_auth_error(e);
            return Err(AxiError::new(
                format!("Registration failed: {msg}"),
                "AUTH_ERROR",
                vec!["Ensure the email is valid and password is at least 6 characters".to_string()],
            ));
        }
    };

    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let user_id = body
        .get("user")
        .and_then(|u| u.get("id"))
        .or_else(|| body.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if access_token.is_some() {
        let local_cache = save_hosted_session(
            access_token,
            refresh_token,
            user_id.clone(),
            Some(email.to_string()),
            vault_id,
        );
        return Ok(json!({
            "ok": true,
            "action": "registered_and_logged_in",
            "email": email,
            "userId": user_id,
            "vaultId": vault_id.unwrap_or("default"),
            "localVaultCache": local_cache,
            "autoSync": true,
            "next": ["vault sync", "vault ls", "vault login --mfa-enroll"]
        }));
    }

    // If the user already confirmed their account earlier, password login will succeed immediately
    if let Ok((tok, refr, uid)) = try_password_login(clean_url, anon_key, email, password) {
        let final_uid = uid.or(user_id);
        let local_cache = save_hosted_session(
            Some(tok),
            refr,
            final_uid.clone(),
            Some(email.to_string()),
            vault_id,
        );
        return Ok(json!({
            "ok": true,
            "action": "registered_and_logged_in",
            "email": email,
            "userId": final_uid,
            "vaultId": vault_id.unwrap_or("default"),
            "localVaultCache": local_cache,
            "autoSync": true,
            "next": ["vault sync", "vault ls", "vault login --mfa-enroll"]
        }));
    }

    // Wait for the user to click the confirmation link (serving localhost:3000 + polling)
    if let Some((tok, refr, uid)) =
        wait_for_email_confirmation(clean_url, anon_key, email, password)
    {
        let final_uid = uid.or(user_id);
        let local_cache = save_hosted_session(
            Some(tok),
            refr,
            final_uid.clone(),
            Some(email.to_string()),
            vault_id,
        );
        return Ok(json!({
            "ok": true,
            "action": "registered_and_logged_in",
            "email": email,
            "userId": final_uid,
            "vaultId": vault_id.unwrap_or("default"),
            "localVaultCache": local_cache,
            "autoSync": true,
            "next": ["vault sync", "vault ls", "vault login --mfa-enroll"]
        }));
    }

    Ok(json!({
        "ok": true,
        "status": "awaiting_email_confirmation",
        "email": email,
        "message": format!("Confirmation email sent to {email}. Once confirmed, run `vault login --email {email}`.")
    }))
}

/// Log in to the official Hosted Cloud with email + password (+ optional TOTP MFA code).
pub fn login_hosted(
    email: Option<&str>,
    password: Option<&str>,
    mfa_code: Option<&str>,
    token: Option<&str>,
    vault_id: Option<&str>,
) -> Result<Value, AxiError> {
    let clean_url = &hosted_url();
    let anon_key = HOSTED_SUPABASE_ANON_KEY;

    let mut final_token = token.map(|s| s.to_string()).or_else(env_api_key);
    let mut final_refresh: Option<String> = None;
    let mut final_user_id: Option<String> = None;
    let mut final_email: Option<String> = email.map(|s| s.to_string());
    let mut final_vault_id: String = vault_id.unwrap_or("default").to_string();
    let mut final_scope: String = "write".to_string();

    if let Some(ref tok) = final_token.clone() {
        if tok.starts_with("vlt_") {
            let (new_access, refr, uid, em, vid, sc) = exchange_api_key(tok).map_err(|msg| {
                AxiError::new(
                    format!("API key login failed: {msg}"),
                    "AUTH_ERROR",
                    vec![
                        "Verify your vlt_... API key or generate a new one with `vault token create`"
                            .to_string(),
                    ],
                )
            })?;
            final_token = Some(new_access);
            final_refresh = refr;
            final_user_id = uid;
            if em.is_some() {
                final_email = em;
            }
            if vault_id.is_none() {
                final_vault_id = vid;
            }
            final_scope = sc;
        }
    }

    if final_token.is_none() {
        let (Some(em), Some(pw)) = (email, password) else {
            return Err(AxiError::new(
                "Missing email or password",
                "VALIDATION_ERROR",
                vec![
                    "Usage: vault login --email <email> [--mfa-code <6-digit-totp>]".to_string(),
                    "Headless container? Set VAULT_API_KEY=vlt_... (created via `vault token create`)"
                        .to_string(),
                    "New user? Run `vault register --email <email>`".to_string(),
                ],
            ));
        };
        match try_password_login(clean_url, anon_key, em, pw) {
            Ok((tok, refr, uid)) => {
                final_token = Some(tok);
                final_refresh = refr;
                final_user_id = uid;
            }
            Err(msg) => {
                if msg.to_lowercase().contains("not confirmed") {
                    if let Some((tok, refr, uid)) =
                        wait_for_email_confirmation(clean_url, anon_key, em, pw)
                    {
                        final_token = Some(tok);
                        final_refresh = refr;
                        final_user_id = uid;
                    } else {
                        return Err(AxiError::new(
                            format!("Login failed: {msg}"),
                            "AUTH_ERROR",
                            vec![
                                "Check your email inbox and click the confirmation link, then run `vault login` again".to_string(),
                            ],
                        ));
                    }
                } else {
                    return Err(AxiError::new(
                        format!("Login failed: {msg}"),
                        "AUTH_ERROR",
                        vec![
                            "Verify your email and password, or register first with `vault register --email <email>`".to_string(),
                        ],
                    ));
                }
            }
        }

        // If user provided an MFA TOTP code, elevate session to AAL2
        if let (Some(code), Some(ref aal1_token)) = (mfa_code, &final_token) {
            let (aal2_access, aal2_refresh) =
                verify_mfa_totp(clean_url, anon_key, aal1_token, code)?;
            final_token = Some(aal2_access);
            final_refresh = Some(aal2_refresh);
        }
    }

    let local_cache = save_hosted_session_with_scope(
        final_token,
        final_refresh,
        final_user_id.clone(),
        final_email.clone(),
        Some(&final_vault_id),
        Some(&final_scope),
    );

    Ok(json!({
        "ok": true,
        "backend": "vault-cloud",
        "email": final_email,
        "userId": final_user_id,
        "vaultId": final_vault_id,
        "scope": final_scope,
        "localVaultCache": local_cache,
        "autoSync": true,
        "next": ["vault sync", "vault ls", "vault token create --name agent"]
    }))
}

/// Create, list, or revoke long-lived `vlt_...` API keys for headless containers and agents.
pub fn manage_api_tokens(
    subaction: &str,
    name: Option<&str>,
    vault_id: Option<&str>,
    scope: Option<&str>,
    target: Option<&str>,
) -> Result<Value, AxiError> {
    let cfg = load_config();
    let (mut bearer, refresh_tok, cfg_vault_id) = match cfg.remote {
        Some(RemoteBackendConfig::Cloud {
            access_token: Some(tok),
            refresh_token,
            vault_id: vid,
            ..
        }) => (tok, refresh_token, vid),
        _ => {
            return Err(AxiError::new(
                "Not logged into Vault Cloud",
                "AUTH_ERROR",
                vec!["Run `vault login --email <email>` first".to_string()],
            ));
        }
    };

    if bearer.starts_with("vlt_") {
        if let Ok((new_access, _, _, _, _, _)) = exchange_api_key(&bearer) {
            bearer = new_access;
        }
    }

    let url = format!("{}/functions/v1/token-exchange", hosted_url());
    let effective_vault_id = vault_id
        .map(|s| s.to_string())
        .or(cfg_vault_id)
        .unwrap_or_else(|| "default".to_string());
    let effective_scope = scope.unwrap_or("write").to_lowercase();
    if effective_scope != "read" && effective_scope != "write" {
        return Err(AxiError::new(
            format!("Invalid token scope \"{effective_scope}\" (use 'read' or 'write')"),
            "VALIDATION_ERROR",
            vec!["Usage: vault token create [--name <label>] [--vault-id <id>] [--scope read|write] [--read-only]".to_string()],
        ));
    }

    let payload = match subaction {
        "create" => json!({
            "action": "create",
            "name": name.unwrap_or("agent"),
            "vault_id": effective_vault_id,
            "scope": effective_scope,
        }),
        "list" => json!({ "action": "list" }),
        "revoke" => {
            let Some(t) = target.filter(|s| !s.trim().is_empty()) else {
                return Err(AxiError::new(
                    "Missing API key id or prefix to revoke",
                    "VALIDATION_ERROR",
                    vec![
                        "Usage: vault token revoke <id-or-prefix>".to_string(),
                        "Run `vault token list` to see active keys".to_string(),
                    ],
                ));
            };
            json!({
                "action": "revoke",
                "target": t,
            })
        }
        other => {
            return Err(AxiError::new(
                format!("Unknown token subcommand: {other} (use 'create', 'list', or 'revoke')"),
                "VALIDATION_ERROR",
                vec![
                    "vault token create [--name <label>] [--vault-id <id>] [--scope read|write] [--read-only]".to_string(),
                    "vault token list".to_string(),
                    "vault token revoke <id-or-prefix>".to_string(),
                ],
            ));
        }
    };

    let mut resp = ureq::post(&url)
        .set("apikey", HOSTED_SUPABASE_ANON_KEY)
        .set("Authorization", &format!("Bearer {bearer}"))
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string());

    if resp.is_err() {
        if let Some(rt) = refresh_tok.as_deref() {
            if let Some((new_access, _)) = try_refresh_hosted_token(rt) {
                bearer = new_access;
                resp = ureq::post(&url)
                    .set("apikey", HOSTED_SUPABASE_ANON_KEY)
                    .set("Authorization", &format!("Bearer {bearer}"))
                    .set("Content-Type", "application/json")
                    .send_string(&payload.to_string());
            }
        }
    }

    let r = resp.map_err(|e| {
        let msg = extract_auth_error(e);
        AxiError::new(
            format!("Token operation failed: {msg}"),
            "AUTH_ERROR",
            vec!["Run `vault login --email <email>` to refresh your session".to_string()],
        )
    })?;

    let body: Value =
        serde_json::from_str(&r.into_string().unwrap_or_default()).unwrap_or_else(|_| json!({}));

    if subaction == "create" {
        let api_key = body.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
        let returned_scope = body
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or(&effective_scope);
        return Ok(json!({
            "ok": true,
            "action": "token_created",
            "id": body.get("id"),
            "name": body.get("name"),
            "vaultId": body.get("vault_id").unwrap_or(&json!(effective_vault_id)),
            "scope": returned_scope,
            "keyPrefix": body.get("key_prefix"),
            "apiKey": api_key,
            "usage": format!("export VAULT_API_KEY=\"{api_key}\""),
            "note": "Save this key now; it will not be shown again. Any container with VAULT_API_KEY set will silently authenticate on first command."
        }));
    }

    Ok(body)
}

fn save_hosted_session(
    access_token: Option<String>,
    refresh_token: Option<String>,
    user_id: Option<String>,
    email: Option<String>,
    vault_id: Option<&str>,
) -> Option<String> {
    save_hosted_session_with_scope(
        access_token,
        refresh_token,
        user_id,
        email,
        vault_id,
        Some("write"),
    )
}

fn save_hosted_session_with_scope(
    access_token: Option<String>,
    refresh_token: Option<String>,
    user_id: Option<String>,
    email: Option<String>,
    vault_id: Option<&str>,
    scope: Option<&str>,
) -> Option<String> {
    let mut cfg = load_config();
    if cfg.vault_path.is_none() {
        let default_vault = crate::config::config_dir().join("cloud-vault");
        let _ = fs::create_dir_all(&default_vault);
        cfg.vault_path = Some(default_vault.to_string_lossy().to_string());
    }
    cfg.auto_sync = Some(true);
    cfg.remote = Some(RemoteBackendConfig::Cloud {
        access_token,
        refresh_token,
        user_id,
        email,
        vault_id: Some(vault_id.unwrap_or("default").to_string()),
        scope: Some(scope.unwrap_or("write").to_string()),
    });
    save_config(&cfg);
    cfg.vault_path
}

/// Enroll or verify TOTP Multi-Factor Authentication (MFA) on the hosted account.
pub fn handle_mfa(action: &str, code: Option<&str>) -> Result<Value, AxiError> {
    let cfg = load_config();
    let Some(RemoteBackendConfig::Cloud {
        access_token: Some(token),
        ..
    }) = cfg.remote
    else {
        return Err(AxiError::new(
            "Not logged into Vault Cloud",
            "AUTH_ERROR",
            vec!["Run `vault login --email <e> --password <p>` first".to_string()],
        ));
    };

    let clean_url = &hosted_url();
    let anon_key = HOSTED_SUPABASE_ANON_KEY;
    match action {
        "enroll" => {
            let enroll_url = format!("{clean_url}/auth/v1/factors");
            let payload = json!({
                "factor_type": "totp",
                "friendly_name": "vault-axi-cli"
            });
            let resp = ureq::post(&enroll_url)
                .set("apikey", anon_key)
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(&payload.to_string())
                .map_err(|e| {
                    AxiError::new(format!("MFA enroll failed: {e}"), "AUTH_ERROR", vec![])
                })?;
            let body: Value = serde_json::from_str(&resp.into_string().unwrap_or_default())
                .unwrap_or_else(|_| json!({}));
            Ok(json!({
                "ok": true,
                "action": "mfa_enrolled",
                "factorId": body.get("id"),
                "totpSecret": body.get("totp").and_then(|t| t.get("secret")),
                "totpUri": body.get("totp").and_then(|t| t.get("uri")),
                "next": ["Add totpSecret to your authenticator app, then verify with `vault login --mfa-verify <6-digit-code>`"]
            }))
        }
        "verify" => {
            let Some(c) = code else {
                return Err(AxiError::new(
                    "Missing 6-digit TOTP code",
                    "VALIDATION_ERROR",
                    vec!["Usage: vault login --mfa-verify <6-digit-code>".to_string()],
                ));
            };
            let (new_access, new_refresh) = verify_mfa_totp(clean_url, anon_key, &token, c)?;
            let mut updated_cfg = load_config();
            if let Some(RemoteBackendConfig::Cloud {
                access_token,
                refresh_token,
                ..
            }) = &mut updated_cfg.remote
            {
                *access_token = Some(new_access);
                *refresh_token = Some(new_refresh);
                save_config(&updated_cfg);
            }
            Ok(json!({
                "ok": true,
                "action": "mfa_verified_aal2",
                "assuranceLevel": "aal2"
            }))
        }
        _ => Err(AxiError::new(
            format!("Unknown MFA action: {action} (use 'enroll' or 'verify')"),
            "VALIDATION_ERROR",
            vec![],
        )),
    }
}

fn verify_mfa_totp(
    clean_url: &str,
    anon_key: &str,
    token: &str,
    code: &str,
) -> Result<(String, String), AxiError> {
    let user_url = format!("{clean_url}/auth/v1/user");
    let user_resp = ureq::get(&user_url)
        .set("apikey", anon_key)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| {
            AxiError::new(
                format!("Failed to fetch MFA factors: {e}"),
                "AUTH_ERROR",
                vec![],
            )
        })?;
    let user_body: Value = serde_json::from_str(&user_resp.into_string().unwrap_or_default())
        .unwrap_or_else(|_| json!({}));
    let factor_id = user_body
        .get("factors")
        .and_then(|f| f.as_array())
        .and_then(|arr| arr.first())
        .and_then(|f| f.get("id"))
        .and_then(|id| id.as_str())
        .ok_or_else(|| {
            AxiError::new(
                "No TOTP MFA factor found on account",
                "AUTH_ERROR",
                vec!["Enroll first with `vault login --mfa-enroll`".to_string()],
            )
        })?;

    let challenge_url = format!("{clean_url}/auth/v1/factors/{factor_id}/challenge");
    let ch_resp = ureq::post(&challenge_url)
        .set("apikey", anon_key)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string("{}")
        .map_err(|e| AxiError::new(format!("MFA challenge failed: {e}"), "AUTH_ERROR", vec![]))?;
    let ch_body: Value = serde_json::from_str(&ch_resp.into_string().unwrap_or_default())
        .unwrap_or_else(|_| json!({}));
    let challenge_id = ch_body
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AxiError::new("Invalid MFA challenge response", "AUTH_ERROR", vec![]))?;

    let verify_url = format!("{clean_url}/auth/v1/factors/{factor_id}/verify");
    let v_payload = json!({
        "challenge_id": challenge_id,
        "code": code
    });
    let v_resp = ureq::post(&verify_url)
        .set("apikey", anon_key)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .send_string(&v_payload.to_string())
        .map_err(|e| {
            AxiError::new(
                format!("MFA TOTP verification failed: {e}"),
                "AUTH_ERROR",
                vec![],
            )
        })?;
    let v_body: Value = serde_json::from_str(&v_resp.into_string().unwrap_or_default())
        .unwrap_or_else(|_| json!({}));
    let new_access = v_body
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            AxiError::new(
                "MFA verify did not return access_token",
                "AUTH_ERROR",
                vec![],
            )
        })?
        .to_string();
    let new_refresh = v_body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((new_access, new_refresh))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_validation_and_email_confirmation_server() {
        // login_hosted without email/password or token returns VALIDATION_ERROR
        let err = login_hosted(Some("user@example.com"), None, None, None, None).unwrap_err();
        assert_eq!(err.code, "VALIDATION_ERROR");
        let err2 = login_hosted(None, Some("secret"), None, None, None).unwrap_err();
        assert_eq!(err2.code, "VALIDATION_ERROR");

        // parse_query_param and local confirmation server HTML/callback handling
        assert_eq!(
            parse_query_param("access_token=abc&refresh_token=def", "access_token"),
            Some("abc".to_string())
        );
        assert_eq!(
            parse_query_param("access_token=abc&refresh_token=def", "refresh_token"),
            Some("def".to_string())
        );
        assert_eq!(
            parse_query_param("access_token=&other=1", "access_token"),
            None
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // 1. Landing page request `GET /` returns HTML confirmation page
        let client_thread = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut client = std::net::TcpStream::connect(addr).unwrap();
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut resp = String::new();
            client.read_to_string(&mut resp).unwrap();
            resp
        });
        let (srv_stream, _) = listener.accept().unwrap();
        assert!(serve_confirm_request(srv_stream).is_none());
        let html_resp = client_thread.join().unwrap();
        assert!(html_resp.contains("200 OK"));
        assert!(html_resp.contains("Email Confirmed"));

        // 2. Callback request `GET /auth-callback?access_token=...` extracts tokens
        let client_thread2 = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut client = std::net::TcpStream::connect(addr).unwrap();
            client
                .write_all(
                    b"GET /auth-callback?access_token=jwt123&refresh_token=ref456 HTTP/1.1\r\nHost: localhost\r\n\r\n",
                )
                .unwrap();
            let mut resp = String::new();
            client.read_to_string(&mut resp).unwrap();
            resp
        });
        let (srv_stream2, _) = listener.accept().unwrap();
        let extracted = serve_confirm_request(srv_stream2);
        assert_eq!(
            extracted,
            Some(("jwt123".to_string(), Some("ref456".to_string())))
        );
        let json_resp = client_thread2.join().unwrap();
        assert!(json_resp.contains(r#"{"ok":true}"#));
    }
}
