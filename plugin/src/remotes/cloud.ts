// Hosted cloud: the `vault_notes` table behind PostgREST. Same protocol as the CLI's
// `sync/cloud.rs`.
import { HOSTED_ANON_KEY, HOSTED_URL, renewSession, type CloudSession } from "../auth";
import { keyringPath, sha1Hex, type Keychain, type RemoteFile } from "../core";
import { describe, isOk, type Http, type HttpResponse } from "../http";
import { SyncError, type Fetched, type Put, type PutOutcome, type Remote, type StoredKeyring } from "../remote";

const PAGE_SIZE = 1000;
const BATCH_ROWS = 20;
const BATCH_BYTES = 256_000;
const BATCHABLE_ROW_BYTES = 48_000;
const LEGACY_KEY_ID = "enc-v1";

interface Row {
  path: string;
  hash?: string;
  mtime_ms?: number;
  deleted?: boolean;
  enc_key_id?: string;
  content?: string;
}

const RESERVED = /[,()"\\]/;

/** Percent-encode everything except unreserved characters and `/` (as the CLI does). */
export function percentEncode(value: string): string {
  let out = "";
  for (const byte of new TextEncoder().encode(value)) {
    const c = String.fromCharCode(byte);
    out += /[A-Za-z0-9\-_.~/]/.test(c) ? c : `%${byte.toString(16).toUpperCase().padStart(2, "0")}`;
  }
  return out;
}

/** Encode a value for a PostgREST `eq.` filter, quoting reserved characters. */
export function eqValue(value: string): string {
  return percentEncode(RESERVED.test(value) ? `"${value.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"` : value);
}

export class CloudRemote implements Remote {
  readonly name = "vault-cloud";
  private hasKeyIdColumn = true;

  constructor(
    private readonly http: Http,
    private session: CloudSession,
    private readonly onSessionChange: (session: CloudSession) => void,
  ) {}

  get readOnly(): boolean {
    return this.session.scope.toLowerCase() === "read";
  }

  private async request(method: string, url: string, body?: unknown, prefer?: string): Promise<HttpResponse> {
    const send = () => {
      const headers: Record<string, string> = {
        apikey: HOSTED_ANON_KEY,
        Authorization: `Bearer ${this.session.accessToken}`,
      };
      if (body !== undefined) headers["Content-Type"] = "application/json";
      if (prefer) headers.Prefer = prefer;
      return this.http({ url, method, headers, ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
    };
    let res = await send();
    if (res.status === 401) {
      const renewed = await renewSession(this.http, this.session);
      if (renewed) {
        this.session = renewed;
        this.onSessionChange(renewed);
        res = await send();
      }
    }
    if (res.status === 401) {
      throw new SyncError("AUTH_ERROR", "The hosted-cloud session expired; sign in again in the plugin settings");
    }
    return res;
  }

  private table(): string {
    return `${HOSTED_URL}/rest/v1/vault_notes`;
  }

  private rowUrl(path: string): string {
    return `${this.table()}?vault_id=eq.${percentEncode(this.session.vaultId)}&path=eq.${eqValue(path)}`;
  }

  /** Columns a write sets; `keyId: null` leaves `enc_key_id` out. */
  private fields(content: string, hash: string, mtimeMs: number, keyId: string | null): Record<string, unknown> {
    const fields: Record<string, unknown> = {
      content,
      hash,
      mtime_ms: mtimeMs,
      deleted: false,
      updated_at: new Date().toISOString(),
    };
    if (this.hasKeyIdColumn && keyId !== null) fields.enc_key_id = keyId;
    return fields;
  }

  /** A full row for inserts and upserts: identity columns plus `fields`. */
  private row(path: string, fields: Record<string, unknown>): Record<string, unknown> {
    const row: Record<string, unknown> = { vault_id: this.session.vaultId, path, ...fields };
    if (this.session.userId) row.user_id = this.session.userId;
    return row;
  }

  private putRow(p: Put): Record<string, unknown> {
    return this.row(p.path, this.fields(p.content, p.hash, p.mtimeMs, p.keyId));
  }

  private async upsert(rows: unknown): Promise<string | null> {
    const res = await this.request(
      "POST",
      `${this.table()}?on_conflict=user_id,vault_id,path`,
      rows,
      "resolution=merge-duplicates",
    );
    return isOk(res) ? null : describe(res);
  }

  private async conditionalUpdate(put: Put, expected: string): Promise<PutOutcome> {
    const payload = this.fields(put.content, put.hash, put.mtimeMs, put.keyId);
    const res = await this.request(
      "PATCH",
      `${this.rowUrl(put.path)}&hash=eq.${percentEncode(expected)}&select=path`,
      payload,
      "return=representation",
    );
    if (!isOk(res)) return { kind: "failed", reason: describe(res) };
    return (JSON.parse(res.text) as unknown[]).length > 0 ? { kind: "stored" } : { kind: "conflict" };
  }

  async list(): Promise<RemoteFile[]> {
    const files: RemoteFile[] = [];
    for (let offset = 0; ; ) {
      const cols = this.hasKeyIdColumn ? "path,hash,mtime_ms,deleted,enc_key_id" : "path,hash,mtime_ms,deleted";
      const url = `${this.table()}?vault_id=eq.${percentEncode(this.session.vaultId)}&select=${cols}&order=path.asc&limit=${PAGE_SIZE}&offset=${offset}`;
      const res = await this.request("GET", url);
      if (res.status === 400 && this.hasKeyIdColumn) {
        this.hasKeyIdColumn = false;
        continue;
      }
      if (!isOk(res)) throw new SyncError("SYNC_ERROR", `Listing the cloud vault failed (${describe(res)})`);
      const page = JSON.parse(res.text) as Row[];
      for (const r of page) {
        files.push({ path: r.path, hash: r.hash ?? "", deleted: r.deleted ?? false, keyId: r.enc_key_id ?? "" });
      }
      if (page.length < PAGE_SIZE) return files;
      offset += page.length;
    }
  }

  /** Stamp rows from pre-fingerprint versions if they already decrypt under the active key. */
  async adoptLegacyKeyIds(keychain: Keychain, files: RemoteFile[]): Promise<void> {
    const sample = files.find((f) => !f.deleted && f.keyId === LEGACY_KEY_ID);
    if (!sample) return;
    const raw = await this.fetch(sample.path);
    if (!raw || !keychain.isCurrent(raw.content, sample.path)) return;
    const keyId = keychain.activeKeyId();
    const res = await this.request(
      "PATCH",
      `${this.table()}?vault_id=eq.${percentEncode(this.session.vaultId)}&enc_key_id=eq.${LEGACY_KEY_ID}`,
      { enc_key_id: keyId },
    );
    if (isOk(res)) {
      for (const f of files) if (f.keyId === LEGACY_KEY_ID) f.keyId = keyId;
    }
  }

  async fetch(path: string): Promise<Fetched | null> {
    const res = await this.request("GET", `${this.rowUrl(path)}&select=path,content,hash,mtime_ms,deleted`);
    if (!isOk(res)) throw new SyncError("SYNC_ERROR", `Fetching '${path}' failed (${describe(res)})`);
    const row = (JSON.parse(res.text) as Row[]).find((r) => !r.deleted);
    return row ? { content: row.content ?? "", hash: row.hash ?? "", mtimeMs: row.mtime_ms ?? 0 } : null;
  }

  async put(items: Put[]): Promise<PutOutcome[]> {
    const outcomes: PutOutcome[] = items.map(() => ({ kind: "failed", reason: "not attempted" }));
    const groups: number[][] = [];
    let groupBytes = 0;
    const conditional: number[] = [];
    items.forEach((p, i) => {
      const batchable =
        (p.expectedRemoteHash === null || p.expectedRemoteHash === p.hash) && p.content.length <= BATCHABLE_ROW_BYTES;
      if (!batchable) {
        conditional.push(i);
        return;
      }
      const last = groups.at(-1);
      if (last && last.length < BATCH_ROWS && groupBytes + p.content.length <= BATCH_BYTES) {
        last.push(i);
        groupBytes += p.content.length;
      } else {
        groups.push([i]);
        groupBytes = p.content.length;
      }
    });

    for (const group of groups) {
      const rows = group.map((i) => this.putRow(items[i] as Put));
      if ((await this.upsert(rows)) === null) {
        for (const i of group) outcomes[i] = { kind: "stored" };
        continue;
      }
      for (const [n, i] of group.entries()) {
        const error = await this.upsert(rows[n]);
        outcomes[i] = error === null ? { kind: "stored" } : { kind: "failed", reason: error };
      }
    }
    for (const i of conditional) {
      const p = items[i] as Put;
      if (p.expectedRemoteHash === null) {
        const error = await this.upsert(this.putRow(p));
        outcomes[i] = error === null ? { kind: "stored" } : { kind: "failed", reason: error };
      } else {
        outcomes[i] = await this.conditionalUpdate(p, p.expectedRemoteHash);
      }
    }
    return outcomes;
  }

  async tombstone(path: string, mtimeMs: number): Promise<void> {
    const error = await this.upsert(this.row(path, { ...this.fields("", "", mtimeMs, null), deleted: true }));
    if (error !== null) throw new SyncError("SYNC_ERROR", `Deleting '${path}' remotely failed (${error})`);
  }

  async finish(): Promise<void> {
    // Every write is committed immediately.
  }

  async fetchKeyring(): Promise<StoredKeyring | null> {
    const row = await this.fetch(keyringPath());
    return row ? { keyring: row.content, version: row.hash } : null;
  }

  async createKeyring(keyring: string): Promise<boolean> {
    const res = await this.request(
      "POST",
      this.table(),
      this.row(keyringPath(), this.fields(keyring, sha1Hex(keyring), Date.now(), "keyring")),
    );
    if (res.status === 409) return false;
    if (!isOk(res)) throw new SyncError("SYNC_ERROR", `Creating the vault keyring failed (${describe(res)})`);
    return true;
  }

  async replaceKeyring(version: string, keyring: string): Promise<void> {
    const outcome = await this.conditionalUpdate(
      { path: keyringPath(), content: keyring, hash: sha1Hex(keyring), mtimeMs: Date.now(), keyId: "keyring", expectedRemoteHash: version },
      version,
    );
    if (outcome.kind === "conflict") throw new SyncError("KEYRING_CHANGED", "The vault keyring changed on another device");
    if (outcome.kind === "failed") throw new SyncError("SYNC_ERROR", `Updating the vault keyring failed (${outcome.reason})`);
  }
}
