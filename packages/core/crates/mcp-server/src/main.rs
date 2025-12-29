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
mod graph;
mod tools;

use config::Config;
use graph::GraphIndex;

/// Parameters for the Log tool
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LogParams {
    /// Timeline entry content (single bullet point). Tool adds timestamp and day headers automatically.
    /// Tag work items with associated jira tickets or github issues when relevant.
    pub content: String,
}

/// The main MCP server state, holding configuration and shared resources.
#[derive(Clone)]
pub struct MemoryServer {
    config: Arc<Config>,
    graph: Arc<RwLock<GraphIndex>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MemoryServer {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            graph: Arc::new(RwLock::new(GraphIndex::new())),
            tool_router: Self::tool_router(),
        }
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

    // Create and run the server with STDIO transport
    let server = MemoryServer::new(config);
    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("Error starting server: {}", e);
    })?;

    tracing::info!("Obsidian Memory MCP server started");
    service.waiting().await?;

    Ok(())
}
