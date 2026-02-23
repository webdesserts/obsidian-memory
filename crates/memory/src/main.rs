use clap::Parser;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
    transport::stdio,
    ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[cfg(feature = "http")]
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpService,
};

mod config;
mod embeddings;
mod graph;
mod projects;
mod storage;
mod tools;
mod watcher;

use config::Config;
use embeddings::EmbeddingManager;
use graph::GraphIndex;
use storage::FileStorage;
use watcher::VaultWatcher;

/// Parameters for the Log tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LogParams {
    /// Timeline entry content (single bullet point). Tool adds timestamp and day headers automatically.
    /// Tag work items with associated jira tickets or github issues when relevant.
    pub content: String,
}

/// Parameters for the GetNoteInfo tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetNoteInfoParams {
    /// Note reference - supports: "memory:Note Name", "memory:knowledge/Note Name", "knowledge/Note Name", "[[Note Name]]"
    pub note: String,
}

/// Parameters for the UpdateFrontmatter tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateFrontmatterParams {
    /// Note reference - supports wiki-links ([[Note]]), memory URIs (memory:knowledge/Note), or plain names
    pub note: String,
    /// Frontmatter fields to update
    pub updates: std::collections::HashMap<String, serde_json::Value>,
    /// Content hash from ReadNote - required to verify note hasn't changed
    pub content_hash: String,
}

/// Parameters for the Search tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// The search query - what information are you looking for? Supports wiki-links: [[Note]] searches using that note's content. Multiple notes: [[TypeScript]] [[Projects]] finds notes similar to BOTH. Mixed: 'type safety in [[TypeScript]]' combines note content with text. Wiki-links enable graph boosting (connected notes rank higher).
    pub query: String,
    /// Whether to include private notes in search. Requires explicit user consent.
    #[serde(default)]
    pub include_private: bool,
    /// Show detailed score breakdown (semantic, graph proximity, boost calculation). Useful for understanding how results are ranked.
    #[serde(default)]
    pub debug: bool,
}

/// Parameters for the WriteLogs tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteLogsParams {
    /// ISO week date in YYYY-Www-D format (e.g., '2025-W50-1' for Monday of week 50). Week starts on Monday (1=Mon, 7=Sun).
    #[serde(rename = "isoWeekDate")]
    pub iso_week_date: String,
    /// Object mapping time strings to log messages. Keys: '9:00 AM', '2:30 PM', etc. (12-hour format with AM/PM). Values: Log entry content. Example: { '9:00 AM': 'Started investigation', '2:30 PM': 'Fixed bug #123' }
    pub entries: std::collections::HashMap<String, String>,
}

/// Parameters for the Remember tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberParams {
    /// The client's current working directory path. Used for project discovery
    /// via git remote and directory name matching. When omitted (e.g. on iOS
    /// or other clients without a filesystem), project discovery is skipped
    /// but Log, Working Memory, and the weekly note are still loaded.
    pub cwd: Option<String>,
}

/// Parameters for the Reflect tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReflectParams {
    /// Include private notes in reflection (default: false)
    #[serde(default, rename = "includePrivate")]
    pub include_private: bool,
}

/// Parameters for the LoadPrivateMemory tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoadPrivateMemoryParams {
    /// Reason for loading private memory
    pub reason: String,
}

/// Parameters for the ReadNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadNoteParams {
    /// Note reference - supports wiki-links ([[Note]]), memory URIs (memory:knowledge/Note), or plain names
    pub note: String,
}

/// Parameters for the WriteNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteNoteParams {
    /// Note reference - supports wiki-links ([[Note]]), memory URIs (memory:knowledge/Note), or plain names
    pub note: String,
    /// The content to write to the note
    pub content: String,
    /// Content hash from ReadNote - required for existing notes, omit for new notes
    pub content_hash: Option<String>,
}

/// A single find-and-replace operation for the ReplaceInNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplaceOperation {
    /// Text to search for - must match exactly and appear only once
    #[serde(rename = "oldText")]
    pub old_text: String,
    /// Text to replace with
    #[serde(rename = "newText")]
    pub new_text: String,
}

