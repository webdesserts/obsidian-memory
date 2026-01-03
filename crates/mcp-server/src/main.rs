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

/// The main MCP server state, holding configuration and shared resources.
#[derive(Clone)]
pub struct MemoryServer {
    config: Arc<Config>,
    graph: Arc<RwLock<GraphIndex>>,
    embeddings: Arc<EmbeddingManager>,
    storage: Arc<FileStorage>,
    /// Tracks which notes have been read, enabling "must read before write" checks.
    read_whitelist: Arc<RwLock<ReadWhitelist>>,
    tool_router: ToolRouter<Self>,
    /// File watcher handle - kept alive for the lifetime of the server.
    /// Wrapped in Arc for Clone, Option for tests that don't need watching.
    #[allow(dead_code)]
    watcher: Option<Arc<VaultWatcher>>,
}

#[tool_router]
impl MemoryServer {
    pub async fn new(config: Config) -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize graph index by scanning the vault
        let mut graph = GraphIndex::new();
        graph.initialize(&config.vault_path).await?;

        let graph = Arc::new(RwLock::new(graph));

        // Create embedding manager (model download happens lazily on first search)
        let embeddings = Arc::new(EmbeddingManager::new(&config.vault_path));

        // Create storage backend
        let storage = Arc::new(FileStorage::new(config.vault_path.clone()));

        // Create read whitelist for "must read before write" tracking
        let read_whitelist = Arc::new(RwLock::new(ReadWhitelist::new()));

        // Start file watcher to keep graph index, embeddings, and whitelist up to date
        let watcher = match VaultWatcher::start(
            config.vault_path.clone(),
            graph.clone(),
            embeddings.clone(),
            read_whitelist.clone(),
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
            read_whitelist,
            tool_router: Self::tool_router(),
            watcher,
        })
    }

    #[tool(description = "Get the current date and time in ISO format for use in Working Memory timeline entries. Returns ISO 8601 formatted datetime (YYYY-MM-DDTHH:MM) and additional context.")]
    async fn get_current_datetime(&self) -> Result<CallToolResult, ErrorData> {
        tools::get_current_datetime::execute()
    }

    #[tool(description = "Append a timestamped entry to Log.md for active work state and debugging context tracking. Records chronological session activity - what happened when. The tool automatically adds timestamps and organizes entries by day. Use this for tracking work in progress, debugging steps, state changes, and decisions made during active work.")]
    async fn log(&self, params: Parameters<LogParams>) -> Result<CallToolResult, ErrorData> {
        tools::log::execute(&self.config.vault_path, &params.0.content).await
    }

    #[tool(description = "Get the URI for the current week's journal note. Returns a resource link that can be read to access the note content.")]
    async fn get_weekly_note(&self) -> Result<CallToolResult, ErrorData> {
        tools::get_weekly_note::execute()
    }

    #[tool(description = "Get metadata and graph connections for a note. Returns frontmatter, file paths, and links/backlinks. Use ReadNote tool to get content.")]
    async fn get_note_info(&self, params: Parameters<GetNoteInfoParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph.read().await;
        tools::get_note_info::execute(
            &self.config.vault_path,
            &self.config.vault_name,
            &graph,
            &params.0.note,
        )
        .await
    }

    #[tool(description = "Update frontmatter metadata in a note")]
    async fn update_frontmatter(&self, params: Parameters<UpdateFrontmatterParams>) -> Result<CallToolResult, ErrorData> {
        tools::update_frontmatter::execute(
            &self.config.vault_path,
            &params.0.path,
            params.0.updates,
        )
        .await
    }

    #[tool(description = "Load all session context files in a single call. Returns Log.md, Working Memory.md, current weekly note, and discovered project notes. Automatically discovers projects based on git remotes and directory names. Use this at the start of every session to get complete context about recent work, current focus, this week's activity, and project context.")]
    async fn remember(&self) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph.read().await;
        let cwd = std::env::current_dir().unwrap_or_default();
        tools::remember::execute(&self.config.vault_path, &graph, &cwd).await
    }

    #[tool(description = "Search for relevant notes using semantic similarity. Encodes the query and compares it against all note embeddings. Returns similarity-ordered list of potentially relevant notes. Supports note references via wiki-links: [[Note Name]]")]
    async fn search(&self, params: Parameters<SearchParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph.read().await;
        tools::search::execute(
            &self.config.vault_path,
            &graph,
            &self.embeddings,
            &params.0.query,
            params.0.include_private,
            params.0.debug,
        )
        .await
    }

    #[tool(description = "Replace an entire day's log entries with consolidated/compacted entries. Use this ONLY during memory consolidation to rewrite or summarize a day's logs. For adding new entries during active work, use the Log tool instead (it's simpler and doesn't require reading the log first). This tool automatically formats entries with correct timestamps, en-dashes, and chronological sorting. Pass an empty object to delete the entire day section (header and all entries).")]
    async fn write_logs(&self, params: Parameters<WriteLogsParams>) -> Result<CallToolResult, ErrorData> {
        tools::write_logs::execute(
            &self.config.vault_path,
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
        tools::load_private_memory::execute(&self.config.vault_path, &params.0.reason).await
    }

    #[tool(description = "Read the complete contents of a note. Marks the note as readable for subsequent writes. Use this before WriteNote or EditNote to see current content.")]
    async fn read_note(&self, params: Parameters<ReadNoteParams>) -> Result<CallToolResult, ErrorData> {
        let graph = self.graph.read().await;
        tools::read_note::execute(
            self.storage.as_ref(),
            &graph,
            &self.read_whitelist,
            ClientId::stdio(),
            &params.0.note,
        )
        .await
    }

    #[tool(description = "Create a new note or overwrite an existing note. For existing notes, you must call ReadNote first to see the current content and confirm the overwrite. Uses atomic writes for safety.")]
    async fn write_note(&self, params: Parameters<WriteNoteParams>) -> Result<CallToolResult, ErrorData> {
        tools::write_note::execute(
            &self.config.vault_path,
            self.storage.as_ref(),
            &self.read_whitelist,
            ClientId::stdio(),
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

        tools::edit_note::execute(
            &self.config.vault_path,
            self.storage.as_ref(),
            &self.read_whitelist,
            ClientId::stdio(),
            &params.0.note,
            edits,
            params.0.dry_run,
        )
        .await
    }

    #[tool(description = "Permanently delete a note from the vault. Returns an error if the note doesn't exist.")]
    async fn delete_note(&self, params: Parameters<DeleteNoteParams>) -> Result<CallToolResult, ErrorData> {
        tools::delete_note::execute(
            &self.config.vault_path,
            self.storage.as_ref(),
            &params.0.note,
        )
        .await
    }

    #[tool(description = "Move or rename a note. Automatically updates wiki-links in all notes that reference the moved note. Fails if destination already exists.")]
    async fn move_note(&self, params: Parameters<MoveNoteParams>) -> Result<CallToolResult, ErrorData> {
        tools::move_note::execute(
            &self.config.vault_path,
            self.storage.as_ref(),
            &self.graph,
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

    // Create server (this scans the vault and builds the graph index)
    let server = MemoryServer::new(config).await?;

    // Run the server with STDIO transport
    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("Error starting server: {}", e);
    })?;

    tracing::info!("Obsidian Memory MCP server started");
    service.waiting().await?;

    Ok(())
}
