// Executes the core sync plan against a Remote with this device's vault and base store. A
// line-for-line counterpart of the CLI's `sync/engine.rs`; all decisions come from the core.
import type { BaseRecord, BaseStore } from "./base-store";
import { createFrameYielder, runConcurrent } from "./concurrency";
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
const DEFAULT_CONCURRENCY = 4;
/** Bounded upload chunks keep mobile RAM low, start transfers immediately, and checkpoint base state. */
const UPLOAD_CHUNK_MAX_ITEMS = 40;
const UPLOAD_CHUNK_MAX_BYTES = 1024 * 1024;
const ADOPT_BATCH_SIZE = 40;
/** Minimum plausible epoch-ms timestamp (filters out stubbed mtime values in tests). */
const REAL_MTIME_FLOOR_MS = 1_000_000_000_000;
/** Re-hash files touched within this window to guard against coarse filesystem mtime granularity. */
const RACY_CLEAN_WINDOW_MS = 2_000;
const SCAN_PROGRESS_INTERVAL_MS = 50;

export interface VaultFile {
  path: string;
  size: number;
  mtime?: number;
}

/** Cached content hash for an unchanged local file. */
export interface FileHashEntry {
  size: number;
  mtime: number;
  hash: string;
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

export type SyncPhase = "listing" | "unlocking" | "scanning" | "pulling" | "pushing" | "committing";

export interface SyncProgress {
  phase: SyncPhase;
  /** Short human-readable label for the current step, e.g. "Downloading", "Uploading". */
  label: string;
  /** Completed items in the current step (0..total). */
  done: number;
  /** Total items in the current step (0 when indeterminate). */
  total: number;
  /** Vault-relative paths currently being processed right now. */
  active: string[];
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
  /** Optional per-vault `(size, mtime) -> hash` cache to skip re-hashing unchanged files. */
  hashCache?: Map<string, FileHashEntry>;
  /** Max concurrent network transfers (defaults to 4). */
  concurrency?: number;
  onProgress?: (progress: SyncProgress) => void;
}

function clockMs(): number {
  return typeof performance !== "undefined" ? performance.now() : Date.now();
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

  const emit = (progress: SyncProgress): void => {
    options.onProgress?.(progress);
  };

  emit({ phase: "listing", label: "Listing remote", done: 0, total: 0, active: [] });
  const remoteFiles = await remote.list();

  emit({ phase: "unlocking", label: "Unlocking vault", done: 0, total: 0, active: [] });
  const keychain = await prepareKeychain(remote, options.keys, options.keyCache);
  await remote.adoptLegacyKeyIds?.(keychain, remoteFiles);

  const run = new Run(options, keychain, await options.base.load());
  await run.scanLocal();
  const pullPlan = planPull(run.localFiles(), run.baseEntries(), remoteFiles, pull);
  await run.executePull(pullPlan);
  let pushedAny = false;
  if (push && !remote.readOnly) {
    const pushPlan = planPush(run.localFiles(), run.baseEntries(), remoteFiles, keychain.activeKeyId());
    pushedAny = pushPlan.length > 0;
    await run.executePush(pushPlan);
  }
  if (pushedAny) {
    emit({ phase: "committing", label: "Finishing sync", done: 0, total: 0, active: [] });
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
  private readonly concurrency: number;
  private readonly local = new Map<string, LocalFile>();
  private readonly maybeYield = createFrameYielder();

  constructor(
    private readonly options: SyncOptions,
    private readonly kc: Keychain,
    private readonly base: Map<string, BaseRecord>,
  ) {
    this.now = (options.now ?? Date.now)();
    this.concurrency = Math.max(1, options.concurrency ?? DEFAULT_CONCURRENCY);
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

  localFiles(): LocalFile[] {
    return [...this.local.values()];
  }

  baseEntries(): BaseEntry[] {
    return [...this.base].map(([path, b]) => ({ path, remoteHash: b.remoteHash }));
  }

  private emit(progress: SyncProgress): void {
    this.options.onProgress?.(progress);
  }

  async scanLocal(): Promise<void> {
    this.emit({ phase: "scanning", label: "Scanning vault", done: 0, total: 0, active: [] });
    const files = (await this.options.io.listFiles()).filter((f) => isSyncable(f.path, f.size));
    const cache = this.options.hashCache;
    const seen = new Set<string>();
    this.local.clear();

    const total = files.length;
    let done = 0;
    let lastReport = 0;

    for (const f of files) {
      seen.add(f.path);
      const cached = cache?.get(f.path);
      const cacheUsable =
        cached !== undefined &&
        f.mtime !== undefined &&
        f.mtime > REAL_MTIME_FLOOR_MS &&
        this.now - f.mtime > RACY_CLEAN_WINDOW_MS &&
        cached.size === f.size &&
        cached.mtime === f.mtime;

      if (cacheUsable) {
        this.local.set(f.path, { path: f.path, hash: cached.hash });
        done++;
        continue;
      }

      const t = clockMs();
      if (lastReport === 0 || t - lastReport >= SCAN_PROGRESS_INTERVAL_MS) {
        this.emit({ phase: "scanning", label: "Scanning vault", done, total, active: [f.path] });
        lastReport = t;
      }
      await this.maybeYield();

      const content = await this.readLocal(f.path);
      if (content !== null) {
        const hash = sha1Hex(content);
        this.local.set(f.path, { path: f.path, hash });
        if (cache && f.mtime !== undefined) {
          cache.set(f.path, { size: f.size, mtime: f.mtime, hash });
        }
      }
      done++;
    }

    if (cache) {
      for (const path of cache.keys()) {
        if (!seen.has(path)) cache.delete(path);
      }
    }
    if (lastReport > 0) {
      this.emit({ phase: "scanning", label: "Scanning vault", done: total, total, active: [] });
    }
  }

  private async readLocal(path: string): Promise<string | null> {
    const bytes = await this.options.io.read(path);
    return bytes === null ? null : payloadFromFile(path, bytes);
  }

  private async writeLocal(path: string, content: string): Promise<boolean> {
    try {
      await this.options.io.write(path, payloadToFile(content));
      this.local.set(path, { path, hash: sha1Hex(content) });
      this.options.hashCache?.delete(path);
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

  private async setBaseMany(entries: readonly [string, BaseRecord][]): Promise<void> {
    if (entries.length === 0) return;
    if (this.options.base.putMany) {
      await this.options.base.putMany(entries);
    } else {
      for (const [path, record] of entries) await this.options.base.put(path, record);
    }
    for (const [path, record] of entries) this.base.set(path, record);
  }

  private async forgetBase(path: string): Promise<void> {
    await this.options.base.delete(path);
    this.base.delete(path);
  }

  private async forgetBaseMany(paths: readonly string[]): Promise<void> {
    if (paths.length === 0) return;
    if (this.options.base.deleteMany) {
      await this.options.base.deleteMany(paths);
    } else {
      for (const path of paths) await this.options.base.delete(path);
    }
    for (const path of paths) this.base.delete(path);
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

  /** Fetch and decrypt the live remote version; undecryptable payloads are reported, not written. */
  private async fetch(path: string): Promise<RemoteVersion | null> {
    const fetched = await this.options.remote.fetch(path);
    if (!fetched) return null;
    await this.maybeYield();
    try {
      return { content: this.kc.decrypt(fetched.content, path), hash: fetched.hash, mtimeMs: fetched.mtimeMs };
    } catch (e) {
      this.fail(path, e instanceof Error ? e.message : String(e));
      return null;
    }
  }

  async executePull(actions: Action[]): Promise<void> {
    const toForget: string[] = [];
    const toAdopt: Extract<Action, { action: "adoptBase" }>[] = [];
    const toDelete: Extract<Action, { action: "deleteLocal" }>[] = [];
    const transfers: Extract<Action, { action: "download" | "merge" }>[] = [];

    for (const a of actions) {
      switch (a.action) {
        case "adoptBase":
          toAdopt.push(a);
          break;
        case "download":
        case "merge":
          transfers.push(a);
          break;
        case "deleteLocal":
          toDelete.push(a);
          break;
        case "keepLocalOverRemoteDelete":
          toForget.push(a.path);
          this.conflict(
            a.path,
            "kept_local_edit_over_remote_delete",
            null,
            `'${a.path}' was deleted on another device but edited here; kept your version.`,
          );
          break;
        case "forgetBase":
          toForget.push(a.path);
          break;
        default:
          throw new Error(`not a pull action: ${a.action}`);
      }
    }

    await this.forgetBaseMany(toForget);

    if (toAdopt.length > 0) {
      let adopted = 0;
      let batch: [string, BaseRecord][] = [];
      let lastReport = 0;
      for (const a of toAdopt) {
        const t = clockMs();
        if (lastReport === 0 || t - lastReport >= SCAN_PROGRESS_INTERVAL_MS) {
          this.emit({ phase: "pulling", label: "Indexing synced files", done: adopted, total: toAdopt.length, active: [a.path] });
          lastReport = t;
        }
        await this.maybeYield();
        const content = await this.readLocal(a.path);
        if (content !== null) {
          batch.push([a.path, { remoteHash: a.hash, stored: this.kc.encrypt(content, LOCAL_CONTEXT) }]);
          if (batch.length >= ADOPT_BATCH_SIZE) {
            await this.setBaseMany(batch);
            batch = [];
          }
        }
        adopted++;
      }
      await this.setBaseMany(batch);
      this.emit({ phase: "pulling", label: "Indexing synced files", done: toAdopt.length, total: toAdopt.length, active: [] });
    }

    if (toDelete.length > 0) {
      const deletedBase: string[] = [];
      for (const [i, a] of toDelete.entries()) {
        this.emit({ phase: "pulling", label: "Removing", done: i, total: toDelete.length, active: [a.path] });
        await this.options.io.trash(a.path);
        this.local.delete(a.path);
        this.options.hashCache?.delete(a.path);
        this.summary.deletedLocal++;
        deletedBase.push(a.path);
      }
      await this.forgetBaseMany(deletedBase);
      this.emit({ phase: "pulling", label: "Removing", done: toDelete.length, total: toDelete.length, active: [] });
    }

    if (transfers.length > 0) {
      const hasDownload = transfers.some((a) => a.action === "download");
      const hasMerge = transfers.some((a) => a.action === "merge");
      const label = hasDownload && hasMerge ? "Pulling" : hasMerge ? "Merging" : "Downloading";
      const active = new Set<string>();
      let done = 0;
      const emitTransfer = () => {
        this.emit({ phase: "pulling", label, done, total: transfers.length, active: [...active] });
      };

      await runConcurrent(transfers, this.concurrency, async (a) => {
        active.add(a.path);
        emitTransfer();
        try {
          if (a.action === "download") {
            const rv = await this.fetch(a.path);
            if (rv && (await this.writeLocal(a.path, rv.content))) {
              await this.setBase(a.path, rv.hash, rv.content);
              this.summary.pulled++;
            }
          } else {
            await this.merge(a.path);
          }
        } finally {
          active.delete(a.path);
          done++;
          emitTransfer();
        }
      });
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
    await this.maybeYield();
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
    const uploads: Extract<Action, { action: "upload" }>[] = [];
    const deletes: Extract<Action, { action: "deleteRemote" }>[] = [];
    const restores: Extract<Action, { action: "restoreRemote" }>[] = [];

    for (const a of actions) {
      switch (a.action) {
        case "upload":
          uploads.push(a);
          break;
        case "deleteRemote":
          deletes.push(a);
          break;
        case "restoreRemote":
          restores.push(a);
          break;
        default:
          throw new Error(`not a push action: ${a.action}`);
      }
    }

    if (deletes.length > 0) {
      const active = new Set<string>();
      let done = 0;
      const emitDelete = () => {
        this.emit({ phase: "pushing", label: "Deleting remotely", done, total: deletes.length, active: [...active] });
      };
      await runConcurrent(deletes, this.concurrency, async (a) => {
        active.add(a.path);
        emitDelete();
        try {
          await this.options.remote.tombstone(a.path, this.now);
          await this.forgetBase(a.path);
          this.summary.deletedRemote++;
        } finally {
          active.delete(a.path);
          done++;
          emitDelete();
        }
      });
    }

    if (restores.length > 0) {
      const active = new Set<string>();
      let done = 0;
      const emitRestore = () => {
        this.emit({ phase: "pushing", label: "Restoring", done, total: restores.length, active: [...active] });
      };
      await runConcurrent(restores, this.concurrency, async (a) => {
        active.add(a.path);
        emitRestore();
        try {
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
        } finally {
          active.delete(a.path);
          done++;
          emitRestore();
        }
      });
    }

    if (uploads.length > 0) {
      await this.executeUploads(uploads);
    }
  }

  private async executeUploads(uploads: readonly Extract<Action, { action: "upload" }>[]): Promise<void> {
    const total = uploads.length;
    const active = new Set<string>();
    let done = 0;
    const emitUpload = () => {
      this.emit({ phase: "pushing", label: "Uploading", done, total, active: [...active] });
    };

    let cursor = 0;
    let lastPrepareReport = 0;
    while (cursor < uploads.length) {
      const chunk: { put: Put; content: string }[] = [];
      let chunkBytes = 0;

      while (
        cursor < uploads.length &&
        chunk.length < UPLOAD_CHUNK_MAX_ITEMS &&
        (chunk.length === 0 || chunkBytes < UPLOAD_CHUNK_MAX_BYTES)
      ) {
        const action = uploads[cursor++] as Extract<Action, { action: "upload" }>;
        active.add(action.path);
        const t = clockMs();
        if (lastPrepareReport === 0 || t - lastPrepareReport >= SCAN_PROGRESS_INTERVAL_MS) {
          emitUpload();
          lastPrepareReport = t;
        }
        const prepared = await this.preparePut(action.path, action.expectedRemoteHash);
        active.delete(action.path);
        if (prepared) {
          chunk.push(prepared);
          chunkBytes += prepared.put.content.length;
        } else {
          done++;
          emitUpload();
        }
      }

      if (chunk.length === 0) continue;

      const finishedByRemote = new Set<string>();
      const outcomes = await this.options.remote.put(
        chunk.map((u) => u.put),
        {
          onStart: (paths) => {
            for (const p of paths) active.add(p);
            emitUpload();
          },
          onDone: (paths) => {
            for (const p of paths) {
              active.delete(p);
              if (!finishedByRemote.has(p)) {
                finishedByRemote.add(p);
                done++;
              }
            }
            emitUpload();
          },
        },
      );

      for (const { put } of chunk) {
        active.delete(put.path);
        if (!finishedByRemote.has(put.path)) {
          finishedByRemote.add(put.path);
          done++;
        }
      }
      emitUpload();

      const baseUpdates: [string, BaseRecord][] = [];
      const conflicts: string[] = [];
      for (const [i, { put, content }] of chunk.entries()) {
        const outcome = outcomes[i];
        if (outcome?.kind === "stored") {
          await this.maybeYield();
          baseUpdates.push([put.path, { remoteHash: put.hash, stored: this.kc.encrypt(content, LOCAL_CONTEXT) }]);
          this.summary.pushed++;
        } else if (outcome?.kind === "conflict") {
          conflicts.push(put.path);
        } else {
          this.fail(put.path, outcome?.reason ?? "no upload outcome");
        }
      }
      await this.setBaseMany(baseUpdates);

      for (const path of conflicts) {
        active.add(path);
        emitUpload();
        try {
          await this.resolveUploadConflict(path);
        } finally {
          active.delete(path);
          emitUpload();
        }
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
