import { readdirSync, statSync } from "node:fs";
import { join, relative, resolve, sep } from "node:path";
import { parseDoc } from "./markdown/sections.js";

const IGNORED_DIRS = new Set([".obsidian", ".smart-env", ".trash", ".git", "node_modules"]);

/** List all markdown files under the vault, as vault-relative POSIX paths. */
export function listMarkdownFiles(vaultRoot: string): string[] {
  const out: string[] = [];
  walk(vaultRoot, vaultRoot, out);
  out.sort();
  return out;
}

function walk(dir: string, root: string, out: string[]): void {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    if (entry.name.startsWith(".") && IGNORED_DIRS.has(entry.name)) continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (IGNORED_DIRS.has(entry.name)) continue;
      walk(full, root, out);
    } else if (entry.isFile() && entry.name.toLowerCase().endsWith(".md")) {
      out.push(toRel(root, full));
    }
  }
}

export function toRel(vaultRoot: string, absPath: string): string {
  return relative(vaultRoot, absPath).split(sep).join("/");
}

/** Resolve a vault-relative path to an absolute path, refusing to escape the vault. */
export function resolveInVault(vaultRoot: string, relPath: string): string {
  const clean = relPath.replace(/^\/+/, "");
  const abs = resolve(vaultRoot, clean);
  const rootResolved = resolve(vaultRoot);
  if (abs !== rootResolved && !abs.startsWith(rootResolved + sep)) {
    throw new Error(`Path escapes the vault: ${relPath}`);
  }
  return abs;
}

/** Normalize a user-supplied note path: strip leading slash, add .md if missing. */
export function normalizeNotePath(relPath: string): string {
  let p = relPath.replace(/^\/+/, "").split(sep).join("/");
  if (!p.toLowerCase().endsWith(".md")) p += ".md";
  return p;
}

export function fileStat(absPath: string): { mtimeMs: number; size: number } | null {
  try {
    const s = statSync(absPath);
    return { mtimeMs: Math.floor(s.mtimeMs), size: s.size };
  } catch {
    return null;
  }
}

export function deriveTitle(
  frontmatter: Record<string, unknown> | null,
  raw: string,
  relPath: string,
): string {
  const fmTitle = frontmatter?.title;
  if (typeof fmTitle === "string" && fmTitle.trim()) return fmTitle.trim();
  const h1 = parseDoc(raw).headings.find((h) => h.level === 1);
  if (h1) return h1.title;
  return relPath.slice(relPath.lastIndexOf("/") + 1).replace(/\.md$/i, "");
}
