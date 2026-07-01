import { existsSync } from "node:fs";
import { AxiError } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { flagInt, parseArgs } from "../args.js";
import { normalizeNotePath, resolveInVault } from "../vault.js";
import { cosine, fromBlob } from "../index/embed.js";
import { fileMeanVector } from "../index/query.js";

export async function relatedCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  const root = c.requireVault();
  const { positionals, flags } = parseArgs(args);
  const rel = positionals[0];
  if (!rel) throw new AxiError("Missing file path", "VALIDATION_ERROR", ["Usage: vault related <file> [--limit N]"]);
  const notePath = normalizeNotePath(rel);
  if (!existsSync(resolveInVault(root, notePath))) {
    throw new AxiError(`Note not found: ${notePath}`, "NOT_FOUND", ["Run `vault ls` or `vault search <query>`"]);
  }
  const limit = flagInt(flags, "limit", 10);

  await c.sync({ withEmbeddings: true });
  const db = c.db();

  const outbound = new Set(
    (db
      .prepare("SELECT DISTINCT target_path AS p FROM links WHERE src_path = ? AND target_path IS NOT NULL AND target_path != ?")
      .all(notePath, notePath) as { p: string }[]).map((r) => r.p),
  );
  const backlinks = new Set(
    (db
      .prepare("SELECT DISTINCT src_path AS p FROM links WHERE target_path = ? AND src_path != ?")
      .all(notePath, notePath) as { p: string }[]).map((r) => r.p),
  );

  const semantic = new Map<string, number>();
  const qvec = fileMeanVector(db, notePath);
  if (qvec) {
    const rows = db.prepare("SELECT path, embedding FROM blocks WHERE embedding IS NOT NULL AND path != ?").all(notePath) as {
      path: string;
      embedding: Buffer;
    }[];
    // best-matching block per file = that file's relevance
    for (const r of rows) {
      const s = cosine(qvec, fromBlob(r.embedding));
      if (s > (semantic.get(r.path) ?? -1)) semantic.set(r.path, s);
    }
  }

  const candidates = new Set<string>([...outbound, ...backlinks, ...[...semantic.keys()]]);
  const scored = [...candidates].map((path) => {
    const via: string[] = [];
    if (outbound.has(path)) via.push("link");
    if (backlinks.has(path)) via.push("backlink");
    const sem = semantic.get(path) ?? 0;
    const linked = outbound.has(path) || backlinks.has(path);
    if (sem > 0 && (!linked || sem >= 0.5)) via.push("semantic");
    const title = (db.prepare("SELECT title FROM files WHERE path = ?").get(path) as { title: string | null } | undefined)
      ?.title;
    return { path, title: title ?? undefined, via: via.join("+") || "semantic", linked, score: Number(sem.toFixed(4)) };
  });

  // Linked notes rank first (definite relationship), then by semantic similarity.
  scored.sort((a, b) => (Number(b.linked) - Number(a.linked)) || b.score - a.score);
  const top = scored.slice(0, limit);

  if (top.length === 0) {
    return { file: notePath, related: "0 related notes (no links, and no embeddings yet — run `vault reindex`)" };
  }
  return {
    file: notePath,
    linkedCount: outbound.size + backlinks.size,
    shown: top.length,
    related: top,
    ...(qvec ? {} : { note: "No embeddings for this note — showing graph links only. Run `vault reindex`." }),
  };
}
