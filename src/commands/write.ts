import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { AxiError } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { flagBool, parseArgs, resolveContent } from "../args.js";
import { normalizeNotePath, resolveInVault } from "../vault.js";

export async function writeCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  const root = c.requireVault();
  const { positionals, flags } = parseArgs(args);
  const rel = positionals[0];
  if (!rel) {
    throw new AxiError("Missing file path", "VALIDATION_ERROR", [
      'Usage: vault write <file> --content "..."  (or --content - to read stdin)',
    ]);
  }
  const notePath = normalizeNotePath(rel);
  const abs = resolveInVault(root, notePath);
  const existed = existsSync(abs);
  if (existed && !flagBool(flags, "force")) {
    throw new AxiError(`Note already exists: ${notePath}`, "VALIDATION_ERROR", [
      "Pass --force to overwrite, or use `vault update` for a surgical edit",
    ]);
  }
  let content = resolveContent(flags);
  if (!content.endsWith("\n")) content += "\n";

  mkdirSync(dirname(abs), { recursive: true });
  writeFileSync(abs, content, "utf8");
  await c.sync({ withEmbeddings: false });

  return {
    ok: true,
    action: existed ? "overwritten" : "created",
    file: notePath,
    chars: content.length,
    next: [`vault read ${notePath} --outline`, `vault links ${notePath}`],
  };
}
