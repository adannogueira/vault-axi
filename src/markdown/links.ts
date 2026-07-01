import { joinLines, parseDoc, splitLines } from "./sections.js";

export interface WikiLink {
  /** Raw target as written, e.g. "plans/foo" or "foo.md" or "Foo#Heading". */
  raw: string;
  /** Target after stripping #heading and |alias, and the .md extension. */
  target: string;
  heading: string | null;
  alias: string | null;
  /** True for embeds (`![[...]]`). */
  embed: boolean;
}

const WIKILINK_RE = /(!?)\[\[([^\]]+)\]\]/g;
const FENCE_RE = /^([ \t]{0,3})(`{3,}|~{3,})/;
const INLINE_CODE_RE = /`[^`]*`/g;

/** Parse all [[wikilinks]] from markdown, ignoring fenced code and inline code spans. */
export function parseWikiLinks(raw: string): WikiLink[] {
  const { lines } = splitLines(raw);
  const doc = parseDoc(raw);
  const fmEnd = doc.frontmatter ? doc.frontmatter.endLine : -1;

  const links: WikiLink[] = [];
  let fence: string | null = null;

  for (let i = 0; i < lines.length; i++) {
    if (i <= fmEnd) continue;
    const line = lines[i] ?? "";

    const fm = FENCE_RE.exec(line);
    if (fm) {
      const marker = fm[2]![0]!;
      if (fence === null) fence = marker;
      else if (marker === fence) fence = null;
      continue;
    }
    if (fence !== null) continue;

    const scrubbed = line.replace(INLINE_CODE_RE, (m) => " ".repeat(m.length));
    WIKILINK_RE.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = WIKILINK_RE.exec(scrubbed)) !== null) {
      const embed = m[1] === "!";
      const inner = m[2] ?? "";
      links.push(parseInner(inner, embed));
    }
  }
  return links;
}

/**
 * Rewrite the target of wikilinks for which `mapTarget` returns a new target
 * string (heading/alias/embed markers preserved). Fence-aware; returns the new
 * text and the number of links rewritten.
 */
export function rewriteWikiLinks(
  raw: string,
  mapTarget: (link: WikiLink) => string | null,
): { text: string; count: number } {
  const { lines, eol, trailingNewline } = splitLines(raw);
  let fence: string | null = null;
  let count = 0;

  const out = lines.map((line) => {
    const fm = FENCE_RE.exec(line);
    if (fm) {
      const marker = fm[2]![0]!;
      if (fence === null) fence = marker;
      else if (marker === fence) fence = null;
      return line;
    }
    if (fence !== null) return line;

    WIKILINK_RE.lastIndex = 0;
    return line.replace(WIKILINK_RE, (whole, bang: string, inner: string) => {
      const link = parseInner(inner, bang === "!");
      const next = mapTarget(link);
      if (next === null) return whole;
      count++;
      const heading = link.heading ? `#${link.heading}` : "";
      const alias = link.alias ? `|${link.alias}` : "";
      return `${bang}[[${next}${heading}${alias}]]`;
    });
  });

  return { text: joinLines(out, eol, trailingNewline), count };
}

function parseInner(inner: string, embed: boolean): WikiLink {
  const raw = inner.trim();
  let rest = raw;
  let alias: string | null = null;
  const pipe = rest.indexOf("|");
  if (pipe !== -1) {
    alias = rest.slice(pipe + 1).trim();
    rest = rest.slice(0, pipe);
  }
  let heading: string | null = null;
  const hash = rest.indexOf("#");
  if (hash !== -1) {
    heading = rest.slice(hash + 1).trim() || null;
    rest = rest.slice(0, hash);
  }
  const target = rest.trim().replace(/\.md$/i, "");
  return { raw, target, heading, alias, embed };
}

/**
 * Resolve a wikilink target to a vault-relative path.
 * - A target containing "/" is treated as a path from the vault root.
 * - A bare name resolves by basename against the known file set (Obsidian style).
 * Returns null when nothing matches (a dangling link).
 */
export function resolveLinkTarget(
  target: string,
  knownPaths: readonly string[],
  basenameIndex: Map<string, string[]>,
): string | null {
  if (!target) return null;
  const normalized = target.replace(/^\.\//, "");

  if (normalized.includes("/")) {
    const withExt = normalized.toLowerCase().endsWith(".md") ? normalized : `${normalized}.md`;
    const hit = knownPaths.find((p) => p.toLowerCase() === withExt.toLowerCase());
    return hit ?? null;
  }

  const key = normalized.toLowerCase();
  const matches = basenameIndex.get(key);
  if (matches && matches.length > 0) return matches[0]!;
  return null;
}

/** Build a basename (lowercased, no extension) -> paths index for link resolution. */
export function buildBasenameIndex(paths: readonly string[]): Map<string, string[]> {
  const index = new Map<string, string[]>();
  for (const p of paths) {
    const base = p
      .slice(p.lastIndexOf("/") + 1)
      .replace(/\.md$/i, "")
      .toLowerCase();
    const list = index.get(base) ?? [];
    list.push(p);
    index.set(base, list);
  }
  return index;
}
