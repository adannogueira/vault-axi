import { errorCode, newKeychain, resolveVaultKey, type DataKey, type Keychain } from "./core";
import { SyncError, type Remote } from "./remote";

export interface KeySettings {
  passphrase: string | null;
  /** Hosted-cloud account ids, for data written with the legacy server-derivable key. */
  cloudUserId: string | null;
  cloudVaultId: string | null;
}

/** Device-local cache of the unlocked vault data key (Argon2id runs once per device). */
export interface DataKeyCache {
  get(): DataKey | null;
  set(dataKey: DataKey): void;
}

function coreError(e: unknown): SyncError {
  const message = e instanceof Error ? e.message.replace(/^[A-Z_]+:\s*/, "") : String(e);
  return new SyncError(errorCode(e), message);
}

/**
 * Unlock (or create) the remote keyring and return the keys to sync with. The decision itself is
 * the core's `resolveVaultKey`, shared with the CLI.
 */
export async function prepareKeychain(
  remote: Remote,
  settings: KeySettings,
  cache: DataKeyCache,
  now: () => Date = () => new Date(),
): Promise<Keychain> {
  const decide = (keyring: string | null) => {
    try {
      return resolveVaultKey(keyring, settings.passphrase, cache.get(), now().toISOString());
    } catch (e) {
      throw coreError(e);
    }
  };

  let dataKey: DataKey | null = null;
  const step = decide((await remote.fetchKeyring())?.keyring ?? null);
  if (step.kind === "use") {
    dataKey = step.dataKey;
  } else if (step.kind === "create") {
    if (await remote.createKeyring(step.keyring)) {
      dataKey = step.dataKey;
    } else {
      const winner = decide((await remote.fetchKeyring())?.keyring ?? null);
      if (winner.kind !== "use") throw new SyncError("SYNC_ERROR", "Keyring creation conflicted but no keyring was found");
      dataKey = winner.dataKey;
    }
  }

  const cached = cache.get();
  if (dataKey && (cached?.id !== dataKey.id || cached.key !== dataKey.key)) cache.set(dataKey);

  try {
    return newKeychain({
      ...(settings.passphrase ? { passphrase: settings.passphrase } : {}),
      ...(settings.cloudUserId ? { cloudUserId: settings.cloudUserId } : {}),
      ...(settings.cloudVaultId ? { cloudVaultId: settings.cloudVaultId } : {}),
      ...(dataKey ? { dataKey } : {}),
    });
  } catch (e) {
    if (errorCode(e) === "NO_KEY") {
      throw new SyncError("ENCRYPTION_KEY_REQUIRED", "Set the vault encryption passphrase in the plugin settings");
    }
    throw coreError(e);
  }
}
