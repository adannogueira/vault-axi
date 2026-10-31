use crate::markdown::sections::parse_doc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// " > "-joined heading path, or "(preamble)" for content before the first heading.
    pub heading_path: String,
    /// 1-based inclusive start line.
    pub start_line: usize,
    /// 1-based inclusive end line.
    pub end_line: usize,
    /// Heading title + own content (excludes nested subsections).
    pub text: String,
}

/// Split a document into heading-granular blocks (each heading owns the text up to
/// the next heading of ANY level; nested subsections become their own blocks).
pub fn to_blocks(raw: &str) -> Vec<Block> {
    let doc = parse_doc(raw);
    let mut blocks = Vec::new();
    let body_start = doc
        .frontmatter
        .as_ref()
        .map(|fm| fm.end_line + 1)
        .unwrap_or(0);

    let first_heading_line = doc
        .headings
        .first()
        .map(|h| h.line)
        .unwrap_or(doc.lines.len());
    if body_start < first_heading_line {
        let preamble = doc.lines[body_start..first_heading_line]
            .join("\n")
            .trim()
            .to_string();
        if !preamble.is_empty() {
            blocks.push(Block {
                heading_path: "(preamble)".to_string(),
                start_line: body_start + 1,
                end_line: first_heading_line,
                text: preamble,
            });
        }
    }

    for (i, h) in doc.headings.iter().enumerate() {
        let end = doc
            .headings
            .get(i + 1)
            .map(|next| next.line)
            .unwrap_or(doc.lines.len());
        let own = doc.lines[h.line..end].join("\n").trim().to_string();
        blocks.push(Block {
            heading_path: h.path.join(" > "),
            start_line: h.line + 1,
            end_line: end,
            text: own,
        });
    }

    blocks
}
