import { existsSync, rmSync } from "node:fs";
import { AxiError } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { parseArgs } from "../args.js";
import { normalizeNotePath, resolveInVault } from "../vault.js";

export async function deleteCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  const root = c.requireVault();
  const { positionals } = parseArgs(args);
  const rel = positionals[0];
  if (!rel) throw new AxiError("Missing file path", "VALIDATION_ERROR", ["Usage: vault delete <file>"]);
  const notePath = normalizeNotePath(rel);
  const abs = resolveInVault(root, notePath);

  // Idempotent: deleting an absent note is a no-op success.
  if (!existsSync(abs)) {
    return { ok: true, deleted: false, file: notePath, note: "Already absent" };
  }

  await c.sync({ withEmbeddings: false });
  const db = c.db();
  const backlinks = (
    db.prepare("SELECT DISTINCT src_path AS p FROM links WHERE target_path = ? AND src_path != ?").all(notePath, notePath) as {
      p: string;
    }[]
  ).map((r) => r.p);

  rmSync(abs);
  await c.sync({ withEmbeddings: false });

  return {
    ok: true,
    deleted: true,
    file: notePath,
    ...(backlinks.length
      ? { warning: `${backlinks.length} note(s) now have dangling links`, danglingIn: backlinks }
      : {}),
  };
}
