import { AxiError } from "axi-sdk-js";
import { openDb, type Db } from "./db.js";
import { resolveVaultPath } from "./config.js";
import { reconcile } from "./index/reconcile.js";

export interface Ctx {
  vaultPath: string | null;
  requireVault(): string;
  db(): Db;
  /** Bring the index up to date; safe to call at the front of read commands. */
  sync(opts?: { force?: boolean; withEmbeddings?: boolean }): Promise<void>;
}

/** Build the per-invocation context. Vault resolution is lazy so `setup` can run first. */
export function createContext(): Ctx {
  const vaultPath = resolveVaultPath();
  let dbHandle: Db | null = null;

  return {
    vaultPath,
    requireVault(): string {
      if (!vaultPath) {
        throw new AxiError("No vault configured", "VALIDATION_ERROR", [
          "Run `vault setup --vault <path>` to point at your Obsidian vault",
          "Or set VAULT_AXI_VAULT=<path>",
        ]);
      }
      return vaultPath;
    },
    db(): Db {
      if (!dbHandle) dbHandle = openDb();
      return dbHandle;
    },
    async sync(opts): Promise<void> {
      const root = this.requireVault();
      await reconcile(this.db(), root, opts);
    },
  };
}