/// Parameters for the ReplaceInNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplaceInNoteParams {
    /// Note reference - supports wiki-links ([[Note]]), memory URIs (memory:knowledge/Note), or plain names
    pub note: String,
    /// Array of edit operations. Each oldText must appear exactly once in the note.
    pub edits: Vec<ReplaceOperation>,
    /// Content hash from ReadNote - required to verify note hasn't changed
    pub content_hash: String,
    /// Preview changes without applying them (default: false)
    #[serde(default, rename = "dryRun")]
    pub dry_run: bool,
}

/// A single line-range edit operation for the EditNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LineEditOperation {
    /// First line to replace (1-indexed, inclusive). Matches line numbers from ReadNote output.
    #[serde(rename = "startLine")]
    pub start_line: usize,
    /// Last line to replace (1-indexed, inclusive). Use the same value as startLine to replace a single line.
    #[serde(rename = "endLine")]
    pub end_line: usize,
    /// Replacement text. Use empty string to delete lines. May contain newlines to expand a range into multiple lines.
    #[serde(rename = "newText")]
    pub new_text: String,
}

/// Parameters for the EditNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditNoteParams {
    /// Note reference - supports wiki-links ([[Note]]), memory URIs (memory:knowledge/Note), or plain names
    pub note: String,
    /// Array of line-range edit operations. Ranges must not overlap.
    pub edits: Vec<LineEditOperation>,
    /// Content hash from ReadNote - required to verify note hasn't changed
    pub content_hash: String,
    /// Preview changes without applying them (default: false)
    #[serde(default, rename = "dryRun")]
    pub dry_run: bool,
}

/// Parameters for the DeleteNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteNoteParams {
    /// Note reference - supports wiki-links ([[Note]]), memory URIs (memory:knowledge/Note), or plain names
    pub note: String,
}

/// Parameters for the MoveNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveNoteParams {
    /// Source note reference
    pub from: String,
    /// Destination note reference
    pub to: String,
}

/// Shared state that can be reused across multiple HTTP sessions.
/// Pre-initialized once, then passed to each session's MemoryServer.
#[derive(Clone)]
pub struct SharedState {
    config: Arc<Config>,
    graph: Arc<RwLock<GraphIndex>>,
    embeddings: Arc<EmbeddingManager>,
    storage: Arc<FileStorage>,
    /// File watcher handle - kept alive for the lifetime of the shared state.
    #[allow(dead_code)]
    watcher: Option<Arc<VaultWatcher>>,
}

impl SharedState {
    /// Initialize shared state (async, call once before starting HTTP server).
    pub async fn new(config: Config) -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize graph index by scanning the vault
        let mut graph = GraphIndex::new();
        graph.initialize(&config.vault_path).await?;

        let graph = Arc::new(RwLock::new(graph));

        // Create embedding manager and preload model + embeddings at startup
        let embeddings = Arc::new(EmbeddingManager::new(&config.vault_path));

        // Spawn background task to preload embeddings
        // Server starts immediately - search will wait for model but not for preload
        {
            let graph_clone = graph.clone();
            let embeddings_clone = embeddings.clone();
            let vault_path = config.vault_path.clone();

            tokio::spawn(async move {
                // Collect paths first, then drop lock before doing I/O
                let paths: Vec<String> = {
                    let graph_read = graph_clone.read().await;
                    graph_read.all_paths().cloned().collect()
                };

                // Read files asynchronously without holding lock
                let mut notes = Vec::with_capacity(paths.len());
                for path in paths {
                    let full_path = vault_path.join(&path);
                    if let Ok(content) = tokio::fs::read_to_string(&full_path).await {
                        notes.push((path, content));
                    }
                }

                if !notes.is_empty() {
                    tracing::info!("Preloading embeddings for {} notes in background...", notes.len());
                    if let Err(e) = embeddings_clone.get_embeddings_batch(&notes).await {
                        tracing::warn!("Failed to preload embeddings: {}. First search will be slower.", e);
                    } else {
                        tracing::info!("Embeddings preloaded successfully");
                    }
                }
            });
        }

        // Create storage backend
        let storage = Arc::new(FileStorage::new(config.vault_path.clone()));

        // Start file watcher to keep graph index and embeddings up to date
        let watcher = match VaultWatcher::start(
            config.vault_path.clone(),
            graph.clone(),
            embeddings.clone(),
        ) {
            Ok(w) => {
                tracing::info!("File watcher started successfully");
                Some(Arc::new(w))
            }
            Err(e) => {
                tracing::warn!("Failed to start file watcher: {}. Graph index will not auto-update.", e);
                None
            }
        };

