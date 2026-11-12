// Executes the core sync plan against a Remote with this device's vault and base store. A
// line-for-line counterpart of the CLI's `sync/engine.rs`; all decisions come from the core.
import type { BaseRecord, BaseStore } from "./base-store";
import {
  isSyncable,
  payloadFromFile,
  payloadToFile,
  planPull,
  planPush,
  resolve,
  sha1Hex,
  type Action,
  type BaseEntry,
  type Keychain,
  type LocalFile,
} from "./core";
import { prepareKeychain, type DataKeyCache, type KeySettings } from "./keys";
import { REMOTE_CHANGED, SyncError, type Put, type Remote } from "./remote";

/** Associated-data context for base content kept on this device. */
const LOCAL_CONTEXT = "local-db";
const MAX_ATTEMPTS = 3;

export interface VaultFile {
  path: string;
  size: number;
}

/** File access for the vault, including `.obsidian/`. Paths are vault-relative, `/`-separated. */
export interface VaultIO {
  listFiles(): Promise<VaultFile[]>;
  read(path: string): Promise<Uint8Array | null>;
  write(path: string, bytes: Uint8Array): Promise<void>;
  trash(path: string): Promise<void>;
  mtime(path: string): Promise<number | null>;
}

export interface ConflictInfo {
  path: string;
  resolution:
    | "auto_merged"
    | "conflict_copy_saved"
    | "kept_local_edit_over_remote_delete"
    | "restored_remote_edit_over_local_delete";
  conflictCopy: string | null;
  message: string;
}

export interface SyncSummary {
  backend: string;
  pulled: number;
  pushed: number;
  deletedRemote: number;
  deletedLocal: number;
  conflicts: ConflictInfo[];
  /** Paths that could not be transferred this run (retried on the next sync). */
  failures: string[];
}

export interface SyncOptions {
  io: VaultIO;
  base: BaseStore;
  remote: Remote;
  keys: KeySettings;
  keyCache: DataKeyCache;
  pull: boolean;
  push: boolean;
  now?: () => number;
  onProgress?: (action: string, done: number, total: number, path: string) => void;
}

export async function syncVault(options: SyncOptions): Promise<SyncSummary> {
  for (let attempt = 1; ; attempt++) {
    try {
      return await runOnce(options);
    } catch (e) {
      if (!(e instanceof SyncError && e.code === REMOTE_CHANGED) || attempt >= MAX_ATTEMPTS) throw e;
    }
  }
}

async function runOnce(options: SyncOptions): Promise<SyncSummary> {
  const { remote, pull, push } = options;
  if (push && !pull && remote.readOnly) {
    throw new SyncError("PERMISSION_DENIED", "This API token is read-only and cannot push changes");
  }
  const remoteFiles = await remote.list();
  const keychain = await prepareKeychain(remote, options.keys, options.keyCache);
  await remote.adoptLegacyKeyIds?.(keychain, remoteFiles);

  const run = new Run(options, keychain, await options.base.load());
  await run.executePull(planPull(await run.scanLocal(), run.baseEntries(), remoteFiles, pull));
  if (push && !remote.readOnly) {
    const local = await run.scanLocal();
    await run.executePush(planPush(local, run.baseEntries(), remoteFiles, keychain.activeKeyId()));
  }
  await remote.finish();
  return run.summary;
}

interface RemoteVersion {
  content: string;
  hash: string;
  mtimeMs: number;
}

class Run {
  readonly summary: SyncSummary;
  private readonly now: number;

  constructor(
    private readonly options: SyncOptions,
    private readonly kc: Keychain,
    private readonly base: Map<string, BaseRecord>,
  ) {
    this.now = (options.now ?? Date.now)();
    this.summary = {
      backend: options.remote.name,
      pulled: 0,
      pushed: 0,
      deletedRemote: 0,
      deletedLocal: 0,
      conflicts: [],
      failures: [],
    };
  }

