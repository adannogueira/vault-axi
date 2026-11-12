import { Modal, Notice, PluginSettingTab, Setting, type App } from "obsidian";
import type { S3Location } from "./remotes/s3";
import type VaultAxiSyncPlugin from "./main";

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

function passwordField(input: HTMLInputElement): void {
  input.type = "password";
  input.autocomplete = "off";
}

export class SyncSettingTab extends PluginSettingTab {
  constructor(
    app: App,
    private readonly plugin: VaultAxiSyncPlugin,
  ) {
    super(app, plugin);
  }

  override display(): void {
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
            await this.plugin.signIn({ email, password, totp: totp || null });
            this.display();
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
          await this.plugin.signInWithToken(token);
          this.display();
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
    new Setting(el)
      .setName("Status")
      .setDesc(this.plugin.statusText())
      .addButton((b) =>
        b.setButtonText("Sync now").onClick(() => {
          this.plugin.requestSync("manual", 0);
        }),
      );
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
