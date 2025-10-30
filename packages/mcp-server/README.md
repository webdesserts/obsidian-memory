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
npm run build
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
