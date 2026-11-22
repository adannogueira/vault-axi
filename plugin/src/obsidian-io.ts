// VaultIO over Obsidian's APIs. Indexed vault files go through `Vault` so editors and the
// metadata cache update; `.obsidian/` and hidden files go through the raw adapter.
import { normalizePath, requestUrl, type App, type TFile } from "obsidian";
import { runConcurrent } from "./concurrency";
import { isIgnoredDir, isSyncable } from "./core";
import type { VaultFile, VaultIO } from "./engine";
import { lowerCaseHeaders, type Http } from "./http";

const LIST_CONCURRENCY = 8;
const STAT_CONCURRENCY = 16;

function toArrayBuffer(bytes: Uint8Array): ArrayBuffer {
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
}

const isHidden = (path: string): boolean => path.split("/").some((s) => s.startsWith("."));

export class ObsidianVaultIO implements VaultIO {
  private readonly pendingDirs = new Map<string, Promise<void>>();

  constructor(
    private readonly app: App,
    /** Called before every write or delete, so the plugin can ignore its own file events. */
    private readonly beforeWrite: (path: string) => void,
  ) {}

  private get adapter() {
    return this.app.vault.adapter;
  }

  async listFiles(): Promise<VaultFile[]> {
    const out: VaultFile[] = [];
    const needStat: { raw: string; path: string }[] = [];
    const walk = async (dir: string): Promise<void> => {
      const listed = await this.adapter.list(dir);
      const subdirs = listed.folders.filter((folder) => !isIgnoredDir(folder.split("/").pop() ?? ""));
      for (const file of listed.files) {
        const path = normalizePath(file);
        if (!isSyncable(path, 0)) continue;
        // Indexed vault files already carry size & mtime in memory; skip redundant adapter.stat calls.
        const indexed = this.app.vault.getFileByPath(path) as Partial<TFile> | null;
        if (indexed?.stat) {
          out.push({ path, size: indexed.stat.size, mtime: indexed.stat.mtime });
        } else {
          needStat.push({ raw: file, path });
        }
      }
      await runConcurrent(subdirs, LIST_CONCURRENCY, walk);
    };
    await walk("/");
    await runConcurrent(needStat, STAT_CONCURRENCY, async ({ raw, path }) => {
      const stat = await this.adapter.stat(raw);
      if (stat?.type === "file") out.push({ path, size: stat.size, mtime: stat.mtime });
    });
    return out;
  }

  async read(path: string): Promise<Uint8Array | null> {
    try {
      return new Uint8Array(await this.adapter.readBinary(path));
    } catch {
      return null;
    }
  }

  private async ensureDir(dir: string, viaAdapter: boolean): Promise<void> {
    const inflight = this.pendingDirs.get(dir);
    if (inflight) {
      await inflight;
      return;
    }
    const task = (async () => {
      if (await this.adapter.exists(dir)) return;
      try {
        if (viaAdapter) await this.adapter.mkdir(dir);
        else await this.app.vault.createFolder(dir);
      } catch (e) {
        if (!(await this.adapter.exists(dir))) throw e;
      }
    })().finally(() => {
      this.pendingDirs.delete(dir);
    });
    this.pendingDirs.set(dir, task);
    await task;
  }

  private async ensureParent(path: string, viaAdapter: boolean): Promise<void> {
    const parts = path.split("/").slice(0, -1);
    for (let i = 1; i <= parts.length; i++) {
      await this.ensureDir(parts.slice(0, i).join("/"), viaAdapter);
    }
  }

  async write(path: string, bytes: Uint8Array): Promise<void> {
    this.beforeWrite(path);
    const data = toArrayBuffer(bytes);
    const file: TFile | null = this.app.vault.getFileByPath(path);
    if (file) {
      await this.app.vault.modifyBinary(file, data);
      return;
    }
    const raw = isHidden(path);
    await this.ensureParent(path, raw);
    if (raw) await this.adapter.writeBinary(path, data);
    else await this.app.vault.createBinary(path, data);
  }

  async trash(path: string): Promise<void> {
    this.beforeWrite(path);
    const file = this.app.vault.getAbstractFileByPath(path);
    if (file) await this.app.fileManager.trashFile(file);
    else if (await this.adapter.exists(path)) await this.adapter.trashLocal(path);
  }

  async mtime(path: string): Promise<number | null> {
    const indexed = this.app.vault.getFileByPath(path) as Partial<TFile> | null;
    if (indexed?.stat) return indexed.stat.mtime;
    return (await this.adapter.stat(path))?.mtime ?? null;
  }
}

/** HTTP through Obsidian's `requestUrl`, which isn't subject to CORS on desktop or mobile. */
export const obsidianHttp: Http = async ({ url, method, headers, body }) => {
  const res = await requestUrl({ url, method, ...(headers ? { headers } : {}), ...(body !== undefined ? { body } : {}), throw: false });
  let text = "";
  try {
    text = res.text;
  } catch {
    // empty or binary body
  }
  return { status: res.status, headers: lowerCaseHeaders(res.headers), text };
};
