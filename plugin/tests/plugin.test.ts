import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import VaultAxiSyncPlugin, { formatStatusBarText, formatStatusText } from "../src/main";
import { S3Remote } from "../src/remotes/s3";
import { Device, FakeS3 } from "./fakes";
import { App, MemoryAdapter, notices, setRequestHandler } from "./obsidian-stub";

class MemoryStorage {
  private readonly items = new Map<string, string>();
  getItem(k: string) {
    return this.items.get(k) ?? null;
  }
  setItem(k: string, v: string) {
    this.items.set(k, v);
  }
  removeItem(k: string) {
    this.items.delete(k);
  }
}

/** Let debounce timers fire and wait for the sync they start (and any follow-up) to finish. */
/** Seed what `loadData()` returns (the stubbed Plugin keeps it in memory). */
function seedData(plugin: VaultAxiSyncPlugin, data: unknown): void {
  (plugin as unknown as { data: unknown }).data = data;
}

function savedData(plugin: VaultAxiSyncPlugin): unknown {
  return (plugin as unknown as { data: unknown }).data;
}

async function settle(plugin: VaultAxiSyncPlugin): Promise<void> {
  await vi.advanceTimersByTimeAsync(4_000);
  for (let i = 0; i < 500 && plugin.isSyncing(); i++) await vi.advanceTimersByTimeAsync(10);
  await vi.advanceTimersByTimeAsync(0);
  for (let i = 0; i < 500 && plugin.isSyncing(); i++) await vi.advanceTimersByTimeAsync(10);
}