        Ok(Self {
            config: Arc::new(config),
            graph,
            embeddings,
            storage,
            watcher,
        })
    }
}

/// The main MCP server state, holding configuration and shared resources.
#[derive(Clone)]
pub struct MemoryServer {
    /// Shared state (graph, embeddings, storage, config) - same across all sessions
    shared: SharedState,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MemoryServer {
    /// Create a new server for stdio transport (single client).
    pub async fn new(config: Config) -> Result<Self, Box<dyn std::error::Error>> {
        let shared = SharedState::new(config).await?;
        Ok(Self::from_shared(shared))
    }

    /// Create a server from pre-initialized shared state (sync, for HTTP factory).
    pub fn from_shared(shared: SharedState) -> Self {
        Self {
            shared,
            tool_router: Self::tool_router(),
        }
    }

    // Accessor methods for shared state fields
    fn config(&self) -> &Config {
        &self.shared.config
    }

    fn graph(&self) -> &Arc<RwLock<GraphIndex>> {
        &self.shared.graph
    }

    fn embeddings(&self) -> &Arc<EmbeddingManager> {
        &self.shared.embeddings
    }

    fn storage(&self) -> &FileStorage {
        &self.shared.storage
    }

    #[tool(description = "Get the current date and time in ISO format for use in Working Memory timeline entries. Returns ISO 8601 formatted datetime (YYYY-MM-DDTHH:MM) and additional context.")]
    async fn get_current_datetime(&self) -> Result<CallToolResult, ErrorData> {
        tools::get_current_datetime::execute()
    }

    #[tool(description = "Append a timestamped entry to Log.md for active work state and debugging context tracking. Records chronological session activity - what happened when. The tool automatically adds timestamps and organizes entries by day. Use this for tracking work in progress, debugging steps, state changes, and decisions made during active work.")]
    async fn log(&self, params: Parameters<LogParams>) -> Result<CallToolResult, ErrorData> {
        tools::log::execute(&self.config().vault_path, &params.0.content).await
    }

    #[tool(description = "Get metadata and graph connections for the current week's journal note. Returns path, URIs, frontmatter, and links/backlinks. Works whether or not the note exists yet. Use ReadNote tool to get content.")]
    async fn get_weekly_note_info(&self) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        tools::get_weekly_note_info::execute(
            &self.config().vault_path,
            &self.config().vault_name,
            &graph,
        )
        .await
    }

    #[tool(description = "Get metadata and graph connections for a note. Returns frontmatter, file paths, and links/backlinks. Use ReadNote tool to get content.")]
    async fn get_note_info(&self, params: Parameters<GetNoteInfoParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        tools::get_note_info::execute(
            &self.config().vault_path,
            &self.config().vault_name,
            &graph,
            &params.0.note,
        )
        .await
    }

    #[tool(description = "Update frontmatter metadata in a note. Requires content_hash from ReadNote. Returns JSON with new content_hash.")]
    async fn update_frontmatter(&self, params: Parameters<UpdateFrontmatterParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        tools::update_frontmatter::execute(
            &*self.storage(),
            &graph,
            &params.0.note,
            params.0.updates,
            &params.0.content_hash,
        )
        .await
    }

    #[tool(description = "Load all session context files in a single call. Returns Log.md, Working Memory.md, current weekly note, and discovered project notes. Automatically discovers projects based on git remotes and directory names. Use this at the start of every session to get complete context about recent work, current focus, this week's activity, and project context.")]
    async fn remember(&self, params: Parameters<RememberParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        let cwd = params.0.cwd.map(std::path::PathBuf::from);
        tools::remember::execute(&self.config().vault_path, &graph, cwd.as_deref()).await
    }

    #[tool(description = "Search for relevant notes using semantic similarity. Encodes the query and compares it against all note embeddings. Returns similarity-ordered list of potentially relevant notes. Supports note references via wiki-links: [[Note Name]]")]
    async fn search(&self, params: Parameters<SearchParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        tools::search::execute(
            &self.config().vault_path,
            &graph,
            self.embeddings(),
            &params.0.query,
            params.0.include_private,
            params.0.debug,
        )
        .await
    }

