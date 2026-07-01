import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { AxiError } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { flagBool, flagString, parseArgs, resolveContent } from "../args.js";
import { normalizeNotePath, resolveInVault } from "../vault.js";
import { appendToDoc, insertAtHeading, prependToDoc, SectionError } from "../markdown/sections.js";

export async function insertCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
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
  const content = resolveContent(flags);

  const before = flagString(flags, "before-heading");
  const after = flagString(flags, "after-heading");

  let out: string;
  let where: string;
  try {
    if (before !== undefined) {
      out = insertAtHeading(raw, before, "before-heading", content);
      where = `before "${before}"`;
    } else if (after !== undefined) {
      out = insertAtHeading(raw, after, "after-heading", content);
      where = `after "${after}"`;
    } else if (flagBool(flags, "prepend")) {
      out = prependToDoc(raw, content);
      where = "document start";
    } else if (flagBool(flags, "append")) {
      out = appendToDoc(raw, content);
      where = "document end";
    } else {
      throw usage("Choose --before-heading, --after-heading, --append, or --prepend");
    }
  } catch (e) {
    if (e instanceof SectionError) {
      const code = e.kind === "not_found" ? "NOT_FOUND" : "VALIDATION_ERROR";
      throw new AxiError(e.message, code, [`vault read ${notePath} --outline`]);
    }
    throw e;
  }

  if (out === raw) return { ok: true, changed: false, file: notePath, note: "Empty content, nothing inserted" };
  writeFileSync(abs, out, "utf8");
  await c.sync({ withEmbeddings: false });
  return { ok: true, changed: true, file: notePath, inserted: where };
}

function usage(message: string): AxiError {
  return new AxiError(message, "VALIDATION_ERROR", [
    'vault insert <file> --after-heading "<h>" --content "..."',
    'vault insert <file> --before-heading "<h>" --content "..."',
    'vault insert <file> --append --content "..."   (or --prepend)',
  ]);
}
