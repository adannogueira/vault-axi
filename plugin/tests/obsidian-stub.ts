// Just enough of the Obsidian API to run the plugin's lifecycle in Node.
/* eslint-disable @typescript-eslint/no-extraneous-class, @typescript-eslint/no-unused-vars */
import type { HttpRequest, HttpResponse } from "../src/http";

export const notices: string[] = [];
export class Notice {
  constructor(message: string) {
    notices.push(message);
  }
}

export let requestHandler: (req: HttpRequest) => Promise<HttpResponse> = () => Promise.reject(new Error("no handler"));
export function setRequestHandler(handler: typeof requestHandler): void {
  requestHandler = handler;
}
export async function requestUrl(req: { url: string; method?: string; headers?: Record<string, string>; body?: string }) {
  return requestHandler({ url: req.url, method: req.method ?? "GET", ...(req.headers ? { headers: req.headers } : {}), ...(req.body !== undefined ? { body: req.body } : {}) });
}

export const normalizePath = (p: string): string => p.replace(/^\/+/, "").replace(/\/+/g, "/");

export class TFile {
  constructor(readonly path: string) {}
}

/** In-memory filesystem with Obsidian's adapter surface. */
export class MemoryAdapter {
  readonly files = new Map<string, Uint8Array>();
  readonly dirs = new Set<string>([""]);
  readonly trashed: string[] = [];

  private parent(path: string): string {
    return path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "";
  }
  list(dir: string) {
    const d = normalizePath(dir);
    return Promise.resolve({
      files: [...this.files.keys()].filter((f) => this.parent(f) === d),
      folders: [...this.dirs].filter((f) => f !== "" && this.parent(f) === d),
    });
  }
  stat(path: string) {
    const f = this.files.get(path);
    if (f) return Promise.resolve({ type: "file", size: f.length, mtime: 1, ctime: 1 });
    return Promise.resolve(this.dirs.has(path) ? { type: "folder", size: 0, mtime: 1, ctime: 1 } : null);
  }
  exists(path: string) {
    return Promise.resolve(this.files.has(path) || this.dirs.has(path));
  }
  readBinary(path: string) {
    const f = this.files.get(path);
    return f ? Promise.resolve(f.slice().buffer) : Promise.reject(new Error(`ENOENT ${path}`));
  }
  writeBinary(path: string, data: ArrayBuffer) {
    if (!this.dirs.has(this.parent(path))) return Promise.reject(new Error(`no parent for ${path}`));
    this.files.set(path, new Uint8Array(data));
    return Promise.resolve();
  }
  mkdir(path: string) {
    this.dirs.add(path);
    return Promise.resolve();
  }
  trashLocal(path: string) {
    this.files.delete(path);
    this.trashed.push(path);
    return Promise.resolve();
  }
  write(path: string, text: string) {
    const parts = path.split("/");
    for (let i = 1; i < parts.length; i++) this.dirs.add(parts.slice(0, i).join("/"));
    this.files.set(path, new TextEncoder().encode(text));
  }
  text(path: string) {
    const f = this.files.get(path);
    return f && new TextDecoder().decode(f);
  }
}

type Listener = (...args: unknown[]) => void;

export class App {
  readonly listeners = new Map<string, Listener[]>();
  readonly vault;
  readonly fileManager;
  readonly workspace = { onLayoutReady: (cb: () => void) => { cb(); } };

  constructor(readonly adapter: MemoryAdapter) {
    const indexed = (path: string) => !path.split("/").some((s) => s.startsWith(".")) && adapter.files.has(path);
    this.vault = {
      adapter,
      configDir: ".obsidian",
      on: (name: string, cb: Listener) => {
        this.listeners.set(name, [...(this.listeners.get(name) ?? []), cb]);
        return { name };
      },
      getFileByPath: (path: string) => (indexed(path) ? new TFile(path) : null),
      getAbstractFileByPath: (path: string) => (indexed(path) ? new TFile(path) : null),
      createFolder: (path: string) => adapter.mkdir(path),
      createBinary: (path: string, data: ArrayBuffer) => adapter.writeBinary(path, data),
      modifyBinary: (file: TFile, data: ArrayBuffer) => adapter.writeBinary(file.path, data),
    };
    this.fileManager = { trashFile: (file: TFile) => adapter.trashLocal(file.path) };
  }

  emit(name: string, ...args: unknown[]): void {
    for (const cb of this.listeners.get(name) ?? []) cb(...args);
  }
}

class Component {
  registerEvent(_ref: unknown): void {}
  registerDomEvent(_el: unknown, _name: string, _cb: unknown): void {}
  registerInterval(id: number): number {
    return id;
  }
}

export class Plugin extends Component {
  data: unknown = null;
  readonly commands: { id: string; callback: () => void }[] = [];
  constructor(readonly app: App, readonly manifest: unknown) {
    super();
  }
  loadData() {
    return Promise.resolve(this.data);
  }
  saveData(data: unknown) {
    this.data = structuredClone(data);
    return Promise.resolve();
  }
  addStatusBarItem() {
    return { setText: (_: string) => undefined, setAttribute: (_: string, __: string) => undefined };
  }
  addRibbonIcon() {
    return {};
  }
  addCommand(cmd: { id: string; callback: () => void }) {
    this.commands.push(cmd);
  }
  addSettingTab() {}
}

export class PluginSettingTab {}
export class Modal {}
export class Setting {}
