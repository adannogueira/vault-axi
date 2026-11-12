import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  isSyncable,
  newKeychain,
  objectKey,
  parseManifest,
  payloadFromFile,
  payloadToFile,
  planPull,
  resolve,
  resolveVaultKey,
  sha1Hex,
} from "../src/core";

const fixture = (name: string) =>
  JSON.parse(readFileSync(fileURLToPath(new URL(`../../crates/core/tests/fixtures/${name}`, import.meta.url)), "utf8")) as {
    passphrase: string;
    keyring: unknown;
    cases: { name?: string; path?: string; plaintext: string; ciphertext: string; key?: { kind: string; input?: string } }[];
  };

describe("shared core through WASM", () => {
  it("decrypts every frozen v1 payload derived from a passphrase or the cloud key", () => {
    for (const c of fixture("crypto-v1.json").cases) {
      if (c.key?.kind !== "pbkdf2" || !c.key.input) continue;
      const input = c.key.input;
      const cloud = /^vault-axi-cloud-e2ee:(.+):(.+)$/.exec(input);
      if (input.startsWith("vault-axi-local-at-rest:")) continue; // path-derived; CLI only
      const kc = cloud
        ? newKeychain({ cloudUserId: cloud[1] ?? "", cloudVaultId: cloud[2] ?? "" })
        : newKeychain({ passphrase: input });
      expect(kc.decrypt(c.ciphertext, "any"), c.name).toBe(c.plaintext);
    }
  });

  it("unlocks the frozen v2 keyring and decrypts its payloads", () => {
    const v2 = fixture("crypto-v2.json");
    const step = resolveVaultKey(JSON.stringify(v2.keyring), v2.passphrase, null, "t");
    expect(step.kind).toBe("use");
    if (step.kind !== "use") return;
    const kc = newKeychain({ passphrase: v2.passphrase, dataKey: step.dataKey });
    for (const c of v2.cases) expect(kc.decrypt(c.ciphertext, c.path ?? "")).toBe(c.plaintext);
    expect(() => resolveVaultKey(JSON.stringify(v2.keyring), "wrong", null, "t")).toThrow(/^ENCRYPTION_KEY_MISMATCH/);
  });

  it("refuses to build a keychain without any key", () => {
    expect(() => newKeychain({})).toThrow(/^NO_KEY/);
  });

  it("maps files to payloads like the CLI", () => {
    const enc = new TextEncoder();
    expect(payloadFromFile("a.md", enc.encode("# hi"))).toBe("# hi");
    const bin = payloadFromFile("i.png", new Uint8Array([0, 255]));
    expect(bin.startsWith("b64:v1:")).toBe(true);
    expect([...payloadToFile(bin)]).toEqual([0, 255]);
    expect(sha1Hex("abc")).toBe("a9993e364706816aba3e25717850c26c9cd0d89d");
  });

  it("applies the shared syncable-path rules", () => {
    expect(isSyncable("notes/a.md", 1)).toBe(true);
    expect(isSyncable(".obsidian/app.json", 1)).toBe(true);
    expect(isSyncable(".obsidian/plugins/vault-axi-sync/data.json", 1)).toBe(false);
    expect(isSyncable(".vault-axi/keyring.json", 1)).toBe(false);
    expect(isSyncable(".git/HEAD", 1)).toBe(false);
  });

  it("plans and resolves with camelCase actions", () => {
    expect(planPull([], [], [{ path: "a.md", hash: "1", deleted: false, keyId: "" }], true)).toEqual([
      { action: "download", path: "a.md" },
    ]);
    const r = resolve("n.md", "a\nb\nc\n", "A\nb\nc\n", "a\nb\nC\n", 0);
    expect(r).toEqual({ kind: "autoMerged", content: "A\nb\nC\n", conflictCopy: null });
  });

  it("reads legacy and v2 S3 manifests", () => {
    const legacy = parseManifest('{"a.md": "h:k"}');
    expect(legacy.entries["a.md"]).toMatchObject({ hash: "h", keyId: "k", object: "a.md", deleted: false });
    expect(objectKey("a.md", "h", "k2_1")).toMatch(/^\.vault-axi\/objects\/[0-9a-f]{32}\/h\.k2_1$/);
  });
});
