# Obsidian Memory

Collection of tools for deep Obsidian integration with AI agents, including an MCP server, utilities, and Claude Code plugin.

## Packages

This monorepo contains multiple independently-usable packages:

### [@obsidian-memory/mcp-server](./packages/mcp-server)

Model Context Protocol (MCP) server providing graph-aware memory system for Obsidian vaults.

**Features:**
- Graph navigation (wiki links, backlinks, neighborhoods)
- Smart note lookup with auto-search
- Memory system with Index.md and WorkingMemory.md
- Private memory with consent-based access
- MCP resources for auto-loaded memory files

**Usage:** Compatible with Claude Code, GitHub Copilot, or any MCP-compatible AI agent.

### [@obsidian-memory/utils](./packages/utils)

Shared utilities for working with Obsidian vaults in Node.js.

**Features:**
- Wiki link parsing (basic links, aliases, headers, blocks, embeds)
- Path validation and helpers
- ES module exports with tree-shaking support

**Usage:** Import directly into your own tools for Obsidian integration.

### @obsidian-memory/claude-plugin

*(Coming soon)* Claude Code plugin with notetaker agent and session hooks.

## Development

### Prerequisites

- Node.js 18+
- npm 7+ (for workspaces support)

### Setup

```bash
# Clone repository
git clone https://github.com/webdesserts/obsidian-memory.git
cd obsidian-memory

# Install dependencies
npm install

# Build all packages
npm run build
```

### Commands

```bash
# Build all packages
npm run build

# Build individual packages
npm run build:utils
npm run build:mcp

# Watch mode (all packages)
npm run dev

# Run tests
npm run test

# Clean build artifacts
npm run clean
```

## Architecture

### Monorepo Structure

```
obsidian-memory/
├── packages/
│   ├── utils/          # Shared Obsidian utilities
│   ├── mcp-server/     # MCP server implementation
│   └── claude-plugin/  # Claude Code plugin (future)
├── package.json        # Root workspace config
└── tsconfig.base.json  # Shared TypeScript config
```

### TypeScript Configuration

- **ES Modules:** All packages use ES module format with tree-shaking support
- **Project References:** Packages reference each other for type-safe development
- **Composite Mode:** Enabled for incremental builds

## License

MIT
