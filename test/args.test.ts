import { describe, it, expect, afterEach } from "vitest";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs, resolveContent } from "../src/args.js";

let tmp: string | null = null;
afterEach(() => {
  if (tmp) rmSync(tmp, { recursive: true, force: true });
  tmp = null;
});

describe("parseArgs", () => {
  it("parses positionals, --key value, --key=value, and booleans", () => {
    const { positionals, flags } = parseArgs(["notes/a.md", "--section", "Design", "--mode=replace", "--force"]);
    expect(positionals).toEqual(["notes/a.md"]);
    expect(flags.section).toBe("Design");
    expect(flags.mode).toBe("replace");
    expect(flags.force).toBe(true);
  });
});

describe("resolveContent", () => {
  it("reads content from --from <file>", () => {
    tmp = mkdtempSync(join(tmpdir(), "vx-args-"));
    const file = join(tmp, "body.md");
    writeFileSync(file, "# Big Note\n\nlots of content\n");
    const { flags } = parseArgs(["--from", file]);
    expect(resolveContent(flags)).toBe("# Big Note\n\nlots of content\n");
  });

  it("uses inline --content", () => {
    const { flags } = parseArgs(["--content", "hello"]);
    expect(resolveContent(flags)).toBe("hello");
  });

  it("errors clearly when --from file is missing", () => {
    const { flags } = parseArgs(["--from", "/no/such/file-xyz.md"]);
    expect(() => resolveContent(flags)).toThrow(/--from file not found/);
  });

  it("errors when no content source is given", () => {
    const { flags } = parseArgs(["notes/a.md"]);
    expect(() => resolveContent(flags)).toThrow(/No content provided/);
  });
});
