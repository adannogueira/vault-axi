import { existsSync, readFileSync } from "node:fs";
import { AxiError } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { flagBool, flagString, parseArgs } from "../args.js";
import { normalizeNotePath, resolveInVault } from "../vault.js";
import { outline, parseDoc, resolveSection, SectionError, splitLines } from "../markdown/sections.js";
import { readFrontmatter } from "../markdown/frontmatter.js";
import { truncateText } from "../render.js";

export async function readCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  const root = c.requireVault();
  const { positionals, flags } = parseArgs(args);
  const rel = positionals[0];
  if (!rel) {
    throw new AxiError("Missing file path", "VALIDATION_ERROR", ["Usage: vault read <file> [--outline|--section <h>|--frontmatter|--lines a-b|--full]"]);
  }
  const notePath = normalizeNotePath(rel);
  const abs = resolveInVault(root, notePath);
  if (!existsSync(abs)) {
    throw new AxiError(`Note not found: ${notePath}`, "NOT_FOUND", ["Run `vault ls` to list notes, or `vault search <query>`"]);
  }
  const raw = readFileSync(abs, "utf8");
  const full = flagBool(flags, "full");

  if (flagBool(flags, "outline")) {
    const entries = outline(parseDoc(raw));
    if (entries.length === 0) return { file: notePath, outline: "(no headings)" };
    return { file: notePath, headings: entries.length, outline: entries };
  }

  if (flagBool(flags, "frontmatter")) {
    const { data } = readFrontmatter(raw);
    return { file: notePath, frontmatter: data ?? "(none)" };
  }

  const lines = flagString(flags, "lines");
  if (lines) {
    const m = /^(\d+)-(\d+)$/.exec(lines.trim());
    if (!m) throw new AxiError("Invalid --lines range (use a-b)", "VALIDATION_ERROR");
    const { lines: all, eol } = splitLines(raw);
    const a = Math.max(1, Number.parseInt(m[1]!, 10));
    const b = Math.min(all.length, Number.parseInt(m[2]!, 10));
    const slice = all.slice(a - 1, b).join(eol);
    return { file: notePath, lines: `${a}-${b}`, content: slice };
  }

  const section = flagString(flags, "section");
  if (section) {
    try {
      const s = resolveSection(parseDoc(raw), section);
      const t = full ? { content: s.text, truncated: false, totalChars: s.text.length } : truncateText(s.text);
      return {
        file: notePath,
        section: s.heading.path.join(" / "),
        lines: `${s.headingLine + 1}-${s.bodyEnd}`,
        ...t,
        ...(t.truncated ? { hint: "Pass --full for the whole section" } : {}),
      };
    } catch (e) {
      if (e instanceof SectionError) {
        const code = e.kind === "not_found" ? "NOT_FOUND" : "VALIDATION_ERROR";
        throw new AxiError(e.message, code, ["Run `vault read " + notePath + " --outline` to see headings"]);
      }
      throw e;
    }
  }

  const doc = parseDoc(raw);
  const t = full ? { content: raw, truncated: false, totalChars: raw.length } : truncateText(raw);
  return {
    file: notePath,
    headings: doc.headings.length,
    ...t,
    ...(t.truncated
      ? { hint: `Use --outline for structure, --section <h> for one part, or --full for everything` }
      : {}),
  };
}
