//! Characterization tests for the P4 vault → UUID-store MIGRATION.
//!
//! ## What the migration is
//!
//! `Vault::init` over a directory of materialized `.md` files IS the migration: it
//! creates `.sync/`, mints a fresh `VaultId`, builds a UUID-keyed Index, and walks
//! every `.md` (Flow-1) so each note gets a stable UUID, a `docs/<uuid>.loro`
//! content doc, and an Index node. There is NO net-new migration logic — these
//! tests pin the behavior that ALREADY exists.
//!
//! ## What these tests are
//!
//! These are CHARACTERIZATION / contract tests. They encode the cutover's safety
//! contract — the eight correctness assertions every migrated vault must satisfy —
//! and run them against synthetic edge-case vaults staged on `InMemoryFs`. They are
//! expected to pass green against current code; if an assertion ever fails, that is
//! a REAL FINDING about `Vault::init`, not a reason to weaken the assertion.
//!
//! No test here touches a real on-disk vault — everything runs against `InMemoryFs`.
//! The real-disk proof and the (gated) real-`~/notes`-COPY scale check live in
//! `crates/sync-daemon/tests/migration.rs`.
//!
//! ## The eight assertions (the cutover safety contract)
//!
//! 1. Every `.md` is indexed with a UUID (no dropped files).
//! 2. Counts match: N `.md` in → N file-nodes indexed.
//! 3. No duplicate UUIDs.
//! 4. Every node's content `.loro` exists on disk.
//! 5. No orphaned `.loro` (on-disk doc UUIDs == live-node UUIDs).
//! 6. Content preserved via the doc's NORMALIZED `content_hash` (NOT raw `.md`
//!    bytes — the migration normalizes, identical to a normal sync).
//! 7. Folder paths preserved (the `.md` set is exactly the on-disk `.md` set).
//! 8. Re-clone convergence: a second replica that loads a COPY of the migrated
//!    store converges with zero conflict files (the collision-firewall proof).

use std::collections::BTreeSet;
use std::sync::Arc;

use uuid::Uuid;
use vault_sync::index::StructuralNode;
use vault_sync::{ContentDoc, FileSystem, InMemoryFs, Vault, content_doc_path, content_hash};

/// A device's Loro author id (a Loro peer id). The migration mints UUIDs into the
/// content docs; the author only stamps Loro ops, so any stable value works.
const AUTHOR: u64 = 0x0101_0101_0101_0101;

/// A SECOND device's author id — the re-clone replica in assertion 8. Distinct from
/// `AUTHOR` so the converged version vector carries one entry per device (the
/// independent-authorship tripwire).
const AUTHOR_B: u64 = 0x0202_0202_0202_0202;

/// The `.sync/docs/` directory the migration writes content docs into — walked by
/// the no-orphan assertion to enumerate on-disk doc UUIDs.
const DOCS_DIR: &str = ".sync/docs";

// ============================ staging helpers ============================

/// Stage a `.md` tree on a fresh `InMemoryFs` and run the migration (`Vault::init`).
///
/// `files` is `(path, content)` pairs; `InMemoryFs::write` auto-creates parent dirs,
/// so a nested path like `a/b/c.md` materializes its folders. Returns the vault and
/// its retained `Arc<InMemoryFs>` so the test can inspect the on-disk `.sync/` store.
async fn migrate(files: &[(&str, &str)]) -> (Vault<Arc<InMemoryFs>>, Arc<InMemoryFs>) {
    let fs = Arc::new(InMemoryFs::new());
    for (path, content) in files {
        fs.write(path, content.as_bytes()).await.unwrap();
    }
    let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
    (fs, vault).swap()
}

/// Tiny tuple-swap so `migrate` can return `(vault, fs)` while building `(fs, vault)`
/// (the fs must be cloned before `init` consumes the original). Keeps the call sites
/// reading `let (vault, fs) = migrate(...)`.
trait Swap<A, B> {
    fn swap(self) -> (B, A);
}
impl<A, B> Swap<A, B> for (A, B) {
    fn swap(self) -> (B, A) {
        (self.1, self.0)
    }
}

// ============================ the eight assertions ============================

/// Every `File` node in the structural scan, as `(path, uuid)` — the raw input the
/// assertions iterate. Files only; folder nodes are checked separately.
fn file_nodes(vault: &Vault<Arc<InMemoryFs>>) -> Vec<(String, Uuid)> {
    vault
        .index()
        .scan_structural_nodes()
        .into_iter()
        .filter_map(|n| match n {
            StructuralNode::File { path, uuid } => Some((path, uuid)),
            StructuralNode::Folder { .. } => None,
        })
        .collect()
}

