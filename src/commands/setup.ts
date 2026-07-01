import { existsSync, statSync } from "node:fs";
import { resolve } from "node:path";
import { AxiError, installSessionStartHooks } from "axi-sdk-js";
import type { Ctx } from "../context.js";
import { flagBool, flagString, parseArgs } from "../args.js";
import { configPath, loadConfig, saveConfig } from "../config.js";

export async function setupCommand(args: string[], _ctx: Ctx | undefined): Promise<Record<string, unknown>> {
  const { positionals, flags } = parseArgs(args);
  const wantHooks = positionals[0] === "hooks" || flagBool(flags, "hooks");
  const vault = flagString(flags, "vault");

  const out: Record<string, unknown> = { ok: true };

  if (vault) {
    const abs = resolve(vault);
    if (!existsSync(abs) || !statSync(abs).isDirectory()) {
      throw new AxiError(`Not a directory: ${abs}`, "VALIDATION_ERROR", ["Pass --vault <path to your Obsidian vault>"]);
    }
    const config = loadConfig();
    config.vaultPath = abs;
    saveConfig(config);
    out.vault = abs;
    out.config = configPath();
  }

  if (wantHooks) {
    let hookInstalled = true;
    installSessionStartHooks({
      marker: "vault-axi",
      binaryNames: ["vault"],
      onError: (message) => {
        hookInstalled = false;
        out.hookError = message;
      },
    });
    out.sessionHook = hookInstalled ? "installed (Claude Code SessionStart)" : "failed";
  }

  if (!vault && !wantHooks) {
    return {
      ok: true,
      help: "Nothing to do",
      usage: ["vault setup --vault <path>", "vault setup hooks", "vault setup --vault <path> --hooks"],
      current: loadConfig().vaultPath ?? "(unset)",
    };
  }

  out.next = ["vault reindex", 'vault search "<query>"'];
  return out;
}
