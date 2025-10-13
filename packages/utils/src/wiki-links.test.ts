import { describe, it, expect } from "vitest";
import { parseWikiLinks, extractLinkedNotes } from "./wiki-links.js";

describe("parseWikiLinks()", () => {
  it("should parse basic wiki links", () => {
    const content = "- [[CLAUDE]] - test\n- [[CLAUDE.local]] - another";
    const links = parseWikiLinks(content);

    expect(links).toHaveLength(2);
    expect(links[0].target).toBe("CLAUDE");
    expect(links[0].isEmbed).toBe(false);
    expect(links[1].target).toBe("CLAUDE.local");
  });

  it("should parse links with aliases", () => {
    const content = "[[Note Name|Display Text]]";
    const links = parseWikiLinks(content);

    expect(links).toHaveLength(1);
    expect(links[0].target).toBe("Note Name");
    expect(links[0].alias).toBe("Display Text");
  });

  it("should parse links with headers", () => {
    const content = "[[Note#Header Section]]";
    const links = parseWikiLinks(content);

    expect(links).toHaveLength(1);
    expect(links[0].target).toBe("Note");
    expect(links[0].header).toBe("Header Section");
  });

  it("should parse links with block references", () => {
    const content = "[[Note#^block-123]]";
    const links = parseWikiLinks(content);

    expect(links).toHaveLength(1);
    expect(links[0].target).toBe("Note");
    expect(links[0].blockId).toBe("block-123");
  });

  it("should parse embed links", () => {
    const content = "![[Image]]";
    const links = parseWikiLinks(content);

    expect(links).toHaveLength(1);
    expect(links[0].target).toBe("Image");
    expect(links[0].isEmbed).toBe(true);
  });

  it("should parse links with paths", () => {
    const content = "[[folder/subfolder/Note]]";
    const links = parseWikiLinks(content);

    expect(links).toHaveLength(1);
    expect(links[0].target).toBe("Note");
  });

  it("should handle multiple links in one line", () => {
    const content = "See [[Note1]] and [[Note2]] for details";
    const links = parseWikiLinks(content);

    expect(links).toHaveLength(2);
    expect(links[0].target).toBe("Note1");
    expect(links[1].target).toBe("Note2");
  });

  it("should handle links with .md extension", () => {
    const content = "[[Note.md]]";
    const links = parseWikiLinks(content);

    expect(links).toHaveLength(1);
    expect(links[0].target).toBe("Note");
  });
});

describe("extractLinkedNotes()", () => {
  it("should extract unique note names", () => {
    const content = `
      - [[CLAUDE]] - test
      - [[CLAUDE.local]] - another
      - [[CLAUDE]] - duplicate
    `;
    const notes = extractLinkedNotes(content);

    expect(notes).toHaveLength(2);
    expect(notes).toContain("CLAUDE");
    expect(notes).toContain("CLAUDE.local");
  });

  it("should extract notes from complex content", () => {
    const content = `
      # Knowledge Index

      ## Meta
      - [[CLAUDE]] - General vault navigation
      - [[CLAUDE.local]] - Current work

      ## Projects
      - [[Obsidian Memory MCP Server]]
    `;
    const notes = extractLinkedNotes(content);

    expect(notes).toHaveLength(3);
    expect(notes).toContain("CLAUDE");
    expect(notes).toContain("CLAUDE.local");
    expect(notes).toContain("Obsidian Memory MCP Server");
  });

  it("should handle embeds and regular links", () => {
    const content = "![[Image]] and [[Note]]";
    const notes = extractLinkedNotes(content);

    expect(notes).toHaveLength(2);
    expect(notes).toContain("Image");
    expect(notes).toContain("Note");
  });

  it("should return empty array for no links", () => {
    const content = "Just some text with no links";
    const notes = extractLinkedNotes(content);

    expect(notes).toHaveLength(0);
  });
});
