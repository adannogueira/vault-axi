import { AxiError } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { flagBool, flagInt, flagString, parseArgs } from "../args.js";
import {
  blockById,
  breadcrumb,
  ftsSearch,
  rrfFuse,
  semanticSearch,
} from "../index/query.js";

type Mode = "hybrid" | "fts" | "semantic";
const PREVIEW = 220;

export async function searchCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  c.requireVault();
  const { positionals, flags } = parseArgs(args);
  const query = positionals.join(" ").trim();
  if (!query) {
    throw new AxiError("Missing search query", "VALIDATION_ERROR", ['Usage: vault search "<query>" [--mode hybrid|fts|semantic] [--limit N] [--folder <dir>]']);
  }
  const mode = (flagString(flags, "mode") ?? "hybrid") as Mode;
  const limit = flagInt(flags, "limit", 8);
  const folder = flagString(flags, "folder");
  const full = flagBool(flags, "full");

  await c.sync({ withEmbeddings: mode !== "fts" });
  const db = c.db();

  const pool = Math.max(limit * 3, 24);
  const fts = ftsSearch(db, query, pool);
  let vec = mode === "fts" ? null : await semanticSearch(db, query, pool);
  let effectiveMode = mode;
  if ((mode === "semantic" || mode === "hybrid") && vec === null) {
    effectiveMode = "fts";
  }

  let orderedIds: number[];
  const scoreById = new Map<number, number>();
  if (effectiveMode === "fts") {
    fts.forEach((h) => scoreById.set(h.id, h.score));
    orderedIds = fts.map((h) => h.id);
  } else if (effectiveMode === "semantic") {
    (vec ?? []).forEach((h) => scoreById.set(h.id, h.score));
    orderedIds = (vec ?? []).map((h) => h.id);
  } else {
    const fused = rrfFuse([fts, vec ?? []]);
    orderedIds = [...fused.entries()].sort((a, b) => b[1] - a[1]).map(([id]) => id);
    fused.forEach((s, id) => scoreById.set(id, s));
  }

  const results: Record<string, unknown>[] = [];
  for (const id of orderedIds) {
    if (results.length >= limit) break;
    const block = blockById(db, id);
    if (!block) continue;
    if (folder && !block.path.startsWith(folder.replace(/\/$/, "") + "/") && block.path !== folder) continue;
    const text = block.text.replace(/\s+/g, " ").trim();
    results.push({
      path: block.path,
      breadcrumb: breadcrumb(db, block),
      score: Number((scoreById.get(id) ?? 0).toFixed(4)),
      preview: full ? block.text : text.length > PREVIEW ? `${text.slice(0, PREVIEW)}…` : text,
    });
  }

  if (results.length === 0) {
    return { query, mode: effectiveMode, results: `0 matches for "${query}"`, hint: "Try fewer/broader terms or --mode semantic" };
  }

  return {
    query,
    mode: effectiveMode,
    ...(effectiveMode !== mode ? { note: "Embeddings unavailable — fell back to FTS5. Run `vault reindex`." } : {}),
    shown: results.length,
    results,
  };
}
