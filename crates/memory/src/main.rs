use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
    transport::stdio,
    ServiceExt,
};
use schemars::JsonSchema;
use serde::Deserialize;
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
use storage::{ClientId, FileStorage, ReadWhitelist};
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
    /// Path to the note relative to vault root
    pub path: String,
    /// Frontmatter fields to update
    pub updates: std::collections::HashMap<String, serde_json::Value>,
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
}

/// A single edit operation for the EditNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditOperation {
    /// Text to search for - must match exactly and appear only once
    #[serde(rename = "oldText")]
    pub old_text: String,
    /// Text to replace with
    #[serde(rename = "newText")]
    pub new_text: String,
}

/// Parameters for the EditNote tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditNoteParams {
    /// Note reference - supports wiki-links ([[Note]]), memory URIs (memory:knowledge/Note), or plain names
    pub note: String,
    /// Array of edit operations. Each oldText must appear exactly once in the note.
    pub edits: Vec<EditOperation>,
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

        // Preload model and compute embeddings for all notes at startup
        // This avoids a 45+ second delay on first search
        {
            let graph_read = graph.read().await;
            let notes: Vec<(String, String)> = graph_read
                .all_paths()
                .filter_map(|path: &String| {
                    let full_path = config.vault_path.join(path);
                    std::fs::read_to_string(&full_path)
                        .ok()
                        .map(|content| (path.clone(), content))
                })
                .collect();
            drop(graph_read);

            if !notes.is_empty() {
                tracing::info!("Preloading embeddings for {} notes...", notes.len());
                if let Err(e) = embeddings.get_embeddings_batch(&notes).await {
                    tracing::warn!("Failed to preload embeddings: {}. First search will be slower.", e);
                } else {
                    tracing::info!("Embeddings preloaded successfully");
                }
            }
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
    /// Tracks which notes have been read, enabling "must read before write" checks.
    /// Per-session state - not shared between HTTP clients.
    read_whitelist: Arc<RwLock<ReadWhitelist>>,
    /// Unique identifier for this client session.
    /// For stdio: always "stdio". For HTTP: unique per session.
    client_id: ClientId,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MemoryServer {
    /// Create a new server for stdio transport (single client).
    pub async fn new(config: Config) -> Result<Self, Box<dyn std::error::Error>> {
        let shared = SharedState::new(config).await?;
        Ok(Self::from_shared(shared, ClientId::stdio()))
    }

    /// Create a server from pre-initialized shared state (sync, for HTTP factory).
    /// Each HTTP session gets its own read_whitelist and client_id.
    pub fn from_shared(shared: SharedState, client_id: ClientId) -> Self {
        Self {
            shared,
            read_whitelist: Arc::new(RwLock::new(ReadWhitelist::new())),
            client_id,
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

    #[tool(description = "Update frontmatter metadata in a note")]
    async fn update_frontmatter(&self, params: Parameters<UpdateFrontmatterParams>) -> Result<CallToolResult, ErrorData> {
        tools::update_frontmatter::execute(
            &self.config().vault_path,
            &params.0.path,
            params.0.updates,
        )
        .await
    }

    #[tool(description = "Load all session context files in a single call. Returns Log.md, Working Memory.md, current weekly note, and discovered project notes. Automatically discovers projects based on git remotes and directory names. Use this at the start of every session to get complete context about recent work, current focus, this week's activity, and project context.")]
    async fn remember(&self) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        let cwd = std::env::current_dir().unwrap_or_default();
        tools::remember::execute(&self.config().vault_path, &graph, &cwd).await
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

    #[tool(description = "Read the complete contents of a note. Marks the note as readable for subsequent writes. Use this before WriteNote or EditNote to see current content.")]
    async fn read_note(&self, params: Parameters<ReadNoteParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        tools::read_note::execute(
            self.storage(),
            &graph,
            &self.read_whitelist,
            self.client_id.clone(),
            &params.0.note,
        )
        .await
    }

    #[tool(description = "Create a new note or overwrite an existing note. For existing notes, you must call ReadNote first to see the current content and confirm the overwrite. Uses atomic writes for safety.")]
    async fn write_note(&self, params: Parameters<WriteNoteParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph().read().await;
        tools::write_note::execute(
            &self.config().vault_path,
            self.storage(),
            &graph,
            &self.read_whitelist,
            self.client_id.clone(),
            &params.0.note,
            &params.0.content,
        )
        .await
    }

    #[tool(description = "Make surgical text replacements in a note. Each edit specifies oldText (must match exactly and appear once) and newText. You must call ReadNote first. Use dryRun to preview changes without modifying the file.")]
    async fn edit_note(&self, params: Parameters<EditNoteParams>) -> Result<CallToolResult, ErrorData> {
        let edits: Vec<tools::edit_note::Edit> = params.0.edits
            .into_iter()
            .map(|e| tools::edit_note::Edit {
                old_text: e.old_text,
                new_text: e.new_text,
            })
            .collect();

        let graph = self.graph().read().await;
        tools::edit_note::execute(
            &self.config().vault_path,
            self.storage(),
            &graph,
            &self.read_whitelist,
            self.client_id.clone(),
            &params.0.note,
            edits,
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

/// CLI arguments for the MCP server.
#[cfg(feature = "http")]
#[derive(clap::Parser)]
#[command(name = "obsidian-memory")]
#[command(about = "MCP server for Obsidian memory integration")]
struct Cli {
    /// Run in HTTP mode instead of stdio
    #[arg(long)]
    http: bool,

    /// Port to listen on in HTTP mode
    #[arg(long, default_value_t = DEFAULT_HTTP_PORT)]
    port: u16,

    /// Address to bind to in HTTP mode. Use 0.0.0.0 for all interfaces (unsafe without auth).
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
}

#[cfg(feature = "http")]
const DEFAULT_HTTP_PORT: u16 = 3000;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for logging
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(EnvFilter::from_default_env())
        .init();

    // Load configuration from environment
    let config = Config::from_env()?;
    tracing::info!("Vault path: {}", config.vault_path.display());

    #[cfg(feature = "http")]
    {
        use clap::Parser;
        let cli = Cli::parse();

        if cli.http {
            return run_http_server(config, &cli.bind, cli.port).await;
        }
    }

    // Default: Run with STDIO transport
    run_stdio_server(config).await
}

/// Run the server with STDIO transport (default mode).
async fn run_stdio_server(config: Config) -> Result<(), Box<dyn std::error::Error>> {
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
    // Pre-initialize shared state (graph, embeddings, storage, watcher).
    // This is done once before starting the HTTP server.
    let shared = Arc::new(SharedState::new(config).await?);

    // The StreamableHttpService creates a new MemoryServer for each session.
    // Each session gets its own read_whitelist and client_id.
    let service = StreamableHttpService::new(
        {
            let shared = shared.clone();
            move || {
                // Sync factory: create server from pre-initialized shared state
                Ok(MemoryServer::from_shared(
                    (*shared).clone(),
                    ClientId::generate(),
                ))
            }
        },
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);

    // Parse bind address - default to localhost for safety
    let bind_addr: std::net::IpAddr = bind.parse().map_err(|e| {
        format!("Invalid bind address '{}': {}", bind, e)
    })?;
    let addr = std::net::SocketAddr::from((bind_addr, port));

    if bind_addr.is_unspecified() {
        tracing::info!(
            "Binding to all interfaces ({}). Ensure a reverse proxy handles authentication.",
            bind
        );
    }

    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
        format!("Failed to bind to {}:{} - {}", bind, port, e)
    })?;

    tracing::info!("Obsidian Memory MCP server started (HTTP) at http://{}/mcp", addr);

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Wait for shutdown signal (Ctrl+C or SIGTERM on Unix).
#[cfg(feature = "http")]
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("Shutdown signal received, stopping server...");
}
