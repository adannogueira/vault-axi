import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { AxiError } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { flagString, parseArgs, resolveContent, type ParsedArgs } from "../args.js";
import { normalizeNotePath, resolveInVault } from "../vault.js";
import { editSection, replaceAnchor, SectionError, type SectionMode } from "../markdown/sections.js";
import { coerceValue, setFrontmatterField, unsetFrontmatterField } from "../markdown/frontmatter.js";

const MODES: SectionMode[] = ["replace", "append", "prepend"];

export async function updateCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  const root = c.requireVault();
  const { positionals, flags } = parseArgs(args);
  const rel = positionals[0];
  if (!rel) throw usage("Missing file path");
  const notePath = normalizeNotePath(rel);
  const abs = resolveInVault(root, notePath);
  if (!existsSync(abs)) {
    throw new AxiError(`Note not found: ${notePath}`, "NOT_FOUND", ["Use `vault write` to create it"]);
  }
  const raw = readFileSync(abs, "utf8");

  let out: string;
  let summary: Record<string, unknown>;

  const section = flagString(flags, "section");
  const frontmatter = collectFrontmatter(flags);
  const anchor = flagString(flags, "anchor");

  try {
    if (section !== undefined) {
      const modeRaw = (flagString(flags, "mode") ?? "replace") as SectionMode;
      if (!MODES.includes(modeRaw)) throw usage(`Invalid --mode "${modeRaw}" (replace|append|prepend)`);
      const content = resolveContent(flags);
      out = editSection(raw, section, modeRaw, content);
      summary = { edit: "section", section, mode: modeRaw };
    } else if (frontmatter.length > 0) {
      out = raw;
      const changed: string[] = [];
      for (const { key, value } of frontmatter) {
        out = value === null ? unsetFrontmatterField(out, key) : setFrontmatterField(out, key, coerceValue(value));
        changed.push(key);
      }
      summary = { edit: "frontmatter", fields: changed };
    } else if (anchor !== undefined) {
      const replacement = flagString(flags, "replace");
      if (replacement === undefined) throw usage("--anchor requires --replace <text>");
      out = replaceAnchor(raw, anchor, replacement);
      summary = { edit: "anchor" };
    } else {
      throw usage("Nothing to update");
    }
  } catch (e) {
    if (e instanceof SectionError) {
      const code = e.kind === "not_found" ? "NOT_FOUND" : "VALIDATION_ERROR";
      throw new AxiError(e.message, code, [`vault read ${notePath} --outline`]);
    }
    throw e;
  }

  if (out === raw) {
    return { ok: true, changed: false, file: notePath, note: "No change (content already matched)", ...summary };
  }
  writeFileSync(abs, out, "utf8");
  await c.sync({ withEmbeddings: false });
  return { ok: true, changed: true, file: notePath, ...summary };
}

function collectFrontmatter(flags: ParsedArgs["flags"]): { key: string; value: string | null }[] {
  const raw = flags["frontmatter"];
  if (raw === undefined || raw === true) return [];
  const items = Array.isArray(raw) ? raw : [String(raw)];
  return items.map((item) => {
    const eq = item.indexOf("=");
    if (eq === -1) return { key: item.trim(), value: null }; // key with no value => unset
    return { key: item.slice(0, eq).trim(), value: item.slice(eq + 1) };
  });
}

function usage(message: string): AxiError {
  return new AxiError(message, "VALIDATION_ERROR", [
    'vault update <file> --section "<heading>" --mode replace|append|prepend --content "..."',
    'vault update <file> --frontmatter status=active [--frontmatter tags="[a, b]"]',
    'vault update <file> --anchor "exact text" --replace "new text"',
  ]);
}
