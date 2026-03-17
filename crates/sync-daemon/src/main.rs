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
    /// Path to the vault directory
    #[arg(short, long)]
    vault: PathBuf,

    /// Bootstrap peer(s) to connect to on startup (iroh EndpointId hex strings)
    #[arg(long)]
    bootstrap: Vec<String>,

    /// Address to serve the /health endpoint on (e.g. 127.0.0.1:8081)
    #[arg(long)]
    health_listen: Option<String>,

    /// Path to an alternate ed25519 identity key file (default: .sync/daemon.key)
    #[arg(long)]
    identity_key: Option<PathBuf>,

    /// Enable verbose logging
    #[arg(long)]
    verbose: bool,
}

impl From<Args> for DaemonRunConfig {
    fn from(args: Args) -> Self {
        Self {
            vault: args.vault,
            identity_key: args.identity_key,
            bootstrap_peers: args.bootstrap,
            health_listen: args.health_listen,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    memory_common::init_tracing(args.verbose, "sync_daemon");

    sync_daemon::daemon::run(args.into()).await
}
