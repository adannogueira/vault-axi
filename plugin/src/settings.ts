import { Modal, Notice, PluginSettingTab, Setting, type App, type ButtonComponent } from "obsidian";
import type { S3Location } from "./remotes/s3";
import type VaultAxiSyncPlugin from "./main";
import type { SyncStatus } from "./main";

export interface SyncSettings {
  /** Random per-install id namespacing this device's secrets and sync state. */
  installId: string;
  backend: "cloud" | "s3";
  /** Hosted-cloud vault id used when signing in with email and password. */
  vaultId: string;
  s3: S3Location;
  autoSync: boolean;
  intervalMinutes: number;
}

export const DEFAULT_SETTINGS: Omit<SyncSettings, "installId"> = {
  backend: "cloud",
  vaultId: "default",
  s3: { endpoint: "", bucket: "", prefix: "" },
  autoSync: true,
  intervalMinutes: 5,
};

const MAX_DISPLAYED_ACTIVE_FILES = 8;

function passwordField(input: HTMLInputElement): void {
  input.type = "password";
  input.autocomplete = "off";
}

function renderActiveFilesList(container: HTMLElement, status: SyncStatus): void {
  container.empty();
  if (status.kind !== "syncing" || status.progress.active.length === 0) {
    container.style.display = "none";
    return;
  }
  container.style.display = "";
  const heading = container.createDiv({ text: "In progress:" });
  heading.style.fontSize = "var(--font-ui-smaller)";
  heading.style.color = "var(--text-muted)";
  heading.style.marginBottom = "4px";

  const list = container.createEl("ul");
  list.style.margin = "0";
  list.style.paddingLeft = "1.2em";
  list.style.fontSize = "var(--font-ui-smaller)";
  list.style.fontFamily = "var(--font-monospace)";
  list.style.color = "var(--text-muted)";

  const shown = status.progress.active.slice(0, MAX_DISPLAYED_ACTIVE_FILES);
  for (const path of shown) {
    list.createEl("li", { text: path });
  }
  const extra = status.progress.active.length - shown.length;
  if (extra > 0) {
    list.createEl("li", { text: `…and ${extra} more` });
  }
}

function statusHeadline(status: SyncStatus, fallbackText: string): string {
  if (status.kind === "syncing") {
    const { label, done, total } = status.progress;
    return total > 0 ? `${label} (${done}/${total})` : `${label}…`;
  }
  if (status.kind === "ok") {
    const { pulled, pushed, deletedLocal, deletedRemote, failures } = status.summary;
    const deleted = deletedLocal + deletedRemote;
    const counts = `${pulled} in, ${pushed} out, ${deleted} deleted`;
    const failed = failures.length > 0 ? `, ${failures.length} failed` : "";
    return `${fallbackText} (${counts}${failed})`;
  }
  return fallbackText;
}

export class SyncSettingTab extends PluginSettingTab {
  private unsubscribeStatus: (() => void) | null = null;
  private statusDescEl: HTMLElement | null = null;
  private activeListEl: HTMLElement | null = null;
  private syncButton: ButtonComponent | null = null;

  constructor(
    app: App,
    private readonly plugin: VaultAxiSyncPlugin,
  ) {
    super(app, plugin);
  }

  override display(): void {
    this.cleanupStatusSubscription();
    const { containerEl } = this;
    containerEl.empty();
    const settings = this.plugin.settings;

    new Setting(containerEl)
      .setName("Backend")
      .setDesc("Where your vault-axi CLI syncs this vault.")
      .addDropdown((d) =>
        d
          .addOption("cloud", "vault-axi hosted cloud")
          .addOption("s3", "S3 / R2 bucket")
          .setValue(settings.backend)
          .onChange(async (value) => {
            settings.backend = value === "s3" ? "s3" : "cloud";
            await this.plugin.saveSettings();
            this.display();
          }),
      );

    if (settings.backend === "cloud") this.cloudSection(containerEl);
    else this.s3Section(containerEl);
    this.encryptionSection(containerEl);
    this.syncSection(containerEl);
  }

  override hide(): void {
    this.cleanupStatusSubscription();
  }

  private cleanupStatusSubscription(): void {
    this.unsubscribeStatus?.();
    this.unsubscribeStatus = null;
    this.statusDescEl = null;
    this.activeListEl = null;
    this.syncButton = null;
  }

  private updateStatusUI(): void {
    const status = this.plugin.getStatus();
    if (this.statusDescEl) {
      this.statusDescEl.setText(statusHeadline(status, this.plugin.statusText()));
    }
    if (this.activeListEl) {
      renderActiveFilesList(this.activeListEl, status);
    }
    if (this.syncButton) {
      const syncing = status.kind === "syncing";
      this.syncButton.setButtonText(syncing ? "Syncing…" : "Sync now");
      this.syncButton.setDisabled(syncing);
    }
  }

