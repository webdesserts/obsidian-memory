//! Fix-3 receiver-convergence acceptance tests (the 2-machine arbiter) plus the
//! crash-mid-rename guard, run against the case-INSENSITIVE [`CaseInsensitiveFs`]
//! double (APFS model). `InMemoryFs` is case-sensitive and cannot reproduce the
//! ping-pong / data-loss this surface guards, so these tests use the double.
//!
//! The headline property is **CONTENT SURVIVES**: a folder case-rename
//! (`Plans/ → plans/`) propagated to a peer must NOT vanish the peer's files on the
//! same-inode write+delete the naive move-apply would perform. It also asserts the
//! ping-pong is dead (both disks converge to lowercase, the sweep goes quiet).

mod common;

use std::sync::Arc;

use common::case_insensitive_fs::CaseInsensitiveFs;
use uuid::Uuid;
use vault_sync::{FileSystem, Vault};

/// The UUID a path currently resolves to (panics if no live node).
fn uuid_at(vault: &CiVault, path: &str) -> Uuid {
    let node = vault
        .index()
        .node_for_path(path)
        .unwrap_or_else(|| panic!("no Index node for {path}"));
    vault
        .index()
        .node_uuid(&node)
        .unwrap_or_else(|| panic!("node for {path} carries no UUID"))
}

const AUTHOR_A: u64 = 0x0101_0101_0101_0101;
const AUTHOR_B: u64 = 0x0202_0202_0202_0202;

type CiFs = Arc<CaseInsensitiveFs>;
type CiVault = Vault<CiFs>;

/// Two vaults over independent case-insensitive filesystems.
async fn two_case_insensitive_vaults() -> (CiVault, CiVault, CiFs, CiFs) {
    let fs_a = Arc::new(CaseInsensitiveFs::new());
    let fs_b = Arc::new(CaseInsensitiveFs::new());
    let a = Vault::init(Arc::clone(&fs_a), AUTHOR_A).await.unwrap();
    let b = Vault::init(Arc::clone(&fs_b), AUTHOR_B).await.unwrap();
    (a, b, fs_a, fs_b)
}

/// Pump the full handshake A→B then B→A to quiescence (mirrors `common::sync_both_ways`
/// but for the `CaseInsensitiveFs`-backed vaults).
async fn sync_both_ways(a: &CiVault, b: &CiVault) {
    full_sync(a, b).await;
    full_sync(b, a).await;
}

async fn full_sync(a: &CiVault, b: &CiVault) {
    let mut payload = a.prepare_request().await.unwrap();
    let mut receiver_is_b = true;
    loop {
        let receiver = if receiver_is_b { b } else { a };
        let outcome = receiver.process_message(&payload).await.unwrap();
        match outcome.reply {
            Some(next) => {
                payload = next;
                receiver_is_b = !receiver_is_b;
            }
            None => break,
        }
    }
}

/// The display casing of every `.md` path on disk (what the sweep would list).
async fn disk_md_paths(fs: &CiFs) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for entry in fs.list(&dir).await.unwrap() {
            let path = if dir.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", dir, entry.name)
            };
            if path.starts_with(".sync") || path.starts_with('.') {
                continue;
            }
            if entry.is_dir {
                dirs.push(path);
            } else if path.ends_with(".md") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Sanity-check the double: case-insensitive resolution, case-preserving listing, and
/// the APFS directory two-step requirement. If the double is wrong, the arbiter test's
/// red/green signal is meaningless — so pin its behavior first.
#[tokio::test]
async fn case_insensitive_fs_double_models_apfs() {
    let fs = CaseInsensitiveFs::new();

    // Case-insensitive resolution: a write under `Foo/` is readable as `foo/`.
    fs.write("Foo/x.md", b"hello").await.unwrap();
    assert_eq!(fs.read("foo/x.md").await.unwrap(), b"hello");
    assert!(fs.exists("FOO/X.MD").await.unwrap());

    // Case-preserving listing: `list` returns the stored display casing, not lowercase.
    let root = fs.list("").await.unwrap();
    assert!(
        root.iter().any(|e| e.name == "Foo" && e.is_dir),
        "list preserves the stored `Foo` casing"
    );

    // A single-step case-only DIRECTORY rename is a no-op (APFS); the casing stays.
    fs.rename("Foo", "foo").await.unwrap();
    let root = fs.list("").await.unwrap();
    assert!(
        root.iter().any(|e| e.name == "Foo"),
        "a single-step case-only directory rename is a no-op (still `Foo`)"
    );

    // The two-step DOES converge: Foo -> Foo.casemv-tmp -> foo.
    fs.rename("Foo", "Foo.casemv-tmp").await.unwrap();
    fs.rename("Foo.casemv-tmp", "foo").await.unwrap();
    let root = fs.list("").await.unwrap();
    assert!(
        root.iter().any(|e| e.name == "foo"),
        "the two-step rename converges the directory to lowercase"
    );
    // The child rode along and is still readable with its content intact.
    assert_eq!(fs.read("foo/x.md").await.unwrap(), b"hello");
}

