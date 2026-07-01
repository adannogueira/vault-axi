import type { Ctx } from "../context.js";
import { flagBool, parseArgs } from "../args.js";
import { reconcile } from "../index/reconcile.js";

export async function reindexCommand(args: string[], ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const c = ctx!;
  const root = c.requireVault();
  const { flags } = parseArgs(args);
  const force = flagBool(flags, "force");
  const withEmbeddings = !flagBool(flags, "no-embed");
  const res = await reconcile(c.db(), root, { force, withEmbeddings });
  return {
    ok: true,
    vault: root,
    ...res,
    ...(withEmbeddings && !res.embeddingsAvailable
      ? { note: "Embeddings unavailable (offline or disabled) — search falls back to FTS5 + graph" }
      : {}),
  };
}
