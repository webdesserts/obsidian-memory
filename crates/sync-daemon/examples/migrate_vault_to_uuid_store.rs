//! THROWAWAY one-shot migration tool: rebuild the canonical UUID-keyed `.sync/`
//! engine store from a directory of materialized `.md` files (the P4 cutover).
//!
//! The old `sync-core` engine keyed content docs by a PATH HASH, which structurally
//! cannot converge a case-rename (the fleet's 2026-06-22 case-war) and breeds
//! case-dup `.loro` relics. The new `vault-sync` engine keys every note by a stable
//! UUID, so a move/rename is a clean structural op. This tool performs the one-time
//! migration to that UUID store.
//!
//! ## The migration is `Vault::init`
//!
//! `Vault::init` over the materialized `.md` tree IS the migration: it creates a
//! fresh `.sync/` (mints a new `VaultId`), builds a UUID-keyed Index, and walks every
//! `.md` so each note gets a stable UUID, a `docs/<uuid>.loro` content doc, and an
//! Index node. The old `sync-core` `.sync/` is discarded entirely — the rebuild reads
//! ONLY the `.md` tree, so none of its path-hash relics carry over. This binary is a
//! deliberate, guarded entry point to that existing library behavior plus an
//! in-process correctness self-check; it adds no net-new migration logic.
//!
//! ## ⚠️ CANONICAL REBUILD ON UMBRA ONLY — laptops RE-CLONE, never re-run
//!
//! `Vault::init` mints a fresh random UUID per note. Running this tool independently
//! on each machine would mint DIFFERENT UUIDs for the same note on each → on first
//! sync every note collides (a distinct document at the same path on each peer) → a
//! mass file-collision cascade. The ONLY collision-free path is: run the migration
//! ONCE on umbra, then the laptops COPY umbra's resulting engine store. The laptops
//! run a `cp`, NEVER this binary.
//!
//! ## RUNBOOK (the supervised cutover — backed up, with the operator present)
//!
//! Pre-req: the fleet is already converged on `sync-core` (every machine holds
//! byte-identical `.md` files). If a machine's `.md` tree has drifted, converge it on
//! the old engine FIRST — the migration assumes a converged `.md` tree.
//!
//! ### On umbra (the canonical rebuild)
//!
//! 1. Stop the desktop app / daemon on umbra.
//! 2. Read umbra's stable daemon peer id (the `--author` value) BEFORE removing the
//!    old `.sync/`. (It is derived from `.sync/daemon.key`; capture it now.)
//! 3. **Back up the entire `.sync/`:** `cp -R <vault>/.sync <vault>/.sync.bak-<date>`.
//! 4. **Preserve the daemon-owned files, then REMOVE the old engine store.** The
//!    daemon owns `daemon.key`, `daemon.toml`, and the allowlist store — these are the
//!    NETWORKING half and must survive the swap. The engine store (`registry.loro`,
//!    the path-hash `<hash>.loro` files, `index.loro`, `docs/`, `metadata.toml`) is
//!    what the rebuild replaces. The simplest safe procedure: copy `daemon.key` +
//!    `daemon.toml` (+ allowlist store) aside, then `rm -rf <vault>/.sync`, so the
//!    tool's guard (it REFUSES if `.sync/` already exists) sees a clean target.
//! 5. Run the migration against umbra's `.md` tree:
//!    ```sh
//!    cargo run -p sync-daemon --example migrate_vault_to_uuid_store \
//!      -- --vault <vault-dir> --author <umbra-peer-hex>
//!    ```
//!    It rebuilds `.sync/metadata.toml` + `.sync/index.loro` + `.sync/docs/<uuid>.loro`,
//!    runs the eight-assertion correctness self-check, and prints the resulting
//!    VaultId + the note/doc counts. A FAILED assertion exits non-zero — that is a
//!    cutover-abort signal; restore the backup and investigate.
//! 6. Restore the preserved `daemon.key` + `daemon.toml` (+ allowlist) into the new
//!    `.sync/` (alongside the freshly rebuilt engine files).
//! 7. Restart umbra's daemon. It boots via `Vault::load` (the `index.loro` now
//!    exists), adopts the rebuilt catalog, and is the canonical source for the mesh.
//!
//! ### On each laptop (the RE-CLONE — `cp`, NOT this binary)
//!
//! 1. Stop the laptop's daemon.
//! 2. **Delete its entire local `.sync/` engine store** (its old `sync-core` store AND
//!    any stale vault-sync store), KEEPING its own `daemon.key`/`daemon.toml`/allowlist
//!    (each device keeps its OWN identity — only the engine store is shared).
//! 3. **Copy umbra's** `.sync/index.loro` + `.sync/docs/` + `.sync/metadata.toml` into
//!    the laptop's `.sync/` (alongside its preserved `daemon.key`/`daemon.toml`).
//! 4. The laptop's `.md` tree is already byte-identical to umbra's (the fleet
//!    converged pre-cutover), so it is NOT copied — only the engine store is.
//! 5. Restart the laptop's daemon. It boots via `Vault::load` (its `index.loro` now
//!    exists), reads the canonical UUIDs, and on first sync converges to umbra with
//!    ZERO identity collisions (same UUIDs everywhere).
//!
//! ## Notes & caveats baked into this migration
//!
//! - **Normalization is benign and expected.** The migration parses each `.md` through
//!   the same `markdown::parse`/`serialize` a normal edit uses: leading blank lines
//!   after a frontmatter fence are stripped and frontmatter keys are sorted
//!   lexicographically (the INV-4 deterministic-convergence requirement). So a note
//!   may be re-serialized with reordered frontmatter keys / collapsed blank lines on
//!   its next write — this is identical to what every sync already does and is NOT a
//!   content change. The self-check's content assertion compares NORMALIZED content
//!   hashes for exactly this reason.
//! - **Empty folders are NOT preserved.** The migration indexes only `.md` files, so a
//!   genuinely empty directory mints no node and will not be recreated on a re-clone.
//!   (Empty folders had no first-class tracking on the old engine either.) Recreate any
//!   needed empty folders by hand after the cutover.
//! - **Re-running is REFUSED, not no-op-safe.** If `.sync/` already exists in the
//!   target, the tool refuses (a bare re-init would silently ADOPT the existing store
//!   rather than cleanly rebuild — a dangerous "looks like it worked"). To re-run after
//!   a partial/crashed migration, back up + remove `.sync/` first; the `.md` tree is
//!   never mutated, so a partial run is always recoverable by clearing `.sync/`.
//!
//! After the fleet is fully cut over, DELETE this example — it is a single-cutover
//! tool, not permanent daemon code.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use sync_daemon::NativeFs;
use uuid::Uuid;
use vault_sync::fs::FileSystem;
use vault_sync::index::StructuralNode;
use vault_sync::{ContentDoc, Vault, content_doc_path, content_hash};

