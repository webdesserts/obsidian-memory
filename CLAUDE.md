# Obsidian Memory - Developer Guide

> Collection of tools for Obsidian integration: MCP server, utilities, and Claude Code plugin

**Repository**: https://github.com/webdesserts/obsidian-memory

---

## Working with Notes - IMPORTANT

**Use GetNote tool for note discovery, then Read/Write tools for content:**

✅ **Correct workflow:**
```
1. GetNote(noteRef: "CSS") - Get metadata, links, file path
2. Read(file_path: "/Users/michael/notes/knowledge/CSS.md") - View content
3. Write(file_path: "/Users/michael/notes/knowledge/CSS.md") - Edit content
```

❌ **Incorrect:**
```
ReadMcpResourceTool(server: "obsidian-memory", uri: "memory:knowledge/CSS")
```

**Why this workflow?**
- GetNote provides metadata (frontmatter, links, backlinks, paths) without loading full content
- Read tool satisfies Write tool's requirement (avoids "File has not been read yet" error)
- GetNote's `memory:` URIs are for reference only - use `filePath` for Read/Write
- Clean integration with Claude Code's built-in diff and edit tools

**Note reference formats:** GetNote accepts "Note Name", "knowledge/Note", "memory:Note", or "[[Note]]"

---

## What This Is

A Model Context Protocol (MCP) server that gives Claude Code deep integration with Obsidian vaults:

- **Graph navigation** - Explore notes via wiki links and backlinks
- **Note discovery** - GetNote tool provides metadata, links, and file paths
- **Memory system** - Auto-loaded Index.md for long-term memory
- **Resources** - Index/Working Memory exposed as MCP resources
- **Private memory** - Consent-based access to private notes

---

## Monorepo Structure

**Packages:**
- `@obsidian-memory/utils` - Shared utilities for Obsidian (wiki-links, path helpers)
- `@obsidian-memory/mcp-server` - MCP server implementation
- `@obsidian-memory/claude-plugin` - Stub for future Claude Code plugin

**Architecture:**
- ES modules with tree-shaking support
- TypeScript project references for type-safe cross-package imports
- npm workspaces for dependency management

---

## Current Architecture (MCP Server)

### Core Components

**Graph Index** (`packages/mcp-server/src/graph/graph-index.ts`)
- Scans vault on startup, builds link graph
- Tracks forward links and backlinks
- File watcher for incremental updates
- Stores note paths for ResourceLink generation

**Memory System** (`packages/mcp-server/src/memory/memory-system.ts`)
- Auto-loads `Index.md` on startup
- Access logging for usage statistics
- Private memory gated behind consent

**File Operations** (`packages/mcp-server/src/tools/file-operations.ts`)
- Read/write notes with frontmatter support
- Gray-matter for YAML parsing

**Consolidation** (`packages/mcp-server/src/memory/consolidation.ts`)
- Lock-based workflow for Index.md updates
- Generates prompt for Claude to consolidate Working Memory → Index
- Partially implemented - see GitHub issues

### Shared Utilities (`packages/utils/src/`)

**Wiki Links** (`wiki-links.ts`)
- Parse Obsidian-style wiki links from markdown
- Support for aliases, headers, blocks, embeds
- Extract linked note names

**Path Utilities** (`path.ts`)
- Path validation within vault (prevents directory traversal)
- File existence checks
- Markdown extension helpers

---

## Available Tools

### Note Discovery
- **`GetNote(noteRef)`** - Get metadata and graph connections for a note
  - Returns: frontmatter, file paths, forward links, backlinks
  - Note reference formats: "Note Name", "knowledge/Note", "memory:Note", "[[Note]]"
  - Use returned `filePath` for Read/Write operations
  - Links returned as `memory:` URIs for reference

### Weekly Journal
- **`GetWeeklyNote()`** - Get ResourceLink to current week's journal note
  - Returns URI like `memory:journal/2025-w42`

### Graph Navigation
- **`GetGraphNeighborhood(noteName, depth, includePrivate)`** - Explore multi-hop connections
  - Returns ResourceLinks grouped by distance
  - Use for deep graph exploration (2+ hops)

### Metadata
- **`UpdateFrontmatter(path, updates)`** - Update YAML frontmatter fields

### Statistics
- **`GetNoteUsage(notes, period)`** - Query access stats for consolidation

### Memory Management
- **`LoadPrivateMemory(reason)`** - Load private Index/Working Memory (requires consent)
- **`ConsolidateMemory(includePrivate)`** - Trigger consolidation workflow
- **`CompleteConsolidation()`** - Mark consolidation done, delete Working Memory

---

## Available Resources

### Static Resources

Auto-loaded on startup (auto-discoverable by Claude):

- **`memory:Index`** - Public long-term memory
- **`memory:Working Memory`** - Public short-term memory
- **`memory:private/Index`** - Private long-term (consent required)
- **`memory:private/Working Memory`** - Private short-term (consent required)

Resources support subscriptions for live updates.

**Note:** For reading arbitrary notes, use GetNote tool instead of resources. Resources are only for auto-loaded memory files.

---

## URI Scheme

All resources and ResourceLinks use `memory:` scheme (opaque URLs):

```
memory:Index                      # Root-level files
memory:knowledge/MCP Servers      # Knowledge base notes
memory:journal/2024-w42           # Journal entries
memory:private/Personal Note      # Private notes
```

---

## Integration with Claude Code

### Global Settings (`~/.claude/CLAUDE.md`)
- Documents memory priority rules
- Auto-imports Index.md and Working Memory.md
- Instructs Claude to search memory before answering

### Permissions (`~/.claude/settings.json`)
- All tools allowed for seamless integration

---

## Technical Notes

### Dependencies
- `@modelcontextprotocol/sdk` - Official MCP SDK
- `chokidar` - File watching
- `gray-matter` - YAML frontmatter parsing
- `zod` - Schema validation

### Key Design Decisions

**Why graph index?**
- Fast lookups without filesystem scans
- Enables bi-directional link queries
- Needed for ResourceLink generation

**Why smart lookup?**
- Users reference notes by name, not path
- Graph index finds most notes automatically
- Fallback search provides convenience

**Why resources for memory files?**
- More semantic than tools
- Discoverable in Claude's UI
- Supports subscriptions for live updates
- Natural consent flow for private resources

**Why GetNote tool instead of resource template?**
- Works with Claude Code's Write tool requirement (Read must be called first)
- Returns metadata without loading full content (more efficient)
- Provides both discovery (links/backlinks) and paths for Read/Write
- Clean integration with built-in diff and edit tools

**Why memory: URIs in GetNote responses?**
- Consistent reference format across tools
- Human-readable (easier to understand than file paths)
- Compatible with other tools that accept note references
- Note: Use `filePath` from response for Read/Write, not the memory: URI

**Why return error responses instead of throwing?**
- Missing notes aren't protocol errors
- Error responses provide helpful guidance (where to create the note)
- Tool succeeds with useful information rather than failing
