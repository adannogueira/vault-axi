// The real CLI binary and the plugin engine syncing one vault through a shared server.
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type IncomingMessage } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { Http } from "../src/http";
import { CloudRemote } from "../src/remotes/cloud";
import { S3Remote } from "../src/remotes/s3";
import type { Remote } from "../src/remote";
import { cloudKeys, Device, FakePostgrest, FakeS3 } from "./fakes";

const execFileAsync = promisify(execFile);
const CLI = fileURLToPath(new URL(`../../target/debug/vault${process.platform === "win32" ? ".exe" : ""}`, import.meta.url));
const haveCli = existsSync(CLI);
if (!haveCli && process.env.REQUIRE_CLI === "1") throw new Error(`CLI binary missing at ${CLI}; run cargo build`);

const titleCase = (name: string) => name.replace(/(^|-)([a-z])/g, (_, dash: string, c: string) => dash + c.toUpperCase());

async function serve(http: Http): Promise<{ url: string; close: () => Promise<void> }> {
  const server = createServer((req: IncomingMessage, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const headers = Object.fromEntries(
        Object.entries(req.headers).flatMap(([k, v]) => (typeof v === "string" ? [[titleCase(k), v]] : [])),
      );
      const body = Buffer.concat(chunks).toString("utf8");
      void http({ url: `http://local${req.url ?? "/"}`, method: req.method ?? "GET", headers, ...(body ? { body } : {}) }).then((r) => {
        res.writeHead(r.status, Object.fromEntries(Object.entries(r.headers).map(([k, v]) => [titleCase(k), v])));
        res.end(r.text);
      });
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const { port } = server.address() as AddressInfo;
  return {
    url: `http://127.0.0.1:${port}`,
    close: () =>
      new Promise((resolve) => {
        server.close(() => {
          resolve();
        });
      }),
  };
}

class Cli {
  readonly home = mkdtempSync(join(tmpdir(), "vx-cli-home-"));
  readonly vault = join(this.home, "vault");

  constructor(private readonly env: Record<string, string> = {}) {
    mkdirSync(this.vault, { recursive: true });
  }

  /** Async on purpose: the fake servers live in this process and must keep serving. */
  async run(...args: string[]): Promise<string> {
    const { stdout } = await execFileAsync(CLI, args, {
      timeout: 60_000,
      env: {
        PATH: process.env.PATH ?? "",
        HOME: this.home,
        VAULT_AXI_HOME: this.home,
        VAULT_AXI_NO_EMBED: "1",
        VAULT_AXI_NO_UPDATE: "1",
        VAULT_AXI_QUIET: "1",
        ...this.env,
      },
      encoding: "utf8",
    });
    return stdout;
  }

  config(config: Record<string, unknown>): void {
    writeFileSync(join(this.home, "config.json"), JSON.stringify({ vaultPath: this.vault, autoUpdate: false, ...config }));
  }

  write(path: string, body: string | Uint8Array): void {
    const full = join(this.vault, path);
    mkdirSync(join(full, ".."), { recursive: true });
    writeFileSync(full, body);
  }

  read(path: string): string | null {
    const full = join(this.vault, path);
    return existsSync(full) ? readFileSync(full, "utf8") : null;
  }

  dispose(): void {
    rmSync(this.home, { recursive: true, force: true });
  }
}

/** The same CLI <-> plugin story over any backend. */
async function crossClientStory(cli: Cli, plugin: Device, remote: () => Remote): Promise<void> {
  cli.write("notes/a.md", "# A\nline two\nline three\n");
  cli.write("notes/b.md", "# B\n");
  cli.write("img/p.png", new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0, 255]));
  cli.write(".obsidian/app.json", '{"theme":"dark"}');
  await cli.run("sync");

  expect((await plugin.sync(remote())).pulled).toBe(4);
  expect(plugin.vault.text("notes/a.md")).toBe("# A\nline two\nline three\n");
  expect([...(plugin.vault.files.get("img/p.png") ?? [])]).toEqual([0x89, 0x50, 0x4e, 0x47, 0, 255]);
  expect(plugin.vault.text(".obsidian/app.json")).toBe('{"theme":"dark"}');

  // Concurrent, non-overlapping edits on both clients merge.
  plugin.vault.setText("notes/a.md", "# A from phone\nline two\nline three\n");
  plugin.vault.setText("notes/phone.md", "written on the phone\n");
  await plugin.sync(remote());
  cli.write("notes/a.md", "# A\nline two\nline three from desktop\n");
  await cli.run("sync");
  expect(cli.read("notes/a.md")).toBe("# A from phone\nline two\nline three from desktop\n");
  expect(cli.read("notes/phone.md")).toBe("written on the phone\n");
  await plugin.sync(remote());
  expect(plugin.vault.text("notes/a.md")).toBe("# A from phone\nline two\nline three from desktop\n");

  // Deletions propagate both ways.
  plugin.vault.files.delete("notes/b.md");
  await plugin.sync(remote());
  await cli.run("sync");
  expect(cli.read("notes/b.md")).toBeNull();
}

