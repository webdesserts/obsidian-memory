# Obsidian Memory MCP Server

Custom MCP server for Obsidian with graph-aware memory system and intelligent consolidation.

## Features

- **Graph-based navigation**: Follow wiki links, explore backlinks, discover neighborhoods
- **Memory system**: Two-tier memory (Working Memory.md + Index.md) with automatic consolidation
- **Multi-device coordination**: Lock files and file watching for seamless sync across laptops
- **Private memory**: Separate private folder with explicit consent required

## Installation

```bash
npm install
```

**Note:** The `npm install` command automatically:
1. Builds the TypeScript source
2. Downloads the DeepSeek-R1-Distill-Qwen-1.5B model (~1.12 GB) for the Search tool

To skip model download (e.g., for development without Search):
```bash
SKIP_MODEL_DOWNLOAD=1 npm install
```

To manually download the model later:
```bash
npm run download-model
```

## Configuration

Add to Claude Code:

```bash
claude mcp add --transport stdio obsidian-memory \
  -- node /Users/michael/code/webdesserts/obsidian-memory-mcp/dist/index.js \
  --vault-path /Users/michael/notes
```

## Tools

### File Operations
- `read_note` - Read note content
- `write_note` - Create/modify notes
- `get_frontmatter` - Read metadata
- `UpdateFrontmatter` - Modify metadata

### Graph Navigation
- `follow_link` - Load linked note
- `get_backlinks` - Find what links here
- `GetGraphNeighborhood` - Explore connected notes

### Memory System
- `GetNoteUsage` - Query access statistics
- `LoadPrivateMemory` - Load private indexes (requires consent)

## Development

```bash
npm run dev      # Watch mode
npm test         # Run tests
npm run build    # Build for production
```

## License

MIT
