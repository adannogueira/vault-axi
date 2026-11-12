// What this device last agreed with the remote on, per path (the CLI's `sync_state` table).

export interface BaseRecord {
  remoteHash: string;
  /** Base content, encrypted with the keychain under the local context. */
  stored: string;
}

export interface BaseStore {
  load(): Promise<Map<string, BaseRecord>>;
  put(path: string, record: BaseRecord): Promise<void>;
  delete(path: string): Promise<void>;
}

export class MemoryBaseStore implements BaseStore {
  readonly records = new Map<string, BaseRecord>();

  load(): Promise<Map<string, BaseRecord>> {
    return Promise.resolve(new Map(this.records));
  }
  put(path: string, record: BaseRecord): Promise<void> {
    this.records.set(path, record);
    return Promise.resolve();
  }
  delete(path: string): Promise<void> {
    this.records.delete(path);
    return Promise.resolve();
  }
}

const STORE = "base";

function promisify<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => {
      resolve(request.result);
    };
    request.onerror = () => {
      reject(request.error ?? new Error("IndexedDB request failed"));
    };
  });
}

/** One database per vault and remote, so switching accounts never reuses a stale base. */
export class IndexedDbBaseStore implements BaseStore {
  private db: Promise<IDBDatabase> | null = null;

  constructor(
    private readonly name: string,
    private readonly factory: IDBFactory = indexedDB,
  ) {}

  private open(): Promise<IDBDatabase> {
    this.db ??= new Promise((resolve, reject) => {
      const req = this.factory.open(this.name, 1);
      req.onupgradeneeded = () => {
        req.result.createObjectStore(STORE);
      };
      req.onsuccess = () => {
        resolve(req.result);
      };
      req.onerror = () => {
        reject(req.error ?? new Error("Could not open IndexedDB"));
      };
    });
    return this.db;
  }

  private async store(mode: IDBTransactionMode): Promise<IDBObjectStore> {
    return (await this.open()).transaction(STORE, mode).objectStore(STORE);
  }

  async load(): Promise<Map<string, BaseRecord>> {
    const store = await this.store("readonly");
    const [keys, values] = await Promise.all([promisify(store.getAllKeys()), promisify(store.getAll())]);
    return new Map(keys.map((k, i) => [k as string, values[i] as BaseRecord]));
  }

  async put(path: string, record: BaseRecord): Promise<void> {
    await promisify((await this.store("readwrite")).put(record, path));
  }

  async delete(path: string): Promise<void> {
    await promisify((await this.store("readwrite")).delete(path));
  }

  async close(): Promise<void> {
    (await this.db)?.close();
    this.db = null;
  }
}
