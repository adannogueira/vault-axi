// S3 / R2 over plain HTTP: a manifest plus content-addressed objects, committed with
// compare-and-swap. Same protocol as the CLI's `sync/s3.rs`.
import {
  keyringPath,
  manifestPath,
  manifestToJson,
  objectKey,
  parseManifest,
  type Manifest,
  type ManifestEntry,
  type RemoteFile,
} from "../core";
import { describe, isOk, type Http } from "../http";
import { REMOTE_CHANGED, SyncError, type Fetched, type Put, type PutOutcome, type Remote, type StoredKeyring } from "../remote";

export interface S3Location {
  endpoint: string;
  bucket: string;
  prefix: string;
}

export function s3BaseUrl({ endpoint, bucket, prefix }: S3Location): string {
  const ep = endpoint.replace(/\/+$/, "");
  const pre = prefix.replace(/^\/+|\/+$/g, "");
  return pre ? `${ep}/${bucket}/${pre}` : `${ep}/${bucket}`;
}

export class S3Remote implements Remote {
  readonly name = "s3";
  readonly readOnly = false;
  private readonly base: string;
  private manifest: Manifest = { version: 2, entries: {} };
  private etag: string | null = null;
  private dirty = false;
  private superseded = new Set<string>();

  constructor(
    private readonly http: Http,
    location: S3Location,
  ) {
    this.base = s3BaseUrl(location);
  }

  private url(key: string): string {
    return `${this.base}/${key}`;
  }

  private replaceEntry(path: string, entry: ManifestEntry): void {
    const old = this.manifest.entries[path];
    if (old?.object) this.superseded.add(old.object);
    this.manifest.entries[path] = entry;
    this.dirty = true;
  }

  async list(): Promise<RemoteFile[]> {
    this.dirty = false;
    this.superseded.clear();
    const res = await this.http({ url: this.url(manifestPath()), method: "GET" });
    if (res.status === 404 || res.status === 403) {
      this.etag = null;
      this.manifest = { version: 2, entries: {} };
    } else if (isOk(res)) {
      this.etag = res.headers.etag ?? null;
      this.manifest = parseManifest(res.text);
    } else {
      throw new SyncError("SYNC_ERROR", `Fetching the S3 manifest failed (${describe(res)})`);
    }
    return Object.entries(this.manifest.entries).map(([path, e]) => ({
      path,
      hash: e.hash,
      deleted: e.deleted,
      keyId: e.keyId,
    }));
  }

  async fetch(path: string): Promise<Fetched | null> {
    const entry = this.manifest.entries[path];
    if (!entry || entry.deleted || !entry.object) return null;
    const res = await this.http({ url: this.url(entry.object), method: "GET" });
    if (res.status === 404) throw new SyncError(REMOTE_CHANGED, `S3 object for '${path}' is missing; another device is mid-sync`);
    if (!isOk(res)) throw new SyncError("SYNC_ERROR", `Fetching '${path}' failed (${describe(res)})`);
    return { content: res.text, hash: entry.hash, mtimeMs: entry.mtime };
  }

  async put(items: Put[]): Promise<PutOutcome[]> {
    const outcomes: PutOutcome[] = [];
    for (const put of items) {
      const live = this.manifest.entries[put.path];
      const liveHash = live && !live.deleted ? live.hash : null;
      if (liveHash !== put.expectedRemoteHash) {
        outcomes.push({ kind: "conflict" });
        continue;
      }
      const object = objectKey(put.path, put.hash, put.keyId);
      const res = await this.http({
        url: this.url(object),
        method: "PUT",
        headers: { "Content-Type": "text/plain; charset=utf-8" },
        body: put.content,
      });
      if (!isOk(res)) {
        outcomes.push({ kind: "failed", reason: describe(res) });
        continue;
      }
      this.superseded.delete(object);
      this.replaceEntry(put.path, { hash: put.hash, keyId: put.keyId, deleted: false, mtime: put.mtimeMs, object });
      outcomes.push({ kind: "stored" });
    }
    return outcomes;
  }

  async tombstone(path: string, mtimeMs: number): Promise<void> {
    this.replaceEntry(path, { hash: "", keyId: "", deleted: true, mtime: mtimeMs });
    return Promise.resolve();
  }

  async finish(): Promise<void> {
    if (!this.dirty) return;
    const condition: Record<string, string> = this.etag ? { "If-Match": this.etag } : { "If-None-Match": "*" };
    const res = await this.http({
      url: this.url(manifestPath()),
      method: "PUT",
      headers: { "Content-Type": "application/json", ...condition },
      body: manifestToJson(this.manifest),
    });
    if (res.status === 412 || res.status === 409) throw new SyncError(REMOTE_CHANGED, "The S3 manifest changed during sync");
    if (!isOk(res)) throw new SyncError("SYNC_ERROR", `Committing the S3 manifest failed (${describe(res)})`);
    this.dirty = false;
    const referenced = new Set(Object.values(this.manifest.entries).flatMap((e) => (e.object ? [e.object] : [])));
    for (const object of this.superseded) {
      if (!referenced.has(object)) await this.http({ url: this.url(object), method: "DELETE" }).catch(() => undefined);
    }
    this.superseded.clear();
  }

  async fetchKeyring(): Promise<StoredKeyring | null> {
    const res = await this.http({ url: this.url(keyringPath()), method: "GET" });
    if (res.status === 404 || res.status === 403) return null;
    if (!isOk(res)) throw new SyncError("SYNC_ERROR", `Fetching the vault keyring failed (${describe(res)})`);
    return { keyring: res.text, version: res.headers.etag ?? "" };
  }

  async createKeyring(keyring: string): Promise<boolean> {
    const res = await this.http({
      url: this.url(keyringPath()),
      method: "PUT",
      headers: { "Content-Type": "application/json", "If-None-Match": "*" },
      body: keyring,
    });
    if (res.status === 412 || res.status === 409) return false;
    if (!isOk(res)) throw new SyncError("SYNC_ERROR", `Creating the vault keyring failed (${describe(res)})`);
    return true;
  }

  async replaceKeyring(version: string, keyring: string): Promise<void> {
    const res = await this.http({
      url: this.url(keyringPath()),
      method: "PUT",
      headers: { "Content-Type": "application/json", "If-Match": version },
      body: keyring,
    });
    if (res.status === 412) throw new SyncError("KEYRING_CHANGED", "The vault keyring changed on another device");
    if (!isOk(res)) throw new SyncError("SYNC_ERROR", `Updating the vault keyring failed (${describe(res)})`);
  }
}