  private cloudSection(el: HTMLElement): void {
    new Setting(el).setName("Hosted cloud").setHeading();
    const session = this.plugin.secrets.session;
    if (session) {
      new Setting(el)
        .setName(`Signed in${session.email ? ` as ${session.email}` : ""}`)
        .setDesc(`Vault "${session.vaultId}"${session.scope === "read" ? " (read-only token)" : ""}.`)
        .addButton((b) =>
          b.setButtonText("Sign out").onClick(() => {
            this.plugin.signOut();
            this.display();
          }),
        );
      return;
    }

    let email = "";
    let password = "";
    let totp = "";
    new Setting(el).setName("Email").addText((t) => t.onChange((v) => (email = v.trim())));
    new Setting(el).setName("Password").addText((t) => {
      passwordField(t.inputEl);
      t.onChange((v) => (password = v));
    });
    new Setting(el)
      .setName("Two-factor code")
      .setDesc("Only if your account has TOTP enabled.")
      .addText((t) => t.onChange((v) => (totp = v.trim())));
    new Setting(el)
      .setName("Vault id")
      .setDesc('The CLI\'s `vaultId` (usually "default").')
      .addText((t) =>
        t.setValue(this.plugin.settings.vaultId).onChange(async (v) => {
          this.plugin.settings.vaultId = v.trim() || "default";
          await this.plugin.saveSettings();
        }),
      )
      .addButton((b) =>
        b
          .setButtonText("Sign in")
          .setCta()
          .onClick(async () => {
            b.setButtonText("Signing in…").setDisabled(true);
            try {
              await this.plugin.signIn({ email, password, totp: totp || null });
            } finally {
              this.display();
            }
          }),
      );

    let token = "";
    new Setting(el)
      .setName("Or use an API token")
      .setDesc("Create one with `vault token create` (scoped to one vault).")
      .addText((t) => {
        passwordField(t.inputEl);
        t.setPlaceholder("vlt_…").onChange((v) => (token = v.trim()));
      })
      .addButton((b) =>
        b.setButtonText("Use token").onClick(async () => {
          b.setButtonText("Signing in…").setDisabled(true);
          try {
            await this.plugin.signInWithToken(token);
          } finally {
            this.display();
          }
        }),
      );
  }

  private s3Section(el: HTMLElement): void {
    new Setting(el).setName("S3 / R2 bucket").setHeading();
    const s3 = this.plugin.settings.s3;
    const field = (name: string, desc: string, key: keyof typeof s3) =>
      new Setting(el)
        .setName(name)
        .setDesc(desc)
        .addText((t) =>
          t.setValue(s3[key]).onChange(async (v) => {
            s3[key] = v.trim();
            await this.plugin.saveSettings();
          }),
        );
    field("Endpoint", "Same as `vault setup --endpoint`.", "endpoint");
    field("Bucket", "", "bucket");
    field("Prefix", "Folder inside the bucket, if any.", "prefix");
  }

  private encryptionSection(el: HTMLElement): void {
    new Setting(el).setName("Encryption").setHeading();
    let passphrase = "";
    const keyId = this.plugin.cachedKeyId();
    new Setting(el)
      .setName("Vault passphrase")
      .setDesc(
        keyId
          ? `End-to-end encrypted with vault key ${keyId}.`
          : "The passphrase from `vault setup --encryption-key` on your other devices. Required for S3; optional for the hosted cloud.",
      )
      .addText((t) => {
        passwordField(t.inputEl);
        t.setPlaceholder(this.plugin.secrets.passphrase ? "••••••••" : "").onChange((v) => (passphrase = v));
      })
      .addButton((b) =>
        b.setButtonText("Save").onClick(() => {
          this.plugin.secrets.passphrase = passphrase;
          new Notice(passphrase ? "Passphrase saved on this device." : "Passphrase removed from this device.");
          this.plugin.requestSync("settings", 0);
          this.display();
        }),
      );
    if (keyId) {
      new Setting(el)
        .setName("Change passphrase on every device")
        .setDesc("Re-wraps the vault key; your data is not re-encrypted. Other devices keep working.")
        .addButton((b) =>
          b.setButtonText("Change…").onClick(() => {
            new ChangePassphraseModal(this.app, this.plugin).open();
          }),
        );
    }
  }

