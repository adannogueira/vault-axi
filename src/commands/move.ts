import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { AxiError } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { flagBool, parseArgs } from "../args.js";
import { normalizeNotePath, resolveInVault } from "../vault.js";
import { rewriteWikiLinks, type WikiLink } from "../markdown/links.js";

function baseNoExt(path: string): string {
  return path.slice(path.lastIndexOf("/") + 1).replace(/\.md$/i, "");
}

export async function moveCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  const root = c.requireVault();
  const { positionals, flags } = parseArgs(args);
  const [oldRaw, newRaw] = positionals;
  if (!oldRaw || !newRaw) {
    throw new AxiError("Usage: vault move <old> <new>", "VALIDATION_ERROR", ["Rewrites [[wikilinks]] across the vault"]);
  }
  const oldPath = normalizeNotePath(oldRaw);
  const newPath = normalizeNotePath(newRaw);
  const oldAbs = resolveInVault(root, oldPath);
  const newAbs = resolveInVault(root, newPath);

  if (!existsSync(oldAbs)) throw new AxiError(`Note not found: ${oldPath}`, "NOT_FOUND");
  if (existsSync(newAbs) && !flagBool(flags, "force")) {
    throw new AxiError(`Target already exists: ${newPath}`, "VALIDATION_ERROR", ["Pass --force to overwrite"]);
  }
  if (oldPath === newPath) return { ok: true, changed: false, note: "Source and target are identical" };

  await c.sync({ withEmbeddings: false });
  const db = c.db();

  const referrers = (
    db.prepare("SELECT DISTINCT src_path AS p FROM links WHERE target_path = ? AND src_path != ?").all(oldPath, oldPath) as {
      p: string;
    }[]
  ).map((r) => r.p);

  const oldBase = baseNoExt(oldPath).toLowerCase();
  const oldFull = oldPath.replace(/\.md$/i, "").toLowerCase();
  const newBase = baseNoExt(newPath);
  const newFull = newPath.replace(/\.md$/i, "");

  const matchesOld = (link: WikiLink): boolean => {
    const t = link.target.replace(/^\.\//, "").toLowerCase();
    return t === oldBase || t === oldFull;
  };

  // Move the file first, then rewrite referrers.
  mkdirSync(dirname(newAbs), { recursive: true });
  renameSync(oldAbs, newAbs);

  let rewrittenLinks = 0;
  const touched: string[] = [];
  for (const ref of referrers) {
    const abs = resolveInVault(root, ref);
    if (!existsSync(abs)) continue;
    const raw = readFileSync(abs, "utf8");
    const { text, count } = rewriteWikiLinks(raw, (link) => {
      if (!matchesOld(link)) return null;
      // Preserve the referrer's original style: path-style if it used a slash.
      return link.target.includes("/") ? newFull : newBase;
    });
    if (count > 0 && text !== raw) {
      writeFileSync(abs, text, "utf8");
      rewrittenLinks += count;
      touched.push(ref);
    }
  }

  await c.sync({ withEmbeddings: false });

  return {
    ok: true,
    changed: true,
    from: oldPath,
    to: newPath,
    referrers: referrers.length,
    linksRewritten: rewrittenLinks,
    filesTouched: touched.length ? touched : "(none)",
  };
}