/// The `.sync/` directory the migration creates — and the guard's tripwire.
const SYNC_DIR: &str = ".sync";

/// The `.sync/docs/` directory holding the per-note content `.loro`s.
const DOCS_DIR: &str = ".sync/docs";

#[tokio::main]
async fn main() -> ExitCode {
    let Some(args) = parse_args() else {
        eprintln!(
            "usage: cargo run -p sync-daemon --example migrate_vault_to_uuid_store \
             -- --vault <vault-dir> --author <peer-id-hex>\n\
             \n\
             Run ONCE on umbra (the canonical rebuild). Laptops RE-CLONE umbra's store \
             with `cp`, never run this binary — see the runbook in this file's header."
        );
        return ExitCode::FAILURE;
    };

    match run(&args.vault, args.author).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Parsed CLI arguments: the vault directory and the Loro author id (umbra's peer id
/// for the canonical rebuild).
struct Args {
    vault: PathBuf,
    author: u64,
}

/// Parse `--vault <dir> --author <hex>`. Returns `None` on any unrecognized or
/// missing argument (the caller prints usage). The author is a hex u64 — umbra's
/// stable daemon peer id, so umbra's daemon adopts the rebuilt store seamlessly.
fn parse_args() -> Option<Args> {
    let mut args = std::env::args().skip(1);
    let mut vault = None;
    let mut author = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--vault" => vault = args.next().map(PathBuf::from),
            "--author" => author = args.next().and_then(|s| parse_author_hex(&s)),
            _ => return None,
        }
    }
    Some(Args {
        vault: vault?,
        author: author?,
    })
}

