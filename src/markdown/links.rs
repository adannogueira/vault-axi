use crate::markdown::sections::{join_lines, split_lines};
use regex::{Captures, Regex};
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// Raw target as written, e.g. "plans/foo" or "foo.md" or "Foo#Heading".
    pub raw: String,
    /// Target after stripping #heading and |alias, and the .md extension.
    pub target: String,
    pub heading: Option<String>,
    pub alias: Option<String>,
    /// True for embeds (`![[...]]`).
    pub embed: bool,
}

fn wikilink_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(!?)\[\[([^\]]+)\]\]").unwrap())
}

fn fence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^([ \t]{0,3})(`{3,}|~{3,})").unwrap())
}

fn inline_code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`]*`").unwrap())
}

/// Parse all [[wikilinks]] from markdown, ignoring fenced code and inline code
/// spans. Frontmatter IS scanned, so `related: [[note]]` fields become graph edges.
pub fn parse_wiki_links(raw: &str) -> Vec<WikiLink> {
    let (lines, _, _) = split_lines(raw);
    let mut links = Vec::new();
    let mut fence: Option<char> = None;

    for line in &lines {
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

        let scrubbed = inline_code_re().replace_all(line, |caps: &Captures| {
            " ".repeat(caps[0].len())
        });
        for caps in wikilink_re().captures_iter(&scrubbed) {
            let embed = caps.get(1).map(|m| m.as_str()) == Some("!");
            let inner = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            links.push(parse_inner(inner, embed));
        }
    }

    links
}

/// Rewrite the target of wikilinks for which `map_target` returns a new target
/// string (heading/alias/embed markers preserved). Fence-aware; returns the new
/// text and the number of links rewritten.
pub fn rewrite_wiki_links<F>(raw: &str, mut map_target: F) -> (String, usize)
where
    F: FnMut(&WikiLink) -> Option<String>,
{
    let (lines, eol, trailing_newline) = split_lines(raw);
    let mut fence: Option<char> = None;
    let mut count = 0;

    let out: Vec<String> = lines
        .into_iter()
        .map(|line| {
            if let Some(caps) = fence_re().captures(&line) {
                let marker = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                let ch = marker.chars().next().unwrap_or('`');
                match fence {
                    None => fence = Some(ch),
                    Some(f) if f == ch => fence = None,
                    _ => {}
                }
                return line;
            }
            if fence.is_some() {
                return line;
            }

            wikilink_re()
                .replace_all(&line, |caps: &Captures| {
                    let whole = caps.get(0).unwrap().as_str();
                    let bang = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                    let inner = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                    let link = parse_inner(inner, bang == "!");
                    let Some(next) = map_target(&link) else {
                        return whole.to_string();
                    };
                    count += 1;
                    let heading = link
                        .heading
                        .as_ref()
                        .map(|h| format!("#{h}"))
                        .unwrap_or_default();
                    let alias = link
                        .alias
                        .as_ref()
                        .map(|a| format!("|{a}"))
                        .unwrap_or_default();
                    format!("{bang}[[{next}{heading}{alias}]]")
                })
                .to_string()
        })
        .collect();

    (join_lines(&out, eol, trailing_newline), count)
}

fn parse_inner(inner: &str, embed: bool) -> WikiLink {
    let raw = inner.trim().to_string();
    let mut rest = raw.as_str();
    let mut alias = None;
    if let Some(pipe) = rest.find('|') {
        alias = Some(rest[pipe + 1..].trim().to_string());
        rest = &rest[..pipe];
    }
    let mut heading = None;
    if let Some(hash) = rest.find('#') {
        let h = rest[hash + 1..].trim();
        if !h.is_empty() {
            heading = Some(h.to_string());
        }
        rest = &rest[..hash];
    }
    let trimmed = rest.trim();
    let target = if trimmed.to_lowercase().ends_with(".md") {
        trimmed[..trimmed.len() - 3].to_string()
    } else {
        trimmed.to_string()
    };
    WikiLink {
        raw,
        target,
        heading,
        alias,
        embed,
    }
}

