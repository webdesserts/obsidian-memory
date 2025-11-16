import path from 'node:path';
import type { DiscoveryResult } from './types.js';

/**
 * Generate status message for project discovery results.
 * Returns empty string if strict matches were found (no message needed).
 */
export function generateDiscoveryStatusMessage(
  discoveryResult: DiscoveryResult,
  cwd: string
): string {
  // Strict matches - just return list of loaded projects
  if (discoveryResult.strictMatches.length > 0) {
    const projectNames = discoveryResult.strictMatches
      .map((m) => `[[${m.metadata.name}]]`)
      .join(', ');
    return `Projects auto-loaded: ${projectNames}`;
  }

  // Disconnect detected (loose match)
  if (discoveryResult.looseMatches.length > 0) {
    const match = discoveryResult.looseMatches[0];
    let message =
      `**Project disconnect detected**\n\n` +
      `Found project [[${match.metadata.name}]] via ${match.matchedOn} match.\n\n`;

    if (match.matchedOn === 'old_remote') {
      message +=
        `Current remote: ${discoveryResult.gitRemotes[0] || 'unknown'}\n` +
        `Note's expected remotes: ${match.metadata.remotes?.join(', ') || 'none'}\n\n` +
        `The remote has changed. Update the project note's frontmatter:\n` +
        `1. Move old remote to old_remotes array\n` +
        `2. Add new remote to remotes array\n` +
        `3. Use UpdateFrontmatter tool or edit the file directly`;
    } else if (match.matchedOn === 'old_slug') {
      message +=
        `Current directory: ${path.basename(cwd)}\n` +
        `Note's expected slug: ${match.metadata.slug || 'none'}\n\n` +
        `The directory name has changed. Update the project note's frontmatter:\n` +
        `1. Move old slug to old_slugs array\n` +
        `2. Update slug to match current directory name\n` +
        `3. Use UpdateFrontmatter tool or edit the file directly`;
    }

    return message;
  }

  // No match found - with suggestions
  if (discoveryResult.suggestions.length > 0) {
    const suggestions = discoveryResult.suggestions
      .map((p) => `- [[${p.name}]]`)
      .join('\n');

    return (
      `**No project found**\n\n` +
      `Directory: ${path.basename(cwd)}\n` +
      `Git remotes: ${discoveryResult.gitRemotes.join(', ') || 'none'}\n\n` +
      `Similar projects found:\n${suggestions}\n\n` +
      `Is this one of these existing projects, or a new project?\n` +
      `- To link to existing: Update project frontmatter with current remote/slug\n` +
      `- To create new: Write a new note in projects/ folder with appropriate frontmatter`
    );
  }

  // No match and no suggestions
  return (
    `**No project found**\n\n` +
    `Directory: ${path.basename(cwd)}\n` +
    `Git remotes: ${discoveryResult.gitRemotes.join(', ') || 'none'}\n\n` +
    `Create a new project note in projects/ folder with frontmatter:\n` +
    `\`\`\`yaml\n` +
    `---\n` +
    `type: project\n` +
    (discoveryResult.gitRemotes.length > 0
      ? `remotes:\n  - ${discoveryResult.gitRemotes[0]}\n`
      : `slug: ${path.basename(cwd).toLowerCase()}\n`) +
    `---\n` +
    `\`\`\``
  );
}
