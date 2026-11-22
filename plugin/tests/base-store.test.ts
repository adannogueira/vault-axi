import { IDBFactory } from "fake-indexeddb";
import { describe, expect, it } from "vitest";
import { IndexedDbBaseStore } from "../src/base-store";

describe("IndexedDbBaseStore", () => {
  it("persists records across instances", async () => {
    const factory = new IDBFactory();
    const store = new IndexedDbBaseStore("vault-axi-sync:test", factory);
    await store.put("a.md", { remoteHash: "1", stored: "enc:x" });
    await store.put("b/c.md", { remoteHash: "2", stored: "enc:y" });
    await store.delete("a.md");
    await store.close();

    const reopened = new IndexedDbBaseStore("vault-axi-sync:test", factory);
    expect([...(await reopened.load())]).toEqual([["b/c.md", { remoteHash: "2", stored: "enc:y" }]]);
    expect([...(await new IndexedDbBaseStore("other", factory).load())]).toEqual([]);
  });

  it("batches putMany and deleteMany in single transactions", async () => {
    const factory = new IDBFactory();
    const store = new IndexedDbBaseStore("vault-axi-sync:batch", factory);
    await store.putMany([
      ["a.md", { remoteHash: "1", stored: "enc:a" }],
      ["b.md", { remoteHash: "2", stored: "enc:b" }],
      ["c.md", { remoteHash: "3", stored: "enc:c" }],
    ]);
    await store.deleteMany(["a.md", "c.md"]);
    await store.close();

    const reopened = new IndexedDbBaseStore("vault-axi-sync:batch", factory);
    expect([...(await reopened.load())]).toEqual([["b.md", { remoteHash: "2", stored: "enc:b" }]]);
  });
});
