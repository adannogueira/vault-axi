import { describe, it, expect } from "vitest";
import { parseWikiLinks, rewriteWikiLinks } from "../src/markdown/links.js";

describe("parseWikiLinks", () => {
  it("parses bare, aliased, headed, path, and embed links; ignores fenced/inline code", () => {
    const raw = [
      "See [[beta]] and [[plans/gamma|Gamma]] and [[delta#Section]].",
      "Embed: ![[image.md]]",
      "Inline `[[not-a-link]]` stays.",
      "```",
      "[[also-not]]",
      "```",
    ].join("\n");
    const links = parseWikiLinks(raw);
    const targets = links.map((l) => l.target);
    expect(targets).toContain("beta");
    expect(targets).toContain("plans/gamma");
    expect(targets).toContain("delta");
    expect(targets).toContain("image");
    expect(targets).not.toContain("not-a-link");
    expect(targets).not.toContain("also-not");

    const gamma = links.find((l) => l.target === "plans/gamma")!;
    expect(gamma.alias).toBe("Gamma");
    const delta = links.find((l) => l.target === "delta")!;
    expect(delta.heading).toBe("Section");
    expect(links.find((l) => l.target === "image")!.embed).toBe(true);
  });

  it("captures wikilinks declared in frontmatter (related: [[note]])", () => {
    const raw = ["---", "status: active", "related:", "  - \"[[parent-note]]\"", "---", "", "# Body", ""].join("\n");
    const targets = parseWikiLinks(raw).map((l) => l.target);
    expect(targets).toContain("parent-note");
  });
});

describe("rewriteWikiLinks (move/rename)", () => {
  it("rewrites matching targets, preserving heading and alias", () => {
    const raw = "Link to [[old-note]] and [[old-note#Goals|the goals]] plus [[keep-me]].";
    const { text, count } = rewriteWikiLinks(raw, (link) =>
      link.target.toLowerCase() === "old-note" ? "new-note" : null,
    );
    expect(count).toBe(2);
    expect(text).toContain("[[new-note]]");
    expect(text).toContain("[[new-note#Goals|the goals]]");
    expect(text).toContain("[[keep-me]]");
  });

  it("does not touch links inside code fences", () => {
    const raw = ["[[old]]", "```", "[[old]]", "```"].join("\n");
    const { text, count } = rewriteWikiLinks(raw, () => "new");
    expect(count).toBe(1);
    expect(text).toBe(["[[new]]", "```", "[[old]]", "```"].join("\n"));
  });
});