describe("plugin lifecycle in Obsidian", () => {
  beforeEach(async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "clearInterval"] });
    const { IDBFactory } = await import("fake-indexeddb");
    vi.stubGlobal("indexedDB", new IDBFactory());
    vi.stubGlobal("window", { localStorage: new MemoryStorage(), crypto: globalThis.crypto, setTimeout, clearTimeout, setInterval, clearInterval });
    vi.stubGlobal("document", { visibilityState: "visible" });
    notices.length = 0;
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("syncs a phone vault with S3: startup, edits, remote changes, its own writes", async () => {
    const s3 = new FakeS3();
    setRequestHandler(s3.http);
    const adapter = new MemoryAdapter();
    adapter.write("notes/phone.md", "from the phone\n");
    adapter.write(".obsidian/app.json", "{}");
    adapter.write(".obsidian/workspace-mobile.json", "{}");
    const app = new App(adapter);
    const plugin = new VaultAxiSyncPlugin(app as never, {} as never);
    seedData(plugin, { backend: "s3", s3: { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" } });
    await plugin.onload();
    plugin.secrets.passphrase = "pass";

    plugin.requestSync("manual", 0);
    await settle(plugin);
    expect(plugin.statusText()).toMatch(/^Synced at/);
    const manifest = s3.objects.get("bucket/vault/.vault-manifest.json")?.body ?? "";
    expect(manifest).toContain("notes/phone.md");
    expect(manifest).toContain(".obsidian/app.json");
    expect(manifest).not.toContain("workspace-mobile.json");
    expect(plugin.cachedKeyId()).toMatch(/^k2_/);

    // Another device (desktop) adds a note; the phone's next sync brings it in.
    const desktop = new Device({ passphrase: "pass", cloudUserId: null, cloudVaultId: null });
    await desktop.sync(new S3Remote(s3.http, { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" }));
    desktop.vault.setText("notes/desk/new.md", "from the desktop\n");
    await desktop.sync(new S3Remote(s3.http, { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" }));
    plugin.requestSync("resume", 0);
    await settle(plugin);
    expect(adapter.text("notes/desk/new.md")).toBe("from the desktop\n");

    // The plugin's own write doesn't trigger another sync; a user edit does (debounced).
    const before = s3.log.length;
    app.emit("create", { path: "notes/desk/new.md" });
    await settle(plugin);
    expect(s3.log.length).toBe(before);
    adapter.write("notes/phone.md", "edited on the phone\n");
    app.emit("modify", { path: "notes/phone.md" });
    await settle(plugin);
    await desktop.sync(new S3Remote(s3.http, { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" }));
    expect(desktop.vault.text("notes/phone.md")).toBe("edited on the phone\n");

    // Secrets stay out of data.json.
    expect(JSON.stringify(savedData(plugin))).not.toMatch(/pass|k2_/);
  });

  it("reports a wrong passphrase without touching files", async () => {
    const s3 = new FakeS3();
    setRequestHandler(s3.http);
    const desktop = new Device({ passphrase: "right", cloudUserId: null, cloudVaultId: null });
    desktop.vault.setText("a.md", "a");
    await desktop.sync(new S3Remote(s3.http, { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" }));

    const adapter = new MemoryAdapter();
    const plugin = new VaultAxiSyncPlugin(new App(adapter) as never, {} as never);
    seedData(plugin, { backend: "s3", s3: { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" } });
    await plugin.onload();
    plugin.secrets.passphrase = "wrong";
    plugin.requestSync("manual", 0);
    await settle(plugin);
    expect(plugin.statusText()).toMatch(/passphrase does not unlock/);
    expect(notices.some((n) => n.includes("Vault AXI Sync failed"))).toBe(true);
    expect(adapter.files.size).toBe(0);
  });

  it("shows live active files in the status bar and notifies status subscribers during sync", async () => {
    const s3 = new FakeS3();
    setRequestHandler(s3.http);
    const adapter = new MemoryAdapter();
    adapter.write("notes/alpha.md", "alpha\n");
    adapter.write("notes/beta.md", "beta\n");
    const app = new App(adapter);
    const plugin = new VaultAxiSyncPlugin(app as never, {} as never);
    seedData(plugin, { backend: "s3", s3: { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" } });
    await plugin.onload();
    plugin.secrets.passphrase = "pass";

    const seenStatuses: string[] = [];
    const unsubscribe = plugin.onStatusChange(() => {
      seenStatuses.push(plugin.statusBarText());
    });

    plugin.requestSync("manual", 0);
    await settle(plugin);
    unsubscribe();

    const statusBar = (plugin as unknown as { statusBar: { text: string; history: string[] } }).statusBar;
    expect(statusBar.history.some((t) => t.startsWith("vault-axi ⟳ Uploading") && t.includes("alpha.md"))).toBe(true);
    expect(statusBar.text).toBe("vault-axi ✓");
    expect(seenStatuses.some((t) => t.includes("Uploading"))).toBe(true);
    expect(seenStatuses.at(-1)).toBe("vault-axi ✓");
  });

  it("runs a queued manual sync even when autoSync is disabled and ignores later edit debounces", async () => {
    const s3 = new FakeS3();
    const releases: (() => void)[] = [];
    let holdFirstList = true;
    setRequestHandler(async (req) => {
      if (holdFirstList && req.method === "GET" && req.url.endsWith(".vault-manifest.json")) {
        holdFirstList = false;
        await new Promise<void>((resolve) => {
          releases.push(resolve);
        });
      }
      return s3.http(req);
    });

    const adapter = new MemoryAdapter();
    adapter.write("a.md", "first\n");
    const app = new App(adapter);
    const plugin = new VaultAxiSyncPlugin(app as never, {} as never);
    seedData(plugin, {
      backend: "s3",
      autoSync: false,
      s3: { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" },
    });
    await plugin.onload();
    plugin.secrets.passphrase = "pass";

    // Start first manual sync; it pauses on listing.
    plugin.requestSync("manual", 0);
    await vi.advanceTimersByTimeAsync(0);
    expect(plugin.isSyncing()).toBe(true);

    // Edit while syncing and click Sync now again while autoSync is off.
    adapter.write("b.md", "added mid-sync\n");
    plugin.requestSync("manual", 0);
    await vi.advanceTimersByTimeAsync(0);

    // Release first sync; the queued manual follow-up must run and push b.md.
    releases.shift()?.();
    await settle(plugin);
    const manifest = s3.objects.get("bucket/vault/.vault-manifest.json")?.body ?? "";
    expect(manifest).toContain("a.md");
    expect(manifest).toContain("b.md");
  });

  it("formats status bar and tooltip text cleanly for single, multiple and indeterminate states", () => {
    expect(formatStatusBarText({ kind: "idle" })).toBe("vault-axi ○");
    expect(
      formatStatusBarText({
        kind: "syncing",
        progress: { phase: "listing", label: "Listing remote", done: 0, total: 0, active: [] },
      }),
    ).toBe("vault-axi ⟳ Listing remote…");
    expect(
      formatStatusBarText({
        kind: "syncing",
        progress: { phase: "pulling", label: "Downloading", done: 2, total: 5, active: ["notes/today.md"] },
      }),
    ).toBe("vault-axi ⟳ Downloading 2/5 · today.md");
    expect(
      formatStatusBarText({
        kind: "syncing",
        progress: {
          phase: "pushing",
          label: "Uploading",
          done: 1,
          total: 6,
          active: ["a/one.md", "a/two.md", "a/three.md", "a/four.md"],
        },
      }),
    ).toBe("vault-axi ⟳ Uploading 1/6 · one.md (+3)");
    expect(
      formatStatusText({
        kind: "syncing",
        progress: {
          phase: "pushing",
          label: "Uploading",
          done: 1,
          total: 6,
          active: ["a/one.md", "a/two.md", "a/three.md", "a/four.md"],
        },
      }),
    ).toBe("Uploading (1/6): a/one.md, a/two.md, a/three.md (+1 more)");
  });
});
