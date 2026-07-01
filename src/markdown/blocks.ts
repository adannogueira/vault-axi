import { parseDoc } from "./sections.js";

export interface Block {
  /** " > "-joined heading path, or "(preamble)" for content before the first heading. */
  headingPath: string;
  /** 1-based inclusive start line. */
  startLine: number;
  /** 1-based inclusive end line. */
  endLine: number;
  /** Heading title + own content (excludes nested subsections). */
  text: string;
}

/**
 * Split a document into heading-granular blocks (each heading owns the text up to
 * the next heading of ANY level; nested subsections become their own blocks).
 * Mirrors the block-level granularity Smart Connections uses.
 */
export function toBlocks(raw: string): Block[] {
  const doc = parseDoc(raw);
  const { lines, headings } = doc;
  const blocks: Block[] = [];
  const bodyStart = doc.frontmatter ? doc.frontmatter.endLine + 1 : 0;

  const firstHeadingLine = headings.length > 0 ? headings[0]!.line : lines.length;
  const preamble = lines.slice(bodyStart, firstHeadingLine).join("\n").trim();
  if (preamble.length > 0) {
    blocks.push({
      headingPath: "(preamble)",
      startLine: bodyStart + 1,
      endLine: firstHeadingLine,
      text: preamble,
    });
  }

  for (let i = 0; i < headings.length; i++) {
    const h = headings[i]!;
    const end = i + 1 < headings.length ? headings[i + 1]!.line : lines.length;
    const own = lines.slice(h.line, end).join("\n").trim();
    blocks.push({
      headingPath: h.path.join(" > "),
      startLine: h.line + 1,
      endLine: end,
      text: own,
    });
  }

  return blocks;
}
