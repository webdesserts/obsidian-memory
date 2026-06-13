//! Operator tool: inspect loro-registry debris on an `obsidian-memory` vault.
//!
//! Cross-machine sync history leaves two kinds of debris in the registry tree:
//! **duplicate alive pairs** (more than one alive file node at the same path — the
//! product of two machines indexing the same file before they paired) and **relics**
//! (an alive file node whose `.md` and `.loro` are both gone — a tombstone that never
//! landed). Both are resurrection landmines until cleaned.
//!
//! This binary is the read-only INSPECTOR. It acquires the same exclusive daemon lock
//! the app uses (so it refuses to run while the app is up), loads the vault, and prints
//! the debris it finds. It does NOT mutate the registry — a future `--apply` pass owns
//! the tombstoning.
//!
//! ## Runbook
//!
//! 1. Stop the desktop app / daemon on the target machine (this tool needs the lock).
//! 2. Back up the registry: `cp -r <vault>/.sync <vault>/.sync.bak-<date>`.
//! 3. Dry-run inspect:
//!    `cargo run -p sync-daemon --example registry_maintenance -- --vault <vault>`
//! 4. Review the reported duplicate groups, relics, and (unhandled) folder dups.
//!
//! Mutation (`--apply`) is a separate, later step — not yet implemented here.

use std::path::PathBuf;
use std::process::ExitCode;

use sync_core::Vault;
use sync_daemon::NativeFs;
use sync_daemon::daemon_lock::DaemonLock;
use sync_daemon::persistence::DaemonConfig;

#[tokio::main]
async fn main() -> ExitCode {
    let Some(vault_path) = parse_vault_arg() else {
        eprintln!(
            "usage: cargo run -p sync-daemon --example registry_maintenance -- --vault <path>"
        );
        return ExitCode::FAILURE;
    };

    match run(vault_path).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Hand-parse `--vault <path>`. clap is a sync-daemon dep, but a single positional flag
/// doesn't justify a derive struct here — a future `--apply` commit can graduate this to
/// clap if the surface grows.
fn parse_vault_arg() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--vault" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

async fn run(vault_path: PathBuf) -> Result<(), String> {
    // Acquire the daemon lock FIRST — this is the concurrent-writer guard. If the app or
    // daemon is running it holds this lock, and we must not read a registry it is actively
    // mutating. Fail fast with a clear instruction.
    let _lock = DaemonLock::acquire(&vault_path).map_err(|e| {
        format!("{e}. Stop the desktop app / daemon on this machine first, then retry.")
    })?;

    let fs = NativeFs::new(vault_path.clone());
    // `cfg` (the DaemonConfig) is unused — we only need the identity to author the Loro
    // ops as this device's PeerId, matching the daemon's startup load.
    let (_, identity) = DaemonConfig::load_or_generate(&vault_path, None)
        .await
        .map_err(|e| format!("failed to load daemon identity: {e}"))?;
    let author = identity.peer_id();

    let vault = Vault::load(fs, author)
        .await
        .map_err(|e| format!("failed to load vault at {}: {e}", vault_path.display()))?;

    let report = vault
        .find_registry_debris()
        .await
        .map_err(|e| format!("registry scan failed: {e}"))?;

    print_report(&report);
    Ok(())
}

/// Pretty-print the debris report. Per node we show: state / TreeID peer+counter / name /
/// doc_id / path / parent. Every node the report surfaces is alive (deleted nodes are not
/// debris), so state is always `alive`; name and parent are derived from the resolved path.
fn print_report(report: &sync_core::DebrisReport) {
    println!("=== Registry debris (DRY RUN — no mutations) ===\n");

    println!("Duplicate alive groups: {}", report.duplicate_groups.len());
    for group in &report.duplicate_groups {
        println!("  path: {}  (doc_id {})", group.path, group.doc_id);
        for &id in &group.alive_nodes {
            let marker = if id == group.winner { "WIN " } else { "lose" };
            print_node(marker, &id, &group.path, &group.doc_id);
        }
    }

    println!(
        "\nRelics (alive node, no .md and no .loro): {}",
        report.relics.len()
    );
    for relic in &report.relics {
        print_node("relic", &relic.node, &relic.path, &relic.doc_id);
    }

    println!(
        "\nFolder duplicate groups (NOT handled in v1): {}",
        report.folder_dups.len()
    );
    for group in &report.folder_dups {
        println!("  path: {}", group.path);
        for &id in &group.alive_nodes {
            // Folder nodes carry no doc_id we surface; show the path-derived columns.
            print_node("folder", &id, &group.path, "-");
        }
    }

    if report.duplicate_groups.is_empty()
        && report.relics.is_empty()
        && report.folder_dups.is_empty()
    {
        println!("\nNo registry debris found — the registry is clean.");
    }
}

/// Print one node row: state / peer / counter / name / doc_id / path / parent.
fn print_node(state: &str, id: &sync_core::TreeID, path: &str, doc_id: &str) {
    let (parent, name) = match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    };
    println!(
        "    [{state}] peer={} counter={} name={} doc_id={} path={} parent={}",
        id.peer, id.counter, name, doc_id, path, parent
    );
}
