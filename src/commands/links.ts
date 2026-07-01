import { AxiError } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { parseArgs } from "../args.js";
import { normalizeNotePath } from "../vault.js";

export async function linksCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  c.requireVault();
  const { positionals } = parseArgs(args);
  const rel = positionals[0];
  if (!rel) throw new AxiError("Missing file path", "VALIDATION_ERROR", ["Usage: vault links <file>"]);
  const notePath = normalizeNotePath(rel);

  await c.sync({ withEmbeddings: false });
  const db = c.db();

  const outboundRows = db
    .prepare("SELECT target_raw, target_path, heading, alias FROM links WHERE src_path = ? ORDER BY target_raw")
    .all(notePath) as { target_raw: string; target_path: string | null; heading: string | null; alias: string | null }[];

  const outbound = outboundRows.map((r) => ({
    link: r.target_raw,
    resolvedTo: r.target_path ?? undefined,
    ...(r.heading ? { heading: r.heading } : {}),
    ...(r.alias ? { alias: r.alias } : {}),
    ...(r.target_path ? {} : { dangling: true }),
  }));

  const backlinks = (
    db.prepare("SELECT DISTINCT src_path AS p FROM links WHERE target_path = ? ORDER BY p").all(notePath) as {
      p: string;
    }[]
  ).map((r) => r.p);

  return {
    file: notePath,
    outboundCount: outbound.length,
    backlinkCount: backlinks.length,
    outbound: outbound.length ? outbound : "(none)",
    backlinks: backlinks.length ? backlinks : "(none)",
  };
}