/// Run the full eight-assertion safety contract against a migrated vault.
///
/// `expected_md` is the set of `.md` paths the test staged on disk (assertion 7's
/// truth), and `originals` maps each `.md` path to its ORIGINAL on-disk bytes
/// (assertion 6 re-parses these to a normalized `content_hash`).
///
/// Assertion 8 (re-clone convergence) is exercised separately by
/// `assert_reclone_converges`, since it needs a second filesystem.
async fn assert_migration_contract(
    vault: &Vault<Arc<InMemoryFs>>,
    fs: &Arc<InMemoryFs>,
    expected_md: &BTreeSet<String>,
    originals: &[(&str, &str)],
) {
    let md_files: BTreeSet<String> = vault.list_files().await.unwrap().into_iter().collect();
    let nodes = file_nodes(vault);

    // ---- Assertion 1: every `.md` is indexed with a UUID (no dropped files) ----
    for path in &md_files {
        let node = vault
            .index()
            .node_for_path(path)
            .unwrap_or_else(|| panic!("assertion 1: no Index node for migrated file {path}"));
        assert!(
            vault.index().node_uuid(&node).is_some(),
            "assertion 1: node for {path} carries no UUID"
        );
    }

    // ---- Assertion 2: counts match (N `.md` in → N file-nodes indexed) ----
    assert_eq!(
        md_files.len(),
        nodes.len(),
        "assertion 2: file-node count must equal the `.md` count (no phantom or missing nodes)"
    );

    // ---- Assertion 3: no duplicate UUIDs ----
    let unique_uuids: BTreeSet<Uuid> = nodes.iter().map(|(_, uuid)| *uuid).collect();
    assert_eq!(
        unique_uuids.len(),
        nodes.len(),
        "assertion 3: every file node must have a distinct UUID (no collision)"
    );

    // ---- Assertion 4: every node's content `.loro` exists ----
    for (path, uuid) in &nodes {
        assert!(
            fs.exists(&content_doc_path(uuid)).await.unwrap(),
            "assertion 4: node {path} ({uuid}) has no docs/<uuid>.loro on disk"
        );
    }

    // ---- Assertion 5: no orphaned `.loro` (on-disk docs == live-node UUIDs) ----
    let on_disk_doc_uuids = on_disk_doc_uuids(fs).await;
    assert_eq!(
        on_disk_doc_uuids, unique_uuids,
        "assertion 5: the set of docs/*.loro UUIDs must equal the set of live-node UUIDs \
         (an extra doc is an orphan / dropped-file leak; a missing one is an unbacked node)"
    );

    // ---- Assertion 6: content preserved via NORMALIZED content_hash ----
    // The migration is a NORMALIZING round-trip (leading newlines after frontmatter
    // stripped, frontmatter keys sorted) — identical to what a sync does on every
    // edit. So the contract is "the doc's logical content equals what the original
    // `.md` parses to," NOT a raw-byte comparison.
    let originals_by_path: std::collections::HashMap<&str, &str> =
        originals.iter().copied().collect();
    for (path, _uuid) in &nodes {
        let original = originals_by_path
            .get(path.as_str())
            .unwrap_or_else(|| panic!("assertion 6: no staged original for migrated path {path}"));
        let expected = ContentDoc::from_markdown(original, AUTHOR).unwrap();
        let actual = vault.get_document(path).await.unwrap();
        assert_eq!(
            content_hash(&actual),
            content_hash(&expected),
            "assertion 6: migrated content at {path} must match the original's NORMALIZED \
             content_hash (the migration normalizes whitespace/frontmatter-order — benign)"
        );
    }

    // ---- Assertion 7: folder structure / paths preserved ----
    assert_eq!(
        &md_files, expected_md,
        "assertion 7: the migrated `.md` path set must exactly equal the on-disk `.md` set"
    );
}

/// The set of UUIDs of every `docs/*.loro` on disk — the input to the no-orphan
/// assertion. Lists `.sync/docs/`, strips the `.loro` extension, parses each stem as
/// a UUID. A non-UUID entry would be a structural surprise, so parsing is asserted.
async fn on_disk_doc_uuids(fs: &Arc<InMemoryFs>) -> BTreeSet<Uuid> {
    let entries = match fs.list(DOCS_DIR).await {
        Ok(entries) => entries,
        // An empty vault legitimately has no docs dir contents; treat as the empty set.
        Err(_) => return BTreeSet::new(),
    };
    entries
        .into_iter()
        .filter(|e| !e.is_dir && e.name.ends_with(".loro"))
        .map(|e| {
            let stem = e.name.strip_suffix(".loro").unwrap();
            Uuid::parse_str(stem)
                .unwrap_or_else(|_| panic!("a docs/ entry is not a UUID-named .loro: {}", e.name))
        })
        .collect()
}

