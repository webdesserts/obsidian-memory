# Obsidian Memory

Collection of tools for deep Obsidian integration with AI agents, including an MCP server, utilities, and Claude Code plugin.

## Packages

This monorepo contains multiple independently-usable packages:

### [@obsidian-memory/mcp-server](./packages/mcp-server)

Model Context Protocol (MCP) server providing graph-aware memory system for Obsidian vaults.

**Features:**
- Graph navigation (wiki links, backlinks, neighborhoods)
- Semantic search via embeddings (fast, offline, no API costs)
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
- Rust toolchain (for building semantic-embeddings WASM package)
- wasm-pack (install via `cargo install wasm-pack`)

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

## Troubleshooting

### Model Download Failures

**Problem:** `npm install` fails to download semantic embedding model files.

**Solution:**
```bash
cd packages/semantic-embeddings
npm run download-model
```

If download still fails, check:
- Network connectivity
- Hugging Face CDN availability
- Disk space (~87MB required for all-MiniLM-L6-v2 model)

### WASM Build Errors

**Problem:** `npm run build` fails with WASM-related errors.

**Solution:**

1. **Check Rust toolchain is installed:**
   ```bash
   rustc --version
   # Should show Rust 1.70+
   ```
   If not installed, get it from https://rustup.rs

2. **Check wasm-pack is installed:**
   ```bash
   wasm-pack --version
   # Should show wasm-pack 0.12+
   ```
   If not installed:
   ```bash
   cargo install wasm-pack
   ```

3. **Rebuild from clean state:**
   ```bash
   npm run clean
   cd packages/semantic-embeddings && npm run build
   cd ../.. && npm run build
   ```

### Missing Model Files Error

**Problem:** MCP server fails to start with "Missing model files" error.

**Solution:**

The semantic embeddings model wasn't downloaded. Run:
```bash
cd packages/semantic-embeddings
npm run download-model
npm run build
```

### WASM Module Load Errors at Runtime

**Problem:** Server crashes with "Cannot find module" for WASM files.

**Solution:**

Ensure the semantic-embeddings package was built:
```bash
cd packages/semantic-embeddings
npm run build
```

The WASM build creates `pkg/` directory with `.wasm` and `.js` files that the MCP server imports.

## Migration Guide

### Migrating from LLM-based Search (v0.0.x)

**What Changed:**

Version 0.1.0 replaces the LLM-guided graph traversal search with semantic embedding-based search.

**Why the Change:**

- **Faster:** Embedding search takes ~10ms vs ~5-10s for LLM search
- **No API costs:** No token usage for searches
- **No model download:** 87MB embedding model vs 4-7GB LLM model
- **More accurate:** Pure semantic similarity vs LLM interpretation

**Migration Steps:**

1. **Clean up old LLM files** (if you used the LLM-based search):
   ```bash
   rm -rf packages/mcp-server/models/  # Old GGUF model directory
   ```

2. **Update dependencies:**
   ```bash
   npm install
   ```

3. **Build new WASM package:**
   ```bash
   cd packages/semantic-embeddings
   npm run build
   cd ../..
   npm run build
   ```

4. **Clear old cache** (optional, but recommended):
   ```bash
   rm -f ~/notes/.obsidian/embedding-cache.json
   ```
   Cache will rebuild automatically on first search.

**Behavior Differences:**

- **Search results:** Now based on semantic similarity scores (0-1) instead of LLM relevance judgments
- **Speed:** First search may take 10-30s to encode all notes, subsequent searches are instant
- **Customization:** Use `minSimilarity` parameter to control result quality (default 0.3)
- **No natural language:** Query is embedded directly - use keywords rather than questions

**Example:**

```typescript
// Old LLM-based search (removed)
Search({ query: "How does React SSR work?" })

// New embedding-based search (current)
Search({ query: "React server-side rendering", minSimilarity: 0.3 })
```

## License

MIT
