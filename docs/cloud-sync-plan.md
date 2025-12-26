# Cloud Sync Plan for Obsidian Memory

## Problem Statement

The user uses obsidian-memory locally on their machine with Obsidian Sync to sync notes between computers. They want to access their notes from Claude Code running in cloud environments (e.g., on their phone) where there's no local filesystem access to their vault.

**Current limitation:** obsidian-memory only supports stdio transport, requiring the MCP server to run on the same machine as Claude.

## Requirements

1. **Phone access:** Must work from Claude iOS app (no local filesystem)
2. **Claude Chat support:** Must work with Claude Desktop/Chat, not just Claude Code
3. **Simple setup:** "Just run this" simplicity for home server deployment
4. **Keep Obsidian Sync:** User wants to continue using Obsidian Sync for device-to-device sync
5. **Version history:** Nice to have rollback capability (jj/git)

## Explored Options

### Option A: Git-Based Sync (jj)
- Sync vault to git remote, cloud Claude Code clones on session start
- **Pro:** Works with Claude Code's built-in Read/Write tools
- **Con:** Doesn't work for Claude Chat/iOS (no filesystem)
- **Con:** Requires push before switching devices

### Option B: Syncthing + Home Server
- Real-time file sync to home server
- **Pro:** No commits needed
- **Con:** Still need API for cloud access

### Option C: Home Server + Streamable HTTP (Chosen)
- Run obsidian-memory on home server with HTTP transport
- Expose via Cloudflare Tunnel
- **Pro:** Works for ALL clients (Claude Code, Claude Chat, iOS)
- **Pro:** Obsidian Sync continues working as-is
- **Pro:** Notes stay on user's infrastructure

## Chosen Architecture

```
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│ Claude Code  │  │ Claude Code  │  │ Claude Chat  │
│   (local)    │  │   (cloud)    │  │  (iOS/web)   │
└──────┬───────┘  └──────┬───────┘  └──────┬───────┘
       │                 │                 │
       │ stdio           │ HTTP            │ HTTP
       ▼                 ▼                 ▼
┌──────────────────────────────────────────────────┐
│         obsidian-memory MCP server               │
│         (home server + Cloudflare Tunnel)        │
├──────────────────────────────────────────────────┤
│  Transport: stdio (local) OR Streamable HTTP     │
│  Auth: TBD (token-based? OAuth?)                 │
└──────────────────────────────────────────────────┘
       │
       │ Obsidian Sync (existing)
       ▼
┌──────────────────────────────────────────────────┐
│              Local Obsidian Vault                │
└──────────────────────────────────────────────────┘
```

## What Needs to Be Built

### 1. Read Tool (New)
**Purpose:** Read note content (since Claude's built-in Read won't be available for remote clients)

```typescript
// Proposed API
{
  name: "Read",
  input: {
    note: string,        // Note reference (memory:path, [[Name]], etc.)
    startLine?: number,  // Optional: read from line N
    endLine?: number,    // Optional: read to line N
  },
  output: {
    content: string,
    totalLines: number,
    range?: { start: number, end: number }
  }
}
```

**Considerations:**
- Support subsection reading (line ranges) like Claude's Read tool
- Return line count for large files
- Handle encoding properly

### 2. Write Tool (New)
**Purpose:** Create/modify notes (since Claude's built-in Write won't be available)

```typescript
// Proposed API
{
  name: "Write",
  input: {
    note: string,           // Note reference
    content: string,        // Content to write
    mode: "replace" | "append" | "prepend" | "patch",
    createIfMissing?: boolean,
    // For patch mode:
    startLine?: number,
    endLine?: number,
  },
  output: {
    success: boolean,
    path: string,
    diff?: string,          // Show what changed
  }
}
```

**Considerations:**
- Show diffs when updating (like Claude's Edit tool)
- Support append/prepend for log-style notes
- Patch mode for subsection edits
- Validate paths to prevent directory traversal

### 3. Streamable HTTP Transport
**Purpose:** Expose MCP server over network

**Implementation approach:**
- Use MCP SDK's built-in Streamable HTTP support
- Single endpoint: `POST /mcp`
- Stateless (no persistent connections required)
- Compatible with serverless if needed later

**References:**
- [MCP Transports Spec](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports)
- [Cloudflare Remote MCP](https://blog.cloudflare.com/remote-model-context-protocol-servers-mcp/)

### 4. Authentication
**Options:**
- Simple: Bearer token (configured via env var)
- Better: OAuth (Cloudflare has built-in support)
- Future: Per-user auth for multi-user scenarios

**Recommendation:** Start with bearer token for simplicity, add OAuth later.

### 5. Cloudflare Tunnel Integration
**Purpose:** Expose home server securely without port forwarding

**Implementation:**
- Bundle `cloudflared` or document installation
- Single env var: `TUNNEL_TOKEN`
- Auto-creates HTTPS endpoint

### 6. Docker Packaging
**Purpose:** Simple deployment

```dockerfile
FROM node:20-slim
# Install cloudflared
# Copy obsidian-memory
# Expose port 3000
CMD ["node", "dist/index.js", "--transport", "http", "--port", "3000"]
```

**User experience:**
```bash
docker run -d \
  -v /path/to/vault:/vault \
  -e TUNNEL_TOKEN=xxx \
  obsidian-memory-server
```

## Implementation Phases

### Phase 1: Core Tools
1. Implement Read tool
2. Implement Write tool
3. Add tests

### Phase 2: HTTP Transport
1. Add Streamable HTTP transport option
2. Add bearer token auth
3. Test with remote MCP clients

### Phase 3: Packaging
1. Create Dockerfile
2. Document Cloudflare Tunnel setup
3. Create docker-compose example

### Phase 4: Polish
1. Add OAuth support (optional)
2. Improve error messages for remote scenarios
3. Add health check endpoint

## Optional: jj Integration

For users who want version history alongside sync:

1. Initialize jj in vault
2. Auto-commit on file changes (via watcher)
3. Provide "history" tool to view/restore past versions

This is additive and doesn't block the core cloud sync feature.

## Open Questions

1. **Auth model:** Bearer token sufficient for v1? Or need OAuth from start?
2. **Rate limiting:** Needed for exposed endpoint?
3. **Conflict handling:** What if Obsidian Sync and remote Write race?
4. **Large files:** Should Read/Write support streaming for large notes?
5. **Binary files:** Images/PDFs in vault - handle or skip?

## Resources

- [MCP Streamable HTTP Spec](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports)
- [Cloudflare Remote MCP Servers](https://blog.cloudflare.com/remote-model-context-protocol-servers-mcp/)
- [Claude MCP Connector Docs](https://docs.claude.com/en/docs/agents-and-tools/mcp-connector)
- [jj (Jujutsu) VCS](https://github.com/jj-vcs/jj)
