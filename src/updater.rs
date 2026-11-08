use crate::args::{flag_bool, parse_args};
use crate::axi::AxiError;
use crate::config::{config_dir, load_config};
use crate::context::Ctx;
use crate::crypto::sha256_hex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const UPDATE_COOLDOWN_MS: i64 = 24 * 60 * 60 * 1000;
const DEFAULT_RELEASE_API: &str =
    "https://api.github.com/repos/adannogueira/vault-axi/releases/latest";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub version: String,
    pub asset_name: String,
    pub download_url: String,
    pub sha256_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UpdateState {
    last_checked_ms: i64,
    last_version: Option<String>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn update_state_path() -> PathBuf {
    config_dir().join("update-check.json")
}

pub fn platform_asset_name() -> &'static str {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        "vault-windows-x86_64.exe"
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "vault-macos-aarch64"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "vault-macos-x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "vault-linux-x86_64"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "vault-linux-aarch64"
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64")
    )))]
    {
        "vault"
    }
}

fn platform_asset_candidates() -> &'static [&'static str] {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        &[
            "vault-windows-x86_64.exe",
            "vault-x86_64-pc-windows-msvc.exe",
        ]
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &["vault-macos-aarch64", "vault-aarch64-apple-darwin"]
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        &["vault-macos-x86_64", "vault-x86_64-apple-darwin"]
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        &["vault-linux-x86_64", "vault-x86_64-unknown-linux-gnu"]
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        &["vault-linux-aarch64", "vault-aarch64-unknown-linux-gnu"]
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64")
    )))]
    {
        &["vault"]
    }
}

pub fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let clean = v
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()?;
    let mut parts = clean.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    let patch = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    Some((major, minor, patch))
}

pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    match (parse_semver(current), parse_semver(candidate)) {
        (Some(cur), Some(cand)) => cand > cur,
        _ => false,
    }
}

pub fn fetch_latest_release(api_url_override: Option<&str>) -> Result<ReleaseInfo, String> {
    let url = api_url_override
        .map(|s| s.to_string())
        .or_else(|| std::env::var("VAULT_AXI_UPDATE_URL").ok())
        .unwrap_or_else(|| DEFAULT_RELEASE_API.to_string());

    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(5))
        .set("User-Agent", &format!("vault-axi/{}", env!("CARGO_PKG_VERSION")))
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("Failed to check GitHub releases: {e}"))?;

    let raw = resp
        .into_string()
        .map_err(|e| format!("Invalid release response: {e}"))?;
    let body: Value =
        serde_json::from_str(&raw).map_err(|e| format!("Invalid release JSON: {e}"))?;

    let tag_name = body
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "Release JSON missing tag_name".to_string())?
        .to_string();
    let version = tag_name.trim_start_matches(['v', 'V']).to_string();

    let candidates = platform_asset_candidates();
    let mut matched_asset = platform_asset_name().to_string();
    let mut download_url: Option<String> = None;
    let mut sha256_url: Option<String> = None;

    if let Some(assets) = body.get("assets").and_then(|a| a.as_array()) {
        for &cand in candidates {
            let cand_sha = format!("{cand}.sha256");
            let mut found_bin: Option<String> = None;
            let mut found_sha: Option<String> = None;
            for asset in assets {
                let name = asset.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let b_url = asset
                    .get("browser_download_url")
                    .and_then(|u| u.as_str())
                    .unwrap_or("");
                if name == cand && !b_url.is_empty() {
                    found_bin = Some(b_url.to_string());
                } else if name == cand_sha && !b_url.is_empty() {
                    found_sha = Some(b_url.to_string());
                }
            }
            if let Some(dl) = found_bin {
                matched_asset = cand.to_string();
                download_url = Some(dl);
                sha256_url = found_sha;
                break;
            }
        }
    }

    let Some(dl_url) = download_url else {
        return Err(format!(
            "No binary asset '{}' found in release {tag_name}",
            platform_asset_name()
        ));
    };

    Ok(ReleaseInfo {
        tag_name,
        version,
        asset_name: matched_asset,
        download_url: dl_url,
        sha256_url,
    })
}

