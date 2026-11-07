use rusqlite::{params, Connection, OptionalExtension};
use sha1::{Digest, Sha1};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Extract text from an image attachment using local OCR (`tesseract` or OS-native OCR),
/// caching results by SHA-1 hash in the `ocr_cache` table.
pub fn extract_image_ocr(db: &Connection, abs_path: &str) -> Option<String> {
    let bytes = fs::read(abs_path).ok()?;
    let mut hasher = Sha1::new();
    hasher.update(&bytes);
    let file_hash: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();

    if let Ok(mock) = std::env::var("VAULT_AXI_MOCK_OCR") {
        let trimmed = mock.trim();
        if !trimmed.is_empty() {
            let _ = db.execute(
                "INSERT INTO ocr_cache(hash, text, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(hash) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
                params![&file_hash, trimmed, now_ms()],
            );
            return Some(trimmed.to_string());
        }
    }

    if std::env::var("VAULT_AXI_NO_OCR").as_deref() == Ok("1")
        || std::env::var("VAULT_AXI_NO_EMBED").as_deref() == Ok("1")
            && std::env::var("VAULT_AXI_ENABLE_OCR").as_deref() != Ok("1")
    {
        return None;
    }

    let cached: Option<String> = db
        .query_row(
            "SELECT text FROM ocr_cache WHERE hash = ?1",
            params![&file_hash],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();

    if let Some(c) = cached {
        let trimmed = c.trim();
        return if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }

    let path = Path::new(abs_path);
    if !path.is_file() {
        return None;
    }

    let extracted = run_local_ocr(path).unwrap_or_default();
    let clean = extracted
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    let _ = db.execute(
        "INSERT INTO ocr_cache(hash, text, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(hash) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
        params![&file_hash, &clean, now_ms()],
    );

    if clean.is_empty() {
        None
    } else {
        Some(clean)
    }
}

fn run_local_ocr(path: &Path) -> Option<String> {
    let path_str = path.to_str()?;

    // 1. Try `tesseract` CLI (cross-platform on Linux, macOS, Windows)
    if let Ok(out) = Command::new("tesseract")
        .args([path_str, "stdout", "--oem", "1", "-q"])
        .output()
    {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    // 2. Windows native `Windows.Media.Ocr.OcrEngine` fallback (zero external dependencies)
    #[cfg(target_os = "windows")]
    {
        let escaped = path_str.replace('\'', "''");
        let ps_script = format!(
            "$ErrorActionPreference = 'Stop'; \
             Add-Type -AssemblyName System.Runtime.WindowsRuntime; \
             $null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]; \
             $null = [Windows.Media.Ocr.OcrEngine, Windows.Foundation, ContentType = WindowsRuntime]; \
             $null = [Windows.Graphics.Imaging.BitmapDecoder, Windows.Foundation, ContentType = WindowsRuntime]; \
             $asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {{ $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' }})[0]; \
             Function Await($WinRtTask, $ResultType) {{ $asTask = $asTaskGeneric.MakeGenericMethod($ResultType); $netTask = $asTask.Invoke($null, @($WinRtTask)); $netTask.Wait(-1) | Out-Null; $netTask.Result }}; \
             $file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync('{escaped}')) ([Windows.Storage.StorageFile]); \
             $stream = Await ($file.OpenAsync([Windows.Storage.FileAccessMode]::Read)) ([Windows.Storage.Streams.IRandomAccessStream]); \
             $decoder = Await ([Windows.Graphics.Imaging.BitmapDecoder]::CreateAsync($stream)) ([Windows.Graphics.Imaging.BitmapDecoder]); \
             $bitmap = Await ($decoder.GetSoftwareBitmapAsync()) ([Windows.Graphics.Imaging.SoftwareBitmap]); \
             $engine = [Windows.Media.Ocr.OcrEngine]::TryCreateFromUserProfileLanguages(); \
             $res = Await ($engine.RecognizeAsync($bitmap)) ([Windows.Media.Ocr.OcrResult]); \
             Write-Output $res.Text"
        );
        if let Ok(out) = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &ps_script])
            .output()
        {
            if out.status.success() {
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }

    None
}
