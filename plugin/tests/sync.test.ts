import { describe, expect, it, vi } from "vitest";
import type { CloudSession } from "../src/auth";
import { newKeychain, sha1Hex } from "../src/core";
import { syncVault, type FileHashEntry, type SyncProgress } from "../src/engine";
import { ObsidianVaultIO } from "../src/obsidian-io";
import { CloudRemote, eqValue } from "../src/remotes/cloud";
import { S3Remote } from "../src/remotes/s3";
import { cloudKeys, Device, FakePostgrest, FakeS3 } from "./fakes";
import { App, MemoryAdapter } from "./obsidian-stub";

const session = (over: Partial<CloudSession> = {}): CloudSession => ({
  accessToken: "jwt-1",
  refreshToken: "r-0",
  userId: "u1",
  email: "me@example.com",
  vaultId: "v1",
  scope: "write",
  apiToken: null,
  ...over,
});

function cloud(api: FakePostgrest, over: Partial<CloudSession> = {}): CloudRemote {
  return new CloudRemote(api.http, session(over), () => undefined);
}

function s3(store: FakeS3): S3Remote {
  return new S3Remote(store.http, { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" });
}

describe("two devices over the hosted cloud", () => {
  it("converge on creates, edits, binaries and deletes", async () => {
    const api = new FakePostgrest();
    const a = new Device(cloudKeys());
    const b = new Device(cloudKeys());
    a.vault.setText("notes/one.md", "# One\n");
    a.vault.files.set("img/p.png", new Uint8Array([0, 1, 2, 255]));

    expect((await a.sync(cloud(api))).pushed).toBe(2);
    expect(api.rows.find((r) => r.path === "notes/one.md")?.content).toMatch(/^enc:v1:/);
    expect((await a.sync(cloud(api))).pushed).toBe(0);

    expect((await b.sync(cloud(api))).pulled).toBe(2);
    expect(b.vault.text("notes/one.md")).toBe("# One\n");
    expect([...(b.vault.files.get("img/p.png") ?? [])]).toEqual([0, 1, 2, 255]);

    b.vault.setText("notes/one.md", "# One\nfrom B\n");
    await b.sync(cloud(api));
    await a.sync(cloud(api));
    expect(a.vault.text("notes/one.md")).toBe("# One\nfrom B\n");

    a.vault.files.delete("img/p.png");
    expect((await a.sync(cloud(api))).deletedRemote).toBe(1);
    expect((await b.sync(cloud(api))).deletedLocal).toBe(1);
    expect(b.vault.trashed).toEqual(["img/p.png"]);
  });

  it("merges concurrent edits and keeps a conflict copy when they overlap", async () => {
    const api = new FakePostgrest();
    const a = new Device(cloudKeys());
    const b = new Device(cloudKeys());
    a.vault.setText("plan.md", "title\n\nbody\n\nfooter\n");
    await a.sync(cloud(api));
    await b.sync(cloud(api));

    a.vault.setText("plan.md", "TITLE A\n\nbody\n\nfooter\n");
    b.vault.setText("plan.md", "title\n\nbody\n\nfooter B\n");
    await a.sync(cloud(api));
    expect((await b.sync(cloud(api))).conflicts[0]?.resolution).toBe("auto_merged");
    await a.sync(cloud(api));
    expect(a.vault.text("plan.md")).toBe("TITLE A\n\nbody\n\nfooter B\n");

    a.vault.setText("plan.md", "from A\n\nbody\n\nfooter B\n");
    b.vault.setText("plan.md", "from B\n\nbody\n\nfooter B\n");
    await a.sync(cloud(api));
    const conflict = (await b.sync(cloud(api))).conflicts[0];
    expect(conflict?.resolution).toBe("conflict_copy_saved");
    expect(b.vault.text(conflict?.conflictCopy ?? "")).toBe("from B\n\nbody\n\nfooter B\n");
    expect(b.vault.text("plan.md")).toBe("from A\n\nbody\n\nfooter B\n");
  });

  it("renews an expired session and reports it", async () => {
    const api = new FakePostgrest();
    const renewed: CloudSession[] = [];
    const remote = new CloudRemote(api.http, session({ accessToken: "expired" }), (s) => renewed.push(s));
    const a = new Device(cloudKeys());
    a.vault.setText("a.md", "a");
    await a.sync(remote);
    expect(api.refreshCount).toBe(1);
    expect(renewed[0]?.accessToken).toBe("jwt-2");
  });

  it("read-only tokens pull but never push", async () => {
    const api = new FakePostgrest();
    const writer = new Device(cloudKeys());
    writer.vault.setText("a.md", "a");
    await writer.sync(cloud(api));

    const reader = new Device(cloudKeys());
    reader.vault.setText("draft.md", "local only");
    await expect(reader.sync(cloud(api, { scope: "read" }), false, true)).rejects.toMatchObject({ code: "PERMISSION_DENIED" });
    const summary = await reader.sync(cloud(api, { scope: "read" }));
    expect([summary.pulled, summary.pushed]).toEqual([1, 0]);
    expect(api.rows.some((r) => r.path === "draft.md")).toBe(false);
  });

  it("syncs and updates paths containing parentheses, commas, spaces and quotes without double-quoting eq filters", async () => {
    expect(eqValue("Resources/rush00_(colle00).en.pdf")).toBe("Resources/rush00_%28colle00%29.en.pdf");
    expect(eqValue("passaporte (1).pdf")).toBe("passaporte%20%281%29.pdf");
    expect(eqValue('a,b "c"\\d.md')).toBe("a%2Cb%20%22c%22%5Cd.md");

    const api = new FakePostgrest();
    const a = new Device(cloudKeys("pass"));
    const b = new Device(cloudKeys("pass"));
    const path = "Resources/rush00_(colle00).en.pdf";
    a.vault.files.set(path, new Uint8Array([1, 2, 3, 4]));
    expect((await a.sync(cloud(api))).pushed).toBe(1);
    expect((await b.sync(cloud(api))).pulled).toBe(1);
    expect([...(b.vault.files.get(path) ?? [])]).toEqual([1, 2, 3, 4]);

    b.vault.files.set(path, new Uint8Array([5, 6, 7, 8]));
    expect((await b.sync(cloud(api))).pushed).toBe(1);
    expect((await a.sync(cloud(api))).pulled).toBe(1);
    expect([...(a.vault.files.get(path) ?? [])]).toEqual([5, 6, 7, 8]);
  });
});

describe("migration and end-to-end encryption", () => {
  it("adopts a vault the CLI migrated to v2 and refuses wrong passphrases", async () => {
    const api = new FakePostgrest();
    // A vault-axi 1.1 vault: v1 rows under the server-derivable cloud key.
    const legacy = newKeychain({ cloudUserId: "u1", cloudVaultId: "v1" });
    for (const [path, body] of [["a.md", "# A\n"], ["b.md", "# B\n"]] as const) {
      api.rows.push({
        user_id: "u1",
        vault_id: "v1",
        path,
        content: legacy.encrypt(body, path),
        hash: sha1Hex(body),
        mtime_ms: 1,
        deleted: false,
        enc_key_id: legacy.activeKeyId(),
      });
    }

    const first = new Device(cloudKeys("correct horse"));
    const summary = await first.sync(cloud(api));
    expect(summary.pulled).toBe(2);
    expect(summary.pushed).toBe(2);
    const keyring = api.rows.find((r) => r.path === ".vault-axi/keyring.json");
    expect(keyring?.content).toContain('"keyId"');
    for (const row of api.rows.filter((r) => r.path.endsWith(".md"))) {
      expect(row.content).toMatch(/^enc:v2:k2_/);
      expect(row.enc_key_id).toBe(first.cachedKey?.id);
    }

    await expect(new Device(cloudKeys(null)).sync(cloud(api))).rejects.toMatchObject({ code: "ENCRYPTION_KEY_REQUIRED" });
    await expect(new Device(cloudKeys("nope")).sync(cloud(api))).rejects.toMatchObject({ code: "ENCRYPTION_KEY_MISMATCH" });

    const second = new Device(cloudKeys("correct horse"));
    expect((await second.sync(cloud(api))).pulled).toBe(2);
    expect(second.vault.text("b.md")).toBe("# B\n");
    expect(second.cachedKey).toEqual(first.cachedKey);
  });

  it("moving a ciphertext to another path is detected", async () => {
    const api = new FakePostgrest();
    const a = new Device(cloudKeys("p"));
    a.vault.setText("public.md", "fine");
    a.vault.setText("secret.md", "secret");
    await a.sync(cloud(api));
    const secret = api.rows.find((r) => r.path === "secret.md");
    const pub = api.rows.find((r) => r.path === "public.md");
    if (!secret || !pub) throw new Error("rows missing");
    pub.content = secret.content;
    pub.hash = "swapped";

    const b = new Device(cloudKeys("p"));
    const summary = await b.sync(cloud(api));
    expect(summary.failures.some((f) => f.startsWith("public.md:"))).toBe(true);
    expect(b.vault.text("public.md")).toBeUndefined();
  });
});

describe("S3", () => {
  it("syncs through a manifest and content-addressed objects", async () => {
    const store = new FakeS3();
    const keys = { passphrase: "pass", cloudUserId: null, cloudVaultId: null };
    const a = new Device(keys);
    const b = new Device(keys);
    a.vault.setText("a.md", "# A\n");
    await a.sync(s3(store));
    expect(store.objects.has("bucket/vault/.vault-axi/keyring.json")).toBe(true);
    expect(store.objects.get("bucket/vault/.vault-manifest.json")?.body).toContain('"version": 2');

    await b.sync(s3(store));
    expect(b.vault.text("a.md")).toBe("# A\n");
    b.vault.setText("a.md", "# A\nB\n");
    await b.sync(s3(store));
    await a.sync(s3(store));
    expect(a.vault.text("a.md")).toBe("# A\nB\n");
    const objects = [...store.objects.keys()].filter((k) => k.includes("/objects/"));
    expect(objects).toHaveLength(1);
  });

  it("retries when another device commits the manifest first", async () => {
    const store = new FakeS3();
    const keys = { passphrase: "pass", cloudUserId: null, cloudVaultId: null };
    const a = new Device(keys);
    const b = new Device(keys);
    a.vault.setText("a.md", "a");
    await a.sync(s3(store));
    await b.sync(s3(store));
    b.vault.setText("b.md", "from b");
    const bRemote = s3(store);
    await b.sync(bRemote);

    // Another commit lands between A's listing and A's manifest commit.
    const manifestKey = "bucket/vault/.vault-manifest.json";
    a.vault.setText("c.md", "from a");
    store.interceptOnce = {
      method: "PUT",
      run: () => {
        const m = store.objects.get(manifestKey);
        if (m) store.objects.set(manifestKey, { ...m, etag: '"someone-else"' });
      },
    };
    store.log.length = 0;
    const summary = await a.sync(s3(store));
    const listings = store.log.filter((l) => l === "GET /bucket/vault/.vault-manifest.json");
    expect(listings).toHaveLength(2);
    expect(summary.pushed).toBe(1);
    expect(a.vault.text("b.md")).toBe("from b");

    await b.sync(s3(store));
    expect(b.vault.text("c.md")).toBe("from a");
  });

  it("requires a passphrase", async () => {
    const d = new Device({ passphrase: null, cloudUserId: null, cloudVaultId: null });
    d.vault.setText("a.md", "a");
    await expect(d.sync(s3(new FakeS3()))).rejects.toMatchObject({ code: "ENCRYPTION_KEY_REQUIRED" });
  });
});

describe("progress accuracy, concurrency and caching", () => {
  it("reports exact in-flight upload files as S3 PUTs start and finish", async () => {
    const store = new FakeS3();
    const keys = { passphrase: "pass", cloudUserId: null, cloudVaultId: null };
    const device = new Device(keys);
    // Seed keyring first so the timed upload test focuses on file objects.
    await device.sync(s3(store));

    for (const name of ["a.md", "b.md", "c.md", "d.md", "e.md"]) {
      device.vault.setText(name, `body of ${name}\n`);
    }

    const pending: { url: string; release: () => void }[] = [];
    const gatedHttp = async (req: Parameters<typeof store.http>[0]) => {
      if (req.method === "PUT" && req.url.includes("/objects/")) {
        await new Promise<void>((resolve) => {
          pending.push({ url: req.url, release: resolve });
        });
      }
      return store.http(req);
    };

    const events: SyncProgress[] = [];
    const syncPromise = syncVault({
      io: device.vault,
      base: device.base,
      remote: new S3Remote(gatedHttp, { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" }),
      keys: device.keys,
      keyCache: { get: () => device.cachedKey, set: (k) => { device.cachedKey = k; } },
      pull: true,
      push: true,
      onProgress: (p) => {
        events.push({ ...p, active: [...p.active] });
      },
    });

    // Wait until the concurrency pool (4) fills with the first 4 object PUTs.
    await vi.waitFor(() => {
      expect(pending).toHaveLength(4);
    });
    expect(events.at(-1)).toMatchObject({ phase: "pushing", label: "Uploading", done: 0, total: 5 });
    expect(events.at(-1)?.active).toEqual(["a.md", "b.md", "c.md", "d.md"]);

    // Completing the first PUT decrements active, advances done to 1, and starts the 5th file.
    pending.shift()?.release();
    await vi.waitFor(() => {
      expect(pending).toHaveLength(4);
    });
    expect(events.at(-1)).toMatchObject({ phase: "pushing", label: "Uploading", done: 1, total: 5 });
    expect(events.at(-1)?.active).toEqual(["b.md", "c.md", "d.md", "e.md"]);

    // Release the remaining 4 PUTs.
    while (pending.length > 0) pending.shift()?.release();
    const summary = await syncPromise;
    expect(summary.pushed).toBe(5);
  });

  it("reports exact in-flight upload files during CloudRemote batches and conditional updates", async () => {
    const api = new FakePostgrest();
    const device = new Device(cloudKeys());
    for (const name of ["one.md", "two.md", "three.md"]) {
      device.vault.setText(name, `content ${name}\n`);
    }

    const pending: (() => void)[] = [];
    const gatedHttp = async (req: Parameters<typeof api.http>[0]) => {
      if (req.method === "POST" && req.url.includes("on_conflict")) {
        await new Promise<void>((resolve) => {
          pending.push(resolve);
        });
      }
      return api.http(req);
    };

    const events: SyncProgress[] = [];
    const syncPromise = syncVault({
      io: device.vault,
      base: device.base,
      remote: new CloudRemote(gatedHttp, session(), () => undefined),
      keys: device.keys,
      keyCache: { get: () => device.cachedKey, set: (k) => { device.cachedKey = k; } },
      pull: true,
      push: true,
      onProgress: (p) => {
        events.push({ ...p, active: [...p.active] });
      },
    });

    await vi.waitFor(() => {
      expect(pending).toHaveLength(1);
    });
    // While the batch POST is in flight, done must still be 0 and all 3 files in the batch are active.
    expect(events.at(-1)).toMatchObject({ phase: "pushing", label: "Uploading", done: 0, total: 3 });
    expect(events.at(-1)?.active).toEqual(["one.md", "three.md", "two.md"]);

    pending.shift()?.();
    const summary = await syncPromise;
    expect(summary.pushed).toBe(3);
  });

  it("downloads files concurrently with bounded concurrency and accurate active progress", async () => {
    const api = new FakePostgrest();
    const writer = new Device(cloudKeys());
    for (const name of ["f1.md", "f2.md", "f3.md", "f4.md", "f5.md"]) {
      writer.vault.setText(name, `text ${name}\n`);
    }
    await writer.sync(cloud(api));

    const reader = new Device(cloudKeys());
    const pending: (() => void)[] = [];
    const gatedHttp = async (req: Parameters<typeof api.http>[0]) => {
      if (req.method === "GET" && req.url.includes("path=eq.") && !req.url.includes("keyring")) {
        await new Promise<void>((resolve) => {
          pending.push(resolve);
        });
      }
      return api.http(req);
    };

    const events: SyncProgress[] = [];
    const syncPromise = syncVault({
      io: reader.vault,
      base: reader.base,
      remote: new CloudRemote(gatedHttp, session(), () => undefined),
      keys: reader.keys,
      keyCache: { get: () => reader.cachedKey, set: (k) => { reader.cachedKey = k; } },
      pull: true,
      push: true,
      concurrency: 2,
      onProgress: (p) => {
        events.push({ ...p, active: [...p.active] });
      },
    });

    await vi.waitFor(() => {
      expect(pending).toHaveLength(2);
    });
    expect(events.at(-1)).toMatchObject({ phase: "pulling", label: "Downloading", done: 0, total: 5 });
    expect(events.at(-1)?.active).toEqual(["f1.md", "f2.md"]);

    pending.shift()?.();
    await vi.waitFor(() => {
      expect(pending).toHaveLength(2);
    });
    expect(events.at(-1)).toMatchObject({ phase: "pulling", label: "Downloading", done: 1, total: 5 });
    expect(events.at(-1)?.active).toEqual(["f2.md", "f3.md"]);

    while (pending.length > 0) {
      pending.shift()?.();
      await Promise.resolve();
      await new Promise((r) => setTimeout(r, 0));
    }
    const summary = await syncPromise;
    expect(summary.pulled).toBe(5);
  });

  it("deduplicates concurrent 401 token refreshes on CloudRemote", async () => {
    const api = new FakePostgrest();
    const writer = new Device(cloudKeys());
    for (const name of ["a.md", "b.md", "c.md", "d.md"]) writer.vault.setText(name, name);
    await writer.sync(cloud(api));

    const reader = new Device(cloudKeys());
    // Expire the token right after list() and keyring fetch succeed so all 4 downloads hit 401 together.
    const origHttp = api.http;
    let gets = 0;
    const expiringHttp = (req: Parameters<typeof api.http>[0]) => {
      if (req.method === "GET" && req.url.includes("path=eq.") && !req.url.includes("keyring")) {
        gets++;
        if (gets === 1) api.validTokens.delete("jwt-1");
      }
      return origHttp(req);
    };

    const summary = await reader.sync(new CloudRemote(expiringHttp, session(), () => undefined));
    expect(summary.pulled).toBe(4);
    expect(api.refreshCount).toBe(1);
  });

  it("scans the vault once per sync and reuses the hash cache for unchanged files", async () => {
    const api = new FakePostgrest();
    const device = new Device(cloudKeys());
    const hashCache = new Map<string, FileHashEntry>();
    const oldMtime = 1_700_000_000_000;
    const now = oldMtime + 10_000;
    const mtimes = new Map<string, number>([
      ["a.md", oldMtime],
      ["b.md", oldMtime],
    ]);
    device.vault.setText("a.md", "alpha\n");
    device.vault.setText("b.md", "beta\n");

    let listCalls = 0;
    const readPaths: string[] = [];
    const countingIO = {
      listFiles: async () => {
        listCalls++;
        return (await device.vault.listFiles()).map((f) => ({ ...f, mtime: mtimes.get(f.path) ?? oldMtime }));
      },
      read: async (path: string) => {
        readPaths.push(path);
        return device.vault.read(path);
      },
      write: (path: string, bytes: Uint8Array) => device.vault.write(path, bytes),
      trash: (path: string) => device.vault.trash(path),
      mtime: (path: string) => Promise.resolve(mtimes.get(path) ?? oldMtime),
    };

    const run = () =>
      syncVault({
        io: countingIO,
        base: device.base,
        remote: cloud(api),
        keys: device.keys,
        keyCache: { get: () => device.cachedKey, set: (k) => { device.cachedKey = k; } },
        hashCache,
        pull: true,
        push: true,
        now: () => now,
      });

    // First sync: scans once (not twice for pull + push), reads both files to hash + upload.
    await run();
    expect(listCalls).toBe(1);

    // Second sync with no changes: scans once and reads ZERO files because hashCache hits.
    listCalls = 0;
    readPaths.length = 0;
    await run();
    expect(listCalls).toBe(1);
    expect(readPaths).toEqual([]);

    // Editing one file (new mtime) reads only that file on the next sync.
    device.vault.setText("a.md", "alpha edited\n");
    mtimes.set("a.md", oldMtime + 5_000);
    listCalls = 0;
    readPaths.length = 0;
    expect((await run()).pushed).toBe(1);
    expect(listCalls).toBe(1);
    expect(readPaths.every((p) => p === "a.md")).toBe(true);
  });

  it("handles concurrent ObsidianVaultIO writes into the same new nested folder", async () => {
    const adapter = new MemoryAdapter();
    let mkdirCount = 0;
    const origMkdir = adapter.mkdir.bind(adapter);
    adapter.mkdir = async (path: string) => {
      mkdirCount++;
      // Simulate async folder creation so concurrent writers race on the same directory.
      await new Promise((r) => setTimeout(r, 5));
      if (adapter.dirs.has(path)) throw new Error(`Folder already exists: ${path}`);
      return origMkdir(path);
    };
    const app = new App(adapter);
    const io = new ObsidianVaultIO(app as never, () => undefined);
    const bytes = new TextEncoder().encode("hello");

    await Promise.all([
      io.write("deep/nested/a.md", bytes),
      io.write("deep/nested/b.md", bytes),
      io.write("deep/nested/c.md", bytes),
      io.write("deep/nested/d.md", bytes),
    ]);
    expect(mkdirCount).toBe(2); // "deep" and "deep/nested", once each
    expect(adapter.text("deep/nested/d.md")).toBe("hello");
  });
});
