import { describe, it, expect } from "vitest";
import {
  readFrontmatter,
  setFrontmatterField,
  unsetFrontmatterField,
  coerceValue,
} from "../src/markdown/frontmatter.js";

const DOC = `---
date: 2026-07-01
status: draft
tags:
  - axi
---

# Body

Content here.
`;

describe("readFrontmatter", () => {
  it("parses fields", () => {
    const { data } = readFrontmatter(DOC);
    expect(data).toMatchObject({ status: "draft", tags: ["axi"] });
  });
  it("returns null when absent", () => {
    expect(readFrontmatter("# No fm\n").data).toBeNull();
  });
});

describe("setFrontmatterField", () => {
  it("updates an existing field without touching the body", () => {
    const out = setFrontmatterField(DOC, "status", "active");
    expect(readFrontmatter(out).data).toMatchObject({ status: "active" });
    expect(out).toContain("# Body\n\nContent here.");
    expect(out).toContain("date: 2026-07-01");
  });

  it("adds a new field", () => {
    const out = setFrontmatterField(DOC, "owner", "adan");
    expect(readFrontmatter(out).data).toMatchObject({ owner: "adan" });
  });

  it("creates a frontmatter block when none exists", () => {
    const out = setFrontmatterField("# Title\n\nbody\n", "status", "draft");
    expect(out.startsWith("---\nstatus: draft\n---\n")).toBe(true);
    expect(out).toContain("# Title\n\nbody");
  });
});

describe("unsetFrontmatterField", () => {
  it("removes a field", () => {
    const out = unsetFrontmatterField(DOC, "status");
    expect(readFrontmatter(out).data).not.toHaveProperty("status");
    expect(readFrontmatter(out).data).toMatchObject({ date: "2026-07-01" });
  });
});

describe("coerceValue", () => {
  it("keeps date-like strings as strings", () => {
    expect(coerceValue("2026-07-01")).toBe("2026-07-01");
  });
  it("coerces numbers and booleans", () => {
    expect(coerceValue("42")).toBe(42);
    expect(coerceValue("true")).toBe(true);
  });
  it("coerces inline lists", () => {
    expect(coerceValue("[a, b]")).toEqual(["a", "b"]);
  });
});
