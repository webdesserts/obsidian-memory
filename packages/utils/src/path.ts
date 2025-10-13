import path from "path";
import fs from "fs/promises";

/**
 * Validates that a file path is within the vault and safe to access
 */
export function validatePath(vaultPath: string, relativePath: string): string {
  // Remove leading slash if present
  const cleanPath = relativePath.startsWith("/")
    ? relativePath.slice(1)
    : relativePath;

  // Resolve absolute path
  const absolutePath = path.resolve(vaultPath, cleanPath);

  // Ensure path is within vault (prevent directory traversal)
  if (!absolutePath.startsWith(path.resolve(vaultPath))) {
    throw new Error(`Path outside vault: ${relativePath}`);
  }

  return absolutePath;
}

/**
 * Checks if a file exists
 */
export async function fileExists(filePath: string): Promise<boolean> {
  try {
    await fs.access(filePath);
    return true;
  } catch {
    return false;
  }
}

/**
 * Ensures .md extension on note paths
 */
export function ensureMarkdownExtension(notePath: string): string {
  return notePath.endsWith(".md") ? notePath : `${notePath}.md`;
}