/// Resolve a wikilink target to a vault-relative path.
pub fn resolve_link_target(
    target: &str,
    known_paths: &[String],
    basename_index: &HashMap<String, Vec<String>>,
) -> Option<String> {
    if target.is_empty() {
        return None;
    }
    let normalized = target.strip_prefix("./").unwrap_or(target);

    if normalized.contains('/') {
        let with_ext = if normalized.to_lowercase().ends_with(".md") {
            normalized.to_string()
        } else {
            format!("{normalized}.md")
        };
        let lower = with_ext.to_lowercase();
        return known_paths
            .iter()
            .find(|p| p.to_lowercase() == lower)
            .cloned();
    }

    let key = normalized.to_lowercase();
    basename_index.get(&key).and_then(|m| m.first()).cloned()
}

/// Build a basename (lowercased, no extension) -> paths index for link resolution.
pub fn build_basename_index(paths: &[String]) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for p in paths {
        let base_part = match p.rfind('/') {
            Some(idx) => &p[idx + 1..],
            None => p.as_str(),
        };
        let base_no_ext = if base_part.to_lowercase().ends_with(".md") {
            &base_part[..base_part.len() - 3]
        } else {
            base_part
        };
        let key = base_no_ext.to_lowercase();
        index.entry(key).or_default().push(p.clone());
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_various_wikilinks_and_ignores_code() {
        let raw = [
            "See [[beta]] and [[plans/gamma|Gamma]] and [[delta#Section]].",
            "Embed: ![[image.md]]",
            "Inline `[[not-a-link]]` stays.",
            "```",
            "[[also-not]]",
            "```",
        ]
        .join("\n");
        let links = parse_wiki_links(&raw);
        let targets: Vec<&str> = links.iter().map(|l| l.target.as_str()).collect();
        assert!(targets.contains(&"beta"));
        assert!(targets.contains(&"plans/gamma"));
        assert!(targets.contains(&"delta"));
        assert!(targets.contains(&"image"));
        assert!(!targets.contains(&"not-a-link"));
        assert!(!targets.contains(&"also-not"));

        let gamma = links.iter().find(|l| l.target == "plans/gamma").unwrap();
        assert_eq!(gamma.alias.as_deref(), Some("Gamma"));
        let delta = links.iter().find(|l| l.target == "delta").unwrap();
        assert_eq!(delta.heading.as_deref(), Some("Section"));
        assert!(links.iter().find(|l| l.target == "image").unwrap().embed);
    }

    #[test]
    fn captures_frontmatter_wikilinks_and_rewrites() {
        let raw = [
            "---",
            "status: active",
            "related:",
            "  - \"[[parent-note]]\"",
            "---",
            "",
            "# Body",
            "",
        ]
        .join("\n");
        let targets: Vec<String> = parse_wiki_links(&raw).into_iter().map(|l| l.target).collect();
        assert!(targets.contains(&"parent-note".to_string()));

        let raw2 = "Link to [[old-note]] and [[old-note#Goals|the goals]] plus [[keep-me]].";
        let (text, count) = rewrite_wiki_links(raw2, |link| {
            if link.target.to_lowercase() == "old-note" {
                Some("new-note".to_string())
            } else {
                None
            }
        });
        assert_eq!(count, 2);
        assert!(text.contains("[[new-note]]"));
        assert!(text.contains("[[new-note#Goals|the goals]]"));
        assert!(text.contains("[[keep-me]]"));

        let raw3 = ["[[old]]", "```", "[[old]]", "```"].join("\n");
        let (text3, count3) = rewrite_wiki_links(&raw3, |_| Some("new".to_string()));
        assert_eq!(count3, 1);
        assert_eq!(text3, ["[[new]]", "```", "[[old]]", "```"].join("\n"));
    }
}
