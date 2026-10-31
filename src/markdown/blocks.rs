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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_doc_into_preamble_and_nested_heading_blocks() {
        let raw = [
            "---",
            "status: active",
            "---",
            "",
            "Preamble text before first heading.",
            "",
            "# Main Title",
            "Intro under main.",
            "",
            "## Sub A",
            "Line 1 of A.",
            "Line 2 of A.",
            "",
            "### Deep A1",
            "Deep content.",
        ]
        .join("\n");

        let blocks = to_blocks(&raw);
        assert_eq!(blocks.len(), 4);

        assert_eq!(blocks[0].heading_path, "(preamble)");
        assert_eq!(blocks[0].start_line, 4);
        assert_eq!(blocks[0].end_line, 6);
        assert_eq!(blocks[0].text, "Preamble text before first heading.");

        assert_eq!(blocks[1].heading_path, "Main Title");
        assert_eq!(blocks[1].start_line, 7);
        assert_eq!(blocks[1].end_line, 9);
        assert_eq!(blocks[1].text, "# Main Title\nIntro under main.");

        assert_eq!(blocks[2].heading_path, "Main Title > Sub A");
        assert_eq!(blocks[2].start_line, 10);
        assert_eq!(blocks[2].end_line, 13);

        assert_eq!(blocks[3].heading_path, "Main Title > Sub A > Deep A1");
        assert_eq!(blocks[3].start_line, 14);
        assert_eq!(blocks[3].end_line, 15);
    }

    #[test]
    fn skips_whitespace_only_preamble_and_handles_headingless_doc() {
        let raw = "---\na: 1\n---\n\n   \n# Heading\nBody\n";
        let blocks = to_blocks(raw);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].heading_path, "Heading");

        let no_headings = "Just a plain note without any headings.\nSecond line.\n";
        let blocks_nh = to_blocks(no_headings);
        assert_eq!(blocks_nh.len(), 1);
        assert_eq!(blocks_nh[0].heading_path, "(preamble)");
        assert_eq!(blocks_nh[0].start_line, 1);
        assert_eq!(blocks_nh[0].end_line, 2);

        let empty = "   \n\n  ";
        assert!(to_blocks(empty).is_empty());
    }
}
