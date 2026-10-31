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