/// Assertion 8 — the re-clone convergence proof (the collision firewall).
///
/// Models the cutover's re-clone procedure: a second replica COPIES umbra's migrated
/// engine store (`.sync/index.loro` + `docs/` + `metadata.toml`) plus the already-
/// present `.md` tree, then loads it under a DIFFERENT author. Because the store is
/// copied (not independently rebuilt), every note shares one UUID across replicas, so
/// the two converge to byte-identical state with ZERO conflict files.
///
/// Contrast with an INDEPENDENT rebuild (a second `Vault::init` over the same `.md`
/// tree): that mints fresh, different UUIDs per note → mass path-collision cascade.
/// This test proves the COPY path is collision-free; `independent_rebuild_*` below
/// proves the rebuild path is the hazard the runbook forbids.
async fn assert_reclone_converges(source_fs: &Arc<InMemoryFs>) {
    // The re-clone: a byte-for-byte copy of the source vault's entire filesystem
    // (the `.md` tree + the migrated `.sync/` engine store). `cp` in the runbook.
    let clone_fs = Arc::new(clone_fs(source_fs).await);

    // The clone boots via `Vault::load` (its `.sync/index.loro` already exists), under
    // a distinct device author. No re-index, no fresh UUIDs — it adopts the copied
    // catalog wholesale.
    let clone = Vault::load(Arc::clone(&clone_fs), AUTHOR_B).await.unwrap();
    let source = Vault::load(Arc::clone(source_fs), AUTHOR).await.unwrap();

    // Both replicas already hold the same UUIDs (the clone copied them), so they are
    // converged before any sync: identical `.md` sets, byte-identical content, and —
    // the headline — identical Index version vectors with zero conflict files.
    let source_md: BTreeSet<String> = source.list_files().await.unwrap().into_iter().collect();
    let clone_md: BTreeSet<String> = clone.list_files().await.unwrap().into_iter().collect();
    assert_eq!(
        source_md, clone_md,
        "assertion 8: the re-clone has the identical `.md` path set (no conflict files)"
    );
    for path in &source_md {
        assert!(
            !path.contains("(conflict "),
            "assertion 8: a conflict file appeared at {path} — the re-clone collided"
        );
    }

    let source_uuids: BTreeSet<Uuid> = file_nodes(&source).into_iter().map(|(_, u)| u).collect();
    let clone_uuids: BTreeSet<Uuid> = file_nodes(&clone).into_iter().map(|(_, u)| u).collect();
    assert_eq!(
        source_uuids, clone_uuids,
        "assertion 8: the re-clone shares every note's UUID (the anti-collision guarantee)"
    );

    assert_eq!(
        source.index().state_vv(),
        clone.index().state_vv(),
        "assertion 8: the re-clone has the identical Index version vector — fully converged"
    );
}

/// Byte-for-byte clone of an `InMemoryFs` (every file at the same path). The in-memory
/// stand-in for the runbook's `cp -r` of umbra's vault onto a laptop.
async fn clone_fs(source: &Arc<InMemoryFs>) -> InMemoryFs {
    let dest = InMemoryFs::new();
    for path in all_files(source).await {
        let bytes = source.read(&path).await.unwrap();
        dest.write(&path, &bytes).await.unwrap();
    }
    dest
}