/// 🔴 BINDING — the 2-machine arbiter. A case-renames a folder `Plans/ → plans/`;
/// after propagation to B (and B's own sweep), BOTH vaults' content survives, both
/// converge to the `plans/` index path, both disks physically read `plans/`, and B's
/// sweep emits NO reverse `plans → Plans` move (the ping-pong is dead).
#[tokio::test]
async fn case_only_folder_rename_converges_without_data_loss() {
    let (a, b, fs_a, fs_b) = two_case_insensitive_vaults().await;

    // Both converge on `Plans/a.md` + `Plans/b.md` (same UUIDs).
    fs_a.write("Plans/a.md", b"alpha body").await.unwrap();
    fs_a.write("Plans/b.md", b"bravo body").await.unwrap();
    a.on_file_changed("Plans/a.md").await.unwrap();
    a.on_file_changed("Plans/b.md").await.unwrap();
    a.save_index().await.unwrap();
    sync_both_ways(&a, &b).await;

    let uuid_a_before = uuid_at(&a, "Plans/a.md");
    let uuid_b_before = uuid_at(&b, "Plans/a.md");
    assert_eq!(uuid_a_before, uuid_b_before, "precondition: UUIDs agree");

    // A case-renames the folder: disk relocates `Plans/* → plans/*` via the two-step,
    // and the Index re-homes the subtree (the sweep's `move_subtree`).
    fs_a.rename("Plans", "Plans.casemv-tmp").await.unwrap();
    fs_a.rename("Plans.casemv-tmp", "plans").await.unwrap();
    a.index().move_subtree("Plans", "plans").unwrap();
    a.save_index().await.unwrap();

    // Propagate A's rename to B.
    sync_both_ways(&a, &b).await;

    // B runs its own case-drift sweep — the step that, on the naive code, emits the
    // reverse `plans → Plans` (the ping-pong). After the fix it finds nothing.
    let b_drift = b.index().detect_case_drift(&disk_md_paths(&fs_b).await);
    assert!(
        b_drift.is_empty(),
        "B's sweep emits NO reverse move — the ping-pong is dead (got {:?})",
        b_drift
    );

    // A second round produces no further structural change (quiescence).
    sync_both_ways(&b, &a).await;

    // 🔴 ASSERTION #1 (BINDING): CONTENT SURVIVES on BOTH vaults.
    assert_eq!(
        fs_a.read("plans/a.md").await.unwrap(),
        b"alpha body",
        "A's content survives the case rename"
    );
    assert_eq!(
        fs_a.read("plans/b.md").await.unwrap(),
        b"bravo body",
        "A's second file survives"
    );
    assert_eq!(
        fs_b.read("plans/a.md").await.unwrap(),
        b"alpha body",
        "B's content survives the propagated case rename (no same-inode vanish)"
    );
    assert_eq!(
        fs_b.read("plans/b.md").await.unwrap(),
        b"bravo body",
        "B's second file survives"
    );

    // UUIDs preserved on both (no ghost mint).
    assert_eq!(
        uuid_at(&a, "plans/a.md"),
        uuid_a_before,
        "A keeps the original UUID"
    );
    assert_eq!(
        uuid_at(&b, "plans/a.md"),
        uuid_b_before,
        "B keeps the original UUID — no ghost mint"
    );

    // Both disks physically read `plans/` (the CaseInsensitiveFs display casing
    // converged — catches the receiver write-into-`Plans/` bug).
    let a_paths = disk_md_paths(&fs_a).await;
    let b_paths = disk_md_paths(&fs_b).await;
    assert_eq!(
        a_paths,
        vec!["plans/a.md".to_string(), "plans/b.md".to_string()],
        "A's physical casing is lowercase"
    );
    assert_eq!(
        b_paths,
        vec!["plans/a.md".to_string(), "plans/b.md".to_string()],
        "B's physical casing converged to lowercase (no stale `Plans/`)"
    );
}