  private syncSection(el: HTMLElement): void {
    new Setting(el).setName("Sync").setHeading();
    const settings = this.plugin.settings;
    new Setting(el)
      .setName("Sync automatically")
      .setDesc("On startup, after edits, when the app comes back to the foreground, and on an interval.")
      .addToggle((t) =>
        t.setValue(settings.autoSync).onChange(async (v) => {
          settings.autoSync = v;
          await this.plugin.saveSettings();
        }),
      );
    new Setting(el)
      .setName("Interval (minutes)")
      .addSlider((s) =>
        s
          .setLimits(1, 60, 1)
          .setValue(settings.intervalMinutes)
          .setDynamicTooltip()
          .onChange(async (v) => {
            settings.intervalMinutes = v;
            await this.plugin.saveSettings();
          }),
      );
    const statusSetting = new Setting(el)
      .setName("Status")
      .setDesc(this.plugin.statusText())
      .addButton((b) => {
        this.syncButton = b;
        b.setButtonText("Sync now").onClick(() => {
          this.plugin.requestSync("manual", 0);
        });
      });
    this.statusDescEl = statusSetting.descEl;

    this.activeListEl = el.createDiv({ cls: "vault-axi-active-files" });
    this.activeListEl.style.marginTop = "-6px";
    this.activeListEl.style.marginBottom = "12px";

    this.updateStatusUI();
    this.unsubscribeStatus = this.plugin.onStatusChange(() => {
      this.updateStatusUI();
    });
  }
}

export class SyncStatusModal extends Modal {
  private unsubscribe: (() => void) | null = null;
  private headlineEl: HTMLElement | null = null;
  private activeListEl: HTMLElement | null = null;
  private detailEl: HTMLElement | null = null;
  private syncButton: ButtonComponent | null = null;

  constructor(
    app: App,
    private readonly plugin: VaultAxiSyncPlugin,
  ) {
    super(app);
  }

  override onOpen(): void {
    this.setTitle("Vault AXI Sync");
    this.headlineEl = this.contentEl.createEl("p");
    this.activeListEl = this.contentEl.createDiv({ cls: "vault-axi-active-files" });
    this.activeListEl.style.marginBottom = "12px";
    this.detailEl = this.contentEl.createDiv();
    this.detailEl.style.marginBottom = "12px";
    new Setting(this.contentEl).addButton((b) => {
      this.syncButton = b;
      b.setButtonText("Sync now")
        .setCta()
        .onClick(() => {
          this.plugin.requestSync("manual", 0);
        });
    });

    this.render();
    this.unsubscribe = this.plugin.onStatusChange(() => {
      this.render();
    });
  }

  override onClose(): void {
    this.unsubscribe?.();
    this.unsubscribe = null;
    this.headlineEl = null;
    this.activeListEl = null;
    this.detailEl = null;
    this.syncButton = null;
    this.contentEl.empty();
  }

  private render(): void {
    const status = this.plugin.getStatus();
    if (this.headlineEl) {
      this.headlineEl.setText(statusHeadline(status, this.plugin.statusText()));
    }
    if (this.activeListEl) {
      renderActiveFilesList(this.activeListEl, status);
    }
    if (this.detailEl) {
      this.detailEl.empty();
      if (status.kind === "ok" && status.summary.failures.length > 0) {
        const header = this.detailEl.createDiv({ text: "Could not sync:" });
        header.style.fontSize = "var(--font-ui-smaller)";
        header.style.color = "var(--text-error)";
        const list = this.detailEl.createEl("ul");
        list.style.fontSize = "var(--font-ui-smaller)";
        list.style.fontFamily = "var(--font-monospace)";
        for (const failure of status.summary.failures) {
          list.createEl("li", { text: failure });
        }
      }
    }
    if (this.syncButton) {
      const syncing = status.kind === "syncing";
      this.syncButton.setButtonText(syncing ? "Syncing…" : "Sync now");
      this.syncButton.setDisabled(syncing);
    }
  }
}

class ChangePassphraseModal extends Modal {
  constructor(
    app: App,
    private readonly plugin: VaultAxiSyncPlugin,
  ) {
    super(app);
  }

  override onOpen(): void {
    this.setTitle("Change vault passphrase");
    let next = "";
    let confirm = "";
    new Setting(this.contentEl).setName("New passphrase").addText((t) => {
      passwordField(t.inputEl);
      t.onChange((v) => (next = v));
    });
    new Setting(this.contentEl).setName("Repeat it").addText((t) => {
      passwordField(t.inputEl);
      t.onChange((v) => (confirm = v));
    });
    new Setting(this.contentEl).addButton((b) =>
      b
        .setButtonText("Change")
        .setCta()
        .onClick(async () => {
          if (!next || next !== confirm) {
            new Notice("The passphrases are empty or don't match.");
            return;
          }
          if (await this.plugin.changePassphrase(next)) this.close();
        }),
    );
  }

  override onClose(): void {
    this.contentEl.empty();
  }
}
