// In-memory stand-ins for the vault, the remotes' HTTP APIs and a device.
import { MemoryBaseStore } from "../src/base-store";
import type { DataKey } from "../src/core";
import { syncVault, type SyncSummary, type VaultFile, type VaultIO } from "../src/engine";
import type { Http, HttpRequest, HttpResponse } from "../src/http";
import type { KeySettings } from "../src/keys";
import type { Remote } from "../src/remote";

export class MemoryVault implements VaultIO {
  readonly files = new Map<string, Uint8Array>();
  readonly trashed: string[] = [];

  setText(path: string, text: string): void {
    this.files.set(path, new TextEncoder().encode(text));
  }
  text(path: string): string | undefined {
    const bytes = this.files.get(path);
    return bytes && new TextDecoder().decode(bytes);
  }
  listFiles(): Promise<VaultFile[]> {
    return Promise.resolve([...this.files].map(([path, b]) => ({ path, size: b.length })));
  }
  read(path: string): Promise<Uint8Array | null> {
    return Promise.resolve(this.files.get(path) ?? null);
  }
  write(path: string, bytes: Uint8Array): Promise<void> {
    this.files.set(path, bytes);
    return Promise.resolve();
  }
  trash(path: string): Promise<void> {
    this.files.delete(path);
    this.trashed.push(path);
    return Promise.resolve();
  }
  mtime(): Promise<number | null> {
    return Promise.resolve(1);
  }
}

export class Device {
  readonly vault = new MemoryVault();
  readonly base = new MemoryBaseStore();
  cachedKey: DataKey | null = null;

  constructor(public keys: KeySettings) {}

  sync(remote: Remote, pull = true, push = true): Promise<SyncSummary> {
    return syncVault({
      io: this.vault,
      base: this.base,
      remote,
      keys: this.keys,
      keyCache: {
        get: () => this.cachedKey,
        set: (k) => {
          this.cachedKey = k;
        },
      },
      pull,
      push,
      now: () => 1_700_000_000_000,
    });
  }
}

export const cloudKeys = (passphrase: string | null = null): KeySettings => ({
  passphrase,
  cloudUserId: "u1",
  cloudVaultId: "v1",
});

function response(status: number, body: unknown = "", headers: Record<string, string> = {}): HttpResponse {
  return { status, headers, text: typeof body === "string" ? body : JSON.stringify(body) };
}

/** S3 semantics the backend relies on: ETags and conditional PUTs. */
export class FakeS3 {
  readonly objects = new Map<string, { body: string; etag: string }>();
  private version = 0;
  /** Runs once before the next request matching `method`, to simulate another device. */
  interceptOnce: { method: string; run: () => void } | null = null;

  readonly log: string[] = [];

  readonly http: Http = (req: HttpRequest) => {
    this.log.push(`${req.method} ${new URL(req.url).pathname}`);
    if (this.interceptOnce?.method === req.method) {
      const { run } = this.interceptOnce;
      this.interceptOnce = null;
      run();
    }
    const key = new URL(req.url).pathname.slice(1);
    const current = this.objects.get(key);
    switch (req.method) {
      case "GET":
        return Promise.resolve(current ? response(200, current.body, { etag: current.etag }) : response(404));
      case "PUT": {
        const ifMatch = req.headers?.["If-Match"];
        const ifNone = req.headers?.["If-None-Match"];
        if ((ifNone && current) || (ifMatch !== undefined && ifMatch !== current?.etag)) {
          return Promise.resolve(response(412));
        }
        const etag = `"${++this.version}"`;
        this.objects.set(key, { body: req.body ?? "", etag });
        return Promise.resolve(response(200, "", { etag }));
      }
      case "DELETE":
        this.objects.delete(key);
        return Promise.resolve(response(204));
      default:
        return Promise.resolve(response(405));
    }
  };
}

interface NoteRow {
  user_id: string;
  vault_id: string;
  path: string;
  content: string;
  hash: string;
  mtime_ms: number;
  deleted: boolean;
  enc_key_id: string;
}

/** Enough of PostgREST + Supabase auth for the `vault_notes` protocol. */
export class FakePostgrest {
  readonly rows: NoteRow[] = [];
  validTokens = new Set(["jwt-1"]);
  refreshCount = 0;
  requests: HttpRequest[] = [];

  readonly http: Http = (req: HttpRequest) => {
    this.requests.push(req);
    const url = new URL(req.url);
    if (url.pathname === "/auth/v1/token" && url.searchParams.get("grant_type") === "refresh_token") {
      this.refreshCount++;
      const token = `jwt-${this.refreshCount + 1}`;
      this.validTokens.add(token);
      return Promise.resolve(response(200, { access_token: token, refresh_token: `r-${this.refreshCount}` }));
    }
    const auth = req.headers?.Authorization?.replace("Bearer ", "") ?? "";
    if (!this.validTokens.has(auth)) return Promise.resolve(response(401, { message: "JWT expired" }));
    if (url.pathname !== "/rest/v1/vault_notes") return Promise.resolve(response(404));

    const filters = [...url.searchParams].filter(([, v]) => v.startsWith("eq."));
    const match = (r: NoteRow) =>
      filters.every(([k, v]) => String(r[k as keyof NoteRow]) === v.slice(3).replace(/^"(.*)"$/, "$1"));
    const pick = (r: NoteRow) => {
      const cols = url.searchParams.get("select")?.split(",");
      return cols ? Object.fromEntries(cols.map((c) => [c, r[c as keyof NoteRow]])) : r;
    };

    switch (req.method) {
      case "GET": {
        const offset = Number(url.searchParams.get("offset") ?? 0);
        const limit = Number(url.searchParams.get("limit") ?? 1e9);
        const found = this.rows.filter(match).sort((a, b) => a.path.localeCompare(b.path));
        return Promise.resolve(response(200, found.slice(offset, offset + limit).map(pick)));
      }
      case "POST": {
        const body = JSON.parse(req.body ?? "null") as Partial<NoteRow> | Partial<NoteRow>[];
        const upsert = url.searchParams.has("on_conflict");
        for (const r of Array.isArray(body) ? body : [body]) {
          const row: NoteRow = { user_id: "u1", enc_key_id: "", deleted: false, ...r } as NoteRow;
          const i = this.rows.findIndex((x) => x.vault_id === row.vault_id && x.path === row.path);
          if (i >= 0 && !upsert) return Promise.resolve(response(409, { message: "duplicate key" }));
          if (i >= 0) this.rows[i] = { ...this.rows[i], ...row } as NoteRow;
          else this.rows.push(row);
        }
        return Promise.resolve(response(201));
      }
      case "PATCH": {
        const patch = JSON.parse(req.body ?? "{}") as Partial<NoteRow>;
        const updated = this.rows.filter(match);
        for (const r of updated) Object.assign(r, patch);
        const representation = req.headers?.Prefer === "return=representation";
        return Promise.resolve(response(200, representation ? updated.map(pick) : ""));
      }
      default:
        return Promise.resolve(response(405));
    }
  };
}
