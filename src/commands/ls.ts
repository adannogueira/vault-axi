import type { Ctx } from "../context.js";
import { flagInt, parseArgs } from "../args.js";

export async function lsCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  c.requireVault();
  const { positionals, flags } = parseArgs(args);
  const dir = positionals[0]?.replace(/\/$/, "");
  const limit = flagInt(flags, "limit", 50);

  await c.sync({ withEmbeddings: false });
  const db = c.db();

  const rows = (
    dir
      ? db
          .prepare("SELECT path, title, frontmatter FROM files WHERE path LIKE ? ORDER BY path")
          .all(`${dir}/%`)
      : db.prepare("SELECT path, title, frontmatter FROM files ORDER BY path").all()
  ) as { path: string; title: string | null; frontmatter: string | null }[];

  const total = rows.length;
  const files = rows.slice(0, limit).map((r) => {
    let status: string | undefined;
    let tags: unknown;
    if (r.frontmatter) {
      try {
        const fm = JSON.parse(r.frontmatter);
        status = fm.status;
        tags = fm.tags;
      } catch {
        /* ignore */
      }
    }
    return {
      path: r.path,
      ...(r.title ? { title: r.title } : {}),
      ...(status ? { status } : {}),
      ...(Array.isArray(tags) && tags.length ? { tags } : {}),
    };
  });

  if (total === 0) {
    return { where: dir ?? "(vault root)", files: dir ? `0 notes under "${dir}"` : "0 notes in vault" };
  }
  return {
    where: dir ?? "(whole vault)",
    total,
    shown: files.length,
    ...(files.length < total ? { note: `Showing ${files.length} of ${total}; pass --limit to see more` } : {}),
    files,
  };
}