pub fn download_and_replace_binary(
    info: &ReleaseInfo,
    target_exe: &Path,
) -> Result<(), String> {
    let resp = ureq::get(&info.download_url)
        .timeout(Duration::from_secs(60))
        .set("User-Agent", &format!("vault-axi/{}", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|e| format!("Failed to download update binary: {e}"))?;

    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Failed to read update stream: {e}"))?;

    if bytes.is_empty() {
        return Err("Downloaded binary is empty".to_string());
    }

    if let Some(ref sha_url) = info.sha256_url {
        if let Ok(sha_resp) = ureq::get(sha_url)
            .timeout(Duration::from_secs(10))
            .set("User-Agent", &format!("vault-axi/{}", env!("CARGO_PKG_VERSION")))
            .call()
        {
            if let Ok(sha_text) = sha_resp.into_string() {
                let expected = sha_text
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                if expected.len() == 64 {
                    let actual = sha256_hex(&bytes);
                    if actual != expected {
                        return Err(format!(
                            "SHA-256 checksum mismatch: expected {expected}, got {actual}"
                        ));
                    }
                }
            }
        }
    }

    let new_path = target_exe.with_extension("new_bin");
    let mut old_path = target_exe.with_extension("old_bin");
    if old_path.exists() && fs::remove_file(&old_path).is_err() {
        old_path = target_exe.with_extension(format!("old_bin.{}", std::process::id()));
        let _ = fs::remove_file(&old_path);
    }

    fs::write(&new_path, &bytes)
        .map_err(|e| format!("Failed to write staged binary: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&new_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = fs::set_permissions(&new_path, perms);
        }
    }

    if target_exe.exists() {
        fs::rename(target_exe, &old_path)
            .map_err(|e| format!("Failed to move current binary aside: {e}"))?;
    }
    if let Err(e) = fs::rename(&new_path, target_exe) {
        // Roll back if rename failed
        let _ = fs::rename(&old_path, target_exe);
        return Err(format!("Failed to activate updated binary: {e}"));
    }
    let _ = fs::remove_file(&old_path);
    Ok(())
}

pub fn cleanup_old_binaries() {
    if let Ok(exe) = std::env::current_exe() {
        let old_path = exe.with_extension("old_bin");
        if old_path.exists() {
            let _ = fs::remove_file(old_path);
        }
        if let Some(parent) = exe.parent() {
            if let Ok(entries) = fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.contains(".old_bin.") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
}

/// Clean up any leftover `.old_bin` from a previous self-update and perform a background check
/// at most once every 24 hours when `auto_update` is enabled.
pub fn auto_update_if_due() {
    cleanup_old_binaries();

    if cfg!(test)
        || std::env::var("VAULT_AXI_NO_UPDATE").as_deref() == Ok("1")
        || std::env::var("VAULT_AXI_NO_SYNC").as_deref() == Ok("1")
    {
        return;
    }

    let cfg = load_config();
    if cfg.auto_update == Some(false) {
        return;
    }

    let state_path = update_state_path();
    let now = now_ms();
    let mut state: UpdateState = fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if now - state.last_checked_ms < UPDATE_COOLDOWN_MS {
        return;
    }

    state.last_checked_ms = now;
    if let Some(parent) = state_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(serialized) = serde_json::to_string_pretty(&state) {
        let _ = fs::write(&state_path, serialized);
    }

    let current = env!("CARGO_PKG_VERSION");
    if let Ok(info) = fetch_latest_release(None) {
        if is_newer_version(current, &info.version) {
            if let Ok(exe) = std::env::current_exe() {
                if download_and_replace_binary(&info, &exe).is_ok() {
                    state.last_version = Some(info.version.clone());
                    if let Ok(serialized) = serde_json::to_string_pretty(&state) {
                        let _ = fs::write(&state_path, serialized);
                    }
                    eprintln!(
                        "==> vault-axi auto-updated from v{current} to v{}.",
                        info.version
                    );
                }
            }
        }
    }
}

pub fn self_update_command(args: &[String], _ctx: &mut Ctx) -> Result<Value, AxiError> {
    let parsed = parse_args(args);
    let check_only = flag_bool(&parsed.flags, "check");
    let force = flag_bool(&parsed.flags, "force");
    let current = env!("CARGO_PKG_VERSION");

    let info = fetch_latest_release(None).map_err(|e| {
        AxiError::new(
            e,
            "UPDATE_ERROR",
            vec!["Check your internet connection or download from https://github.com/adannogueira/vault-axi/releases".to_string()],
        )
    })?;

    let update_available = is_newer_version(current, &info.version);
    if check_only || (!update_available && !force) {
        return Ok(json!({
            "ok": true,
            "currentVersion": current,
            "latestVersion": info.version,
            "updateAvailable": update_available,
            "updated": false,
            "asset": info.asset_name
        }));
    }

    let exe = std::env::current_exe().map_err(|e| {
        AxiError::new(
            format!("Could not determine current executable path: {e}"),
            "IO_ERROR",
            vec![],
        )
    })?;

    download_and_replace_binary(&info, &exe)
        .map_err(|e| AxiError::new(e, "UPDATE_ERROR", vec![]))?;

    Ok(json!({
        "ok": true,
        "updated": true,
        "fromVersion": current,
        "toVersion": info.version,
        "asset": info.asset_name,
        "binaryPath": exe.to_string_lossy()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn semver_parsing_and_comparison_work() {
        assert_eq!(parse_semver("v1.0.0"), Some((1, 0, 0)));
        assert_eq!(parse_semver("1.2.3-beta.1"), Some((1, 2, 3)));
        assert!(is_newer_version("1.0.0", "1.0.1"));
        assert!(is_newer_version("1.0.0", "v1.1.0"));
        assert!(is_newer_version("0.9.9", "1.0.0"));
        assert!(!is_newer_version("1.0.0", "1.0.0"));
        assert!(!is_newer_version("1.2.0", "1.1.9"));
    }

    #[test]
    fn download_and_replace_verifies_sha256_and_swaps_binary() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let payload = b"mock-new-vault-binary-v2";
        let expected_sha = sha256_hex(payload);
        let sha_clone = expected_sha.clone();

        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 1024];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    if req.contains("GET /bin ") {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len()
                        );
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(payload);
                    } else {
                        let body = format!("{sha_clone}  vault\n");
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(body.as_bytes());
                    }
                }
            }
        });

        let tmp_dir = std::env::temp_dir().join(format!("vault-updater-test-{}", now_ms()));
        let _ = fs::create_dir_all(&tmp_dir);
        let target_exe = tmp_dir.join("vault.exe");
        fs::write(&target_exe, b"old-binary").unwrap();

        let info = ReleaseInfo {
            tag_name: "v9.9.9".to_string(),
            version: "9.9.9".to_string(),
            asset_name: platform_asset_name().to_string(),
            download_url: format!("http://{addr}/bin"),
            sha256_url: Some(format!("http://{addr}/sha")),
        };

        download_and_replace_binary(&info, &target_exe).unwrap();
        let updated_bytes = fs::read(&target_exe).unwrap();
        assert_eq!(updated_bytes, payload);
        let _ = server.join();
        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