/// DEFENSE-IN-DEPTH INVARIANT GUARD — pins the skip-guard's binding rule "a case-only
/// move must NEVER reach the downstream write+delete loop" against a future refactor that
/// reintroduces the data loss. This is NOT a live data-loss reproduction: the
/// `.loro`-absent state it constructs is effectively UNREACHABLE for a case-only move in
/// normal prod. A document's `.md` and its `<uuid>.loro` are created together (materialize
/// renders loro→md; a local create writes md→loro) and tombstoned together (the
/// `vacated.deleted` branch deletes BOTH), so md-present-without-loro arises only from
/// crash-residue or store corruption — never from a normal case-only-move flow. The test
/// therefore manufactures that abnormal state cheaply to keep the guard honest.
///
/// Why the guard matters: the write+delete loop is `remove_md_file(from)` then
/// `rematerialize_moved_md(to)`; on a case-insensitive volume `from` and `to` are ONE
/// physical inode, so the remove deletes the file and re-materialize is the only thing
/// that can bring it back — from the local `<uuid>.loro`. With the `.loro` absent,
/// `rematerialize` early-returns and the removed `.md` is GONE. The fix skips every
/// case-only move (`is_case_only`) before that loop, so a failed receiver rename leaves
/// the disk untouched — no write+delete, no data loss.
///
/// Real failure behavior (stated honestly): on a failed receiver rename the skip-guard
/// leaves the disk at its old casing; this receiver does NOT re-converge. The case-drift
/// sweep is DISK-AS-TRUTH (it emits `old=index_path → new=disk_path`), so the next sweep
/// REVERTS the move fleet-wide back toward the still-old-cased disk — tracked in the
/// separate receiver-side index-as-truth re-convergence ticket. No data loss either way.
///
/// This test puts B in the `.loro`-absent state, forces B's rename to fail, propagates
/// the case rename, and asserts B's file SURVIVES. RED without the fix (the file
/// vanishes via remove-then-failed-rematerialize); GREEN with it.
#[tokio::test]
async fn case_only_move_rename_failure_preserves_content() {
    let (a, b, fs_a, fs_b) = two_case_insensitive_vaults().await;

    // Both converge on `Plans/a.md` + `Plans/b.md` (same UUIDs).
    fs_a.write("Plans/a.md", b"alpha body").await.unwrap();
    fs_a.write("Plans/b.md", b"bravo body").await.unwrap();
    a.on_file_changed("Plans/a.md").await.unwrap();
    a.on_file_changed("Plans/b.md").await.unwrap();
    a.save_index().await.unwrap();
    sync_both_ways(&a, &b).await;

    // Precondition: B physically holds both files with content.
    assert_eq!(fs_b.read("Plans/a.md").await.unwrap(), b"alpha body");
    assert_eq!(fs_b.read("Plans/b.md").await.unwrap(), b"bravo body");

    // Put B's `a.md` document in the "never materialized the .loro locally" state by
    // removing its content `.loro`. This is the documented `rematerialize_moved_md`
    // early-return branch — so if the failed-rename move falls through to the
    // remove-then-rematerialize loop, the remove deletes the `.md` and nothing
    // re-creates it.
    let uuid_a = uuid_at(&b, "Plans/a.md");
    let loro_a = vault_sync::index::content_doc_path(&uuid_a);
    fs_b.delete(&loro_a).await.unwrap();

    // A case-renames the folder `Plans/ → plans/`.
    fs_a.rename("Plans", "Plans.casemv-tmp").await.unwrap();
    fs_a.rename("Plans.casemv-tmp", "plans").await.unwrap();
    a.index().move_subtree("Plans", "plans").unwrap();
    a.save_index().await.unwrap();

    // B's filesystem rejects every vault-content rename — model an EACCES/EXDEV/transient
    // I/O error exactly when the receiver tries to converge the propagated case-only move.
    fs_b.set_fail_rename(true);

    // Propagate A's rename to B. B's `converge_case_only_moves` rename fails; the fix
    // must keep the files off the same-inode write+delete path rather than destroy them.
    sync_both_ways(&a, &b).await;

    // 🔴 ASSERTION (BINDING): B's `a.md` SURVIVES the rename failure even though its
    // `.loro` was absent — it was never routed through the destructive write+delete loop.
    // Read case-insensitively (the casing may be unconverged, which is fine; the bytes
    // must be intact).
    assert_eq!(
        fs_b.read("Plans/a.md").await.unwrap(),
        b"alpha body",
        "B's file survives the rename failure (never reached the data-losing write+delete)"
    );
    assert_eq!(
        fs_b.read("Plans/b.md").await.unwrap(),
        b"bravo body",
        "B's second file survives the rename failure"
    );

    // The UUIDs are intact too — the document was never tombstoned. B's index re-homed
    // the node to `plans/a.md` (the move applied in-memory); only the on-disk casing is
    // unconverged because the fs rename failed.
    assert_eq!(
        uuid_at(&b, "plans/a.md"),
        uuid_at(&a, "plans/a.md"),
        "B keeps the original UUID — the failed rename did not delete the node"
    );
}

