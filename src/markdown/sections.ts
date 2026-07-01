/**
 * Heading-aware markdown surgery. Operations edit the raw text by splicing
 * line ranges derived from a heading scan — the document is never re-serialized
 * through a parser, so formatting outside the touched range is preserved exactly.
 *
 * Fenced code blocks (``` and ~~~) and a leading YAML frontmatter block are
 * skipped when detecting `#` headings.
 */

export interface Heading {
  /** ATX level, 1-6. */
  level: number;
  /** Heading text with leading #'s and any trailing closing #'s removed. */
  title: string;
  /** 0-based index into `lines` of the heading line. */
  line: number;
  /** Ancestor titles from the document root down to and including this heading. */
  path: string[];
}

export interface Frontmatter {
  /** 0-based line of the opening `---`. */
  startLine: number;
  /** 0-based line of the closing `---`. */
  endLine: number;
}

export interface ParsedDoc {
  eol: "\n" | "\r\n";
  trailingNewline: boolean;
  lines: string[];
  frontmatter: Frontmatter | null;
  headings: Heading[];
}

export interface Section {
  heading: Heading;
  /** Line of the heading itself. */
  headingLine: number;
  /** First line of the body (headingLine + 1). */
  bodyStart: number;
  /** Exclusive end of the body: the next heading of equal/higher level, or EOF. */
  bodyEnd: number;
  /** Body text (between heading and next boundary), without the heading line. */
  body: string;
  /** Whole section text including the heading line. */
  text: string;
}

