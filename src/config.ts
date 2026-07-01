import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";

export interface VaultConfig {
  vaultPath?: string;
}

const CONFIG_DIR = join(homedir(), ".vault-axi");
const CONFIG_PATH = join(CONFIG_DIR, "config.json");
const DB_PATH = join(CONFIG_DIR, "index.db");

export function configDir(): string {
  return CONFIG_DIR;
}

export function configPath(): string {
  return CONFIG_PATH;
}

export function dbPath(): string {
  return DB_PATH;
}

export function ensureConfigDir(): void {
  if (!existsSync(CONFIG_DIR)) {
    mkdirSync(CONFIG_DIR, { recursive: true });
  }
}

export function loadConfig(): VaultConfig {
  if (!existsSync(CONFIG_PATH)) return {};
  try {
    return JSON.parse(readFileSync(CONFIG_PATH, "utf8")) as VaultConfig;
  } catch {
    return {};
  }
}

export function saveConfig(config: VaultConfig): void {
  ensureConfigDir();
  writeFileSync(CONFIG_PATH, `${JSON.stringify(config, null, 2)}\n`, "utf8");
}

/**
 * Resolve the vault root. Precedence: VAULT_AXI_VAULT env var, then config file.
 * Returns an absolute path or null if the vault has not been configured.
 */
export function resolveVaultPath(): string | null {
  const fromEnv = process.env.VAULT_AXI_VAULT;
  if (fromEnv && fromEnv.trim()) return resolve(fromEnv.trim());
  const fromConfig = loadConfig().vaultPath;
  if (fromConfig && fromConfig.trim()) return resolve(fromConfig.trim());
  return null;
}