/// A crash BETWEEN the two directory-rename steps leaves a stray `Plans.casemv-tmp/`
/// on disk (step 1 ran, step 2 did not), and the persisted index is the pre-apply
/// state (`Plans/`). Without a boot guard, the stray dir's children would look like
/// brand-new files to reconcile → it would mint ghost UUIDs for them (the pollution
/// class this whole change is fixing). The boot sweep must detect the stray, complete
/// the interrupted rename so the children survive at a real tracked path, and NOT let
/// reconcile ghost on them — NEVER losing the children.
#[tokio::test]
async fn crash_mid_two_step_rename_recovers_children_no_ghost_mint() {
    let fs = Arc::new(CaseInsensitiveFs::new());
    let uuid_a;
    let uuid_b;
    {
        let vault = Vault::init(Arc::clone(&fs), AUTHOR_A).await.unwrap();
        fs.write("Plans/a.md", b"alpha body").await.unwrap();
        fs.write("Plans/b.md", b"bravo body").await.unwrap();
        vault.on_file_changed("Plans/a.md").await.unwrap();
        vault.on_file_changed("Plans/b.md").await.unwrap();
        vault.save_index().await.unwrap();
        uuid_a = uuid_at(&vault, "Plans/a.md");
        uuid_b = uuid_at(&vault, "Plans/b.md");

        // Simulate a crash AFTER step 1 of the two-step (`Plans -> Plans.casemv-tmp`)
        // but BEFORE step 2 and before the index was re-saved: the index still records
        // `Plans/`, while disk holds the stray `Plans.casemv-tmp/`.
        fs.rename("Plans", "Plans.casemv-tmp").await.unwrap();
    }

    // Boot recovery: the sweep runs before reconcile.
    let reloaded = Vault::load(Arc::clone(&fs), AUTHOR_A).await.unwrap();

    // The children survive with their ORIGINAL UUIDs — no ghost mint.
    let live = disk_md_paths(&fs).await;
    assert!(
        !live.iter().any(|p| p.contains(".casemv-tmp")),
        "no stray `.casemv-tmp` directory remains on disk (got {:?})",
        live
    );
    assert_eq!(live.len(), 2, "both children survive (got {:?})", live);

    // Resolve each surviving child and assert its UUID is the original (not ghosted).
    for path in &live {
        let node = reloaded.index().node_for_path(path).unwrap_or_else(|| {
            panic!("recovered child {path} has no live index node");
        });
        let u = reloaded.index().node_uuid(&node).unwrap();
        assert!(
            u == uuid_a || u == uuid_b,
            "recovered child {path} kept an original UUID (no ghost mint); got {u}"
        );
    }

    // Content is intact for both children.
    let bodies: Vec<Vec<u8>> = {
        let mut v = Vec::new();
        for p in &live {
            v.push(fs.read(p).await.unwrap());
        }
        v.sort();
        v
    };
    let mut expected = vec![b"alpha body".to_vec(), b"bravo body".to_vec()];
    expected.sort();
    assert_eq!(
        bodies, expected,
        "both children's content survives the crash recovery"
    );
}