const HEADING_RE = /^(#{1,6})[ \t]+(.*)$/;
const FENCE_RE = /^([ \t]{0,3})(`{3,}|~{3,})(.*)$/;

function detectEol(raw: string): "\n" | "\r\n" {
  return raw.includes("\r\n") ? "\r\n" : "\n";
}

export function splitLines(raw: string): {
  lines: string[];
  eol: "\n" | "\r\n";
  trailingNewline: boolean;
} {
  const eol = detectEol(raw);
  const trailingNewline = /\r?\n$/.test(raw);
  const lines = raw.split(/\r?\n/);
  if (trailingNewline) lines.pop(); // drop the artificial empty element
  return { lines, eol, trailingNewline };
}

export function joinLines(lines: string[], eol: "\n" | "\r\n", trailingNewline: boolean): string {
  return lines.join(eol) + (trailingNewline ? eol : "");
}

function normalizeTitle(text: string): string {
  return text.replace(/[ \t]+#+[ \t]*$/, "").trim();
}

export function parseDoc(raw: string): ParsedDoc {
  const { lines, eol, trailingNewline } = splitLines(raw);

  let frontmatter: Frontmatter | null = null;
  let scanStart = 0;
  if (lines[0] === "---") {
    for (let i = 1; i < lines.length; i++) {
      const line = lines[i];
      if (line === "---" || line === "...") {
        frontmatter = { startLine: 0, endLine: i };
        scanStart = i + 1;
        break;
      }
    }
  }

  const headings: Heading[] = [];
  const stack: Heading[] = [];
  let fence: string | null = null;

  for (let i = scanStart; i < lines.length; i++) {
    const line = lines[i] ?? "";

    const fenceMatch = FENCE_RE.exec(line);
    if (fenceMatch) {
      const marker = fenceMatch[2] ?? "";
      if (fence === null) {
        fence = marker[0] === "`" ? "`" : "~";
      } else if (marker[0] === fence) {
        fence = null;
      }
      continue;
    }
    if (fence !== null) continue;

    const m = HEADING_RE.exec(line);
    if (!m) continue;

    const level = m[1]!.length;
    const title = normalizeTitle(m[2] ?? "");
    while (stack.length > 0 && stack[stack.length - 1]!.level >= level) {
      stack.pop();
    }
    const path = [...stack.map((h) => h.title), title];
    const heading: Heading = { level, title, line: i, path };
    headings.push(heading);
    stack.push(heading);
  }

  return { eol, trailingNewline, lines, frontmatter, headings };
}

function segments(path: string): string[] {
  return path
    .split(/[/]|::/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

/** All headings whose ancestor chain ends with the given path segments (case-insensitive). */
export function findHeadings(doc: ParsedDoc, path: string): Heading[] {
  const segs = segments(path).map((s) => s.toLowerCase());
  if (segs.length === 0) return [];
  return doc.headings.filter((h) => {
    if (segs.length > h.path.length) return false;
    const tail = h.path.slice(h.path.length - segs.length).map((s) => s.toLowerCase());
    return tail.every((t, idx) => t === segs[idx]);
  });
}

function sectionBounds(doc: ParsedDoc, heading: Heading): { bodyStart: number; bodyEnd: number } {
  const idx = doc.headings.indexOf(heading);
  const bodyStart = heading.line + 1;
  let bodyEnd = doc.lines.length;
  for (let j = idx + 1; j < doc.headings.length; j++) {
    if (doc.headings[j]!.level <= heading.level) {
      bodyEnd = doc.headings[j]!.line;
      break;
    }
  }
  return { bodyStart, bodyEnd };
}

export function getSection(doc: ParsedDoc, heading: Heading): Section {
  const { bodyStart, bodyEnd } = sectionBounds(doc, heading);
  const body = doc.lines.slice(bodyStart, bodyEnd).join(doc.eol);
  const text = doc.lines.slice(heading.line, bodyEnd).join(doc.eol);
  return { heading, headingLine: heading.line, bodyStart, bodyEnd, body, text };
}

export class SectionError extends Error {
  constructor(
    message: string,
    readonly kind: "not_found" | "ambiguous",
  ) {
    super(message);
    this.name = "SectionError";
  }
}

/** Resolve exactly one section for a heading path, or throw a SectionError. */
export function resolveSection(doc: ParsedDoc, path: string): Section {
  const matches = findHeadings(doc, path);
  if (matches.length === 0) {
    throw new SectionError(`No heading matches "${path}"`, "not_found");
  }
  if (matches.length > 1) {
    const where = matches.map((h) => h.path.join(" / ")).join("; ");
    throw new SectionError(
      `Heading "${path}" is ambiguous (${matches.length} matches: ${where}). Use a fuller path like "Parent/Child".`,
      "ambiguous",
    );
  }
  return getSection(doc, matches[0]!);
}

function toBlockLines(text: string): string[] {
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  while (lines.length > 0 && lines[0]!.trim() === "") lines.shift();
  while (lines.length > 0 && lines[lines.length - 1]!.trim() === "") lines.pop();
  return lines;
}

function splice(lines: string[], start: number, end: number, insert: string[]): string[] {
  return [...lines.slice(0, start), ...insert, ...lines.slice(end)];
}

export type SectionMode = "replace" | "append" | "prepend";

/**
 * Edit the body of a section in place. Never touches the heading line itself, so
 * a `replace` on `## Design` swaps its body without duplicating the heading.
 */
export function editSection(raw: string, path: string, mode: SectionMode, content: string): string {
  const doc = parseDoc(raw);
  const section = resolveSection(doc, path);
  const block = toBlockLines(content);
  const { bodyStart, bodyEnd } = section;
  const hasFollowingHeading = bodyEnd < doc.lines.length;

  let out: string[];
  if (mode === "replace") {
    const insert = block.length > 0 && hasFollowingHeading ? [...block, ""] : block;
    out = splice(doc.lines, bodyStart, bodyEnd, insert);
  } else if (mode === "prepend") {
    let firstContent = bodyStart;
    while (firstContent < bodyEnd && doc.lines[firstContent]!.trim() === "") firstContent++;
    const insert = block.length > 0 ? [...block, ""] : [];
    out = splice(doc.lines, bodyStart, firstContent, insert);
  } else {
    // append: place after the last non-blank body line
    let lastContent = bodyEnd;
    while (lastContent > bodyStart && doc.lines[lastContent - 1]!.trim() === "") lastContent--;
    const hadBody = lastContent > bodyStart;
    const lead = hadBody ? [""] : [];
    const trail = hasFollowingHeading ? [""] : [];
    const insert = block.length > 0 ? [...lead, ...block, ...trail] : [];
    out = splice(doc.lines, lastContent, bodyEnd, insert);
  }

  return joinLines(out, doc.eol, doc.trailingNewline);
}

export type HeadingAnchor = "before-heading" | "after-heading";

/** Insert a fresh block immediately before or after a heading line. */
export function insertAtHeading(raw: string, path: string, where: HeadingAnchor, content: string): string {
  const doc = parseDoc(raw);
  const section = resolveSection(doc, path);
  const block = toBlockLines(content);
  if (block.length === 0) return raw;

  let out: string[];
  if (where === "before-heading") {
    out = splice(doc.lines, section.headingLine, section.headingLine, [...block, ""]);
  } else {
    out = splice(doc.lines, section.bodyStart, section.bodyStart, [...block, ""]);
  }
  return joinLines(out, doc.eol, doc.trailingNewline);
}

/** Append a block to the end of the document. */
export function appendToDoc(raw: string, content: string): string {
  const doc = parseDoc(raw);
  const block = toBlockLines(content);
  if (block.length === 0) return raw;
  let end = doc.lines.length;
  while (end > 0 && doc.lines[end - 1]!.trim() === "") end--;
  const lead = end > 0 ? [""] : [];
  const out = splice(doc.lines, end, doc.lines.length, [...lead, ...block]);
  return joinLines(out, doc.eol, doc.trailingNewline);
}

/** Prepend a block to the document, after the frontmatter block if present. */
export function prependToDoc(raw: string, content: string): string {
  const doc = parseDoc(raw);
  const block = toBlockLines(content);
  if (block.length === 0) return raw;
  const start = doc.frontmatter ? doc.frontmatter.endLine + 1 : 0;
  let firstContent = start;
  while (firstContent < doc.lines.length && doc.lines[firstContent]!.trim() === "") firstContent++;
  const out = splice(doc.lines, start, firstContent, ["", ...block, ""]);
  // collapse a possible leading blank at file top
  while (out.length > 0 && out[0]!.trim() === "" && start === 0) out.shift();
  return joinLines(out, doc.eol, doc.trailingNewline);
}

/** Replace an exact text anchor. Errors unless the anchor appears exactly once. */
export function replaceAnchor(raw: string, anchor: string, replacement: string): string {
  if (anchor.length === 0) {
    throw new SectionError("Anchor text must not be empty", "not_found");
  }
  let count = 0;
  let idx = raw.indexOf(anchor);
  const first = idx;
  while (idx !== -1) {
    count++;
    idx = raw.indexOf(anchor, idx + anchor.length);
  }
  if (count === 0) throw new SectionError(`Anchor text not found: ${JSON.stringify(anchor)}`, "not_found");
  if (count > 1)
    throw new SectionError(
      `Anchor text matched ${count} times; it must be unique. Add surrounding context to disambiguate.`,
      "ambiguous",
    );
  return raw.slice(0, first) + replacement + raw.slice(first + anchor.length);
}

export interface OutlineEntry {
  level: number;
  title: string;
  line: number;
  path: string;
}

export function outline(doc: ParsedDoc): OutlineEntry[] {
  return doc.headings.map((h) => ({
    level: h.level,
    title: h.title,
    line: h.line + 1, // 1-based for humans
    path: h.path.join("/"),
  }));
}
