# Obsidian Memory - Developer Guide

> Collection of tools for Obsidian integration: MCP server, utilities, and Claude Code plugin

**Repository**: https://github.com/webdesserts/obsidian-memory

---

## What This Is

A Model Context Protocol (MCP) server that gives Claude Code deep integration with Obsidian vaults:

- **Graph navigation** - Explore notes via wiki links and backlinks
- **Smart note reading** - Flexible path resolution with auto-search
- **Memory system** - Auto-loaded Index.md for long-term memory
- **Resources** - Index/WorkingMemory exposed as MCP resources
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
- Generates prompt for Claude to consolidate WorkingMemory → Index
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

### Note Access
- **`read_note(note)`** - Read note content with smart lookup
  - Supports: `"Note Name"`, `"Note Name.md"`, `"knowledge/Note"`, `"memory://knowledge/Note"`
  - Auto-searches: graph index → `knowledge/` → `journal/` → root
  - Returns JSON metadata + frontmatter + content

### Graph Navigation
- **`get_backlinks(noteName, includePrivate)`** - Find notes linking here
  - Returns ResourceLinks for each backlink
- **`get_graph_neighborhood(noteName, depth, includePrivate)`** - Explore connections
  - Returns ResourceLinks grouped by distance

### Metadata
- **`get_frontmatter(path)`** - Get YAML frontmatter
- **`update_frontmatter(path, updates)`** - Update frontmatter fields

### Statistics
- **`get_note_usage(notes, period)`** - Query access stats for consolidation

### Memory Management
- **`load_private_memory(reason)`** - Load private Index/WorkingMemory (requires consent)
- **`consolidate_memory(includePrivate)`** - Trigger consolidation workflow
- **`complete_consolidation()`** - Mark consolidation done, delete WorkingMemory

---

## Available Resources

Exposed via MCP resources (auto-discoverable by Claude):

- **`memory://Index`** - Public long-term memory
- **`memory://WorkingMemory`** - Public short-term memory
- **`memory://private/Index`** - Private long-term (consent required)
- **`memory://private/WorkingMemory`** - Private short-term (consent required)

Resources support subscriptions for live updates.

---

## URI Scheme

All resources and ResourceLinks use `memory://` scheme:

```
memory://Index                      # Root-level files
memory://knowledge/MCP Servers      # Knowledge base notes
memory://journal/2024-w42           # Journal entries
memory://private/Personal Note      # Private notes
```

---

## Integration with Claude Code

### Global Settings (`~/.claude/CLAUDE.md`)
- Documents memory priority rules
- Auto-imports Index.md and WorkingMemory.md
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

**Why remove `write_note`?**
- Inconsistent with Claude Code's normal file editing
- Users should use standard Edit/Write tools
- `read_note` provides file path in metadata

**Why ResourceLinks in graph tools?**
- Avoids loading full note content
- Client decides what to actually read
- Better performance with large graphs
- Provides rich metadata (relationships, descriptions)
