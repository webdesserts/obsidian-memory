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
      `Found [[${match.metadata.name}]] via ${match.matchedOn} match.\n\n`;

    if (match.matchedOn === 'old_remote') {
      message +=
        `Current: ${discoveryResult.gitRemotes[0] || 'unknown'}\n` +
        `Expected: ${match.metadata.remotes?.join(', ') || 'none'}\n\n` +
        `Update frontmatter to move old remote to old_remotes array.`;
    } else if (match.matchedOn === 'old_slug') {
      message +=
        `Current: ${path.basename(cwd)}\n` +
        `Expected: ${match.metadata.slug || 'none'}\n\n` +
        `Update frontmatter to move old slug to old_slugs array.`;
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
      `Dir: ${path.basename(cwd)} | Remotes: ${discoveryResult.gitRemotes.join(', ') || 'none'}\n\n` +
      `Similar:\n${suggestions}\n\n` +
      `Existing project or new?`
    );
  }

  // No match and no suggestions
  return (
    `**No project found**\n\n` +
    `Dir: ${path.basename(cwd)} | Remotes: ${discoveryResult.gitRemotes.join(', ') || 'none'}\n\n` +
    `Create project note with appropriate frontmatter.`
  );
}
