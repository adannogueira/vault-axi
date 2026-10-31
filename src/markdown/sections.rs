//! Heading-aware markdown surgery. Operations edit the raw text by splicing
//! line ranges derived from a heading scan — the document is never re-serialized
//! through a parser, so formatting outside the touched range is preserved exactly.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    /// ATX level, 1-6.
    pub level: usize,
    /// Heading text with leading #'s and any trailing closing #'s removed.
    pub title: String,
    /// 0-based index into `lines` of the heading line.
    pub line: usize,
    /// Ancestor titles from the document root down to and including this heading.
    pub path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    /// 0-based line of the opening `---`.
    pub start_line: usize,
    /// 0-based line of the closing `---`.
    pub end_line: usize,
}

#[derive(Debug, Clone)]
pub struct ParsedDoc {
    pub eol: &'static str,
    pub trailing_newline: bool,
    pub lines: Vec<String>,
    pub frontmatter: Option<Frontmatter>,
    pub headings: Vec<Heading>,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub heading: Heading,
    /// Line of the heading itself.
    pub heading_line: usize,
    /// First line of the body (heading_line + 1).
    pub body_start: usize,
    /// Exclusive end of the body: the next heading of equal/higher level, or EOF.
    pub body_end: usize,
    /// Body text (between heading and next boundary), without the heading line.
    pub body: String,
    /// Whole section text including the heading line.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionErrorKind {
    NotFound,
    Ambiguous,
}

#[derive(Debug, Clone)]
pub struct SectionError {
    pub message: String,
    pub kind: SectionErrorKind,
}

impl SectionError {
    pub fn new(message: impl Into<String>, kind: SectionErrorKind) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }
}

impl std::fmt::Display for SectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SectionError {}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6})[ \t]+(.*)$").unwrap())
}

fn fence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^([ \t]{0,3})(`{3,}|~{3,})(.*)$").unwrap())
}

fn trailing_hashes_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[ \t]+#+[ \t]*$").unwrap())
}

pub fn detect_eol(raw: &str) -> &'static str {
    if raw.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

pub fn split_lines(raw: &str) -> (Vec<String>, &'static str, bool) {
    let eol = detect_eol(raw);
    let trailing_newline = raw.ends_with("\r\n") || raw.ends_with('\n');
    let mut lines: Vec<String> = Vec::new();
    let mut start = 0;
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
            lines.push(raw[start..i].to_string());
            i += 2;
            start = i;
        } else if bytes[i] == b'\n' {
            lines.push(raw[start..i].to_string());
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() || !trailing_newline {
        lines.push(raw[start..].to_string());
    }
    (lines, eol, trailing_newline)
}

pub fn join_lines(lines: &[String], eol: &str, trailing_newline: bool) -> String {
    let mut out = lines.join(eol);
    if trailing_newline {
        out.push_str(eol);
    }
    out
}

fn normalize_title(text: &str) -> String {
    trailing_hashes_re().replace(text, "").trim().to_string()
}

pub fn parse_doc(raw: &str) -> ParsedDoc {
    let (lines, eol, trailing_newline) = split_lines(raw);

    let mut frontmatter = None;
    let mut scan_start = 0;
    if lines.first().map(|s| s.as_str()) == Some("---") {
        for (i, line) in lines.iter().enumerate().skip(1) {
            if line == "---" || line == "..." {
                frontmatter = Some(Frontmatter {
                    start_line: 0,
                    end_line: i,
                });
                scan_start = i + 1;
                break;
            }
        }
    }

    let mut headings: Vec<Heading> = Vec::new();
    let mut stack: Vec<Heading> = Vec::new();
    let mut fence: Option<char> = None;

    for (i, line) in lines.iter().enumerate().skip(scan_start) {
        if let Some(caps) = fence_re().captures(line) {
            let marker = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let ch = marker.chars().next().unwrap_or('`');
            match fence {
                None => fence = Some(ch),
                Some(f) if f == ch => fence = None,
                _ => {}
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }

        let Some(caps) = heading_re().captures(line) else {
            continue;
        };
        let level = caps.get(1).unwrap().as_str().len();
        let title = normalize_title(caps.get(2).map(|m| m.as_str()).unwrap_or(""));
        while stack.last().map(|h| h.level >= level).unwrap_or(false) {
            stack.pop();
        }
        let mut path: Vec<String> = stack.iter().map(|h| h.title.clone()).collect();
        path.push(title.clone());
        let heading = Heading {
            level,
            title,
            line: i,
            path,
        };
        headings.push(heading.clone());
        stack.push(heading);
    }

    ParsedDoc {
        eol,
        trailing_newline,
        lines,
        frontmatter,
        headings,
    }
}

