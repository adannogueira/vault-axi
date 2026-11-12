import { describe, expect, it } from "vitest";
import type { CloudSession } from "../src/auth";
import { newKeychain, sha1Hex } from "../src/core";
import { CloudRemote } from "../src/remotes/cloud";
import { S3Remote } from "../src/remotes/s3";
import { cloudKeys, Device, FakePostgrest, FakeS3 } from "./fakes";

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