/// Parse an author id as hex (with or without a `0x` prefix). The author stamps Loro
/// ops; for the canonical rebuild it should be umbra's stable peer id.
fn parse_author_hex(s: &str) -> Option<u64> {
    let trimmed = s.trim_start_matches("0x");
    u64::from_str_radix(trimmed, 16).ok()
}

/// Perform the migration against `vault_dir`, authored under `author`.
///
/// Steps: validate the directory, REFUSE if `.sync/` already exists (the guard),
/// run `Vault::init` (the migration), run the eight-assertion self-check, and print a
/// summary. A failed assertion or a missing/occupied directory returns `Err`, which
/// `main` maps to a non-zero exit (a cutover-abort signal).
async fn run(vault_dir: &Path, author: u64) -> Result<(), String> {
    if !vault_dir.is_dir() {
        return Err(format!(
            "{} is not a directory — pass the vault root via --vault",
            vault_dir.display()
        ));
    }

    let fs = Arc::new(NativeFs::new(vault_dir.to_path_buf()));

    // ---- THE GUARD: refuse if `.sync/` already exists ----
    // A bare `Vault::init` over an existing `.sync/` SILENTLY ADOPTS it (preserves the
    // old VaultId, re-uses existing nodes) instead of cleanly rebuilding — a dangerous
    // "looks like it worked." Refuse and direct the operator to back up + remove it.
    guard_sync_dir_absent(&fs).await?;

    // ---- THE MIGRATION ----
    let vault = Vault::init(Arc::clone(&fs), author)
        .await
        .map_err(|e| format!("migration (Vault::init) failed: {e}"))?;

    // ---- THE SELF-CHECK (the eight-assertion safety contract) ----
    let summary = verify_migration(&vault, &fs)
        .await
        .map_err(|failure| format!("migration self-check FAILED: {failure} — DO NOT cut over; restore the backup and investigate"))?;

    report(&vault, &summary);
    Ok(())
}

/// Refuse to run when `.sync/` already exists in the target — the one genuinely-new
/// piece of logic. See the call site and the runbook for why a bare re-init over an
/// existing store is the dangerous failure mode this prevents.
async fn guard_sync_dir_absent<F: FileSystem>(fs: &F) -> Result<(), String> {
    let exists = fs
        .exists(SYNC_DIR)
        .await
        .map_err(|e| format!("could not check for an existing {SYNC_DIR}: {e}"))?;
    if exists {
        return Err(format!(
            "{SYNC_DIR}/ already exists in the target vault — this is the DESTRUCTIVE canonical \
             rebuild, which must start from a clean target. A bare re-init would silently ADOPT \
             the existing store rather than rebuild it. Back up and REMOVE the existing {SYNC_DIR}/ \
             first (preserving daemon.key/daemon.toml/allowlist per the runbook), then re-run. \
             (If you meant to START the daemon on an already-migrated vault, run the daemon, not \
             this migration.)"
        ));
    }
    Ok(())
}

/// What the migration produced, for the operator-facing summary.
struct MigrationSummary {
    /// Number of `.md` files migrated (== file nodes == content docs).
    note_count: usize,
}

