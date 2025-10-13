/**
 * Wiki link parser for Obsidian-style links
 *
 * Supports:
 * - Basic links: [[Note]]
 * - Aliases: [[Note|Alias]]
 * - Headers: [[Note#Header]]
 * - Blocks: [[Note#^block-id]]
 * - Embeds: ![[Note]]
 */

export interface WikiLink {
  /** The target note name (without .md extension) */
  target: string;
  /** The full target including headers/blocks */
  fullTarget: string;
  /** Optional display alias */
  alias?: string;
  /** Whether this is an embed (![[...]]) */
  isEmbed: boolean;
  /** Header reference if present */
  header?: string;
  /** Block ID if present */
  blockId?: string;
}

/**
 * Parse all wiki links from note content
 */
export function parseWikiLinks(content: string): WikiLink[] {
  const links: WikiLink[] = [];

  // Regex patterns
  const embedRegex = /!\[\[([^\]]+?)(?:\|([^\]]+?))?\]\]/g;
  const linkRegex = /(?<!!)\[\[([^\]]+?)(?:\|([^\]]+?))?\]\]/g;

  // Parse embeds first
  let match;
  while ((match = embedRegex.exec(content)) !== null) {
    const fullTarget = match[1];
    const alias = match[2];
    const link = parseLinkTarget(fullTarget);

    links.push({
      ...link,
      fullTarget,
      alias,
      isEmbed: true,
    });
  }

  // Reset regex
  linkRegex.lastIndex = 0;

  // Parse regular links (excluding embeds)
  while ((match = linkRegex.exec(content)) !== null) {
    const fullTarget = match[1];
    const alias = match[2];
    const link = parseLinkTarget(fullTarget);

    links.push({
      ...link,
      fullTarget,
      alias,
      isEmbed: false,
    });
  }

  return links;
}

/**
 * Parse link target to extract note, header, and block references
 */
function parseLinkTarget(fullTarget: string): Pick<WikiLink, "target" | "header" | "blockId"> {
  // Check for block reference: [[Note#^block-id]]
  const blockMatch = fullTarget.match(/^([^#]+?)#\^(.+)$/);
  if (blockMatch) {
    return {
      target: cleanNoteName(blockMatch[1]),
      blockId: blockMatch[2],
    };
  }

  // Check for header reference: [[Note#Header]]
  const headerMatch = fullTarget.match(/^([^#]+?)#(.+)$/);
  if (headerMatch) {
    return {
      target: cleanNoteName(headerMatch[1]),
      header: headerMatch[2],
    };
  }

  // Just a note name: [[Note]]
  return {
    target: cleanNoteName(fullTarget),
  };
}

/**
 * Clean note name (remove leading/trailing slashes, get filename only)
 */
function cleanNoteName(noteName: string): string {
  // Remove .md extension if present
  let cleaned = noteName.replace(/\.md$/, "");

  // Get just the filename (last part of path)
  if (cleaned.includes("/")) {
    const parts = cleaned.split("/");
    cleaned = parts[parts.length - 1];
  }

  return cleaned.trim();
}

/**
 * Extract all unique note names from wiki links
 */
export function extractLinkedNotes(content: string): string[] {
  const links = parseWikiLinks(content);
  const noteNames = links.map((link) => link.target);
  return Array.from(new Set(noteNames));
}