fn segments(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in path.split('/') {
        for sub in part.split("::") {
            let trimmed = sub.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}

/// All headings whose ancestor chain ends with the given path segments (case-insensitive).
pub fn find_headings(doc: &ParsedDoc, path: &str) -> Vec<Heading> {
    let segs: Vec<String> = segments(path).into_iter().map(|s| s.to_lowercase()).collect();
    if segs.is_empty() {
        return Vec::new();
    }
    doc.headings
        .iter()
        .filter(|h| {
            if segs.len() > h.path.len() {
                return false;
            }
            let tail = &h.path[h.path.len() - segs.len()..];
            tail.iter()
                .zip(segs.iter())
                .all(|(t, s)| t.to_lowercase() == *s)
        })
        .cloned()
        .collect()
}

fn section_bounds(doc: &ParsedDoc, heading: &Heading) -> (usize, usize) {
    let idx = doc
        .headings
        .iter()
        .position(|h| h.line == heading.line)
        .unwrap_or(0);
    let body_start = heading.line + 1;
    let mut body_end = doc.lines.len();
    for next in doc.headings.iter().skip(idx + 1) {
        if next.level <= heading.level {
            body_end = next.line;
            break;
        }
    }
    (body_start, body_end)
}

pub fn get_section(doc: &ParsedDoc, heading: &Heading) -> Section {
    let (body_start, body_end) = section_bounds(doc, heading);
    let body = doc.lines[body_start..body_end].join(doc.eol);
    let text = doc.lines[heading.line..body_end].join(doc.eol);
    Section {
        heading: heading.clone(),
        heading_line: heading.line,
        body_start,
        body_end,
        body,
        text,
    }
}

/// Resolve exactly one section for a heading path, or return a SectionError.
pub fn resolve_section(doc: &ParsedDoc, path: &str) -> Result<Section, SectionError> {
    let matches = find_headings(doc, path);
    if matches.is_empty() {
        return Err(SectionError::new(
            format!("No heading matches \"{path}\""),
            SectionErrorKind::NotFound,
        ));
    }
    if matches.len() > 1 {
        let where_str = matches
            .iter()
            .map(|h| h.path.join(" / "))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(SectionError::new(
            format!(
                "Heading \"{path}\" is ambiguous ({} matches: {where_str}). Use a fuller path like \"Parent/Child\".",
                matches.len()
            ),
            SectionErrorKind::Ambiguous,
        ));
    }
    Ok(get_section(doc, &matches[0]))
}

fn to_block_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .replace("\r\n", "\n")
        .split('\n')
        .map(|s| s.to_string())
        .collect();
    while lines.first().map(|s| s.trim().is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(|s| s.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines
}

fn splice(lines: &[String], start: usize, end: usize, insert: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len() - (end - start) + insert.len());
    out.extend_from_slice(&lines[..start]);
    out.extend_from_slice(insert);
    out.extend_from_slice(&lines[end..]);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionMode {
    Replace,
    Append,
    Prepend,
}

impl SectionMode {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "replace" => Some(Self::Replace),
            "append" => Some(Self::Append),
            "prepend" => Some(Self::Prepend),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Append => "append",
            Self::Prepend => "prepend",
        }
    }
}

/// Edit the body of a section in place. Never touches the heading line itself.
pub fn edit_section(
    raw: &str,
    path: &str,
    mode: SectionMode,
    content: &str,
) -> Result<String, SectionError> {
    let doc = parse_doc(raw);
    let section = resolve_section(&doc, path)?;
    let block = to_block_lines(content);
    let body_start = section.body_start;
    let body_end = section.body_end;
    let has_following_heading = body_end < doc.lines.len();

    let out = match mode {
        SectionMode::Replace => {
            let mut insert = block;
            if !insert.is_empty() && has_following_heading {
                insert.push(String::new());
            }
            splice(&doc.lines, body_start, body_end, &insert)
        }
        SectionMode::Prepend => {
            let mut first_content = body_start;
            while first_content < body_end && doc.lines[first_content].trim().is_empty() {
                first_content += 1;
            }
            let mut insert = block;
            if !insert.is_empty() {
                insert.push(String::new());
            }
            splice(&doc.lines, body_start, first_content, &insert)
        }
        SectionMode::Append => {
            let mut last_content = body_end;
            while last_content > body_start && doc.lines[last_content - 1].trim().is_empty() {
                last_content -= 1;
            }
            let had_body = last_content > body_start;
            let mut insert = Vec::new();
            if !block.is_empty() {
                if had_body {
                    insert.push(String::new());
                }
                insert.extend(block);
                if has_following_heading {
                    insert.push(String::new());
                }
            }
            splice(&doc.lines, last_content, body_end, &insert)
        }
    };

    Ok(join_lines(&out, doc.eol, doc.trailing_newline))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadingAnchor {
    BeforeHeading,
    AfterHeading,
}

/// Insert a fresh block immediately before or after a heading line.
pub fn insert_at_heading(
    raw: &str,
    path: &str,
    where_anchor: HeadingAnchor,
    content: &str,
) -> Result<String, SectionError> {
    let doc = parse_doc(raw);
    let section = resolve_section(&doc, path)?;
    let mut block = to_block_lines(content);
    if block.is_empty() {
        return Ok(raw.to_string());
    }
    block.push(String::new());

    let out = match where_anchor {
        HeadingAnchor::BeforeHeading => {
            splice(&doc.lines, section.heading_line, section.heading_line, &block)
        }
        HeadingAnchor::AfterHeading => {
            splice(&doc.lines, section.body_start, section.body_start, &block)
        }
    };
    Ok(join_lines(&out, doc.eol, doc.trailing_newline))
}

/// Append a block to the end of the document.
pub fn append_to_doc(raw: &str, content: &str) -> String {
    let doc = parse_doc(raw);
    let block = to_block_lines(content);
    if block.is_empty() {
        return raw.to_string();
    }
    let mut end = doc.lines.len();
    while end > 0 && doc.lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let mut insert = Vec::new();
    if end > 0 {
        insert.push(String::new());
    }
    insert.extend(block);
    let out = splice(&doc.lines, end, doc.lines.len(), &insert);
    join_lines(&out, doc.eol, doc.trailing_newline)
}

/// Prepend a block to the document, after the frontmatter block if present.
pub fn prepend_to_doc(raw: &str, content: &str) -> String {
    let doc = parse_doc(raw);
    let block = to_block_lines(content);
    if block.is_empty() {
        return raw.to_string();
    }
    let start = doc
        .frontmatter
        .as_ref()
        .map(|fm| fm.end_line + 1)
        .unwrap_or(0);
    let mut first_content = start;
    while first_content < doc.lines.len() && doc.lines[first_content].trim().is_empty() {
        first_content += 1;
    }
    let mut insert = Vec::new();
    insert.push(String::new());
    insert.extend(block);
    insert.push(String::new());
    let mut out = splice(&doc.lines, start, first_content, &insert);
    while start == 0 && out.first().map(|s| s.trim().is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    join_lines(&out, doc.eol, doc.trailing_newline)
}

/// Replace an exact text anchor. Errors unless the anchor appears exactly once.
pub fn replace_anchor(raw: &str, anchor: &str, replacement: &str) -> Result<String, SectionError> {
    if anchor.is_empty() {
        return Err(SectionError::new(
            "Anchor text must not be empty",
            SectionErrorKind::NotFound,
        ));
    }
    let matches: Vec<usize> = raw.match_indices(anchor).map(|(idx, _)| idx).collect();
    if matches.is_empty() {
        let quoted = serde_json::to_string(anchor).unwrap_or_else(|_| format!("\"{anchor}\""));
        return Err(SectionError::new(
            format!("Anchor text not found: {quoted}"),
            SectionErrorKind::NotFound,
        ));
    }
    if matches.len() > 1 {
        return Err(SectionError::new(
            format!(
                "Anchor text matched {} times; it must be unique. Add surrounding context to disambiguate.",
                matches.len()
            ),
            SectionErrorKind::Ambiguous,
        ));
    }
    let first = matches[0];
    let mut out = String::with_capacity(raw.len() - anchor.len() + replacement.len());
    out.push_str(&raw[..first]);
    out.push_str(replacement);
    out.push_str(&raw[first + anchor.len()..]);
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutlineEntry {
    pub level: usize,
    pub title: String,
    pub line: usize,
    pub path: String,
}

pub fn outline(doc: &ParsedDoc) -> Vec<OutlineEntry> {
    doc.headings
        .iter()
        .map(|h| OutlineEntry {
            level: h.level,
            title: h.title.clone(),
            line: h.line + 1,
            path: h.path.join("/"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "---\ndate: 2026-07-01\nstatus: draft\n---\n\n# AXI Tool Scratch Test\n\n## Overview\nThis is the original overview text. It has two sentences. Here is the second.\n\n## Design\nOriginal design notes go here.\n\n### Sub-detail\nA nested subsection with some content.\n\n## Open Questions\n- First question\n- Second question\n";

    #[test]
    fn parse_doc_detects_frontmatter_and_headings() {
        let doc = parse_doc(DOC);
        assert_eq!(
            doc.frontmatter,
            Some(Frontmatter {
                start_line: 0,
                end_line: 3
            })
        );
        let titles: Vec<&str> = doc.headings.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "AXI Tool Scratch Test",
                "Overview",
                "Design",
                "Sub-detail",
                "Open Questions"
            ]
        );
    }

    #[test]
    fn builds_ancestor_paths_and_ignores_code_fences() {
        let doc = parse_doc(DOC);
        let sub = doc.headings.iter().find(|h| h.title == "Sub-detail").unwrap();
        assert_eq!(
            sub.path,
            vec!["AXI Tool Scratch Test", "Design", "Sub-detail"]
        );

        let raw = ["# Real", "", "```", "# not a heading", "```", "", "## Also Real", ""].join("\n");
        let doc2 = parse_doc(&raw);
        let titles: Vec<&str> = doc2.headings.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, vec!["Real", "Also Real"]);
    }

    #[test]
    fn outline_returns_1_based_lines_and_slash_paths() {
        let entries = outline(&parse_doc(DOC));
        let sub = entries.iter().find(|e| e.title == "Sub-detail").unwrap();
        assert_eq!(sub.level, 3);
        assert_eq!(sub.path, "AXI Tool Scratch Test/Design/Sub-detail");
    }

    #[test]
    fn edit_section_replace_in_place() {
        let out = edit_section(DOC, "Design", SectionMode::Replace, "Replaced design notes.").unwrap();
        let design_headings: Vec<&str> = out
            .lines()
            .filter(|l| l.ends_with("Design") && l.starts_with('#'))
            .collect();
        assert_eq!(design_headings, vec!["## Design"]);
        assert!(out.contains("## Design\nReplaced design notes."));
        assert!(!out.contains("Original design notes go here."));
        assert!(!out.contains("### Sub-detail"));
        assert!(out.contains("## Open Questions"));
        assert!(out.contains("- First question"));
    }

    #[test]
    fn edit_section_stops_at_equal_or_higher_and_targets_nested() {
        let out = edit_section(DOC, "Overview", SectionMode::Replace, "New overview.").unwrap();
        assert!(out.contains("## Overview\nNew overview.\n\n## Design"));
        assert!(out.contains("Original design notes go here."));

        let out2 = edit_section(
            DOC,
            "Design/Sub-detail",
            SectionMode::Replace,
            "New nested content.",
        )
        .unwrap();
        assert!(out2.contains("### Sub-detail\nNew nested content."));
        assert!(out2.contains("Original design notes go here."));
    }

    #[test]
    fn edit_section_append_and_prepend() {
        let out = edit_section(DOC, "Overview", SectionMode::Append, "Appended line.").unwrap();
        assert!(out.contains(
            "Overview\nThis is the original overview text. It has two sentences. Here is the second.\n\nAppended line.\n\n## Design"
        ));

        let out2 = edit_section(DOC, "Overview", SectionMode::Prepend, "Prepended line.").unwrap();
        assert!(out2.contains("## Overview\nPrepended line.\n\nThis is the original overview"));
    }

    #[test]
    fn insert_at_heading_and_doc_level() {
        let out = insert_at_heading(
            DOC,
            "Design",
            HeadingAnchor::BeforeHeading,
            "## New Section\nbody",
        )
        .unwrap();
        assert!(out.contains("## New Section\nbody\n\n## Design"));

        let out2 = insert_at_heading(
            DOC,
            "Design",
            HeadingAnchor::AfterHeading,
            "Injected first line.",
        )
        .unwrap();
        assert!(out2.contains("## Design\nInjected first line.\n\nOriginal design notes"));

        let appended = append_to_doc(DOC, "## Appended Tail\ncontent");
        assert!(appended.trim_end().ends_with("## Appended Tail\ncontent"));

        let prepended = prepend_to_doc(DOC, "Intro paragraph.");
        assert!(prepended.starts_with(
            "---\ndate: 2026-07-01\nstatus: draft\n---\n\nIntro paragraph.\n\n# AXI"
        ));
    }

    #[test]
    fn replace_anchor_and_edge_cases() {
        let out = replace_anchor(DOC, "First question", "1st question").unwrap();
        assert!(out.contains("- 1st question"));
        assert!(replace_anchor(DOC, "nope-not-here", "x").is_err());
        let ambig = replace_anchor(DOC, "question", "x").unwrap_err();
        assert!(ambig.message.contains("matched 2 times"));

        assert!(resolve_section(&parse_doc(DOC), "Nonexistent").is_err());

        let crlf = "# Title\r\n\r\n## A\r\nbody a\r\n\r\n## B\r\nbody b\r\n";
        let out_crlf = edit_section(crlf, "A", SectionMode::Replace, "new a").unwrap();
        assert!(out_crlf.contains("\r\n"));
        assert!(!out_crlf.contains("body a"));
        assert!(out_crlf.contains("## B\r\nbody b"));

        let raw = "# Only\nsome text";
        let out_only = edit_section(raw, "Only", SectionMode::Replace, "replaced").unwrap();
        assert_eq!(out_only, "# Only\nreplaced");

        let out_last = edit_section(
            DOC,
            "Open Questions",
            SectionMode::Replace,
            "- New only question",
        )
        .unwrap();
        assert!(out_last
            .trim_end()
            .ends_with("## Open Questions\n- New only question"));
    }
}