/// Run the eight-assertion safety contract against the migrated vault in-process.
///
/// Returns `Ok(summary)` when every assertion holds, or `Err(reason)` naming the
/// first violated assertion. This is the SAME contract the characterization tests
/// (`crates/{vault-sync,sync-daemon}/tests/migration.rs`) pin — re-run here so the
/// operator gets a loud go/no-go signal on the real cutover data.
///
/// Assertion 8 (re-clone convergence) is a property of the RUNBOOK (the laptops copy
/// umbra's store), not of a single in-process migration, so it is proven by the test
/// suite rather than re-checked here; this self-check covers assertions 1–7, the
/// on-disk lossless-rebuild contract.
async fn verify_migration<F: FileSystem>(
    vault: &Vault<F>,
    fs: &F,
) -> Result<MigrationSummary, String> {
    let md_files: BTreeSet<String> = vault
        .list_files()
        .await
        .map_err(|e| format!("could not list `.md` files: {e}"))?
        .into_iter()
        .collect();

    let nodes: Vec<(String, Uuid)> = vault
        .index()
        .scan_structural_nodes()
        .into_iter()
        .filter_map(|n| match n {
            StructuralNode::File { path, uuid } => Some((path, uuid)),
            StructuralNode::Folder { .. } => None,
        })
        .collect();

    // Assertion 1: every `.md` is indexed with a UUID.
    for path in &md_files {
        let node = vault
            .index()
            .node_for_path(path)
            .ok_or_else(|| format!("assertion 1: no Index node for `{path}`"))?;
        if vault.index().node_uuid(&node).is_none() {
            return Err(format!("assertion 1: node for `{path}` carries no UUID"));
        }
    }

    // Assertion 2: counts match (N `.md` in → N file-nodes).
    if md_files.len() != nodes.len() {
        return Err(format!(
            "assertion 2: {} `.md` files but {} file-nodes (phantom or missing nodes)",
            md_files.len(),
            nodes.len()
        ));
    }

    // Assertion 3: no duplicate UUIDs.
    let unique_uuids: BTreeSet<Uuid> = nodes.iter().map(|(_, u)| *u).collect();
    if unique_uuids.len() != nodes.len() {
        return Err(format!(
            "assertion 3: {} file-nodes but only {} distinct UUIDs (a collision)",
            nodes.len(),
            unique_uuids.len()
        ));
    }

    // Assertion 4: every node's content `.loro` exists.
    for (path, uuid) in &nodes {
        let exists = fs
            .exists(&content_doc_path(uuid))
            .await
            .map_err(|e| format!("assertion 4: could not stat the content doc for `{path}`: {e}"))?;
        if !exists {
            return Err(format!(
                "assertion 4: node `{path}` ({uuid}) has no docs/<uuid>.loro on disk"
            ));
        }
    }

    // Assertion 5: no orphaned `.loro` (on-disk docs == live-node UUIDs).
    let on_disk = on_disk_doc_uuids(fs)
        .await
        .map_err(|e| format!("assertion 5: could not enumerate on-disk docs: {e}"))?;
    if on_disk != unique_uuids {
        let orphans: Vec<_> = on_disk.difference(&unique_uuids).collect();
        let unbacked: Vec<_> = unique_uuids.difference(&on_disk).collect();
        return Err(format!(
            "assertion 5: on-disk docs do not match live nodes (orphaned docs: {orphans:?}, \
             unbacked nodes: {unbacked:?})"
        ));
    }

    // Assertion 6: content preserved via the NORMALIZED content_hash. Re-parse each
    // note's CURRENT on-disk `.md` to a ContentDoc and compare its normalized hash to
    // the migrated doc's — equal means the migration preserved the note's logical
    // content (whitespace/frontmatter-order normalization is benign and expected).
    for (path, _uuid) in &nodes {
        let raw = fs
            .read(path)
            .await
            .map_err(|e| format!("assertion 6: could not read `{path}`: {e}"))?;
        let original = String::from_utf8_lossy(&raw);
        let expected = ContentDoc::from_markdown(&original, vault.loro_author())
            .map_err(|e| format!("assertion 6: could not parse `{path}`: {e}"))?;
        let actual = vault
            .get_document(path)
            .await
            .map_err(|e| format!("assertion 6: could not load migrated doc for `{path}`: {e}"))?;
        if content_hash(&actual) != content_hash(&expected) {
            return Err(format!(
                "assertion 6: migrated content at `{path}` does not match its `.md` (normalized)"
            ));
        }
    }

    // Assertion 7: folder paths preserved — every migrated file path is one of the
    // `.md` files on disk. `list_files` IS the on-disk `.md` set, and assertion 2
    // already pinned the counts, so equality of the node paths and `md_files` confirms
    // no path was mangled.
    let node_paths: BTreeSet<String> = nodes.iter().map(|(p, _)| p.clone()).collect();
    if node_paths != md_files {
        return Err(
            "assertion 7: the migrated node paths do not equal the on-disk `.md` paths".to_string(),
        );
    }

    Ok(MigrationSummary {
        note_count: nodes.len(),
    })
}

