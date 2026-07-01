import YAML from "yaml";

const FRONTMATTER_RE = /^---\r?\n([\s\S]*?)\r?\n(?:---|\.\.\.)[ \t]*(\r?\n|$)/;

export interface FrontmatterResult {
  data: Record<string, unknown> | null;
  /** The raw YAML text between the fences, if any. */
  rawBlock: string | null;
}

/** Parse a leading YAML frontmatter block. Returns null data when absent. */
export function readFrontmatter(raw: string): FrontmatterResult {
  const m = FRONTMATTER_RE.exec(raw);
  if (!m) return { data: null, rawBlock: null };
  const rawBlock = m[1] ?? "";
  try {
    const parsed = YAML.parse(rawBlock);
    const data =
      parsed && typeof parsed === "object" && !Array.isArray(parsed)
        ? (parsed as Record<string, unknown>)
        : {};
    return { data, rawBlock };
  } catch {
    return { data: {}, rawBlock };
  }
}

/** Interpret a CLI value string as a typed YAML scalar, falling back to string. */
export function coerceValue(value: string): unknown {
  try {
    const parsed = YAML.parse(value);
    return parsed === undefined ? value : parsed;
  } catch {
    return value;
  }
}

function detectEol(raw: string): "\n" | "\r\n" {
  return raw.includes("\r\n") ? "\r\n" : "\n";
}

/**
 * Set (or create) a single frontmatter field, preserving other fields, comments,
 * and formatting via the YAML document model. Only the frontmatter block is
 * rewritten; the body is untouched.
 */
export function setFrontmatterField(raw: string, key: string, value: unknown): string {
  const eol = detectEol(raw);
  const m = FRONTMATTER_RE.exec(raw);

  if (m) {
    const doc = YAML.parseDocument(m[1] ?? "");
    doc.set(key, value);
    let yamlText = doc.toString();
    yamlText = yamlText.replace(/\n$/, "");
    if (eol === "\r\n") yamlText = yamlText.replace(/\n/g, "\r\n");
    const rest = raw.slice(m[0].length);
    return `---${eol}${yamlText}${eol}---${eol}${rest}`;
  }

  const doc = new YAML.Document({});
  doc.set(key, value);
  let yamlText = doc.toString().replace(/\n$/, "");
  if (eol === "\r\n") yamlText = yamlText.replace(/\n/g, "\r\n");
  const sep = raw.length > 0 ? eol : "";
  return `---${eol}${yamlText}${eol}---${eol}${sep}${raw}`;
}

/** Remove a frontmatter field. No-op if the field or block is absent. */
export function unsetFrontmatterField(raw: string, key: string): string {
  const eol = detectEol(raw);
  const m = FRONTMATTER_RE.exec(raw);
  if (!m) return raw;
  const doc = YAML.parseDocument(m[1] ?? "");
  if (!doc.has(key)) return raw;
  doc.delete(key);
  let yamlText = doc.toString().replace(/\n$/, "");
  if (eol === "\r\n") yamlText = yamlText.replace(/\n/g, "\r\n");
  const rest = raw.slice(m[0].length);
  return `---${eol}${yamlText}${eol}---${eol}${rest}`;
}
