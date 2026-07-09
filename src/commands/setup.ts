import {
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync,
  symlinkSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
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
    out.skill = installSkill((m) => {
      out.skillError = m;
    });
  }

  if (!vault && !wantHooks) {
    return {
      ok: true,
      help: "Nothing to do",
      usage: [
        "vault setup --vault <path>",
        "vault setup hooks   # SessionStart dashboard + the `vault` agent skill",
        "vault setup --vault <path> --hooks",
      ],
      current: loadConfig().vaultPath ?? "(unset)",
    };
  }

  out.next = ["vault reindex", 'vault search "<query>"'];
  return out;
}

/** True if a path exists as a file, dir, or (possibly broken) symlink. */
function pathPresent(p: string): boolean {
  try {
    lstatSync(p);
    return true;
  } catch {
    return false;
  }
}

/**
 * Install the `vault` agent skill into ~/.claude/skills/vault so Claude Code
 * discovers it. Symlinks the repo's skill folder (stays in sync as the tool is
 * iterated); falls back to copying the SKILL.md if symlinking isn't possible.
 */
function installSkill(onError: (message: string) => void): string {
  const here = dirname(fileURLToPath(import.meta.url)); // .../dist/commands
  const sourceDir = resolve(here, "..", "..", ".agents", "skills", "vault");
  const sourceFile = join(sourceDir, "SKILL.md");
  if (!existsSync(sourceFile)) {
    onError(`skill source not found at ${sourceFile}`);
    return "failed (source not found)";
  }

  const destDir = join(homedir(), ".claude", "skills", "vault");
  try {
    if (pathPresent(destDir)) rmSync(destDir, { recursive: true, force: true });
    mkdirSync(dirname(destDir), { recursive: true });
    symlinkSync(sourceDir, destDir, "dir");
    return `installed (symlink: ${destDir} -> ${sourceDir})`;
  } catch (symlinkErr) {
    // Cross-device or permission issue: fall back to copying just the SKILL.md.
    try {
      mkdirSync(destDir, { recursive: true });
      copyFileSync(sourceFile, join(destDir, "SKILL.md"));
      return `installed (copy: ${destDir}/SKILL.md)`;
    } catch (copyErr) {
      onError(String(copyErr instanceof Error ? copyErr.message : symlinkErr));
      return "failed";
    }
  }
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