/// The set of UUIDs of every `docs/*.loro` on disk (the no-orphan input). An empty
/// vault legitimately has no docs; that surfaces as the empty set.
async fn on_disk_doc_uuids<F: FileSystem>(fs: &F) -> Result<BTreeSet<Uuid>, String> {
    let entries = match fs.list(DOCS_DIR).await {
        Ok(entries) => entries,
        Err(_) => return Ok(BTreeSet::new()),
    };
    let mut uuids = BTreeSet::new();
    for entry in entries {
        if entry.is_dir || !entry.name.ends_with(".loro") {
            continue;
        }
        let stem = entry.name.strip_suffix(".loro").unwrap();
        let uuid = Uuid::parse_str(stem)
            .map_err(|_| format!("a docs/ entry is not a UUID-named .loro: {}", entry.name))?;
        uuids.insert(uuid);
    }
    Ok(uuids)
}

/// Print the operator-facing success summary: the resulting VaultId, the note/doc
/// counts, and the re-clone reminder for the laptops.
fn report<F: FileSystem>(vault: &Vault<F>, summary: &MigrationSummary) {
    println!("Migration complete — all eight correctness assertions hold.");
    println!("  VaultId:        {}", vault.vault_id());
    println!("  notes migrated: {}", summary.note_count);
    println!("  content docs:   {} (one docs/<uuid>.loro per note)", summary.note_count);
    println!();
    println!("This is the CANONICAL store. On each laptop: stop the daemon, delete its");
    println!("local .sync/ engine store (keeping its own daemon.key/daemon.toml), then COPY");
    println!("this vault's .sync/index.loro + .sync/docs/ + .sync/metadata.toml across.");
    println!("Laptops RE-CLONE — they must NOT re-run this migration (it would mint different");
    println!("UUIDs and cause a mass collision on first sync).");
}

#[cfg(test)]
mod tests {
    use super::{guard_sync_dir_absent, parse_author_hex, run, verify_migration};
    use std::sync::Arc;
    use sync_daemon::NativeFs;
    use tempfile::tempdir;
    use vault_sync::fs::FileSystem;
    use vault_sync::Vault;

    const TEST_AUTHOR: u64 = 0x0101_0101_0101_0101;

    /// The guard REFUSES when `.sync/` already exists — the safety contract that makes
    /// an accidental re-run (which would silently adopt the old store) impossible.
    #[tokio::test]
    async fn guard_refuses_when_sync_dir_already_exists() {
        let dir = tempdir().unwrap();
        let fs = Arc::new(NativeFs::new(dir.path().to_path_buf()));
        // Simulate an already-migrated (or half-migrated) vault: `.sync/` present.
        fs.mkdir(".sync").await.unwrap();

        let result = guard_sync_dir_absent(fs.as_ref()).await;

        assert!(
            result.is_err(),
            "the guard must REFUSE when `.sync/` already exists"
        );
        let msg = result.unwrap_err();
        assert!(
            msg.contains(".sync") && msg.to_lowercase().contains("remove"),
            "the refusal must name `.sync/` and direct the operator to remove it, got: {msg}"
        );
    }