    #[tool(description = "Replace an entire day's log entries with consolidated/compacted entries. Use this ONLY during memory consolidation to rewrite or summarize a day's logs. For adding new entries during active work, use the Log tool instead (it's simpler and doesn't require reading the log first). This tool automatically formats entries with correct timestamps, en-dashes, and chronological sorting. Pass an empty object to delete the entire day section (header and all entries).")]
    async fn write_logs(&self, params: Parameters<WriteLogsParams>) -> Result<CallToolResult, ErrorData> {
        tools::write_logs::execute(
            &self.config().vault_path,
            &params.0.iso_week_date,
            params.0.entries,
        )
        .await
    }

    #[tool(description = "Review active context (Log.md, Working Memory.md, current weekly journal, project notes) and consolidate content into permanent storage. Optimizes token usage by keeping active/relevant work accessible while compressing or archiving finished work. Applies information lifecycle: active work = keep lean, shipped/merged = compress and archive. Returns detailed consolidation instructions.")]
    async fn reflect(&self, params: Parameters<ReflectParams>) -> Result<CallToolResult, ErrorData> {
        tools::reflect::execute(params.0.include_private)
    }

    #[tool(description = "Load private memory indexes (requires explicit user consent)")]
    async fn load_private_memory(&self, params: Parameters<LoadPrivateMemoryParams>) -> Result<CallToolResult, ErrorData> {
        tools::load_private_memory::execute(&self.config().vault_path, &params.0.reason).await
    }

    #[tool(description = "Read the complete contents of a note. Returns JSON with content and content_hash. Content includes line numbers (cat -n format: right-aligned number + tab). content_hash is computed on raw content — pass it through to WriteNote, EditNote, or ReplaceInNote unchanged.")]
    async fn read_note(&self, params: Parameters<ReadNoteParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        tools::read_note::execute(
            self.storage(),
            &graph,
            &params.0.note,
        )
        .await
    }

    #[tool(description = "Create a new note or overwrite an existing note. For existing notes, include content_hash from ReadNote. Returns JSON with new content_hash for chained writes.")]
    async fn write_note(&self, params: Parameters<WriteNoteParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        tools::write_note::execute(
            &self.config().vault_path,
            self.storage(),
            &graph,
            &params.0.note,
            &params.0.content,
            params.0.content_hash.as_deref(),
        )
        .await
    }

    #[tool(description = "Make surgical text replacements in a note. Each edit specifies oldText (must match exactly and appear once) and newText. Requires content_hash from ReadNote. Returns JSON with new content_hash for chained edits.")]
    async fn replace_in_note(&self, params: Parameters<ReplaceInNoteParams>) -> Result<CallToolResult, ErrorData> {
        let edits: Vec<tools::replace_in_note::Edit> = params.0.edits
            .into_iter()
            .map(|e| tools::replace_in_note::Edit {
                old_text: e.old_text,
                new_text: e.new_text,
            })
            .collect();

        let graph = self.graph().read().await;
        tools::replace_in_note::execute(
            &self.config().vault_path,
            self.storage(),
            &graph,
            &params.0.note,
            edits,
            &params.0.content_hash,
            params.0.dry_run,
        )
        .await
    }

    #[tool(description = "Replace lines in a note by line number. Each edit specifies a startLine and endLine (1-indexed, inclusive, matching ReadNote output) and newText to replace that range. Use empty newText to delete lines. Ranges must not overlap. Requires content_hash from ReadNote. Returns JSON with new content_hash for chained edits.")]
    async fn edit_note(&self, params: Parameters<EditNoteParams>) -> Result<CallToolResult, ErrorData> {
        let edits: Vec<tools::edit_note::LineEdit> = params.0.edits
            .into_iter()
            .map(|e| tools::edit_note::LineEdit {
                start_line: e.start_line,
                end_line: e.end_line,
                new_text: e.new_text,
            })
            .collect();

        let graph = self.graph().read().await;
        tools::edit_note::execute(
            &self.config().vault_path,
            self.storage(),
            &graph,
            &params.0.note,
            edits,
            &params.0.content_hash,
            params.0.dry_run,
        )
        .await
    }

    #[tool(description = "Permanently delete a note from the vault. Returns an error if the note doesn't exist.")]
    async fn delete_note(&self, params: Parameters<DeleteNoteParams>) -> Result<CallToolResult, ErrorData> {
        tools::delete_note::execute(
            &self.config().vault_path,
            self.storage(),
            &params.0.note,
        )
        .await
    }

