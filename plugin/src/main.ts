import { Notice, Plugin, type TAbstractFile } from "obsidian";
import wasmModule from "vault-axi-core-wasm";
import { exchangeApiToken, passwordLogin, type CloudSession } from "./auth";
import { IndexedDbBaseStore } from "./base-store";
import { isSyncable, loadCore, rewrapKeyring, sha1Hex } from "./core";
import { syncVault, type FileHashEntry, type SyncProgress, type SyncSummary } from "./engine";
import { obsidianHttp, ObsidianVaultIO } from "./obsidian-io";
import { SyncError, type Remote } from "./remote";
import { CloudRemote } from "./remotes/cloud";
import { S3Remote } from "./remotes/s3";
import { DeviceSecrets } from "./secrets";
import { DEFAULT_SETTINGS, SyncSettingTab, SyncStatusModal, type SyncSettings } from "./settings";

/** Edits settle for this long before a sync starts. */
const EDIT_DEBOUNCE_MS = 3_000;
/** File events this soon after the plugin wrote the path are its own echo. */
const OWN_WRITE_WINDOW_MS = 5_000;
const RESUME_MIN_GAP_MS = 30_000;

type SyncReason = "manual" | "startup" | "edit" | "resume" | "interval" | "settings";

export type SyncStatus =
  | { kind: "idle" }
  | { kind: "syncing"; progress: SyncProgress }
  | { kind: "ok"; at: number; summary: SyncSummary }
  | { kind: "error"; message: string };

function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function basename(path: string): string {
  const slash = path.lastIndexOf("/");
  return slash >= 0 ? path.slice(slash + 1) : path;
}

