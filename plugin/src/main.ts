import { Notice, Plugin, type TAbstractFile } from "obsidian";
import wasmModule from "vault-axi-core-wasm";
import { exchangeApiToken, passwordLogin, type CloudSession } from "./auth";
import { IndexedDbBaseStore } from "./base-store";
import { isSyncable, loadCore, rewrapKeyring, sha1Hex } from "./core";
import { syncVault, type SyncSummary } from "./engine";
import { obsidianHttp, ObsidianVaultIO } from "./obsidian-io";
import { SyncError, type Remote } from "./remote";
import { CloudRemote } from "./remotes/cloud";
import { S3Remote } from "./remotes/s3";
import { DeviceSecrets } from "./secrets";
import { DEFAULT_SETTINGS, SyncSettingTab, type SyncSettings } from "./settings";

/** Edits settle for this long before a sync starts. */
const EDIT_DEBOUNCE_MS = 3_000;
/** File events this soon after the plugin wrote the path are its own echo. */
const OWN_WRITE_WINDOW_MS = 5_000;
const RESUME_MIN_GAP_MS = 30_000;

type Status = { kind: "idle" } | { kind: "syncing"; detail: string } | { kind: "ok"; at: number } | { kind: "error"; message: string };

function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export default class VaultAxiSyncPlugin extends Plugin {
  settings!: SyncSettings;
  secrets!: DeviceSecrets;
  private status: Status = { kind: "idle" };
  private statusEl!: HTMLElement;
  private running = false;
  private pending = false;
  private timer: number | null = null;
  private intervalId: number | null = null;
  private lastFinished = 0;
  private lastError: string | null = null;
  private readonly ownWrites = new Map<string, number>();

  override async onload(): Promise<void> {
    loadCore(wasmModule);
    await this.loadSettings();
    this.secrets = new DeviceSecrets(this.settings.installId);

    this.statusEl = this.addStatusBarItem();
    this.renderStatus();
    this.addRibbonIcon("refresh-cw", "Sync with vault-axi", () => {
      this.requestSync("manual", 0);
    });
    this.addCommand({
      id: "sync-now",
      name: "Sync now",
      callback: () => {
        this.requestSync("manual", 0);
      },
    });
    this.addSettingTab(new SyncSettingTab(this.app, this));

    const onChange = (file: TAbstractFile) => {
      this.onVaultChange(file.path);
    };
    this.registerEvent(this.app.vault.on("modify", onChange));
    this.registerEvent(this.app.vault.on("create", onChange));
    this.registerEvent(this.app.vault.on("delete", onChange));
    this.registerEvent(
      this.app.vault.on("rename", (file, oldPath) => {
        this.onVaultChange(oldPath);
        this.onVaultChange(file.path);
      }),
    );
    this.registerDomEvent(document, "visibilitychange", () => {
      if (document.visibilityState === "visible" && Date.now() - this.lastFinished > RESUME_MIN_GAP_MS) {
        this.requestSync("resume", 0);
      }
    });
    this.resetInterval();
    this.app.workspace.onLayoutReady(() => {
      this.requestSync("startup", 0);
    });
  }

  override onunload(): void {
    if (this.timer !== null) window.clearTimeout(this.timer);
  }

  async loadSettings(): Promise<void> {
    const saved = ((await this.loadData()) ?? {}) as Partial<SyncSettings>;
    this.settings = {
      ...DEFAULT_SETTINGS,
      ...saved,
      s3: { ...DEFAULT_SETTINGS.s3, ...saved.s3 },
      installId: saved.installId ?? crypto.randomUUID(),
    };
    if (!saved.installId) await this.saveData(this.settings);
  }

  async saveSettings(): Promise<void> {
    await this.saveData(this.settings);
    this.resetInterval();
  }

  private resetInterval(): void {
    if (this.intervalId !== null) window.clearInterval(this.intervalId);
    this.intervalId = window.setInterval(() => {
      this.requestSync("interval", 0);
    }, this.settings.intervalMinutes * 60_000);
    this.registerInterval(this.intervalId);
  }

  private onVaultChange(path: string): void {
    const ownWrite = this.ownWrites.get(path);
    if (ownWrite !== undefined && Date.now() - ownWrite < OWN_WRITE_WINDOW_MS) return;
    if (!isSyncable(path, 0)) return;
    this.requestSync("edit", EDIT_DEBOUNCE_MS);
  }

  /** Debounced, single-flight sync. Automatic triggers respect the auto-sync setting. */
  requestSync(reason: "manual" | "startup" | "edit" | "resume" | "interval" | "settings", delayMs: number): void {
    if (reason !== "manual" && !this.settings.autoSync) return;
    if (this.timer !== null) window.clearTimeout(this.timer);
    this.timer = window.setTimeout(() => {
      this.timer = null;
      void this.runSync(reason === "manual");
    }, delayMs);
  }

  /** Identity of the configured remote; keys and sync state are kept per scope. */
  private scope(): string | null {
    if (this.settings.backend === "cloud") {
      const s = this.secrets.session;
      return s ? `cloud:${s.userId ?? "token"}:${s.vaultId}` : null;
    }
    const { endpoint, bucket, prefix } = this.settings.s3;
    if (!endpoint || !bucket) return null;
    return `s3:${endpoint.replace(/\/+$/, "")}/${bucket}/${prefix.replace(/^\/+|\/+$/g, "")}`;
  }

  cachedKeyId(): string | null {
    const scope = this.scope();
    return scope ? (this.secrets.dataKey(scope)?.id ?? null) : null;
  }

  private remote(): Remote | null {
    if (this.settings.backend === "s3") {
      const { endpoint, bucket } = this.settings.s3;
      return endpoint && bucket ? new S3Remote(obsidianHttp, this.settings.s3) : null;
    }
    const session = this.secrets.session;
    return session
      ? new CloudRemote(obsidianHttp, session, (renewed) => {
          this.secrets.session = renewed;
        })
      : null;
  }

  private async runSync(manual: boolean): Promise<void> {
    if (this.running) {
      this.pending = true;
      return;
    }
    const remote = this.remote();
    const scope = this.scope();
    if (!remote || !scope) {
      if (manual) new Notice("Vault AXI Sync: sign in or configure the S3 bucket in the plugin settings first.");
      return;
    }

    this.running = true;
    this.setStatus({ kind: "syncing", detail: "starting" });
    const session = this.secrets.session;
    const base = new IndexedDbBaseStore(`vault-axi-sync:${this.settings.installId}:${sha1Hex(scope)}`);
    try {
      const summary = await syncVault({
        io: new ObsidianVaultIO(this.app, (path) => this.ownWrites.set(path, Date.now())),
        base,
        remote,
        keys: {
          passphrase: this.secrets.passphrase,
          cloudUserId: this.settings.backend === "cloud" ? (session?.userId ?? null) : null,
          cloudVaultId: this.settings.backend === "cloud" ? (session?.vaultId ?? null) : null,
        },
        keyCache: {
          get: () => this.secrets.dataKey(scope),
          set: (key) => {
            this.secrets.setDataKey(scope, key);
          },
        },
        pull: true,
        push: true,
        onProgress: (action, done, total, path) => {
          this.setStatus({ kind: "syncing", detail: `${action.toLowerCase()} ${done}/${total} ${path}` });
        },
      });
      this.lastError = null;
      this.setStatus({ kind: "ok", at: Date.now() });
      this.report(summary, manual);
    } catch (e) {
      const message = errorMessage(e);
      this.setStatus({ kind: "error", message });
      // Repeated automatic failures (e.g. offline) notify once, not on every attempt.
      if (manual || message !== this.lastError) new Notice(`Vault AXI Sync failed: ${message}`, 10_000);
      this.lastError = message;
    } finally {
      await base.close();
      this.running = false;
      this.lastFinished = Date.now();
      this.pruneOwnWrites();
      if (this.pending) {
        this.pending = false;
        this.requestSync("edit", 0);
      }
    }
  }

  private report(summary: SyncSummary, manual: boolean): void {
    for (const c of summary.conflicts) new Notice(c.message, 10_000);
    if (summary.failures.length > 0) {
      new Notice(`Vault AXI Sync: ${summary.failures.length} file(s) could not sync:\n${summary.failures.join("\n")}`, 15_000);
    }
    if (manual) {
      new Notice(`Synced: ${summary.pulled} in, ${summary.pushed} out, ${summary.deletedLocal + summary.deletedRemote} deleted.`);
    }
  }

  private pruneOwnWrites(): void {
    const cutoff = Date.now() - OWN_WRITE_WINDOW_MS;
    for (const [path, at] of this.ownWrites) if (at < cutoff) this.ownWrites.delete(path);
  }

  private setStatus(status: Status): void {
    this.status = status;
    this.renderStatus();
  }

  statusText(): string {
    switch (this.status.kind) {
      case "idle":
        return "Not synced yet.";
      case "syncing":
        return `Syncing: ${this.status.detail}`;
      case "ok":
        return `Synced at ${new Date(this.status.at).toLocaleTimeString()}.`;
      case "error":
        return `Last sync failed: ${this.status.message}`;
    }
  }

  private renderStatus(): void {
    const label = { idle: "○", syncing: "⟳", ok: "✓", error: "⚠" }[this.status.kind];
    this.statusEl.setText(`vault-axi ${label}`);
    this.statusEl.setAttribute("aria-label", this.statusText());
  }

  async signIn({ email, password, totp }: { email: string; password: string; totp: string | null }): Promise<void> {
    await this.adoptSession(() => passwordLogin(obsidianHttp, email, password, totp, this.settings.vaultId));
  }

  async signInWithToken(token: string): Promise<void> {
    if (!token.startsWith("vlt_")) {
      new Notice("API tokens start with vlt_ (create one with `vault token create`).");
      return;
    }
    await this.adoptSession(() => exchangeApiToken(obsidianHttp, token));
  }

  private async adoptSession(login: () => Promise<CloudSession>): Promise<void> {
    try {
      this.secrets.session = await login();
      new Notice("Signed in to the vault-axi cloud.");
      this.requestSync("manual", 0);
    } catch (e) {
      new Notice(`Sign-in failed: ${errorMessage(e)}`, 10_000);
    }
  }

  signOut(): void {
    this.secrets.session = null;
    this.setStatus({ kind: "idle" });
  }

  /** Re-wrap the remote keyring under a new passphrase. Returns whether it succeeded. */
  async changePassphrase(next: string): Promise<boolean> {
    const remote = this.remote();
    const scope = this.scope();
    const dataKey = scope ? this.secrets.dataKey(scope) : null;
    if (!remote || !scope || !dataKey) {
      new Notice("Sync once with the current passphrase before changing it.");
      return false;
    }
    try {
      const stored = await remote.fetchKeyring();
      if (!stored) throw new SyncError("KEYRING_LOST", "The remote has no keyring yet");
      await remote.replaceKeyring(stored.version, rewrapKeyring(stored.keyring, dataKey, next));
      this.secrets.passphrase = next;
      new Notice("Vault passphrase changed. Use the new one with `vault setup --encryption-key` on your other devices.", 10_000);
      return true;
    } catch (e) {
      new Notice(`Changing the passphrase failed: ${errorMessage(e)}`, 10_000);
      return false;
    }
  }
}
