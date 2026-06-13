//! Operator tool: inspect loro-registry debris on an `obsidian-memory` vault.
//!
//! Cross-machine sync history leaves two kinds of debris in the registry tree:
//! **duplicate alive pairs** (more than one alive file node at the same path — the
//! product of two machines indexing the same file before they paired) and **relics**
//! (an alive file node whose `.md` and `.loro` are both gone — a tombstone that never
//! landed). Both are resurrection landmines until cleaned.
//!
//! This binary is the INSPECTOR (default) and the dedupe APPLY pass (`--apply`). It acquires
//! the same exclusive daemon lock the app uses (so it refuses to run while the app is up),
//! loads the vault, and prints the debris it finds. Without `--apply` it mutates nothing.
//! With `--apply` it tombstones the duplicate-group losers and relics (folder dups are left
//! alone — v1 scope-out) and persists the registry once.
//!
//! ## Runbook
//!
//! 1. Stop the desktop app / daemon on the target machine (this tool needs the lock).
//! 2. Back up the registry: `cp -r <vault>/.sync <vault>/.sync.bak-<date>`.
//! 3. Dry-run inspect:
//!    `cargo run -p sync-daemon --example registry_maintenance -- --vault <vault>`
//! 4. Review the reported duplicate groups, relics, and (unhandled) folder dups.
//! 5. Apply: re-run with `--apply` appended.
//! 6. Restart the app; verify mesh-wide convergence (re-run the inspector on each machine).

use std::path::PathBuf;
use std::process::ExitCode;

use sync_core::Vault;
use sync_daemon::NativeFs;
use sync_daemon::daemon_lock::DaemonLock;
use sync_daemon::persistence::DaemonConfig;

#[tokio::main]
async fn main() -> ExitCode {
    let Some(args) = parse_args() else {
        eprintln!(
            "usage: cargo run -p sync-daemon --example registry_maintenance -- --vault <path> [--apply]"
        );
        return ExitCode::FAILURE;
    };

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

struct Args {
    vault_path: PathBuf,
    /// When true, tombstone the debris after printing the report; otherwise dry-run.
    apply: bool,
}

/// Hand-parse `--vault <path>` and the optional `--apply` flag. clap is a sync-daemon dep,
/// but two flags don't justify a derive struct here.
fn parse_args() -> Option<Args> {
    let mut vault_path = None;
    let mut apply = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--vault" => vault_path = args.next().map(PathBuf::from),
            "--apply" => apply = true,
            _ => {}
        }
    }
    Some(Args {
        vault_path: vault_path?,
        apply,
    })
}

async fn run(args: Args) -> Result<(), String> {
    let Args { vault_path, apply } = args;

    // Acquire the daemon lock FIRST — this is the concurrent-writer guard. If the app or
    // daemon is running it holds this lock, and we must not read (let alone mutate) a
    // registry it is actively mutating. Fail fast with a clear instruction.
    let _lock = DaemonLock::acquire(&vault_path).map_err(|e| {
        format!("{e}. Stop the desktop app / daemon on this machine first, then retry.")
    })?;

    let fs = NativeFs::new(vault_path.clone());
    // The DaemonConfig is unused — we only need the identity to author the Loro ops as this
    // device's PeerId, matching the daemon's startup load.
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

    if apply {
        apply_dedupe(&vault, &report, &vault_path).await?;
    }

    Ok(())
}

/// Print the scale warning, run the dedupe, and print the resulting stats.
///
/// The pre-call line states the planned blast radius (summed loser count across all groups,
/// each keeping one winner) and the safety preconditions; the post-call line reports the
/// ACTUAL counts from the returned [`DedupeStats`]. Keeping the "Tombstoned N…" claim to
/// after the call means a mid-way failure never leaves a misleading "we tombstoned N" on
/// screen — the error surfaces instead.
async fn apply_dedupe(
    vault: &Vault<NativeFs>,
    report: &sync_core::DebrisReport,
    vault_path: &std::path::Path,
) -> Result<(), String> {
    let planned_losers: usize = report
        .duplicate_groups
        .iter()
        .map(|g| g.alive_nodes.len().saturating_sub(1))
        .sum();

    println!("\n=== APPLY ===");
    println!(
        "Applying dedupe to {}: tombstoning up to {} loser node(s) across {} group(s) + {} relic(s).",
        vault_path.display(),
        planned_losers,
        report.duplicate_groups.len(),
        report.relics.len()
    );
    println!(
        "This mutates the registry. The app must be stopped and .sync backed up. \
         Folder dups ({}) are NOT touched (v1 scope-out).",
        report.folder_dups.len()
    );

    let stats = vault
        .apply_dedupe(report)
        .await
        .map_err(|e| format!("dedupe apply failed: {e}"))?;

    println!(
        "\nTombstoned {} loser node(s) across {} group(s) + {} relic(s).",
        stats.nodes_tombstoned, stats.groups_deduped, stats.relics_tombstoned
    );
    Ok(())
}

/// Pretty-print the debris report. Per node we show: state / TreeID peer+counter / name /
/// doc_id / path / parent. Every node the report surfaces is alive (deleted nodes are not
/// debris), so state is always `alive`; name and parent are derived from the resolved path.
fn print_report(report: &sync_core::DebrisReport) {
    // The report is printed before any mutation. Whether this run mutates is decided by
    // the caller — the APPLY section (only with --apply) prints the scale warning + stats.
    println!("=== Registry debris scan ===\n");

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