    #[tool(description = "Move or rename a note. Automatically updates wiki-links in all notes that reference the moved note. Fails if destination already exists.")]
    async fn move_note(&self, params: Parameters<MoveNoteParams>) -> Result<CallToolResult, ErrorData> {
        tools::move_note::execute(
            &self.config().vault_path,
            self.storage(),
            self.graph(),
            &params.0.from,
            &params.0.to,
        )
        .await
    }
}

#[tool_handler]
impl rmcp::ServerHandler for MemoryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "obsidian-memory".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            instructions: Some(
                "Obsidian Memory MCP server - provides tools for managing notes and memory in an Obsidian vault."
                    .into(),
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "memory", about = "Persistent memory for AI coding assistants")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// MCP server (stdio when piped, help when interactive)
    Mcp {
        /// Path to the vault directory
        #[arg(long)]
        vault: Option<PathBuf>,

        #[command(subcommand)]
        action: Option<McpAction>,
    },

    /// Sync daemon
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },

    /// Start all services (sync + MCP HTTP)
    #[cfg(all(feature = "daemon", feature = "http"))]
    Up {
        /// Path to the vault directory
        #[arg(long)]
        vault: PathBuf,

        /// Port to listen on for MCP HTTP
        #[arg(long, default_value_t = 3000)]
        port: u16,

        /// Address to bind MCP HTTP to
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,

        /// Address for sync daemon to listen on
        #[arg(long, default_value = "0.0.0.0:8080")]
        listen: String,

        /// Address to advertise to other sync peers
        #[arg(long)]
        advertise: Option<String>,

        /// Bootstrap peer(s) to connect to on startup
        #[arg(long)]
        bootstrap: Vec<String>,

        /// Enable verbose logging
        #[arg(long)]
        verbose: bool,
    },

    /// Manage the Obsidian plugin
    Obsidian {
        #[command(subcommand)]
        action: ObsidianAction,
    },
}

#[derive(clap::Subcommand)]
enum McpAction {
    /// MCP stdio transport (explicit — `memory mcp` also starts stdio when piped)
    Io,

    /// Start MCP HTTP server
    #[cfg(feature = "http")]
    Up {
        /// Port to listen on
        #[arg(long, default_value_t = 3000)]
        port: u16,

        /// Address to bind to. Use 0.0.0.0 for all interfaces (unsafe without auth).
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
    },
}

#[derive(clap::Subcommand)]
enum SyncAction {
    /// Start sync daemon
    #[cfg(feature = "daemon")]
    Up {
        /// Path to the vault directory
        #[arg(long)]
        vault: PathBuf,

        /// Address to listen on for incoming connections
        #[arg(long, default_value = "0.0.0.0:8080")]
        listen: String,

        /// Address to advertise to other peers
        #[arg(long)]
        advertise: Option<String>,

        /// Bootstrap peer(s) to connect to on startup
        #[arg(long)]
        bootstrap: Vec<String>,

        /// Run in client-only mode (don't listen for incoming connections)
        #[arg(long)]
        client_only: bool,

        /// Peer ID (generated if not provided)
        #[arg(long)]
        peer_id: Option<String>,

        /// Enable verbose logging
        #[arg(long)]
        verbose: bool,
    },
}

#[derive(clap::Subcommand)]
enum ObsidianAction {
    /// Install the Obsidian plugin
    Install {
        /// Path to the vault
        #[arg(long)]
        vault: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        // `memory` with no args → help
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(())
        }

        Some(Command::Mcp { vault, action }) => match action {
            // `memory mcp` → TTY detection
            None => {
                if std::io::stdin().is_terminal() {
                    use clap::CommandFactory;
                    // Print help for the mcp subcommand
                    Cli::command()
                        .find_subcommand_mut("mcp")
                        .expect("mcp subcommand exists")
                        .print_help()?;
                    println!();
                    Ok(())
                } else {
                    let vault = require_vault(vault)?;
                    init_mcp_tracing();
                    run_stdio_server(Config::new(&vault)).await
                }
            }

            // `memory mcp io` → explicit stdio
            Some(McpAction::Io) => {
                let vault = require_vault(vault)?;
                init_mcp_tracing();
                run_stdio_server(Config::new(&vault)).await
            }

            // `memory mcp up` → HTTP server
            #[cfg(feature = "http")]
            Some(McpAction::Up { port, bind }) => {
                let vault = require_vault(vault)?;
                init_mcp_tracing();
                run_http_server(Config::new(&vault), &bind, port).await
            }
        },

