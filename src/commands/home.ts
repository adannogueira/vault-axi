import type { Ctx } from "../context.js";
import { getMeta } from "../db.js";

export async function homeCommand(_args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  if (!c.vaultPath) {
    return {
      status: "not configured",
      next: ["vault setup --vault <path>  # point at your Obsidian vault"],
    };
  }
  const root = c.requireVault();
  await c.sync({ withEmbeddings: false });
  const db = c.db();

  const fileCount = (db.prepare("SELECT COUNT(*) n FROM files").get() as { n: number }).n;
  const blockCount = (db.prepare("SELECT COUNT(*) n FROM blocks").get() as { n: number }).n;
  const embedded = (db.prepare("SELECT COUNT(*) n FROM blocks WHERE embedding IS NOT NULL").get() as { n: number }).n;
  // Blocks under 40 chars are intentionally not embedded, so compare against embeddable ones.
  const embeddable = (db.prepare("SELECT COUNT(*) n FROM blocks WHERE length(text) >= 40").get() as { n: number }).n;
  const linkCount = (db.prepare("SELECT COUNT(*) n FROM links").get() as { n: number }).n;
  const dangling = (db.prepare("SELECT COUNT(*) n FROM links WHERE target_path IS NULL").get() as { n: number }).n;

  const recent = db
    .prepare("SELECT path, title, frontmatter FROM files ORDER BY mtime_ms DESC LIMIT 6")
    .all() as { path: string; title: string | null; frontmatter: string | null }[];

  const recentNotes = recent.map((r) => {
    let status: string | undefined;
    try {
      status = r.frontmatter ? (JSON.parse(r.frontmatter).status as string | undefined) : undefined;
    } catch {
      status = undefined;
    }
    return status ? `${r.path} (${status})` : r.path;
  });

  const lastReconcile = getMeta(db, "last_reconcile");

  return {
    vault: root,
    notes: fileCount,
    blocks: blockCount,
    links: linkCount,
    danglingLinks: dangling,
    embeddings:
      embedded >= embeddable
        ? `complete (${embedded} blocks)`
        : `${embedded}/${embeddable} (run \`vault reindex\`)`,
    lastIndexed: lastReconcile ? new Date(Number(lastReconcile)).toISOString() : "never",
    recent: recentNotes,
    next: [
      'vault search "<query>"',
      "vault read <file> --outline",
      "vault related <file>",
    ],
  };
}
