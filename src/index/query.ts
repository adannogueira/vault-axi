import type { Db } from "../db.js";
import { cosine, embed, fromBlob, initEmbedder, isAvailable } from "./embed.js";

export interface BlockRow {
  id: number;
  path: string;
  heading_path: string;
  text: string;
  embedding: Buffer | null;
}

const FTS_STOPWORDS = new Set(["and", "or", "not", "near"]);

/** Build a safe FTS5 MATCH expression from free text: prefix-match tokens OR-joined. */
export function buildFtsQuery(query: string): string {
  const tokens = (query.toLowerCase().match(/[\p{L}\p{N}_]+/gu) ?? [])
    .filter((t) => t.length > 1 && !FTS_STOPWORDS.has(t))
    .slice(0, 12);
  if (tokens.length === 0) return "";
  return tokens.map((t) => `${t}*`).join(" OR ");
}

export interface FtsHit {
  id: number;
  score: number;
}

/** Keyword search via FTS5. Returns hits best-first (higher score = better). */
export function ftsSearch(db: Db, query: string, limit: number): FtsHit[] {
  const match = buildFtsQuery(query);
  if (!match) return [];
  const rows = db
    .prepare(
      "SELECT rowid AS id, bm25(blocks_fts) AS rank FROM blocks_fts WHERE blocks_fts MATCH ? ORDER BY rank LIMIT ?",
    )
    .all(match, limit) as { id: number; rank: number }[];
  // bm25 returns lower-is-better; flip so higher-is-better for fusion.
  return rows.map((r) => ({ id: r.id, score: -r.rank }));
}

export interface VecHit {
  id: number;
  score: number;
}

/** Semantic search over stored block vectors. Returns null if embeddings unavailable. */
export async function semanticSearch(db: Db, query: string, limit: number): Promise<VecHit[] | null> {
  await initEmbedder();
  if (!isAvailable()) return null;
  const vecs = await embed([query]);
  if (!vecs || !vecs[0]) return null;
  return rankByVector(db, vecs[0], limit);
}

export function rankByVector(db: Db, qvec: Float32Array, limit: number, excludePath?: string): VecHit[] {
  const rows = db.prepare("SELECT id, embedding FROM blocks WHERE embedding IS NOT NULL").all() as {
    id: number;
    embedding: Buffer;
  }[];
  const scored: VecHit[] = [];
  for (const row of rows) {
    scored.push({ id: row.id, score: cosine(qvec, fromBlob(row.embedding)) });
  }
  scored.sort((a, b) => b.score - a.score);
  return scored.slice(0, limit);
}

/** Reciprocal-rank fusion of several ranked lists (by block id). */
export function rrfFuse(lists: { id: number; score: number }[][], k = 60): Map<number, number> {
  const fused = new Map<number, number>();
  for (const list of lists) {
    list.forEach((hit, rank) => {
      fused.set(hit.id, (fused.get(hit.id) ?? 0) + 1 / (k + rank + 1));
    });
  }
  return fused;
}

/** Mean (re-normalized) vector across a file's block embeddings, or null. */
export function fileMeanVector(db: Db, path: string): Float32Array | null {
  const rows = db.prepare("SELECT embedding FROM blocks WHERE path = ? AND embedding IS NOT NULL").all(path) as {
    embedding: Buffer;
  }[];
  if (rows.length === 0) return null;
  const dim = fromBlob(rows[0]!.embedding).length;
  const acc = new Float32Array(dim);
  for (const r of rows) {
    const v = fromBlob(r.embedding);
    for (let i = 0; i < dim; i++) acc[i]! += v[i]!;
  }
  let norm = 0;
  for (let i = 0; i < dim; i++) norm += acc[i]! * acc[i]!;
  norm = Math.sqrt(norm) || 1;
  for (let i = 0; i < dim; i++) acc[i]! /= norm;
  return acc;
}

export function blockById(db: Db, id: number): BlockRow | undefined {
  return db.prepare("SELECT id, path, heading_path, text, embedding FROM blocks WHERE id = ?").get(id) as
    | BlockRow
    | undefined;
}

/** Build a Smart-Connections-style breadcrumb from a block. */
export function breadcrumb(db: Db, block: { path: string; heading_path: string }): string {
  const title = (db.prepare("SELECT title FROM files WHERE path = ?").get(block.path) as
    | { title: string | null }
    | undefined)?.title;
  const base = title ? `${block.path} (${title})` : block.path;
  if (!block.heading_path || block.heading_path === "(preamble)") return base;
  return `${base} > ${block.heading_path}`;
}
