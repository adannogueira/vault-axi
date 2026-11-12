//! Logical sync payloads: UTF-8 text is carried as-is, binary files as `b64:v1:<base64>`.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

pub const BIN_PREFIX: &str = "b64:v1:";

pub fn encode_binary(bytes: &[u8]) -> String {
    format!("{BIN_PREFIX}{}", B64.encode(bytes))
}

/// Returns `None` when `payload` is not a binary payload (or is malformed).
pub fn decode_binary(payload: &str) -> Option<Vec<u8>> {
    let b64 = payload.strip_prefix(BIN_PREFIX)?;
    B64.decode(b64.trim()).ok()
}

pub fn is_binary(payload: &str) -> bool {
    payload.starts_with(BIN_PREFIX)
}

/// Text that starts like an envelope (`b64:` / `enc:`) must travel as binary, or it would be
/// mistaken for one on the other side.
fn looks_like_envelope(text: &str) -> bool {
    text.starts_with("b64:") || text.starts_with("enc:")
}

fn is_text_path(rel_path: &str) -> bool {
    rel_path.to_ascii_lowercase().ends_with(".md") || rel_path.starts_with(".obsidian/")
}

/// Map a vault file's bytes to its sync payload. Notes and `.obsidian/` files travel as text
/// when they are valid UTF-8; everything else as `b64:v1:`.
pub fn from_file(rel_path: &str, bytes: &[u8]) -> String {
    if is_text_path(rel_path) {
        if let Ok(text) = std::str::from_utf8(bytes) {
            if !looks_like_envelope(text) {
                return text.to_string();
            }
        }
    }
    encode_binary(bytes)
}

/// Inverse of [`from_file`]: the bytes to write to disk.
pub fn to_file(payload: &str) -> Vec<u8> {
    decode_binary(payload).unwrap_or_else(|| payload.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_roundtrip() {
        let raw = [0x89, b'P', b'N', b'G', 0x00, 0xFF];
        let encoded = encode_binary(&raw);
        assert!(is_binary(&encoded));
        assert_eq!(decode_binary(&encoded).unwrap(), raw);
        assert_eq!(decode_binary("plain text"), None);
        assert_eq!(decode_binary("b64:v1:not base64!"), None);
    }

    #[test]
    fn file_mapping() {
        assert_eq!(from_file("a/Note.MD", "# hi".as_bytes()), "# hi");
        assert_eq!(from_file(".obsidian/app.json", b"{}"), "{}");
        assert!(is_binary(&from_file("img/a.png", b"\x89PNG")));
        assert!(
            is_binary(&from_file("a.md", &[0xff, 0xfe])),
            "invalid UTF-8 stays lossless"
        );
        let tricky = from_file("a.md", b"b64:v1:aGk=");
        assert!(is_binary(&tricky), "envelope-looking text is escaped");
        assert_eq!(to_file(&tricky), b"b64:v1:aGk=");
        assert!(is_binary(&from_file(".obsidian/x", b"enc:v1:abc")));
        for (path, bytes) in [("n.md", &b"text"[..]), ("b.bin", &[0u8, 1, 2][..])] {
            assert_eq!(to_file(&from_file(path, bytes)), bytes);
        }
    }
}
