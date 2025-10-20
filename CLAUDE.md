# Obsidian Memory - Developer Guide

> Collection of tools for Obsidian integration: MCP server, utilities, and Claude Code plugin

**Repository**: https://github.com/webdesserts/obsidian-memory

---

## Reading Notes - IMPORTANT

**ALWAYS use MCP resources for reading notes**, NOT the Read tool:

✅ **Correct:**
```
ReadMcpResourceTool(server: "obsidian-memory", uri: "memory:knowledge/CSS")
ReadMcpResourceTool(server: "obsidian-memory", uri: "memory:journal/2025-w42")
```

❌ **Incorrect:**
```
Read(file_path: "/Users/michael/notes/knowledge/CSS.md")
```

**Why resources?**
- Smart path resolution (finds notes even without exact path)
- Graph index lookup (searches across knowledge/, journal/, root)
- Structured metadata (filePath, obsidianUri, frontmatter)
- Consistent with resource-based architecture

**When to use Read:** Only for non-note files (config files, source code, etc.)

---

## What This Is

A Model Context Protocol (MCP) server that gives Claude Code deep integration with Obsidian vaults:

- **Graph navigation** - Explore notes via wiki links and backlinks
- **Resource-based note access** - Read notes via `memory:{path}` template
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

### Weekly Journal
- **`GetWeeklyNote()`** - Get ResourceLink to current week's journal note
  - Returns URI like `memory:journal/2025-w42`
  - Use ReadMcpResourceTool to read the note content

### Graph Navigation
- **`GetBacklinks(noteName, includePrivate)`** - Find notes linking here
  - Returns ResourceLinks for each backlink
- **`GetGraphNeighborhood(noteName, depth, includePrivate)`** - Explore connections
  - Returns ResourceLinks grouped by distance

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

### Resource Template

- **`memory:{path}`** - Read any note in the vault
  - Examples: `memory:knowledge/CSS`, `memory:journal/2025-w42`
  - Smart path resolution with auto-search
  - Returns embedded resource with structured metadata

Resources support subscriptions for live updates.

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

**Why resource template instead of ReadNote tool?**
- More semantic - notes are data, not actions
- Better architecture - aligns with MCP resource model
- Discoverable - Claude Code can browse available resources
- Consistent - graph tools return ResourceLinks that work with template

**Why ResourceLinks in graph tools?**
- Avoids loading full note content
- Client decides what to actually read
- Better performance with large graphs
- Provides rich metadata (relationships, descriptions)

**Why return error responses instead of throwing?**
- Missing notes aren't protocol errors
- Error responses provide helpful guidance (where to create the note)
- Tool succeeds with useful information rather than failing
