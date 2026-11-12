// Credentials and keys stay on this device, outside the vault: never in data.json, which other
// sync tools (and `.obsidian/` sync) may copy around.
import type { CloudSession } from "./auth";
import type { DataKey } from "./core";

export class DeviceSecrets {
  constructor(
    private readonly namespace: string,
    private readonly storage: Storage = window.localStorage,
  ) {}

  private key(name: string): string {
    return `vault-axi-sync:${this.namespace}:${name}`;
  }

  private read(name: string): unknown {
    const raw = this.storage.getItem(this.key(name));
    if (raw === null) return null;
    try {
      return JSON.parse(raw);
    } catch {
      return null;
    }
  }

  private write(name: string, value: unknown): void {
    if (value === null) this.storage.removeItem(this.key(name));
    else this.storage.setItem(this.key(name), JSON.stringify(value));
  }

  get session(): CloudSession | null {
    return this.read("session") as CloudSession | null;
  }
  set session(value: CloudSession | null) {
    this.write("session", value);
  }

  get passphrase(): string | null {
    return this.read("passphrase") as string | null;
  }
  set passphrase(value: string | null) {
    this.write("passphrase", value === "" ? null : value);
  }

  dataKey(scope: string): DataKey | null {
    return this.read(`dataKey:${scope}`) as DataKey | null;
  }
  setDataKey(scope: string, value: DataKey | null): void {
    this.write(`dataKey:${scope}`, value);
  }
}
