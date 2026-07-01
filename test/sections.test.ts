import { describe, it, expect } from "vitest";
import {
  parseDoc,
  outline,
  editSection,
  insertAtHeading,
  appendToDoc,
  prependToDoc,
  replaceAnchor,
  resolveSection,
  SectionError,
} from "../src/markdown/sections.js";

const DOC = `---
date: 2026-07-01
status: draft
---

# AXI Tool Scratch Test

## Overview
This is the original overview text. It has two sentences. Here is the second.

## Design
Original design notes go here.

### Sub-detail
A nested subsection with some content.

## Open Questions
- First question
- Second question
`;

describe("parseDoc", () => {
  it("detects frontmatter and skips it when scanning headings", () => {
    const doc = parseDoc(DOC);
    expect(doc.frontmatter).toEqual({ startLine: 0, endLine: 3 });
    expect(doc.headings.map((h) => h.title)).toEqual([
      "AXI Tool Scratch Test",
      "Overview",
      "Design",
      "Sub-detail",
      "Open Questions",
    ]);
  });

  it("builds ancestor paths", () => {
    const doc = parseDoc(DOC);
    const sub = doc.headings.find((h) => h.title === "Sub-detail")!;
    expect(sub.path).toEqual(["AXI Tool Scratch Test", "Design", "Sub-detail"]);
  });

  it("ignores # inside fenced code blocks", () => {
    const raw = ["# Real", "", "```", "# not a heading", "```", "", "## Also Real", ""].join("\n");
    const doc = parseDoc(raw);
    expect(doc.headings.map((h) => h.title)).toEqual(["Real", "Also Real"]);
  });
});

describe("outline", () => {
  it("returns headings with 1-based lines and slash paths", () => {
    const entries = outline(parseDoc(DOC));
    expect(entries.find((e) => e.title === "Sub-detail")).toMatchObject({
      level: 3,
      path: "AXI Tool Scratch Test/Design/Sub-detail",
    });
  });
});

describe("editSection replace (the reproduced MCP bug)", () => {
  it("replaces the body in place, keeps a single heading, no EOF duplicate", () => {
    const out = editSection(DOC, "Design", "replace", "Replaced design notes.");
    // exactly one Design heading, still at ## level
    expect(out.match(/^#+ Design$/gm)).toEqual(["## Design"]);
    expect(out).toContain("## Design\nReplaced design notes.");
    // the original body is gone
    expect(out).not.toContain("Original design notes go here.");
    // nested subsection (a deeper level) survives because it belongs to the section body? No:
    // Sub-detail is level 3 under Design, so it is PART of Design's body and is replaced.
    expect(out).not.toContain("### Sub-detail");
    // sibling section untouched
    expect(out).toContain("## Open Questions");
    expect(out).toContain("- First question");
  });

  it("stops the body at the next equal-or-higher heading", () => {
    const out = editSection(DOC, "Overview", "replace", "New overview.");
    expect(out).toContain("## Overview\nNew overview.\n\n## Design");
    expect(out).toContain("Original design notes go here.");
  });

  it("targets a nested heading via a full path", () => {
    const out = editSection(DOC, "Design/Sub-detail", "replace", "New nested content.");
    expect(out).toContain("### Sub-detail\nNew nested content.");
    expect(out).toContain("Original design notes go here."); // Design's own body kept
  });
});

describe("editSection append / prepend", () => {
  it("appends after existing body, before the next heading", () => {
    const out = editSection(DOC, "Overview", "append", "Appended line.");
    expect(out).toContain(
      "Overview\nThis is the original overview text. It has two sentences. Here is the second.\n\nAppended line.\n\n## Design",
    );
  });

  it("prepends directly under the heading", () => {
    const out = editSection(DOC, "Overview", "prepend", "Prepended line.");
    expect(out).toContain("## Overview\nPrepended line.\n\nThis is the original overview");
  });
});

describe("insertAtHeading", () => {
  it("inserts before a heading", () => {
    const out = insertAtHeading(DOC, "Design", "before-heading", "## New Section\nbody");
    expect(out).toContain("## New Section\nbody\n\n## Design");
  });
  it("inserts after a heading line", () => {
    const out = insertAtHeading(DOC, "Design", "after-heading", "Injected first line.");
    expect(out).toContain("## Design\nInjected first line.\n\nOriginal design notes");
  });
});

describe("doc-level append / prepend", () => {
  it("appends to end of document", () => {
    const out = appendToDoc(DOC, "## Appended Tail\ncontent");
    expect(out.trimEnd().endsWith("## Appended Tail\ncontent")).toBe(true);
  });
  it("prepends after frontmatter", () => {
    const out = prependToDoc(DOC, "Intro paragraph.");
    expect(out).toMatch(/^---\ndate: 2026-07-01\nstatus: draft\n---\n\nIntro paragraph\.\n\n# AXI/);
  });
});

describe("replaceAnchor", () => {
  it("replaces a unique exact anchor", () => {
    const out = replaceAnchor(DOC, "First question", "1st question");
    expect(out).toContain("- 1st question");
  });
  it("throws when the anchor is missing", () => {
    expect(() => replaceAnchor(DOC, "nope-not-here", "x")).toThrow(SectionError);
  });
  it("throws when the anchor is ambiguous", () => {
    expect(() => replaceAnchor(DOC, "question", "x")).toThrow(/matched 2 times/);
  });
});

describe("resolveSection errors", () => {
  it("throws not_found for unknown heading", () => {
    expect(() => resolveSection(parseDoc(DOC), "Nonexistent")).toThrow(SectionError);
  });
});

describe("CRLF and no-frontmatter edge cases", () => {
  it("preserves CRLF line endings", () => {
    const crlf = "# Title\r\n\r\n## A\r\nbody a\r\n\r\n## B\r\nbody b\r\n";
    const out = editSection(crlf, "A", "replace", "new a");
    expect(out).toContain("\r\n");
    expect(out).not.toContain("body a");
    expect(out).toContain("## B\r\nbody b");
  });

  it("handles a document with no frontmatter and no trailing newline", () => {
    const raw = "# Only\nsome text";
    const out = editSection(raw, "Only", "replace", "replaced");
    expect(out).toBe("# Only\nreplaced");
  });

  it("handles the last section as body end", () => {
    const out = editSection(DOC, "Open Questions", "replace", "- New only question");
    expect(out.trimEnd().endsWith("## Open Questions\n- New only question")).toBe(true);
  });
});
