// Typed access to the shared Rust core (crates/core-wasm). The plugin entry point initializes it
// with the bundled module; tests initialize it from disk.
import * as wasm from "../wasm/core.js";

export interface LocalFile {
  path: string;
  hash: string;
}

export interface BaseEntry {
  path: string;
  remoteHash: string;
}

export interface RemoteFile {
  path: string;
  hash: string;
  deleted: boolean;
  keyId: string;
}

export type Action =
  | { action: "adoptBase"; path: string; hash: string }
  | { action: "download"; path: string }
  | { action: "merge"; path: string }
  | { action: "deleteLocal"; path: string }
  | { action: "keepLocalOverRemoteDelete"; path: string }
  | { action: "forgetBase"; path: string }
  | { action: "upload"; path: string; expectedRemoteHash: string | null }
  | { action: "deleteRemote"; path: string }
  | { action: "restoreRemote"; path: string };

export interface Resolution {
  kind: "clean" | "autoMerged" | "conflictCopy";
  content: string;
  conflictCopy: string | null;
}

export interface DataKey {
  id: string;
  key: string;
}

export type VaultKeyStep =
  | { kind: "use"; dataKey: DataKey }
  | { kind: "legacy" }
  | { kind: "create"; keyring: string; dataKey: DataKey };

export interface KeychainOptions {
  passphrase?: string;
  cloudUserId?: string;
  cloudVaultId?: string;
  dataKey?: DataKey;
}

export type Keychain = wasm.Keychain;

let loaded = false;

export function loadCore(module: Uint8Array): void {
  if (!loaded) {
    wasm.initSync({ module });
    loaded = true;
  }
}

/** Errors raised by the core carry a `CODE: message` prefix. */
export function errorCode(e: unknown): string {
  const message = e instanceof Error ? e.message : String(e);
  return /^([A-Z_]+):/.exec(message)?.[1] ?? "ERROR";
}

export function newKeychain(options: KeychainOptions): Keychain {
  return new wasm.Keychain(options);
}

export function resolveVaultKey(
  remoteKeyring: string | null,
  passphrase: string | null,
  cached: DataKey | null,
  createdAt: string,
): VaultKeyStep {
  return wasm.resolveVaultKey(remoteKeyring, passphrase, cached, createdAt) as VaultKeyStep;
}

export function rewrapKeyring(keyring: string, dataKey: DataKey, newPassphrase: string): string {
  return wasm.rewrapKeyring(keyring, dataKey, newPassphrase);
}

export function planPull(local: LocalFile[], base: BaseEntry[], remote: RemoteFile[], pullAll: boolean): Action[] {
  return wasm.planPull(local, base, remote, pullAll) as Action[];
}

export function planPush(local: LocalFile[], base: BaseEntry[], remote: RemoteFile[], currentKeyId: string): Action[] {
  return wasm.planPush(local, base, remote, currentKeyId) as Action[];
}

export function resolve(path: string, base: string, local: string, remote: string, nowMs: number): Resolution {
  return wasm.resolve(path, base, local, remote, nowMs) as Resolution;
}

export const payloadFromFile = (path: string, bytes: Uint8Array): string => wasm.payloadFromFile(path, bytes);
export const payloadToFile = (content: string): Uint8Array => wasm.payloadToFile(content);
export const sha1Hex = (text: string): string => wasm.sha1Hex(text);
export const isSyncable = (path: string, size: number): boolean => wasm.isSyncable(path, size);
export const isIgnoredDir = (name: string): boolean => wasm.isIgnoredDir(name);
export const keyringPath = (): string => wasm.keyringPath();

export interface ManifestEntry {
  hash: string;
  keyId: string;
  deleted: boolean;
  mtime: number;
  /** Object key relative to the prefix; absent for tombstones. */
  object?: string;
}

export interface Manifest {
  version: number;
  entries: Record<string, ManifestEntry>;
}

export const parseManifest = (body: string): Manifest => wasm.parseManifest(body) as Manifest;
export const manifestToJson = (manifest: Manifest): string => wasm.manifestToJson(manifest);
export const objectKey = (path: string, hash: string, keyId: string): string => wasm.objectKey(path, hash, keyId);
export const manifestPath = (): string => wasm.manifestPath();