describe.skipIf(!haveCli)("CLI <-> plugin interop", () => {
  const s3 = new FakeS3();
  const api = new FakePostgrest();
  let s3Server: Awaited<ReturnType<typeof serve>>;
  let apiServer: Awaited<ReturnType<typeof serve>>;

  beforeAll(async () => {
    s3Server = await serve(s3.http);
    apiServer = await serve(api.http);
  });
  afterAll(async () => {
    await s3Server.close();
    await apiServer.close();
  });

  it("over S3 with a passphrase keyring", async () => {
    const cli = new Cli();
    try {
      cli.config({
        encryptionKey: "correct horse",
        remote: { type: "s3", endpoint: s3Server.url, bucket: "bucket", prefix: "vault" },
      });
      const plugin = new Device({ passphrase: "correct horse", cloudUserId: null, cloudVaultId: null });
      const remote = () => new S3Remote(s3.http, { endpoint: "https://s3.test", bucket: "bucket", prefix: "vault" });
      await crossClientStory(cli, plugin, remote);
      expect(s3.objects.get("bucket/vault/.vault-axi/keyring.json")?.body).toContain('"argon2id"');
    } finally {
      cli.dispose();
    }
  });

  it("over the hosted-cloud protocol, migrating a legacy vault to end-to-end encryption", async () => {
    const cli = new Cli({ VAULT_AXI_CLOUD_URL: apiServer.url });
    try {
      const remoteCfg = { type: "cloud", accessToken: "jwt-1", refreshToken: "r-0", userId: "u1", vaultId: "v1", scope: "write" };
      // First a vault-axi 1.1-style vault: no passphrase, v1 rows under the cloud-derived key.
      cli.config({ remote: remoteCfg });
      cli.write("legacy.md", "from before the upgrade\n");
      await cli.run("sync");
      expect(api.rows.find((r) => r.path === "legacy.md")?.content).toMatch(/^enc:v1:/);

      // The user sets a passphrase; the CLI creates the keyring and re-encrypts everything.
      cli.config({ remote: remoteCfg, encryptionKey: "correct horse" });
      await cli.run("sync");
      expect(api.rows.find((r) => r.path === "legacy.md")?.content).toMatch(/^enc:v2:k2_/);

      const plugin = new Device(cloudKeys("correct horse"));
      const session = {
        accessToken: "jwt-1",
        refreshToken: "r-0",
        userId: "u1",
        email: null,
        vaultId: "v1",
        scope: "write",
        apiToken: null,
      };
      const remote = () => new CloudRemote(api.http, session, () => undefined);
      expect((await plugin.sync(remote())).pulled).toBe(1);
      expect(plugin.vault.text("legacy.md")).toBe("from before the upgrade\n");
      await crossClientStory(cli, plugin, remote);
      for (const row of api.rows.filter((r) => !r.deleted && r.path !== ".vault-axi/keyring.json")) {
        expect(row.content, row.path).toMatch(/^enc:v2:/);
      }
    } finally {
      cli.dispose();
    }
  });
});
