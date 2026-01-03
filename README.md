# Obsidian Memory

A pure Rust MCP (Model Context Protocol) server for graph-aware memory integration with Obsidian vaults.

## Features

- **Graph navigation** - Wiki links, backlinks, neighborhood discovery
- **Semantic search** - Fast, offline embeddings (all-MiniLM-L6-v2)
- **Memory system** - Working Memory, Log, weekly journals, project notes
- **Private memory** - Consent-based access to sensitive notes
- **Project discovery** - Auto-loads project notes based on git remotes

## Installation

### Prerequisites

- Rust toolchain (install via [rustup.rs](https://rustup.rs))
- Obsidian vault

### Install

```bash
git clone https://github.com/webdesserts/obsidian-memory.git
cd obsidian-memory

# Install to ~/.cargo/bin (must be in your PATH)
cargo install --path .

# Verify installation
obsidian-memory --help
```

In the future, you'll be able to install directly from crates.io:
```bash
cargo install obsidian-memory  # Not yet published
```

### ML Model

The semantic search uses the all-MiniLM-L6-v2 model (~87MB). The model is **automatically downloaded** on first search to `$VAULT/.obsidian/models/all-MiniLM-L6-v2/`.

Alternatively, for a shared location or offline setup:
```bash
node scripts/download-model.js
```
This downloads to `models/all-MiniLM-L6-v2/` in the project root (requires updating the code to use this path).

## Usage

### Environment Variables

- `OBSIDIAN_VAULT_PATH` - Path to your Obsidian vault (required)

### Running the Server

```bash
OBSIDIAN_VAULT_PATH=/path/to/vault obsidian-memory
```

### Claude Code Configuration

```bash
claude mcp add obsidian-memory --scope user \
  -e OBSIDIAN_VAULT_PATH=/path/to/vault \
  -- obsidian-memory
```

### OpenCode Configuration

Add to `~/.config/opencode/opencode.json`:

```json
{
  "mcp": {
    "obsidian-memory": {
      "type": "local",
      "command": ["obsidian-memory"],
      "environment": {
        "OBSIDIAN_VAULT_PATH": "/path/to/vault"
      },
      "enabled": true
    }
  }
}
```

## Project Structure

```
obsidian-memory/
├── Cargo.toml              # Workspace root
├── crates/
│   ├── mcp-server/         # MCP server binary
│   ├── wiki-links/         # Wiki-link parsing
│   ├── obsidian-fs/        # Vault filesystem utilities
│   └── semantic-embeddings/# ML embeddings
├── models/                 # Downloaded ML models (gitignored)
└── scripts/
    └── download-model.js   # Model download script
```

## Development

```bash
# Run tests
cargo test

# Build debug
cargo build

# Build release
cargo build --release
```

## Available Tools

| Tool | Description |
|------|-------------|
| GetCurrentDatetime | Get current datetime for log entries |
| Log | Append timestamped entry to Log.md |
| GetWeeklyNote | Get URI for current week's journal |
| GetNote | Get metadata and links for a note |
| UpdateFrontmatter | Update note frontmatter |
| Remember | Load session context (Working Memory, Log, projects) |
| Search | Semantic search with graph boosting |
| WriteLogs | Bulk replace day's log entries |
| Reflect | Memory consolidation prompt |
| LoadPrivateMemory | Load private notes (requires consent) |

## License

MIT