  baseEntries(): BaseEntry[] {
    return [...this.base].map(([path, b]) => ({ path, remoteHash: b.remoteHash }));
  }

  async scanLocal(): Promise<LocalFile[]> {
    const files = (await this.options.io.listFiles()).filter((f) => isSyncable(f.path, f.size));
    const local: LocalFile[] = [];
    for (const f of files) {
      const content = await this.readLocal(f.path);
      if (content !== null) local.push({ path: f.path, hash: sha1Hex(content) });
    }
    return local;
  }

  private async readLocal(path: string): Promise<string | null> {
    const bytes = await this.options.io.read(path);
    return bytes === null ? null : payloadFromFile(path, bytes);
  }

  private async writeLocal(path: string, content: string): Promise<boolean> {
    try {
      await this.options.io.write(path, payloadToFile(content));
      return true;
    } catch (e) {
      this.fail(path, `could not write: ${String(e)}`);
      return false;
    }
  }

  private async setBase(path: string, remoteHash: string, content: string): Promise<void> {
    const record = { remoteHash, stored: this.kc.encrypt(content, LOCAL_CONTEXT) };
    await this.options.base.put(path, record);
    this.base.set(path, record);
  }

  private async forgetBase(path: string): Promise<void> {
    await this.options.base.delete(path);
    this.base.delete(path);
  }

  private baseContent(path: string): string {
    const stored = this.base.get(path)?.stored;
    if (stored === undefined) return "";
    try {
      return this.kc.decrypt(stored, LOCAL_CONTEXT);
    } catch {
      return "";
    }
  }

  private conflict(path: string, resolution: ConflictInfo["resolution"], conflictCopy: string | null, message: string): void {
    this.summary.conflicts.push({ path, resolution, conflictCopy, message });
  }

  private fail(path: string, why: string): void {
    this.summary.failures.push(`${path}: ${why}`);
  }

  private progress(action: string, done: number, total: number, path: string): void {
    this.options.onProgress?.(action, done, total, path);
  }

  /** Fetch and decrypt the live remote version; undecryptable payloads are reported, not written. */
  private async fetch(path: string): Promise<RemoteVersion | null> {
    const fetched = await this.options.remote.fetch(path);
    if (!fetched) return null;
    try {
      return { content: this.kc.decrypt(fetched.content, path), hash: fetched.hash, mtimeMs: fetched.mtimeMs };
    } catch (e) {
      this.fail(path, e instanceof Error ? e.message : String(e));
      return null;
    }
  }

  async executePull(actions: Action[]): Promise<void> {
    const total = actions.filter((a) => a.action === "download" || a.action === "merge").length;
    let done = 0;
    for (const a of actions) {
      switch (a.action) {
        case "adoptBase": {
          const content = await this.readLocal(a.path);
          if (content !== null) await this.setBase(a.path, a.hash, content);
          break;
        }
        case "download": {
          this.progress("Downloading", ++done, total, a.path);
          const rv = await this.fetch(a.path);
          if (rv && (await this.writeLocal(a.path, rv.content))) {
            await this.setBase(a.path, rv.hash, rv.content);
            this.summary.pulled++;
          }
          break;
        }
        case "merge":
          this.progress("Merging", ++done, total, a.path);
          await this.merge(a.path);
          break;
        case "deleteLocal":
          await this.options.io.trash(a.path);
          this.summary.deletedLocal++;
          await this.forgetBase(a.path);
          break;
        case "keepLocalOverRemoteDelete":
          await this.forgetBase(a.path);
          this.conflict(
            a.path,
            "kept_local_edit_over_remote_delete",
            null,
            `'${a.path}' was deleted on another device but edited here; kept your version.`,
          );
          break;
        case "forgetBase":
          await this.forgetBase(a.path);
          break;
        default:
          throw new Error(`not a pull action: ${a.action}`);
      }
    }
  }