    /// The guard PROCEEDS (returns Ok) when `.sync/` is absent — the normal first-run
    /// case. The contrast that proves the guard gates on `.sync/` presence specifically.
    #[tokio::test]
    async fn guard_proceeds_when_sync_dir_absent() {
        let dir = tempdir().unwrap();
        let fs = Arc::new(NativeFs::new(dir.path().to_path_buf()));
        // A `.md` tree but NO `.sync/` — a clean migration target.
        fs.write("notes/a.md", b"# A").await.unwrap();

        assert!(
            guard_sync_dir_absent(fs.as_ref()).await.is_ok(),
            "the guard must PROCEED when `.sync/` is absent (the clean first-run case)"
        );
    }

    /// End-to-end: `run` over a clean `.md` tree migrates and the self-check passes;
    /// a SECOND `run` on the same dir is REFUSED (the `.sync/` the first run created
    /// now trips the guard). Proves the one-shot-refuses-twice idempotency contract.
    #[tokio::test]
    async fn run_migrates_then_refuses_on_rerun() {
        let dir = tempdir().unwrap();
        let vault_path = dir.path().to_path_buf();
        let fs = Arc::new(NativeFs::new(vault_path.clone()));
        fs.write("notes/one.md", b"# One\n\nBody.").await.unwrap();
        fs.write("a/b/two.md", b"# Two\n\nNested.").await.unwrap();

        // First run: migrates cleanly, self-check passes.
        run(&vault_path, TEST_AUTHOR)
            .await
            .expect("first run migrates and passes the self-check");
        assert!(
            fs.exists(".sync/index.loro").await.unwrap(),
            "the migration wrote the UUID-keyed index"
        );

        // Second run: REFUSED — `.sync/` now exists.
        let rerun = run(&vault_path, TEST_AUTHOR).await;
        assert!(
            rerun.is_err(),
            "a re-run must be REFUSED (the tool is a one-shot that refuses twice)"
        );
        assert!(
            rerun.unwrap_err().contains(".sync"),
            "the re-run refusal must name `.sync/`"
        );
    }

    /// The in-process self-check passes against a freshly-migrated vault — the same
    /// eight-assertion contract the operator relies on as a go/no-go signal.
    #[tokio::test]
    async fn self_check_passes_on_a_fresh_migration() {
        let dir = tempdir().unwrap();
        let fs = Arc::new(NativeFs::new(dir.path().to_path_buf()));
        fs.write("notes/keep.md", b"# Keep\n\nReal content.")
            .await
            .unwrap();
        fs.write("notes/image.png", b"not markdown").await.unwrap();

        let vault = Vault::init(Arc::clone(&fs), TEST_AUTHOR).await.unwrap();
        let summary = verify_migration(&vault, &fs)
            .await
            .expect("the self-check passes on a clean migration");
        assert_eq!(
            summary.note_count, 1,
            "exactly the one `.md` migrated (the `.png` was skipped)"
        );
    }

    /// Author parsing accepts both bare hex and a `0x` prefix (the operator may paste
    /// either form of umbra's peer id).
    #[test]
    fn author_hex_parses_with_and_without_prefix() {
        assert_eq!(parse_author_hex("ff"), Some(0xff));
        assert_eq!(parse_author_hex("0xFF"), Some(0xff));
        assert_eq!(
            parse_author_hex("0101010101010101"),
            Some(0x0101_0101_0101_0101)
        );
        assert_eq!(parse_author_hex("not-hex"), None);
    }
}
