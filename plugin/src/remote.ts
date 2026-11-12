import type { Keychain, RemoteFile } from "./core";

/** A remote payload as stored (still encrypted). */
export interface Fetched {
  content: string;
  hash: string;
  mtimeMs: number;
}

/** One upload; `content` is already encrypted for `path`. */
export interface Put {
  path: string;
  content: string;
  hash: string;
  mtimeMs: number;
  keyId: string;
  /** Compare-and-swap: the live remote version this replaces (`null` = none). */
  expectedRemoteHash: string | null;
}

export type PutOutcome = { kind: "stored" } | { kind: "conflict" } | { kind: "failed"; reason: string };

export interface StoredKeyring {
  keyring: string;
  /** Opaque version for compare-and-swap replacement. */
  version: string;
}

/** An encrypted sync backend. Mirrors the CLI's `Remote` + `KeyringStore` traits. */
export interface Remote {
  readonly name: string;
  readonly readOnly: boolean;
  list(): Promise<RemoteFile[]>;
  adoptLegacyKeyIds?(keychain: Keychain, files: RemoteFile[]): Promise<void>;
  fetch(path: string): Promise<Fetched | null>;
  put(items: Put[]): Promise<PutOutcome[]>;
  tombstone(path: string, mtimeMs: number): Promise<void>;
  finish(): Promise<void>;
  fetchKeyring(): Promise<StoredKeyring | null>;
  /** Create only if absent; `false` when another device created one first. */
  createKeyring(keyring: string): Promise<boolean>;
  replaceKeyring(version: string, keyring: string): Promise<void>;
}

export const REMOTE_CHANGED = "REMOTE_CHANGED";

export class SyncError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "SyncError";
  }
}
