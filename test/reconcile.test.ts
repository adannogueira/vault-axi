import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { mkdtempSync, rmSync, writeFileSync, mkdirSync, unlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openDbAt, type Db } from "../src/db.js";
import { reconcile } from "../src/index/reconcile.js";

let vault: string;
let db: Db;

beforeEach(() => {
  process.env.VAULT_AXI_NO_EMBED = "1"; // deterministic, offline
  vault = mkdtempSync(join(tmpdir(), "vault-axi-test-"));
  mkdirSync(join(vault, "plans"));
  writeFileSync(
    join(vault, "plans", "alpha.md"),
    `---
status: active
---

# Alpha Plan

## Goals
Ship the [[beta]] feature and link to [[plans/gamma|Gamma]].
`,
  );
  writeFileSync(
    join(vault, "beta.md"),
    "# Beta\n\nSome content about beta with enough characters to index it.\n",
  );
  writeFileSync(
    join(vault, "plans", "gamma.md"),
    "# Gamma\n\nGamma details here, referencing a [[missing-note]] that does not exist.\n",
  );
  db = openDbAt(":memory:");
});

afterEach(() => {
  db.close();
  rmSync(vault, { recursive: true, force: true });
  delete process.env.VAULT_AXI_NO_EMBED;
});

describe("reconcile", () => {
  it("indexes all markdown files and their blocks", async () => {
    const res = await reconcile(db, vault);
    expect(res.scanned).toBe(3);
    expect(res.indexed).toBe(3);
    const files = db.prepare("SELECT path FROM files ORDER BY path").all() as { path: string }[];
    expect(files.map((f) => f.path)).toEqual(["beta.md", "plans/alpha.md", "plans/gamma.md"]);
    const blocks = db.prepare("SELECT COUNT(*) n FROM blocks").get() as { n: number };
    expect(blocks.n).toBeGreaterThanOrEqual(3);
  });

  it("resolves wikilinks by basename and by path, marking danglers null", async () => {
    await reconcile(db, vault);
    const links = db
      .prepare("SELECT target_raw, target_path FROM links WHERE src_path = 'plans/alpha.md' ORDER BY target_raw")
      .all() as { target_raw: string; target_path: string | null }[];
    const byRaw = Object.fromEntries(links.map((l) => [l.target_raw, l.target_path]));
    expect(byRaw["beta"]).toBe("beta.md");
    expect(byRaw["plans/gamma|Gamma"]).toBe("plans/gamma.md");

    const dangling = db
      .prepare("SELECT target_path FROM links WHERE src_path = 'plans/gamma.md'")
      .get() as { target_path: string | null };
    expect(dangling.target_path).toBeNull();
  });

  it("populates FTS so keyword search works", async () => {
    await reconcile(db, vault);
    const rows = db
      .prepare("SELECT path FROM blocks WHERE id IN (SELECT rowid FROM blocks_fts WHERE blocks_fts MATCH ?)")
      .all("beta") as { path: string }[];
    expect(rows.some((r) => r.path === "beta.md")).toBe(true);
  });

  it("only re-indexes changed files on a second run", async () => {
    await reconcile(db, vault);
    const second = await reconcile(db, vault);
    expect(second.unchanged).toBe(3);
    expect(second.indexed).toBe(0);
    expect(second.updated).toBe(0);
  });

  it("detects edits and prunes deleted files", async () => {
    await reconcile(db, vault);
    // edit beta
    writeFileSync(join(vault, "beta.md"), "# Beta\n\nCompletely new beta content, long enough.\n");
    // delete gamma
    unlinkSync(join(vault, "plans", "gamma.md"));
    const res = await reconcile(db, vault);
    expect(res.updated).toBe(1);
    expect(res.removed).toBe(1);
    const remaining = db.prepare("SELECT COUNT(*) n FROM files").get() as { n: number };
    expect(remaining.n).toBe(2);
    const gammaBlocks = db.prepare("SELECT COUNT(*) n FROM blocks WHERE path = 'plans/gamma.md'").get() as {
      n: number;
    };
    expect(gammaBlocks.n).toBe(0);
  });
});
