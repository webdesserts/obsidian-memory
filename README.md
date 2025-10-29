# Obsidian Memory

Collection of tools for deep Obsidian integration with AI agents, including an MCP server, utilities, and Claude Code plugin.

## Packages

This monorepo contains multiple independently-usable packages:

### [@obsidian-memory/mcp-server](./packages/mcp-server)

Model Context Protocol (MCP) server providing graph-aware memory system for Obsidian vaults.

**Features:**
- Graph navigation (wiki links, backlinks, neighborhoods)
- Smart note lookup with auto-search
- Memory system with Index.md and Working Memory.md
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

Claude Code plugin with MCP server integration and notetaker skill.

**Features:**
- Automatic MCP server startup
- Notetaker skill for automatic Working Memory updates
- Future: Session hooks for automated reflection workflow

**Installation:** See [Plugin Installation](#plugin-installation) below.

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

## Plugin Installation

The Claude Code plugin boots the MCP server and provides the notetaker skill.

### Setup

1. **Clone and build this repository:**
   ```bash
   cd ~/code/webdesserts
   git clone https://github.com/webdesserts/obsidian-memory.git
   cd obsidian-memory
   npm install && npm run build
   ```

2. **Link the MCP server globally** (so npx can find it):
   ```bash
   cd packages/mcp-server
   npm link
   ```

3. **Copy plugin to your dotfiles:**
   ```bash
   # Copy plugin files to dotfiles marketplace
   mkdir -p ~/.dots/<your-dotfiles>/claude/plugins/obsidian-memory
   cp -r packages/claude-plugin/* ~/.dots/<your-dotfiles>/claude/plugins/obsidian-memory/
   ```

4. **Create marketplace metadata** (one-time setup):

   Create `~/.dots/<your-dotfiles>/claude/plugins/.claude-plugin/marketplace.json`:
   ```json
   {
     "name": "plugins",
     "owner": {
       "name": "Your Name",
       "email": "your@email.com"
     },
     "plugins": [
       {
         "name": "obsidian-memory",
         "source": "./obsidian-memory",
         "description": "Obsidian Memory integration",
         "version": "1.0.0"
       }
     ]
   }
   ```

5. **Install the plugin in Claude Code:**
   ```bash
   # Add your dotfiles marketplace
   /plugin marketplace add ~/.dots/<your-dotfiles>/claude/plugins

   # Install the plugin
   /plugin install obsidian-memory@plugins

   # Restart Claude Code
   ```

### Configuration

The plugin uses `npx` to run the MCP server, so it works regardless of repository location. You only need to configure:

- **Vault location**: Default is `~/notes`. To change, edit `VAULT_PATH` in `~/.dots/<your-dotfiles>/claude/plugins/obsidian-memory/.claude-plugin/plugin.json`

### Updating

After making changes to the MCP server:
```bash
cd ~/code/webdesserts/obsidian-memory
npm run build
# MCP server will use updated code on next Claude Code restart
```

## License

MIT
