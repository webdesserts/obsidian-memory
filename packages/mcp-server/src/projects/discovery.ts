import { execSync } from 'node:child_process';
import { homedir } from 'node:os';
import path from 'node:path';
import matter from 'gray-matter';
import { readFileSync } from 'node:fs';
import type {
  ProjectMetadata,
  DiscoveryResult,
  DiscoveredProject,
  DirectoryInfo,
} from './types.js';
import type { GraphIndex } from '../graph/graph-index.js';

/**
 * Normalize git remote URL to a consistent format for comparison.
 * Handles both HTTPS and SSH formats, removes .git suffix, trailing slashes.
 *
 * Examples:
 * - https://github.com/user/repo.git → github.com/user/repo
 * - git@github.com:user/repo.git → github.com/user/repo
 * - git@bitbucket.org:user/repo → bitbucket.org/user/repo
 */
function normalizeRemote(remote: string): string {
  let normalized = remote.trim();

  // Convert SSH format (git@host:path) to pseudo-URL (host/path)
  if (normalized.startsWith('git@')) {
    normalized = normalized.replace(/^git@/, '').replace(':', '/');
  }

  // Remove protocol prefix (https://, http://, ssh://)
  normalized = normalized.replace(/^(https?|ssh):\/\//, '');

  // Remove .git suffix
  normalized = normalized.replace(/\.git$/, '');

  // Remove trailing slashes
  normalized = normalized.replace(/\/+$/, '');

  // Lowercase for case-insensitive comparison
  return normalized.toLowerCase();
}

/**
 * Extract git remotes from a directory.
 * Returns empty array if not a git repo or if git command fails.
 */
function getGitRemotes(dirPath: string): string[] {
  try {
    const output = execSync('git remote -v', {
      cwd: dirPath,
      encoding: 'utf-8',
      stdio: ['pipe', 'pipe', 'ignore'], // Suppress stderr
    });

    // Parse git remote output: "origin  git@github.com:user/repo.git (fetch)"
    const remotes = new Set<string>();
    for (const line of output.split('\n')) {
      const match = line.match(/^\S+\s+(\S+)\s+\((fetch|push)\)$/);
      if (match) {
        remotes.add(match[1]);
      }
    }

    return Array.from(remotes);
  } catch {
    return [];
  }
}

/**
 * Crawl from CWD up to home directory, collecting directory info.
 */
function crawlDirectories(cwd: string): DirectoryInfo[] {
  const home = homedir();
  const directories: DirectoryInfo[] = [];
  let current = path.resolve(cwd);
  let depth = 0;

  while (true) {
    const name = path.basename(current);
    const gitRemotes = getGitRemotes(current);

    directories.push({
      path: current,
      name,
      gitRemotes,
      depth,
    });

    // Stop at home directory
    if (current === home) break;

    // Stop at filesystem root
    const parent = path.dirname(current);
    if (parent === current) break;

    current = parent;
    depth++;
  }

  return directories;
}

/**
 * Load project metadata from a project note's frontmatter.
 */
function loadProjectMetadata(
  noteName: string,
  filePath: string
): ProjectMetadata | null {
  try {
    const content = readFileSync(filePath, 'utf-8');
    const { data } = matter(content);

    return {
      name: noteName,
      filePath,
      remotes: data.remotes,
      old_remotes: data.old_remotes,
      slug: data.slug,
      old_slugs: data.old_slugs,
    };
  } catch (error) {
    console.error(`Failed to load project metadata from ${filePath}:`, error);
    return null;
  }
}

/**
 * Get all project notes from the vault's projects/ folder.
 */
function getAllProjects(
  graphIndex: GraphIndex,
  vaultPath: string
): ProjectMetadata[] {
  const projects: ProjectMetadata[] = [];

  for (const noteName of graphIndex.getAllNotes()) {
    // Get all paths for this note (in case of duplicates)
    const paths = graphIndex.getAllNotePaths(noteName);

    for (const relativePath of paths) {
      // Only consider notes in projects/ folder
      if (!relativePath.startsWith('projects/')) continue;

      const filePath = path.join(vaultPath, relativePath + '.md');
      const metadata = loadProjectMetadata(noteName, filePath);
      if (metadata) {
        projects.push(metadata);
      }
    }
  }

  return projects;
}

/**
 * Check if a project matches a directory via strict matching (current remotes/slug).
 */
function isStrictMatch(
  project: ProjectMetadata,
  directory: DirectoryInfo
): { matched: boolean; on?: 'remote' | 'slug'; value?: string } {
  // Check current remotes
  if (project.remotes && directory.gitRemotes.length > 0) {
    const normalizedProjectRemotes = project.remotes.map(normalizeRemote);
    const normalizedDirRemotes = directory.gitRemotes.map(normalizeRemote);

    for (const projRemote of normalizedProjectRemotes) {
      for (const dirRemote of normalizedDirRemotes) {
        if (projRemote === dirRemote) {
          return { matched: true, on: 'remote', value: dirRemote };
        }
      }
    }
  }

  // Check current slug (case-insensitive exact match)
  if (project.slug) {
    if (project.slug.toLowerCase() === directory.name.toLowerCase()) {
      return { matched: true, on: 'slug', value: directory.name };
    }
  }

  return { matched: false };
}

/**
 * Check if a project matches a directory via loose matching (old remotes/slugs).
 * This indicates a disconnect - the project was previously linked but remote/dir was renamed.
 */
function isLooseMatch(
  project: ProjectMetadata,
  directory: DirectoryInfo
): { matched: boolean; on?: 'old_remote' | 'old_slug'; value?: string } {
  // Check old remotes
  if (project.old_remotes && directory.gitRemotes.length > 0) {
    const normalizedOldRemotes = project.old_remotes.map(normalizeRemote);
    const normalizedDirRemotes = directory.gitRemotes.map(normalizeRemote);

    for (const oldRemote of normalizedOldRemotes) {
      for (const dirRemote of normalizedDirRemotes) {
        if (oldRemote === dirRemote) {
          return { matched: true, on: 'old_remote', value: dirRemote };
        }
      }
    }
  }

  // Check old slugs (case-insensitive exact match)
  if (project.old_slugs) {
    for (const oldSlug of project.old_slugs) {
      if (oldSlug.toLowerCase() === directory.name.toLowerCase()) {
        return { matched: true, on: 'old_slug', value: directory.name };
      }
    }
  }

  return { matched: false };
}

/**
 * Find similar project names for suggestions when no match is found.
 * Uses simple case-insensitive substring matching.
 */
function findSimilarProjects(
  dirName: string,
  allProjects: ProjectMetadata[]
): ProjectMetadata[] {
  const lowerDirName = dirName.toLowerCase();
  const similar: ProjectMetadata[] = [];

  for (const project of allProjects) {
    const lowerProjectName = project.name.toLowerCase();
    const lowerSlug = project.slug?.toLowerCase();

    // Check if directory name is substring of project name or slug
    if (
      lowerProjectName.includes(lowerDirName) ||
      (lowerSlug && lowerSlug.includes(lowerDirName)) ||
      lowerDirName.includes(lowerProjectName) ||
      (lowerSlug && lowerDirName.includes(lowerSlug))
    ) {
      similar.push(project);
    }
  }

  return similar;
}

/**
 * Discover projects for a working directory.
 *
 * Algorithm:
 * 1. Crawl from CWD up to home directory
 * 2. For each directory, extract git remotes and directory name
 * 3. Search all project notes for strict matches (current remotes/slug)
 * 4. If no strict match, search for loose matches (old remotes/slugs)
 * 5. If no matches at all, find similar projects for suggestions
 * 6. Return all matches ordered by depth (closest first)
 */
export function discoverProjects(
  cwd: string,
  graphIndex: GraphIndex,
  vaultPath: string
): DiscoveryResult {
  const directories = crawlDirectories(cwd);
  const allProjects = getAllProjects(graphIndex, vaultPath);

  const strictMatches: DiscoveredProject[] = [];
  const looseMatches: DiscoveredProject[] = [];
  const allGitRemotes = new Set<string>();

  // Collect all git remotes from all directories
  for (const dir of directories) {
    for (const remote of dir.gitRemotes) {
      allGitRemotes.add(remote);
    }
  }

  // Check each directory against all projects
  for (const directory of directories) {
    for (const project of allProjects) {
      // Try strict match first
      const strict = isStrictMatch(project, directory);
      if (strict.matched) {
        strictMatches.push({
          metadata: project,
          matchType: 'strict',
          matchedOn: strict.on,
          matchedValue: strict.value,
          depth: directory.depth,
        });
        continue;
      }

      // Try loose match if no strict match
      const loose = isLooseMatch(project, directory);
      if (loose.matched) {
        looseMatches.push({
          metadata: project,
          matchType: 'loose',
          matchedOn: loose.on,
          matchedValue: loose.value,
          depth: directory.depth,
        });
      }
    }
  }

  // Find suggestions if no matches
  const suggestions =
    strictMatches.length === 0 && looseMatches.length === 0
      ? findSimilarProjects(directories[0].name, allProjects)
      : [];

  return {
    cwd,
    gitRemotes: Array.from(allGitRemotes),
    searchedPaths: directories.map((d) => d.path),
    strictMatches,
    looseMatches,
    suggestions,
  };
}