  /** Resolve a concurrent edit; returns the remote version it was resolved against. */
  private async merge(path: string): Promise<RemoteVersion | null> {
    const local = await this.readLocal(path);
    if (local === null) return null;
    const rv = await this.fetch(path);
    if (!rv) return null;
    const { kind, content, conflictCopy } = resolve(path, this.baseContent(path), local, rv.content, this.now);

    if (conflictCopy !== null && !(await this.writeLocal(conflictCopy, local))) return null;
    if (content !== local && !(await this.writeLocal(path, content))) return null;
    await this.setBase(path, rv.hash, rv.content);

    if (kind === "clean") {
      if (sha1Hex(content) === rv.hash) this.summary.pulled++;
    } else if (kind === "autoMerged") {
      this.summary.pulled++;
      this.conflict(path, "auto_merged", null, `'${path}' changed on another device too; the edits were merged.`);
    } else {
      this.summary.pulled++;
      this.conflict(
        path,
        "conflict_copy_saved",
        conflictCopy,
        `'${path}' was edited on two devices at once. The other device's version is in '${path}'; yours was saved to '${conflictCopy ?? ""}'.`,
      );
    }
    return rv;
  }

  private async preparePut(path: string, expectedRemoteHash: string | null): Promise<{ put: Put; content: string } | null> {
    const content = await this.readLocal(path);
    if (content === null) return null;
    return {
      content,
      put: {
        path,
        content: this.kc.encrypt(content, path),
        hash: sha1Hex(content),
        mtimeMs: (await this.options.io.mtime(path)) ?? this.now,
        keyId: this.kc.activeKeyId(),
        expectedRemoteHash,
      },
    };
  }

  async executePush(actions: Action[]): Promise<void> {
    const uploads: { put: Put; content: string }[] = [];
    for (const a of actions) {
      switch (a.action) {
        case "upload": {
          const prepared = await this.preparePut(a.path, a.expectedRemoteHash);
          if (prepared) uploads.push(prepared);
          break;
        }
        case "deleteRemote":
          await this.options.remote.tombstone(a.path, this.now);
          await this.forgetBase(a.path);
          this.summary.deletedRemote++;
          break;
        case "restoreRemote": {
          const rv = await this.fetch(a.path);
          if (rv && (await this.writeLocal(a.path, rv.content))) {
            await this.setBase(a.path, rv.hash, rv.content);
            this.summary.pulled++;
            this.conflict(
              a.path,
              "restored_remote_edit_over_local_delete",
              null,
              `'${a.path}' was deleted here but edited on another device; restored that version.`,
            );
          }
          break;
        }
        default:
          throw new Error(`not a push action: ${a.action}`);
      }
    }

    uploads.forEach(({ put }, i) => {
      this.progress("Uploading", i + 1, uploads.length, put.path);
    });
    const outcomes = await this.options.remote.put(uploads.map((u) => u.put));
    for (const [i, { put, content }] of uploads.entries()) {
      const outcome = outcomes[i];
      if (outcome?.kind === "stored") {
        await this.setBase(put.path, put.hash, content);
        this.summary.pushed++;
      } else if (outcome?.kind === "conflict") {
        await this.resolveUploadConflict(put.path);
      } else {
        this.fail(put.path, outcome?.reason ?? "no upload outcome");
      }
    }
  }

  /** Another device updated the path mid-sync: merge with it, then upload the result. */
  private async resolveUploadConflict(path: string): Promise<void> {
    const rv = await this.merge(path);
    if (!rv) return;
    const prepared = await this.preparePut(path, rv.hash);
    if (!prepared || prepared.put.hash === rv.hash) return;
    const [outcome] = await this.options.remote.put([prepared.put]);
    if (outcome?.kind === "stored") {
      await this.setBase(path, prepared.put.hash, prepared.content);
      this.summary.pushed++;
    } else if (outcome?.kind === "conflict") {
      this.fail(path, "changed again on another device; will retry next sync");
    } else {
      this.fail(path, outcome?.reason ?? "no upload outcome");
    }
  }
}
