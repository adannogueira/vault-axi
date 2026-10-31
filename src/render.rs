pub const PREVIEW_LIMIT: usize = 1200;

#[derive(Debug, Clone)]
pub struct Truncated {
    pub content: String,
    pub truncated: bool,
    pub total_chars: usize,
}

/// Truncate long text with an explicit indicator, per AXI content-truncation.
pub fn truncate_text(text: &str, limit: usize) -> Truncated {
    let total_chars = text.chars().count();
    if total_chars <= limit {
        return Truncated {
            content: text.to_string(),
            truncated: false,
            total_chars,
        };
    }
    let prefix: String = text.chars().take(limit).collect();
    Truncated {
        content: format!("{prefix}\n… [truncated]"),
        truncated: true,
        total_chars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_text_respects_exact_boundary_and_unicode_chars() {
        let exact = truncate_text("abcde", 5);
        assert!(!exact.truncated);
        assert_eq!(exact.total_chars, 5);
        assert_eq!(exact.content, "abcde");

        let shorter = truncate_text("abc", 5);
        assert!(!shorter.truncated);
        assert_eq!(shorter.total_chars, 3);
        assert_eq!(shorter.content, "abc");

        let longer = truncate_text("abcdef", 5);
        assert!(longer.truncated);
        assert_eq!(longer.total_chars, 6);
        assert_eq!(longer.content, "abcde\n… [truncated]");

        // Multi-byte UTF-8 characters (5 chars, 17 bytes)
        let unicode = "🦀日本語🎉";
        let u_exact = truncate_text(unicode, 5);
        assert!(!u_exact.truncated);
        assert_eq!(u_exact.total_chars, 5);
        assert_eq!(u_exact.content, unicode);

        let u_trunc = truncate_text(unicode, 3);
        assert!(u_trunc.truncated);
        assert_eq!(u_trunc.total_chars, 5);
        assert_eq!(u_trunc.content, "🦀日本\n… [truncated]");
    }
}
