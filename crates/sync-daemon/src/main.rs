//! sync-daemon: Headless P2P sync daemon for home server.
//!
//! Thin wrapper around `sync_daemon::daemon::run()` — all logic lives in the
//! library so the `memory` binary can embed it behind a feature flag.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use sync_daemon::daemon::DaemonRunConfig;

#[derive(Parser, Debug)]
#[command(name = "sync-daemon")]
#[command(about = "P2P vault sync daemon")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the vault directory
    #[arg(short, long)]
    vault: PathBuf,

    /// Address to listen on for incoming connections
    #[arg(short, long, default_value = "0.0.0.0:8080")]
    listen: String,

    /// Address to advertise to other peers (how they connect back to us)
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
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Connect to a peer and add them to the mesh
    AddPeer {
        /// WebSocket address of the peer (e.g., ws://peer.example.com:8080)
        address: String,
    },
}

impl From<Args> for DaemonRunConfig {
    fn from(args: Args) -> Self {
        Self {
            vault: args.vault,
            listen: args.listen,
            advertise: args.advertise,
            bootstrap: args.bootstrap,
            client_only: args.client_only,
            peer_id: args.peer_id,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    memory_common::init_tracing(args.verbose, "sync_daemon");

    // Handle subcommands
    if let Some(Command::AddPeer { address }) = args.command {
        tracing::info!("add-peer command: {}", address);
        eprintln!("add-peer subcommand not yet implemented");
        eprintln!("For now, use --bootstrap {} on daemon startup", address);
        return Ok(());
    }

    sync_daemon::daemon::run(args.into()).await
}
