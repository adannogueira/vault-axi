import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import type { Db } from "../db.js";
import { setMeta } from "../db.js";
import { toBlocks } from "../markdown/blocks.js";
import { readFrontmatter } from "../markdown/frontmatter.js";
import { buildBasenameIndex, parseWikiLinks, resolveLinkTarget } from "../markdown/links.js";
import {
  deriveTitle,
  fileStat,
  listMarkdownFiles,
  resolveInVault,
} from "../vault.js";
import { embed, initEmbedder, isAvailable, toBlob } from "./embed.js";

export interface ReconcileResult {
  scanned: number;
  indexed: number;
  updated: number;
  removed: number;
  unchanged: number;
  embedded: number;
  embeddingsAvailable: boolean;
}

interface FileRow {
  path: string;
  mtime_ms: number;
  size: number;
  hash: string;
}

function sha1(text: string): string {
  return createHash("sha1").update(text).digest("hex");
}

/**
 * Bring the index up to date with the vault. Only new/changed files are
 * re-parsed and re-embedded; deleted files are pruned. Cheap enough to run at
 * the front of every read command.
 */
export async function reconcile(
  db: Db,
  vaultRoot: string,
  opts: { force?: boolean; withEmbeddings?: boolean } = {},
): Promise<ReconcileResult> {
  const withEmbeddings = opts.withEmbeddings ?? true;
  const paths = listMarkdownFiles(vaultRoot);
  const basenameIndex = buildBasenameIndex(paths);

  const existing = new Map<string, FileRow>();
  for (const row of db.prepare("SELECT path, mtime_ms, size, hash FROM files").all() as FileRow[]) {
    existing.set(row.path, row);
  }

  const result: ReconcileResult = {
    scanned: paths.length,
    indexed: 0,
    updated: 0,
    removed: 0,
    unchanged: 0,
    embedded: 0,
    embeddingsAvailable: false,
  };

  const seen = new Set<string>();
  const toEmbed: { blockId: number; text: string }[] = [];

  const insertFile = db.prepare(
    `INSERT INTO files(path, mtime_ms, size, hash, title, frontmatter, indexed_at)
     VALUES (@path, @mtime_ms, @size, @hash, @title, @frontmatter, @indexed_at)
     ON CONFLICT(path) DO UPDATE SET
       mtime_ms=excluded.mtime_ms, size=excluded.size, hash=excluded.hash,
       title=excluded.title, frontmatter=excluded.frontmatter, indexed_at=excluded.indexed_at`,
  );
  const deleteBlocks = db.prepare("DELETE FROM blocks WHERE path = ?");
  const deleteLinks = db.prepare("DELETE FROM links WHERE src_path = ?");
  const insertBlock = db.prepare(
    "INSERT INTO blocks(path, heading_path, start_line, end_line, text) VALUES (?, ?, ?, ?, ?)",
  );
  const insertLink = db.prepare(
    "INSERT INTO links(src_path, target_raw, target_path, heading, alias) VALUES (?, ?, ?, ?, ?)",
  );
  const now = Date.now();

  const processFile = db.transaction((relPath: string) => {
    const abs = resolveInVault(vaultRoot, relPath);
    const raw = readFileSync(abs, "utf8");
    const { data: frontmatter } = readFrontmatter(raw);
    const title = deriveTitle(frontmatter, raw, relPath);
    const st = fileStat(abs);

    insertFile.run({
      path: relPath,
      mtime_ms: st?.mtimeMs ?? now,
      size: st?.size ?? raw.length,
      hash: sha1(raw),
      title,
      frontmatter: frontmatter ? JSON.stringify(frontmatter) : null,
      indexed_at: now,
    });

    deleteBlocks.run(relPath);
    deleteLinks.run(relPath);

    for (const block of toBlocks(raw)) {
      const info = insertBlock.run(
        relPath,
        block.headingPath,
        block.startLine,
        block.endLine,
        block.text,
      );
      if (withEmbeddings && block.text.length >= 40) {
        toEmbed.push({ blockId: Number(info.lastInsertRowid), text: block.text });
      }
    }

    for (const link of parseWikiLinks(raw)) {
      const target = resolveLinkTarget(link.target, paths, basenameIndex);
      insertLink.run(relPath, link.raw, target, link.heading, link.alias);
    }
  });

  for (const relPath of paths) {
    seen.add(relPath);
    const abs = resolveInVault(vaultRoot, relPath);
    const st = fileStat(abs);
    const prev = existing.get(relPath);
    const changed =
      opts.force ||
      !prev ||
      !st ||
      prev.mtime_ms !== st.mtimeMs ||
      prev.size !== st.size;

    if (!changed) {
      result.unchanged++;
      continue;
    }
    processFile(relPath);
    if (prev) result.updated++;
    else result.indexed++;
  }

  const removeFile = db.transaction((p: string) => {
    db.prepare("DELETE FROM blocks WHERE path = ?").run(p);
    db.prepare("DELETE FROM links WHERE src_path = ?").run(p);
    db.prepare("DELETE FROM files WHERE path = ?").run(p);
  });
  for (const p of existing.keys()) {
    if (!seen.has(p)) {
      removeFile(p);
      result.removed++;
    }
  }

  if (withEmbeddings && toEmbed.length > 0) {
    await initEmbedder();
    result.embeddingsAvailable = isAvailable();
    if (result.embeddingsAvailable) {
      const updateEmbedding = db.prepare("UPDATE blocks SET embedding = ? WHERE id = ?");
      const BATCH = 32;
      for (let i = 0; i < toEmbed.length; i += BATCH) {
        const batch = toEmbed.slice(i, i + BATCH);
        const vecs = await embed(batch.map((b) => b.text));
        if (!vecs) break;
        const writeBatch = db.transaction(() => {
          batch.forEach((b, idx) => {
            const vec = vecs[idx];
            if (vec) updateEmbedding.run(toBlob(vec), b.blockId);
          });
        });
        writeBatch();
        result.embedded += batch.length;
      }
    }
  } else {
    result.embeddingsAvailable = isAvailable();
  }

  setMeta(db, "last_reconcile", String(now));
  setMeta(db, "vault_root", vaultRoot);
  return result;
}