/// Every file path in an `InMemoryFs`, including hidden / `.sync` files (so the clone
/// copies the engine store, not just the `.md` tree). Recursive directory walk via
/// the `FileSystem` `list` API.
async fn all_files(fs: &Arc<InMemoryFs>) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let entries = match fs.list(&dir).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries {
            let path = if dir.is_empty() {
                entry.name.clone()
            } else {
                format!("{dir}/{}", entry.name)
            };
            if entry.is_dir {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// The set of `.md` paths from a `(path, content)` staging list — assertion 7's
/// expected truth, derived directly from what the test wrote.
fn md_path_set(files: &[(&str, &str)]) -> BTreeSet<String> {
    files
        .iter()
        .filter(|(p, _)| p.ends_with(".md"))
        .map(|(p, _)| p.to_string())
        .collect()
}

// ============================ the edge-case battery ============================

mod nested_folders {
    use super::*;

    /// A vault with deep + sibling folder trees migrates with every `.md` indexed and
    /// every path preserved exactly — no folder flattening, no path mangling.
    #[tokio::test]
    async fn deep_and_sibling_trees_preserve_every_path() {
        let files = &[
            ("a/b/c/deep.md", "# Deep\n\nNested three levels."),
            ("a/b/sibling.md", "# Sibling\n\nTwo levels."),
            ("a/top.md", "# Top\n\nOne level."),
            ("other/tree/leaf.md", "# Leaf\n\nA different tree."),
            ("root.md", "# Root\n\nAt the vault root."),
        ];
        let (vault, fs) = migrate(files).await;

        assert_migration_contract(&vault, &fs, &md_path_set(files), files).await;
        assert_reclone_converges(&fs).await;

        // Folder nodes exist for the non-empty directories that hold `.md` files —
        // assertion 7's structural half (nested folders are reflected as folder nodes).
        let folder_paths: BTreeSet<String> = vault
            .index()
            .scan_structural_nodes()
            .into_iter()
            .filter_map(|n| match n {
                StructuralNode::Folder { path, .. } => Some(path),
                StructuralNode::File { .. } => None,
            })
            .collect();
        for expected in ["a", "a/b", "a/b/c", "other", "other/tree"] {
            assert!(
                folder_paths.contains(expected),
                "a folder node must exist for the non-empty directory {expected}, got {folder_paths:?}"
            );
        }
    }
}

mod empty_folders {
    use super::*;

    /// A genuinely EMPTY folder (no `.md` inside) is NOT tracked by the migration.
    ///
    /// `index_existing_files` walks only `.md` files (`list_files` filters to `.md`),
    /// so an empty directory mints no folder node. This is KNOWN, intended behavior —
    /// empty folders had no first-class tracking pre-migration either, and the cutover
    /// runbook flags it (OQ-2: empty folders are not preserved). The test pins it so a
    /// future change to `init` that started tracking empty folders would surface here.
    #[tokio::test]
    async fn empty_folder_is_not_tracked_by_the_migration() {
        let fs = Arc::new(InMemoryFs::new());
        // One real `.md` (so the vault isn't trivially empty) plus a genuinely empty dir.
        fs.write("notes/real.md", b"# Real\n\nHas content.")
            .await
            .unwrap();
        fs.mkdir("empty-dir").await.unwrap();
        fs.mkdir("notes/also-empty").await.unwrap();

        let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();

        // The `.md` migrated; the empty dirs produced no folder node.
        let folder_paths: BTreeSet<String> = vault
            .index()
            .scan_structural_nodes()
            .into_iter()
            .filter_map(|n| match n {
                StructuralNode::Folder { path, .. } => Some(path),
                StructuralNode::File { .. } => None,
            })
            .collect();
        assert!(
            !folder_paths.contains("empty-dir"),
            "a genuinely empty top-level folder must NOT be tracked by the migration (OQ-2)"
        );
        assert!(
            !folder_paths.contains("notes/also-empty"),
            "a genuinely empty nested folder must NOT be tracked by the migration (OQ-2)"
        );
        // The folder that DOES hold a `.md` is tracked (the contrast that proves the
        // assertion is about emptiness, not folders in general).
        assert!(
            folder_paths.contains("notes"),
            "the non-empty `notes/` folder IS tracked (it holds real.md)"
        );

        // The one real file still satisfies the full contract.
        let originals = &[("notes/real.md", "# Real\n\nHas content.")];
        let expected_md: BTreeSet<String> = ["notes/real.md".to_string()].into_iter().collect();
        assert_migration_contract(&vault, &fs, &expected_md, originals).await;
    }
}

mod unicode_and_special_filenames {
    use super::*;

    /// Unicode, emoji, and spaces in filenames migrate intact — indexed, UUID'd,
    /// content round-tripped. Path bytes are preserved verbatim (no normalization).
    #[tokio::test]
    async fn unicode_emoji_and_spaces_migrate_intact() {
        let files = &[
            ("notes/café ☕ überschrift.md", "# Café\n\nUnicode title."),
            (
                "notes/with spaces in name.md",
                "# Spaces\n\nSpaces in filename.",
            ),
            ("emoji 🎉/party 🥳.md", "# Party\n\nEmoji folder and file."),
            ("日本語/ノート.md", "# 日本語\n\nJapanese path."),
        ];
        let (vault, fs) = migrate(files).await;

        assert_migration_contract(&vault, &fs, &md_path_set(files), files).await;
        assert_reclone_converges(&fs).await;
    }
}

mod large_files {
    use super::*;

    /// A multi-MB `.md` migrates with no truncation: its `docs/<uuid>.loro` is written
    /// and the NORMALIZED content_hash round-trips against the original (assertion 6).
    #[tokio::test]
    async fn large_file_content_round_trips_without_truncation() {
        // ~3 MB of repeated paragraphs — comfortably past any small-buffer boundary.
        let big_body = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(55_000);
        let big_md = format!("# Large note\n\n{big_body}");
        let files: &[(&str, &str)] = &[
            ("notes/large.md", &big_md),
            ("notes/small.md", "# Small\n\nTiny neighbor."),
        ];
        let (vault, fs) = migrate(files).await;

        assert_migration_contract(&vault, &fs, &md_path_set(files), files).await;
        assert_reclone_converges(&fs).await;
    }
}

mod case_variant_paths {
    use super::*;

    /// On a case-SENSITIVE filesystem (`InMemoryFs`), `Plans/x.md` and `plans/y.md`
    /// are two distinct files in two distinct folders — both migrate, each with its
    /// own UUID, no collision. (Real APFS is case-INSENSITIVE, so on the cutover
    /// machine `Plans/` and `plans/` are the SAME directory; the sync-daemon real-disk
    /// test reflects the actual fs semantics. This test documents the engine's
    /// behavior on a case-sensitive fs.)
    #[tokio::test]
    async fn case_distinct_dirs_migrate_as_separate_files() {
        let files = &[
            ("Plans/x.md", "# Plan X\n\nUnder Capital-P Plans."),
            ("plans/y.md", "# plan y\n\nUnder lowercase plans."),
            ("Notes/a.md", "# Note A\n\nCapital N."),
            ("notes/b.md", "# note b\n\nlowercase n."),
        ];
        let (vault, fs) = migrate(files).await;

        assert_migration_contract(&vault, &fs, &md_path_set(files), files).await;
        assert_reclone_converges(&fs).await;

        // Explicit: the case-variant pair carries DISTINCT UUIDs (no accidental merge).
        let by_path: std::collections::HashMap<String, Uuid> =
            file_nodes(&vault).into_iter().collect();
        assert_ne!(
            by_path["Plans/x.md"], by_path["plans/y.md"],
            "case-distinct files must have distinct UUIDs (no collision on a case-sensitive fs)"
        );
    }
}

mod frontmatter_normalization {
    use super::*;

    /// A `.md` with UNSORTED frontmatter keys and blank lines after the `---` fence
    /// migrates correctly: the NORMALIZED content_hash (assertion 6) matches, proving
    /// the migration's whitespace/key-order normalization is benign and expected.
    ///
    /// This is the load-bearing subtlety: a raw-byte comparison of the original `.md`
    /// against the materialized doc would FALSELY fail (keys reorder, blank lines
    /// strip). The contract is normalized-content equality — the same transform a sync
    /// applies on every edit (INV-4 deterministic convergence).
    #[tokio::test]
    async fn unsorted_frontmatter_and_blank_lines_normalize_benignly() {
        // Keys out of lexical order (zebra, apple, mango) AND extra blank lines after
        // the closing fence — both get normalized by `markdown::parse`/`serialize`.
        let messy = "---\nzebra: 1\napple: 2\nmango: 3\n---\n\n\n\n# Heading\n\nBody.";
        let files: &[(&str, &str)] = &[("notes/messy.md", messy)];
        let (vault, fs) = migrate(files).await;

        // The full contract — assertion 6 inside it does the normalized-hash check.
        assert_migration_contract(&vault, &fs, &md_path_set(files), files).await;

        // Make the normalization explicit and visible: the materialized markdown has
        // sorted keys and the collapsed blank-line run, and it is NOT byte-equal to the
        // messy original — yet assertion 6 (above) passed, which is the whole point.
        let materialized = vault
            .get_document("notes/messy.md")
            .await
            .unwrap()
            .to_markdown();
        assert_ne!(
            materialized, messy,
            "precondition: the migration genuinely normalized the messy original \
             (so a raw-byte assertion would have falsely failed — assertion 6 uses the \
             normalized content_hash instead)"
        );
        // Sorted-key order is observable in the materialized frontmatter.
        let apple = materialized.find("apple").expect("apple key present");
        let mango = materialized.find("mango").expect("mango key present");
        let zebra = materialized.find("zebra").expect("zebra key present");
        assert!(
            apple < mango && mango < zebra,
            "frontmatter keys must be emitted in sorted order (apple < mango < zebra)"
        );
    }
}

mod non_markdown_files_present {
    use super::*;

    /// Non-`.md` files in the tree (`.png`, `.txt`, `.DS_Store`) are SKIPPED by the
    /// migration: no Index node, no `.loro`, and the `.md` count is unaffected.
    /// `list_files` filters to `.md` and skips hidden entries, so these never enter the
    /// catalog — exactly as they were untracked pre-migration.
    #[tokio::test]
    async fn non_markdown_and_hidden_files_are_skipped() {
        let fs = Arc::new(InMemoryFs::new());
        // The two real notes that SHOULD migrate.
        fs.write("notes/keep.md", b"# Keep\n\nA real note.")
            .await
            .unwrap();
        fs.write("notes/also.md", b"# Also\n\nAnother real note.")
            .await
            .unwrap();
        // Non-`.md` and hidden files that must NOT migrate.
        fs.write("notes/image.png", b"\x89PNG\r\n\x1a\n binary-ish")
            .await
            .unwrap();
        fs.write("notes/readme.txt", b"plain text, not markdown")
            .await
            .unwrap();
        fs.write("notes/.DS_Store", b"\x00\x01 hidden macOS metadata")
            .await
            .unwrap();

        let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();

        let originals = &[
            ("notes/keep.md", "# Keep\n\nA real note."),
            ("notes/also.md", "# Also\n\nAnother real note."),
        ];
        let expected_md: BTreeSet<String> =
            ["notes/keep.md".to_string(), "notes/also.md".to_string()]
                .into_iter()
                .collect();

        // Only the two `.md` files migrated; the contract's count assertion (2) pins
        // that the `.png`/`.txt`/`.DS_Store` produced no phantom nodes.
        assert_migration_contract(&vault, &fs, &expected_md, originals).await;

        // Belt-and-suspenders: no node resolves to the non-`.md` paths.
        for skipped in ["notes/image.png", "notes/readme.txt", "notes/.DS_Store"] {
            assert!(
                vault.index().node_for_path(skipped).is_none(),
                "{skipped} must not be indexed (not a `.md`)"
            );
        }
    }
}

mod independent_rebuild_is_the_hazard {
    use super::*;

    /// The migration's central correctness constraint, stated as a NEGATIVE test: an
    /// INDEPENDENT rebuild of the same `.md` tree mints DIFFERENT UUIDs per note.
    ///
    /// This is the failure mode the re-clone procedure exists to prevent. Two
    /// `Vault::init` runs over byte-identical `.md` trees produce DISJOINT UUID sets,
    /// so syncing the two replicas would see every note as a distinct document at the
    /// same path → mass collision. The runbook's "laptops `cp` umbra's store, never
    /// run the migration" rule is what makes this impossible at cutover; this test
    /// pins WHY that rule exists (and is the mirror of assertion 8's positive proof).
    #[tokio::test]
    async fn independent_rebuilds_mint_disjoint_uuids() {
        let files = &[
            ("notes/one.md", "# One\n\nFirst note."),
            ("notes/two.md", "# Two\n\nSecond note."),
            ("a/b/three.md", "# Three\n\nNested note."),
        ];

        let (vault_a, _fs_a) = migrate(files).await;
        let (vault_b, _fs_b) = migrate(files).await;

        let uuids_a: BTreeSet<Uuid> = file_nodes(&vault_a).into_iter().map(|(_, u)| u).collect();
        let uuids_b: BTreeSet<Uuid> = file_nodes(&vault_b).into_iter().map(|(_, u)| u).collect();

        // Same paths on both replicas (the rebuilds index the same tree)...
        let paths_a: BTreeSet<String> = file_nodes(&vault_a).into_iter().map(|(p, _)| p).collect();
        let paths_b: BTreeSet<String> = file_nodes(&vault_b).into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths_a, paths_b, "both rebuilds index the same `.md` paths");

        // ...but the UUID sets are DISJOINT — the collision hazard the re-clone avoids.
        assert!(
            uuids_a.is_disjoint(&uuids_b),
            "two independent rebuilds must mint disjoint UUIDs — this is the mass-collision \
             hazard the runbook prevents by re-cloning instead of rebuilding per machine"
        );
    }
}
