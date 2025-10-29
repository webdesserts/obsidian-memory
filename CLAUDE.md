# Obsidian Memory - Developer Guide

> MCP server providing Claude Code agents with graph-based memory via Obsidian vault integration

**Repository**: https://github.com/webdesserts/obsidian-memory

**Project Status**: Active development, currently specific to author's personal workspace. May be generalized for broader use in the future.

---

## What Problem Does This Solve?

Claude Code agents have no memory between sessions. Every conversation starts fresh, requiring users to re-explain project context, past decisions, and discovered patterns. This MCP server solves that by:

1. **Persistent memory** - Index.md auto-loads at session start, providing permanent knowledge
2. **Graph navigation** - Explore interconnected notes via wiki links and backlinks
3. **Working memory** - Temporary scratch space (Log.md, Working Memory.md) for session notes
4. **Consolidation** - Tools to review and move temporary notes into permanent knowledge

The system mirrors human memory: working memory for active thoughts, long-term memory for permanent knowledge, and periodic reflection to consolidate insights.

---

## Exploring the Codebase

### Entry Points

**Start here:**
- `packages/mcp-server/src/index.ts` - Server setup, tool registration, initialization flow
- `packages/mcp-server/src/server.ts` - MCP server wrapper with tool registration helpers

**Core systems:**
- `packages/mcp-server/src/graph/` - Graph index tracking wiki links and backlinks
- `packages/mcp-server/src/memory/` - Memory system, access logging, reindex manager
- `packages/mcp-server/src/tools/` - Individual tool implementations (one file per tool)
- `packages/mcp-server/src/prompts/` - MCP prompts for guided workflows

**Shared utilities:**
- `packages/utils/src/` - Wiki link parsing, path helpers, note name extraction

### How Tools Are Structured

Each tool in `src/tools/` follows this pattern:
- Export a `register*` function that registers the tool with the MCP server
- Define input schema using Zod for validation
- Include JSDoc comments explaining behavior and parameters
- Return structured responses (text content + optional resources)

Tool registration happens in `index.ts` lines 107-119.

### Key Concepts

**Graph Index** - Scans vault on startup, builds link graph, tracks note locations. File watcher keeps it updated. Used for note discovery and neighborhood exploration.

**Memory System** - Loads Index.md on startup, logs access patterns for usage statistics, manages private memory consent.

**Reindex vs. Reflect** - Two separate consolidation processes:
- `reindex` - Updates Index.md entry points based on knowledge graph (no approval needed)
- `reflect` - Reviews Log.md and Working Memory.md, proposes consolidation into permanent notes (requires approval)

---

## Non-Obvious Patterns

### Working with Notes - IMPORTANT

**Use get_note tool for note discovery, then Read/Write tools for content:**

✅ **Correct workflow:**
```
1. get_note(note: "CSS") - Get metadata, links, file path
2. Read(file_path: "/Users/michael/notes/knowledge/CSS.md") - View content
3. Write(file_path: "/Users/michael/notes/knowledge/CSS.md") - Edit content
```

❌ **Incorrect:**
```
ReadMcpResourceTool(server: "obsidian-memory", uri: "memory:knowledge/CSS")
```

**Why this workflow?**
- get_note provides metadata (frontmatter, links, backlinks, paths) without loading full content
- Read tool satisfies Write tool's requirement (avoids "File has not been read yet" error)
- get_note's `memory:` URIs are for reference only - use `filePath` for Read/Write
- Clean integration with Claude Code's built-in diff and edit tools

**Note reference formats:** get_note accepts "Note Name", "knowledge/Note", "memory:Note", or "[[Note]]"

### memory: URIs vs. File Paths

**memory: URIs** - Used for reference and inter-tool communication (e.g., `memory:knowledge/CSS`)
**File paths** - Used with Claude Code's Read/Write tools (e.g., `/Users/.../notes/knowledge/CSS.md`)

Tools return both formats. Use `filePath` field for file operations, `uri` field for references.

### Error Responses vs. Exceptions

Tools return helpful error responses instead of throwing exceptions. Missing notes aren't protocol errors - the response includes guidance on where to create the note. This keeps workflows smooth.

---

## Integration Context

This MCP server is part of a larger memory system. The complete workflow (how notes are organized, when to consolidate, notetaking patterns) is documented separately:

- **Memory system workflow** - See `~/.dots/webdesserts-private/claude/plugins/obsidian-memory/instructions/notetaking.md`
- **Project notes** - Deeper context, design decisions, and future explorations tracked in personal Obsidian vault

This separation exists because the MCP server is a general-purpose tool, while the workflow is specific to personal knowledge management practices.

### Monorepo Structure

- `@obsidian-memory/utils` - Shared utilities (wiki-links, path helpers)
- `@obsidian-memory/mcp-server` - MCP server implementation
- `@obsidian-memory/claude-plugin` - Stub for future Claude Code plugin

Uses npm workspaces, TypeScript project references, and ES modules.

---

## Technical Notes

### Dependencies
- `@modelcontextprotocol/sdk` - Official MCP SDK
- `chokidar` - File watching for graph updates
- `gray-matter` - YAML frontmatter parsing
- `zod` - Schema validation

### Testing

Run tests with `npm test`. Each package has its own test suite in `src/**/*.test.ts`.