        #[cfg(feature = "daemon")]
        Some(Command::Sync { action }) => match action {
            SyncAction::Up {
                vault,
                listen,
                advertise,
                bootstrap,
                client_only,
                peer_id,
                verbose,
            } => {
                memory_common::init_tracing(verbose, "sync_daemon");
                sync_daemon::daemon::run(sync_daemon::daemon::DaemonRunConfig {
                    vault,
                    listen,
                    advertise,
                    bootstrap,
                    client_only,
                    peer_id,
                })
                .await?;
                Ok(())
            }
        },

        #[cfg(not(feature = "daemon"))]
        Some(Command::Sync { .. }) => {
            eprintln!("Sync daemon is not available in this build.");
            eprintln!("Rebuild with: cargo build --features daemon");
            std::process::exit(1);
        }

        #[cfg(all(feature = "daemon", feature = "http"))]
        Some(Command::Up {
            vault,
            port,
            bind,
            listen,
            advertise,
            bootstrap,
            verbose,
        }) => {
            memory_common::init_tracing(verbose, "memory");

            let vault_str = vault.to_string_lossy().to_string();

            // Spawn sync daemon as background task
            let sync_config = sync_daemon::daemon::DaemonRunConfig {
                vault,
                listen,
                advertise,
                bootstrap,
                client_only: false,
                peer_id: None,
            };
            let sync_handle = tokio::spawn(async move {
                if let Err(e) = sync_daemon::daemon::run(sync_config).await {
                    tracing::error!("Sync daemon error: {}", e);
                }
            });

            // Run MCP HTTP server on main task
            let mcp_result = run_http_server(Config::new(&vault_str), &bind, port).await;

            // On shutdown, abort the sync daemon
            sync_handle.abort();
            mcp_result
        }

        Some(Command::Obsidian { action }) => match action {
            ObsidianAction::Install { .. } => {
                eprintln!("Plugin installation is not yet available.");
                Ok(())
            }
        },
    }
}

/// Require --vault flag, returning the path as a string for Config::new().
fn require_vault(vault: Option<PathBuf>) -> Result<String, Box<dyn std::error::Error>> {
    match vault {
        Some(v) => Ok(v.to_string_lossy().to_string()),
        None => {
            // Fall back to OBSIDIAN_VAULT_PATH for backwards compatibility
            std::env::var("OBSIDIAN_VAULT_PATH").map_err(|_| {
                "Vault path required. Use --vault <path> or set OBSIDIAN_VAULT_PATH.".into()
            })
        }
    }
}

/// Initialize tracing for MCP server (logs to stderr, respects RUST_LOG).
fn init_mcp_tracing() {
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env())
        .init();
}

// ---------------------------------------------------------------------------
// Server functions
// ---------------------------------------------------------------------------

/// Run the server with STDIO transport.
async fn run_stdio_server(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Vault path: {}", config.vault_path.display());
    let server = MemoryServer::new(config).await?;

    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("Error starting server: {}", e);
    })?;

    tracing::info!("Obsidian Memory MCP server started (stdio)");
    service.waiting().await?;

    Ok(())
}

/// Run the server with HTTP transport.
#[cfg(feature = "http")]
async fn run_http_server(
    config: Config,
    bind: &str,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("Vault path: {}", config.vault_path.display());

    let shared = Arc::new(SharedState::new(config).await?);

    let service = StreamableHttpService::new(
        {
            let shared = shared.clone();
            move || Ok(MemoryServer::from_shared((*shared).clone()))
        },
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);

    let bind_addr: std::net::IpAddr = bind
        .parse()
        .map_err(|e| format!("Invalid bind address '{}': {}", bind, e))?;
    let addr = std::net::SocketAddr::from((bind_addr, port));

    if bind_addr.is_unspecified() {
        tracing::info!(
            "Binding to all interfaces ({}). Ensure a reverse proxy handles authentication.",
            bind
        );
    }

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("Failed to bind to {}:{} - {}", bind, port, e))?;

    tracing::info!(
        "Obsidian Memory MCP server started (HTTP) at http://{}/mcp",
        addr
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(memory_common::shutdown_signal())
        .await?;

    Ok(())
}

