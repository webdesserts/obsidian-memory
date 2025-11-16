/**
 * Project discovery types for linking working directories to project notes
 */

/**
 * Project metadata extracted from note frontmatter
 */
export interface ProjectMetadata {
  /** Project note name (without .md extension) */
  name: string;
  /** Absolute file path to the project note */
  filePath: string;
  /** Current expected git remotes for this project */
  remotes?: string[];
  /** Previous git remotes (for detecting renames/disconnects) */
  old_remotes?: string[];
  /** Current directory name matcher (case-insensitive) */
  slug?: string;
  /** Previous directory names (for detecting renames/disconnects) */
  old_slugs?: string[];
}

/**
 * Type of match discovered during project discovery
 */
export type MatchType =
  /** Current remote or slug matched - auto-load silently */
  | 'strict'
  /** Old remote or old slug matched - prompt to update */
  | 'loose'
  /** No match found - search for similar projects */
  | 'none';

/**
 * Information about a single discovered project
 */
export interface DiscoveredProject {
  /** Project metadata from frontmatter */
  metadata: ProjectMetadata;
  /** Type of match */
  matchType: MatchType;
  /** What matched (e.g., 'remote', 'slug', 'old_remote', 'old_slug') */
  matchedOn?: 'remote' | 'slug' | 'old_remote' | 'old_slug';
  /** The actual value that matched */
  matchedValue?: string;
  /** Directory depth (0 = CWD, 1 = parent, etc.) */
  depth: number;
}

/**
 * Result of project discovery for a working directory
 */
export interface DiscoveryResult {
  /** Current working directory */
  cwd: string;
  /** Git remotes found in CWD (if any) */
  gitRemotes: string[];
  /** All directories checked (CWD → parents up to home) */
  searchedPaths: string[];
  /** Projects discovered with strict matches */
  strictMatches: DiscoveredProject[];
  /** Projects discovered with loose matches (disconnects) */
  looseMatches: DiscoveredProject[];
  /** Suggested similar projects if no matches */
  suggestions: ProjectMetadata[];
}

/**
 * Directory information extracted during discovery
 */
export interface DirectoryInfo {
  /** Absolute path to directory */
  path: string;
  /** Directory basename */
  name: string;
  /** Git remotes for this directory (empty if not a git repo) */
  gitRemotes: string[];
  /** Depth from CWD (0 = CWD, 1 = parent, etc.) */
  depth: number;
}
