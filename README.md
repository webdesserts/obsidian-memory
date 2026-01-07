# Obsidian Memory

Persistent memory for AI coding assistants.

Obsidian Memory is an [MCP](https://modelcontextprotocol.io/) server that lets Claude, OpenCode, and other AI assistants remember your projects, preferences, and past conversations by storing notes in your [Obsidian](https://obsidian.md) vault. Instead of starting fresh every session, your assistant can recall what you were working on, search through past decisions, and maintain context about your codebase.

**Who is this for?** Developers who use AI coding assistants and want them to actually remember things between sessions.

## Features

- **Graph navigation** - Wiki links, backlinks, neighborhood discovery
- **Semantic search** - Fast, offline embeddings (all-MiniLM-L6-v2) with Personalized PageRank graph boosting
- **Memory system** - Working Memory, Log, weekly journals, project notes
- **Private memory** - Consent-based access to sensitive notes
- **Project discovery** - Auto-loads project notes based on git remotes
- **Note management** - Create, read, update, move, and delete notes

## Installation

### Option 1: Homebrew (Mac)

```bash
brew tap webdesserts/tap
brew install obsidian-memory-mcp
```

### Option 2: Shell Script (Mac/Linux)

```bash
curl -fsSL https://github.com/webdesserts/obsidian-memory/releases/latest/download/obsidian-memory-mcp-installer.sh | sh
```

### Option 3: Build from Source

Requires Rust toolchain ([rustup.rs](https://rustup.rs)).

```bash
git clone https://github.com/webdesserts/obsidian-memory.git
cd obsidian-memory
cargo install --path crates/mcp-server
```

Note: Building from source uses runtime model download from HuggingFace. Pre-built binaries have the model embedded and work on corporate networks that block HuggingFace.

## Usage

### Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `OBSIDIAN_VAULT_PATH` | Yes | Path to your Obsidian vault (e.g., `~/notes` or `/home/user/notes`). Tilde expansion is supported. |

If `OBSIDIAN_VAULT_PATH` is not set, the server will exit with an error message.

### Running the Server

```bash
OBSIDIAN_VAULT_PATH=~/notes obsidian-memory
```

The server communicates over stdio and is designed to be launched by an MCP client. It indexes your vault on first run (may take a few seconds for large vaults) and watches for file changes.

### Claude Code Configuration

```bash
claude mcp add obsidian-memory --scope user \
  -e OBSIDIAN_VAULT_PATH=~/notes \
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
        "OBSIDIAN_VAULT_PATH": "~/notes"
      },
      "enabled": true
    }
  }
}
```

## How It Works

The server is read-only by default - it indexes your existing notes but won't modify them unless you explicitly use write tools. You can point it at an existing vault safely.

The memory system organizes notes into categories:

- **Temporary notes** (`Working Memory.md`, `Log.md`) - Scratchpad and activity log, frequently rewritten
- **Periodic notes** (`journal/`) - Weekly summaries that persist longer
- **Project notes** (`projects/`) - Context for specific codebases
- **Permanent notes** (`knowledge/`) - Stable reference material
- **Private notes** (`private/`) - Sensitive content, loaded only with explicit consent

### Memory Files

| File | Purpose | Auto-loaded |
|------|---------|-------------|
| `Working Memory.md` | Scratchpad for active work | Yes |
| `Log.md` | Chronological session activity | Yes |
| `journal/YYYY-wNN.md` | Weekly summaries and notes | Current week only |
| `projects/*.md` | Project-specific context | Matched by git remote URL or directory name |
| `knowledge/*.md` | Stable long-term notes | On demand |
| `private/*.md` | Sensitive notes | With consent |

### Search

The `Search` tool combines semantic embeddings with graph structure:

```json
{ "query": "typescript projects" }
{ "query": "[[TypeScript]]" }
{ "query": "[[TypeScript]] [[Projects]]" }
```

- Plain text queries use semantic similarity
- Wiki-links (like `[[TypeScript]]`) activate graph boosting - notes connected to the referenced note rank higher
- Multiple wiki-links find notes related to all referenced notes

## Available Tools

| Tool | Description |
|------|-------------|
| `Remember` | Load session context (Working Memory, Log, weekly journal, project notes) at session start |
| `Search` | Find notes by semantic similarity. Supports `query`, `include_private`, and `debug` parameters |
| `ReadNote` | Read full content of a note |
| `WriteNote` | Create or overwrite a note |
| `EditNote` | Make text replacements in a note (find/replace) |
| `MoveNote` | Move/rename a note (automatically updates wiki-links in other notes) |
| `DeleteNote` | Delete a note from the vault |
| `GetNoteInfo` | Get metadata, frontmatter, and links for a note |
| `UpdateFrontmatter` | Update YAML frontmatter fields |
| `Log` | Append a timestamped entry to Log.md |
| `WriteLogs` | Replace an entire day's log entries (for consolidation) |
| `GetWeeklyNote` | Get the path for the current week's journal note |
| `GetCurrentDatetime` | Get current datetime in ISO format |
| `Reflect` | Get instructions for memory consolidation |
| `LoadPrivateMemory` | Load notes from `private/` (requires explicit consent) |

## Development

Requires Rust 1.75+ (edition 2021).

```bash
# Run tests
cargo test

# Run locally (downloads model from HuggingFace on first run)
OBSIDIAN_VAULT_PATH=~/notes cargo run -p obsidian-memory-mcp

# Build release
cargo build --release

# Build with embedded model (for testing release builds)
./scripts/download-model.sh
cargo build --features embedded-model --no-default-features -p obsidian-memory-mcp
```

## Troubleshooting

**Model download fails during build**: If you're behind a corporate firewall that blocks HuggingFace, use the pre-built binaries (Homebrew or shell installer) which have the model embedded.

**Vault not found**: Ensure `OBSIDIAN_VAULT_PATH` is an absolute path to an existing directory. Check that the path doesn't have trailing slashes.

**Search returns no results**: The semantic index builds on first run. Give it a few seconds to index your vault. Large vaults (1000+ notes) may take longer.

**Project notes not loading**: Project discovery looks for notes in `projects/` that match either your git remote URL (e.g., `github.com/user/repo`) or your current directory name. Create a note like `projects/my-repo.md` to get started.

## License

[Apache 2.0](LICENSE)
