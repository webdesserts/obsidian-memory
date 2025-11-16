# Obsidian Memory - Developer Guide

> MCP server providing Claude Code agents with graph-based memory via Obsidian vault integration

**Repository**: https://github.com/webdesserts/obsidian-memory

**Project Status**: Active development, currently specific to author's personal workspace. May be generalized for broader use in the future.

---

## Documentation Principle

**This file teaches you HOW to discover information, not WHAT the information is.**

Don't list all tools, all types, all configuration options, etc. in this file. That creates maintenance burden - every time the code changes, the docs get out of sync.

Instead, document:
- Where to find information (file paths, patterns)
- How to read the code (entry points, structure)
- Non-obvious patterns that aren't clear from code alone
- Context that explains "why" decisions were made

The code is the source of truth. This file is a map to reading the code effectively.

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
- `packages/mcp-server/src/embeddings/` - Semantic embedding cache and WASM manager
- `packages/mcp-server/src/tools/` - Individual tool implementations (one file per tool)
- `packages/mcp-server/src/prompts/` - MCP prompts for guided workflows
- `packages/semantic-embeddings/` - Rust WASM package for sentence transformers

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

**Embedding System** - Generates semantic embeddings for all notes using WASM-based sentence transformers (all-MiniLM-L6-v2). SHA-256 content hashing with disk cache for fast startup. File watcher invalidates cache on changes.

**Graph Proximity System** - Computes structural similarity via Personalized PageRank random walks. SHA-256 link signature caching with disk persistence. Integrated into Search tool for graph-boosted results when wiki-links are used.

**Search** - Uses semantic similarity search (cosine similarity) to find relevant notes. Pre-encodes all notes at startup for instant search results. When wiki-link references like `[[TypeScript]]` are present, automatically enhances results with graph proximity scores (Personalized PageRank).

**Reindex vs. Reflect** - Two separate consolidation processes:
- `reindex` - Updates Index.md entry points based on knowledge graph (no approval needed)
- `reflect` - Reviews Log.md and Working Memory.md, proposes consolidation into permanent notes (requires approval)

---

## Non-Obvious Patterns

### Working with Notes - IMPORTANT

**Use GetNote tool for note discovery, then Read/Write tools for content:**

✅ **Correct workflow:**
```
1. GetNote(note: "CSS") - Get metadata, links, file path
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

### memory: URIs vs. File Paths

**memory: URIs** - Used for reference and inter-tool communication (e.g., `memory:knowledge/CSS`)
**File paths** - Used with Claude Code's Read/Write tools (e.g., `/Users/.../notes/knowledge/CSS.md`)

Tools return both formats. Use `filePath` field for file operations, `uri` field for references.

### Error Responses vs. Exceptions

Tools return helpful error responses instead of throwing exceptions. Missing notes aren't protocol errors - the response includes guidance on where to create the note. This keeps workflows smooth.

### Project Discovery

**How it works:** The Remember tool automatically discovers project notes when it loads session context by:
1. Crawling from CWD up to home directory
2. Extracting git remotes and directory names from each directory
3. Searching `projects/` folder for notes with matching `remotes` or `slug` in frontmatter
4. Detecting disconnects via `old_remotes`/`old_slugs` when current values don't match

**Matching strategy:**
- **Strict match** (auto-load): Current git remote matches note's `remotes`, OR directory name matches note's `slug`
- **Loose match** (disconnect): Current git remote matches note's `old_remotes`, OR directory name matches note's `old_slugs`
- **No match**: Search for similar project names, suggest to user

**Implementation:**
- `packages/mcp-server/src/projects/discovery.ts` - Core discovery algorithm
- `packages/mcp-server/src/projects/types.ts` - Project metadata and result types
- `packages/mcp-server/src/tools/Remember.ts` - Integration point

**Cross-machine resilience:** Git remotes are machine-independent. Slug matching works as long as directory names are consistent. Old remotes/slugs enable recovery after renames.

**Parent directory discovery:** Enables patterns like `/code/spatialkey/skweb` loading both `SpatialKey` (parent, slug-matched) and `SKWeb` (child, remote-matched) project notes.

---

## Discovering Available Tools

**Where tools are defined:** `packages/mcp-server/src/tools/` - One file per tool

**Tool registration:** `packages/mcp-server/src/index.ts` lines 110-122 - All `register*` calls

**Tool naming convention:** PascalCase (e.g., `GetNote`, `LoadPrivateMemory`, `CompleteReflect`)

**Finding tool details:**
- Each tool file exports a `register*` function
- JSDoc comments explain behavior and parameters
- Zod schemas define input validation (look for `inputSchema` in registration)
- MCP annotations show hints: `readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`

**Example - Reading a tool:**
```typescript
// packages/mcp-server/src/tools/GetNote.ts
export function registerGetNote(server: McpServer, context: ToolContext) {
  server.registerTool(
    "GetNote",  // Tool name
    {
      description: "...",
      inputSchema: { note: z.string().describe("...") },  // Parameters
      annotations: { readOnlyHint: true }  // Behavior hints
    },
    async ({ note }) => { ... }  // Implementation
  );
}
```

**Current tool categories** (see registration for current list):
- Note discovery (GetNote, GetWeeklyNote)
- Temporal memory (Log, GetCurrentDatetime)
- Graph navigation (GetGraphNeighborhood)
- Metadata (UpdateFrontmatter)
- Statistics (GetNoteUsage)
- Memory management (LoadPrivateMemory, Reindex, CompleteReindex, Reflect, CompleteReflect)

---

## Integration Context

This MCP server is part of a larger memory system. The complete workflow (how notes are organized, when to consolidate, notetaking patterns) is documented separately:

- **Memory system workflow** - See `~/.dots/webdesserts-private/claude/plugins/obsidian-memory/instructions/notetaking.md`
- **Project notes** - Deeper context, design decisions, and future explorations tracked in personal Obsidian vault

This separation exists because the MCP server is a general-purpose tool, while the workflow is specific to personal knowledge management practices.

### Monorepo Structure

- `@obsidian-memory/utils` - Shared utilities (wiki-links, path helpers)
- `@obsidian-memory/mcp-server` - MCP server implementation
- `@obsidian-memory/semantic-embeddings` - Rust WASM package for embeddings
- `@obsidian-memory/claude-plugin` - Stub for future Claude Code plugin

Uses npm workspaces, TypeScript project references, and ES modules.

### Building

```bash
# Build WASM embeddings package first
cd packages/semantic-embeddings
npm run build

# Build MCP server
cd ../mcp-server
npm run build
```

The semantic-embeddings model files (~87MB) are downloaded automatically via npm prepare script.

---

## Technical Notes

### Dependencies

**TypeScript (MCP Server):**
- `@modelcontextprotocol/sdk` - Official MCP SDK
- `chokidar` - File watching for graph updates
- `gray-matter` - YAML frontmatter parsing
- `zod` - Schema validation

**Rust (Semantic Embeddings WASM):**
- `candle-core`, `candle-nn`, `candle-transformers` - ML inference framework
- `tokenizers` - Hugging Face tokenizer with WASM support
- `wasm-bindgen` - Rust/JavaScript bindings

### Testing

Run tests with `npm test`. Each package has its own test suite in `src/**/*.test.ts`.