function truncate(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

export function formatStatusText(status: SyncStatus): string {
  switch (status.kind) {
    case "idle":
      return "Not synced yet.";
    case "syncing": {
      const { label, done, total, active } = status.progress;
      const head = total > 0 ? `${label} (${done}/${total})` : `${label}…`;
      if (active.length === 0) return head;
      if (active.length <= 3) return `${head}: ${active.join(", ")}`;
      return `${head}: ${active.slice(0, 3).join(", ")} (+${active.length - 3} more)`;
    }
    case "ok":
      return `Synced at ${new Date(status.at).toLocaleTimeString()}.`;
    case "error":
      return `Last sync failed: ${status.message}`;
  }
}

export function formatStatusBarText(status: SyncStatus): string {
  switch (status.kind) {
    case "idle":
      return "vault-axi ○";
    case "ok":
      return "vault-axi ✓";
    case "error":
      return "vault-axi ⚠";
    case "syncing": {
      const { label, done, total, active } = status.progress;
      const count = total > 0 ? ` ${done}/${total}` : "";
      const first = active[0];
      if (first === undefined) {
        return total > 0 ? `vault-axi ⟳ ${label}${count}` : `vault-axi ⟳ ${label}…`;
      }
      if (active.length === 1) {
        return `vault-axi ⟳ ${label}${count} · ${truncate(basename(first), 24)}`;
      }
      return `vault-axi ⟳ ${label}${count} · ${truncate(basename(first), 20)} (+${active.length - 1})`;
    }
  }
}

export default class VaultAxiSyncPlugin extends Plugin {
  settings!: SyncSettings;
  secrets!: DeviceSecrets;
  private status: SyncStatus = { kind: "idle" };
  private statusEl!: HTMLElement;
  private ribbonEl: HTMLElement | null = null;
  private readonly statusListeners = new Set<() => void>();
  private readonly hashCache = new Map<string, FileHashEntry>();
  private running = false;
  private pendingReason: SyncReason | null = null;
  private scheduledReason: SyncReason | null = null;
  private scheduledAt = 0;
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
    if (typeof this.statusEl.addClass === "function") this.statusEl.addClass("mod-clickable");
    this.registerDomEvent(this.statusEl, "click", () => {
      new SyncStatusModal(this.app, this).open();
    });
    this.ribbonEl = this.addRibbonIcon("refresh-cw", "Sync with vault-axi", () => {
      this.requestSync("manual", 0);
    });
    this.renderStatus();
    this.addCommand({
      id: "sync-now",
      name: "Sync now",
      callback: () => {
        this.requestSync("manual", 0);
      },
    });
    this.addCommand({
      id: "show-status",
      name: "Show sync status",
      callback: () => {
        new SyncStatusModal(this.app, this).open();
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
    this.statusListeners.clear();
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
    this.hashCache.delete(path);
    const ownWrite = this.ownWrites.get(path);
    if (ownWrite !== undefined && Date.now() - ownWrite < OWN_WRITE_WINDOW_MS) return;
    if (!isSyncable(path, 0)) return;
    this.requestSync("edit", EDIT_DEBOUNCE_MS);
  }

  /** Debounced, single-flight sync. Automatic triggers respect the auto-sync setting. */
  requestSync(reason: SyncReason, delayMs: number): void {
    if (reason !== "manual" && !this.settings.autoSync) return;
    const fireAt = Date.now() + delayMs;
    if (this.timer !== null) {
      // Never let a background debounce downgrade or delay a queued manual or earlier sync.
      if (this.scheduledReason === "manual" && reason !== "manual") return;
      if (reason !== "manual" && this.scheduledReason !== "edit" && fireAt > this.scheduledAt) return;
      window.clearTimeout(this.timer);
    }
    this.scheduledReason = reason;
    this.scheduledAt = fireAt;
    this.timer = window.setTimeout(() => {
      this.timer = null;
      this.scheduledReason = null;
      this.scheduledAt = 0;
      void this.runSync(reason);
    }, delayMs);
  }

  isSyncing(): boolean {
    return this.running;
  }

  getStatus(): SyncStatus {
    return this.status;
  }

  onStatusChange(listener: () => void): () => void {
    this.statusListeners.add(listener);
    return () => {
      this.statusListeners.delete(listener);
    };
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

  private async runSync(reason: SyncReason): Promise<void> {
    const manual = reason === "manual";
    if (this.running) {
      // Only changes made during a run (or an explicit manual click) need a follow-up run.
      if (manual) this.pendingReason = "manual";
      else if (reason === "edit" && this.pendingReason !== "manual") this.pendingReason = "edit";
      return;
    }
    const remote = this.remote();
    const scope = this.scope();
    if (!remote || !scope) {
      if (manual) new Notice("Vault AXI Sync: sign in or configure the S3 bucket in the plugin settings first.");
      return;
    }

    this.running = true;
    this.setStatus({ kind: "syncing", progress: { phase: "listing", label: "Starting", done: 0, total: 0, active: [] } });
    const session = this.secrets.session;
    const base = new IndexedDbBaseStore(`vault-axi-sync:${this.settings.installId}:${sha1Hex(scope)}`);
    try {
      const summary = await syncVault({
        io: new ObsidianVaultIO(this.app, (path) => {
          this.ownWrites.set(path, Date.now());
          this.hashCache.delete(path);
        }),
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
        hashCache: this.hashCache,
        pull: true,
        push: true,
        onProgress: (progress) => {
          this.setStatus({ kind: "syncing", progress });
        },
      });
      this.lastError = null;
      this.setStatus({ kind: "ok", at: Date.now(), summary });
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
      if (this.pendingReason !== null) {
        const next = this.pendingReason;
        this.pendingReason = null;
        this.requestSync(next, 0);
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

  private setStatus(status: SyncStatus): void {
    this.status = status;
    this.renderStatus();
    for (const listener of this.statusListeners) listener();
  }

  statusText(): string {
    return formatStatusText(this.status);
  }

  statusBarText(): string {
    return formatStatusBarText(this.status);
  }

  private renderStatus(): void {
    const detail = this.statusText();
    this.statusEl.setText(this.statusBarText());
    this.statusEl.setAttribute("aria-label", detail);
    if (this.ribbonEl && typeof this.ribbonEl.setAttribute === "function") {
      this.ribbonEl.setAttribute("aria-label", this.status.kind === "syncing" ? detail : `Sync with vault-axi — ${detail}`);
    }
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
