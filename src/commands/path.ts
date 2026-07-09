import { existsSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import type { Ctx } from "../context.js";
import { parseArgs } from "../args.js";
import { normalizeNotePath, resolveInVault } from "../vault.js";

/**
 * Resolve the absolute filesystem path for a note (creating its parent folders)
 * so an agent can write the note directly with its own file tools,. The vault
 * indexes it automatically on the next read. With no argument, prints the vault
 * root.
 */
export async function pathCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  const root = c.requireVault();
  const { positionals } = parseArgs(args);
  const rel = positionals[0];

  if (!rel) {
    return {
      vault: root,
      hint: [
        "Write .md files anywhere under this folder; vault indexes them on the next read/search",
        "vault path <note> resolves and prepares a specific note's path",
      ],
    };
  }

  const notePath = normalizeNotePath(rel);
  const abs = resolveInVault(root, notePath);
  mkdirSync(dirname(abs), { recursive: true });
  const exists = existsSync(abs);

  return {
    file: notePath,
    path: abs,
    exists,
    indexing: "automatic on next `vault` read/search",
    next: exists
      ? [
          `Note exists - for a partial change use \`vault update ${notePath} --section "<h>"\``,
          `To overwrite, write your file tool directly to: ${abs}`,
        ]
      : [`Write your Markdown directly to: ${abs}`, `Then \`vault read ${notePath} --outline\``],
  };
}
