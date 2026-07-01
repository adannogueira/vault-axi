import { existsSync, readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
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
    installSessionStartHooks({
      marker: "vault",
      binaryNames: ["vault"],
      distEntrypoints: ["dist/cli.js"],
      shouldInstall: () => true,
      onError: (message) => {
        out.hookError = message;
      },
    });
    const hook = findVaultSessionHook();
    out.sessionHook = hook
      ? `installed (Claude Code SessionStart: \`${hook}\`)`
      : "failed (no SessionStart entry written)";
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

/** Read ~/.claude/settings.json and return the command of a vault SessionStart hook, if any. */
function findVaultSessionHook(): string | null {
  const path = join(homedir(), ".claude", "settings.json");
  if (!existsSync(path)) return null;
  try {
    const settings = JSON.parse(readFileSync(path, "utf8")) as {
      hooks?: { SessionStart?: { hooks?: { command?: string }[] }[] };
    };
    for (const group of settings.hooks?.SessionStart ?? []) {
      for (const h of group.hooks ?? []) {
        if (typeof h.command === "string" && h.command.includes("vault")) return h.command;
      }
    }
  } catch {
    return null;
  }
  return null;
}
